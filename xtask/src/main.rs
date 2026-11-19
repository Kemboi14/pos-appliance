//! Build orchestration. Run `cargo xtask help`.

mod image;
mod qemu;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use sha2::Digest;

pub type Res<T> = Result<T, Box<dyn std::error::Error>>;

const EFI_TARGET: &str = "x86_64-unknown-uefi";
const IMAGE_MIB: u64 = 256;
const ESP_MIB: u64 = 64;

#[derive(Debug, Default, Clone)]
struct Flags {
    release: bool,
    debug_console: bool,
    headless: bool,
    keys: Option<PathBuf>,
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Err(e) = run(&args) {
        eprintln!("xtask: error: {e}");
        std::process::exit(1);
    }
}

fn run(args: &[String]) -> Res<()> {
    let cmd = args.first().map(String::as_str).unwrap_or("help");
    let mut f = Flags::default();
    let mut it = args.iter().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--release" => f.release = true,
            "--debug-console" => f.debug_console = true,
            "--headless" => f.headless = true,
            "--keys" => f.keys = Some(PathBuf::from(it.next().ok_or("--keys needs a dir")?)),
            other => return Err(format!("unknown flag {other}").into()),
        }
    }
    match cmd {
        "build" => build_efi(&f).map(|_| ()),
        "image" => image(&f).map(|_| ()),
        "qemu" => {
            let img = image(&f)?;
            let mut child = qemu::spawn(&qemu::QemuOpts {
                image: img,
                media: Some(media_image()?),
                out_dir: out_dir()?,
                headless: f.headless,
                qmp: None,
                reset_vars: true,
                secure_vars: None,
            })?;
            child.wait()?;
            Ok(())
        }
        "smoke" => smoke(&f),
        "shot" => shot(&f),
        "e2e" => e2e(),
        "keys" => keys(&keys_dir(&f)?),
        "sign" => sign(&keys_dir(&f)?).map(|_| ()),
        "sb-test" => sb_test(&keys_dir(&f)?),
        "repro" => repro(),
        "test" => host_tests(),
        "lint" => lint(),
        "ci" => {
            lint()?;
            host_tests()?;
            smoke(&Flags::default())?;
            e2e()?;
            repro()
        }
        _ => {
            eprintln!(
                "usage: cargo xtask <cmd> [--release] [--debug-console] [--headless]\n\
                 \n\
                 build    build BOOTX64.EFI into out/\n\
                 image    build out/pos.img (GPT: ESP + POS data partition)\n\
                 qemu     boot out/pos.img in QEMU/OVMF (printer bytes -> out/printer.bin)\n\
                 smoke    headless boot, screendump, check the UI came up\n\
                 e2e      QEMU end-to-end: setup, import, sale, power cut, reboot, export\n\
                 shot     boot headless and save a screendump (out/shot.ppm)\n\
                 repro    build the release image twice from clean and compare hashes\n\
                 test     host test suite (core/store/ui/escpos/app)\n\
                 lint     rustfmt --check + clippy -D warnings (host and UEFI)\n\
                 keys     generate Secure Boot PK/KEK/db (keys/, private keys git-ignored)\n\
                 sign     sign the release BOOTX64.EFI and build out/pos-signed.img\n\
                 sb-test  OVMF with Secure Boot enforced: signed boots, unsigned refused\n\
                 ci       lint + test + smoke + e2e + repro"
            );
            Ok(())
        }
    }
}

fn root() -> Res<PathBuf> {
    let m = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    Ok(m.parent().ok_or("no workspace root")?.to_path_buf())
}

fn out_dir() -> Res<PathBuf> {
    let d = root()?.join("out");
    std::fs::create_dir_all(&d)?;
    Ok(d)
}

fn source_date_epoch() -> i64 {
    std::env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1_700_000_000)
}

fn sh(cmd: &mut Command) -> Res<()> {
    eprintln!("+ {cmd:?}");
    let st = cmd.status()?;
    if !st.success() {
        return Err(format!("command failed ({st}): {cmd:?}").into());
    }
    Ok(())
}

fn sha256_hex(data: &[u8]) -> String {
    sha2::Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Release images must not contain debug features.
fn check_release_features(f: &Flags) -> Res<()> {
    if f.release && f.debug_console {
        return Err("--debug-console is not allowed in --release builds".into());
    }
    Ok(())
}

fn build_efi_in(f: &Flags, target_dir: &Path) -> Res<PathBuf> {
    check_release_features(f)?;
    let root = root()?;
    let home = std::env::var("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|_| std::env::var("HOME").map(|h| PathBuf::from(h).join(".cargo")))?;
    let rustflags = format!(
        "--remap-path-prefix={}=/cargo --remap-path-prefix={}=/build -C link-arg=/Brepro",
        home.display(),
        root.display()
    );
    let mut cmd = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()));
    cmd.current_dir(&root)
        .env("CARGO_ENCODED_RUSTFLAGS", rustflags.replace(' ', "\x1f"))
        .env("CARGO_TARGET_DIR", target_dir)
        .env("SOURCE_DATE_EPOCH", source_date_epoch().to_string())
        .args([
            "build",
            "--locked",
            "-p",
            "platform-uefi",
            "--target",
            EFI_TARGET,
        ]);
    if f.release {
        cmd.arg("--release");
    }
    if f.debug_console {
        cmd.args(["--features", "debug-console"]);
    }
    sh(&mut cmd)?;
    let prof = if f.release { "release" } else { "debug" };
    Ok(target_dir.join(EFI_TARGET).join(prof).join("pos-uefi.efi"))
}

fn build_efi(f: &Flags) -> Res<PathBuf> {
    let efi = build_efi_in(f, &root()?.join("target"))?;
    let out = out_dir()?.join("BOOTX64.EFI");
    std::fs::copy(&efi, &out)?;
    println!("BOOTX64.EFI sha256 {}", sha256_hex(&std::fs::read(&out)?));
    Ok(out)
}

/// Two blank, unbound superblocks (pos-store format). The device binds the
/// store to itself and assigns a random store id on first boot.
fn data_partition_init() -> Res<Vec<u8>> {
    let l = image::layout(IMAGE_MIB, ESP_MIB);
    pos_store::format_image(image::SECTOR as usize, l.data_sectors)
        .map_err(|e| format!("format data partition: {e:?}").into())
}

fn make_image(efi: &[u8], out: &Path) -> Res<()> {
    let l = image::build_image(
        out,
        IMAGE_MIB,
        ESP_MIB,
        efi,
        &data_partition_init()?,
        source_date_epoch(),
    )?;
    eprintln!(
        "image: {} MiB, ESP @LBA {} ({} sectors), POS data @LBA {} ({} sectors, type {})",
        IMAGE_MIB,
        l.esp_first,
        l.esp_sectors,
        l.data_first,
        l.data_sectors,
        image::POS_DATA_TYPE_GUID
    );
    Ok(())
}

fn image(f: &Flags) -> Res<PathBuf> {
    let efi_path = build_efi(f)?;
    let efi = std::fs::read(&efi_path)?;
    let out = out_dir()?.join("pos.img");
    make_image(&efi, &out)?;
    println!("pos.img sha256 {}", sha256_hex(&std::fs::read(&out)?));
    Ok(out)
}

fn media_image() -> Res<PathBuf> {
    let p = out_dir()?.join("media.img");
    image::build_media_image(&p, 64, &[], source_date_epoch())?;
    Ok(p)
}

fn host_tests() -> Res<()> {
    sh(Command::new("cargo").current_dir(root()?).args([
        "test",
        "--locked",
        "--workspace",
        "--exclude",
        "platform-uefi",
    ]))
}

fn lint() -> Res<()> {
    let r = root()?;
    sh(Command::new("cargo")
        .current_dir(&r)
        .args(["fmt", "--all", "--check"]))?;
    sh(Command::new("cargo").current_dir(&r).args([
        "clippy",
        "--locked",
        "--workspace",
        "--exclude",
        "platform-uefi",
        "--all-targets",
        "--",
        "-D",
        "warnings",
    ]))?;
    sh(Command::new("cargo").current_dir(&r).args([
        "clippy",
        "--locked",
        "-p",
        "platform-uefi",
        "--target",
        EFI_TARGET,
        "--",
        "-D",
        "warnings",
    ]))?;
    sh(Command::new("cargo").current_dir(&r).args([
        "clippy",
        "--locked",
        "-p",
        "platform-uefi",
        "--target",
        EFI_TARGET,
        "--features",
        "debug-console",
        "--",
        "-D",
        "warnings",
    ]))
}

/// Boot headless and wait until the POS UI is on screen (status bar colour
/// at the top-left, app background below it).
fn smoke(f: &Flags) -> Res<()> {
    let img = image(f)?;
    let out = out_dir()?;
    let qmp_path = out.join("qmp.sock");
    let mut child = qemu::spawn(&qemu::QemuOpts {
        image: img,
        media: None,
        out_dir: out.clone(),
        headless: true,
        qmp: Some(qmp_path.clone()),
        reset_vars: true,
        secure_vars: None,
    })?;
    let res = (|| -> Res<()> {
        let mut q = qemu::Qmp::connect(&qmp_path, Duration::from_secs(10))?;
        let (w, h) = wait_ui(&mut q, &out, Duration::from_secs(90))?;
        println!("smoke: OK - POS UI up at {w}x{h}");
        q.quit()
    })();
    stop_qemu(&mut child);
    res
}

const UI_PANEL: u32 = 0x0022_2A35;
const UI_BG: u32 = 0x0016_1B22;

fn wait_ui(q: &mut qemu::Qmp, out: &Path, timeout: Duration) -> Res<(usize, usize)> {
    let shot = out.join("ui.ppm");
    let deadline = std::time::Instant::now() + timeout;
    loop {
        std::thread::sleep(Duration::from_millis(700));
        q.screendump(&shot)?;
        std::thread::sleep(Duration::from_millis(200));
        if let Ok(img) = qemu::read_ppm(&shot) {
            let (w, h) = (img.0, img.1);
            if qemu::pixel(&img, 2, 2) == UI_PANEL && qemu::pixel(&img, 2, h / 2) == UI_BG {
                return Ok((w, h));
            }
        }
        if std::time::Instant::now() > deadline {
            return Err("POS UI did not appear (see out/ui.ppm)".into());
        }
    }
}

/// Build the release image twice in separate clean target dirs and compare.
fn repro() -> Res<()> {
    let f = Flags {
        release: true,
        ..Flags::default()
    };
    let tmp = out_dir()?.join("repro");
    let _ = std::fs::remove_dir_all(&tmp);
    let mut hashes = Vec::new();
    for i in 0..2 {
        // Same path both times (paths can leak into the binary), but wiped
        // in between so the second build is from scratch.
        let td = tmp.join("target");
        let _ = std::fs::remove_dir_all(&td);
        let efi = std::fs::read(build_efi_in(&f, &td)?)?;
        let img = tmp.join(format!("pos{i}.img"));
        make_image(&efi, &img)?;
        hashes.push((sha256_hex(&efi), sha256_hex(&std::fs::read(&img)?)));
    }
    println!("build 1: efi {} img {}", hashes[0].0, hashes[0].1);
    println!("build 2: efi {} img {}", hashes[1].0, hashes[1].1);
    if hashes[0] != hashes[1] {
        return Err("repro: builds differ".into());
    }
    let _ = std::fs::remove_dir_all(&tmp);
    println!("repro: OK - bit-identical");
    Ok(())
}

/// Give QEMU a few seconds to exit after a QMP `quit`, then kill it.
fn stop_qemu(child: &mut std::process::Child) {
    for _ in 0..50 {
        if let Ok(Some(_)) = child.try_wait() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Boot headless, wait, take a screendump to out/shot.ppm (debugging aid).
fn shot(f: &Flags) -> Res<()> {
    let img = image(f)?;
    let out = out_dir()?;
    let qmp_path = out.join("qmp.sock");
    let mut child = qemu::spawn(&qemu::QemuOpts {
        image: img,
        media: Some(media_image()?),
        out_dir: out.clone(),
        headless: true,
        qmp: Some(qmp_path.clone()),
        reset_vars: true,
        secure_vars: None,
    })?;
    let res = (|| -> Res<()> {
        let mut q = qemu::Qmp::connect(&qmp_path, Duration::from_secs(10))?;
        let secs: u64 = std::env::var("SHOT_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(15);
        std::thread::sleep(Duration::from_secs(secs));
        q.screendump(&out.join("shot.ppm"))?;
        std::thread::sleep(Duration::from_millis(500));
        q.quit()
    })();
    stop_qemu(&mut child);
    res
}

const E2E_CATALOG: &str = "sku,barcode,plu,name,price,tax_code,weighable,quick_key\r\n\
MILK,4006381333931,,Milk 1L,1.20,A,0,0\r\n\
BREAD,96385074,,Bread,2.49,A,0,1\r\n";

/// M5 acceptance in QEMU/OVMF:
/// boot 1: first-time setup, sign in, open shift, import catalog from the
///   second USB stick, sell one item for cash (receipt + drawer kick must
///   reach the printer), start a second sale and cut power (QMP quit) right
///   after confirming payment;
/// boot 2 (same NVRAM, same disks): sign in, export to USB;
/// then check on the host: journal CSV has a gap-free receipt sequence that
///   includes receipt 1, its hash chain column is populated, and the printer
///   capture holds receipt #1.
fn e2e() -> Res<()> {
    // E2E_DEBUG=1 runs a debug build with the serial debug console.
    let f = if std::env::var_os("E2E_DEBUG").is_some() {
        Flags {
            debug_console: true,
            ..Flags::default()
        }
    } else {
        Flags {
            release: true,
            ..Flags::default()
        }
    };
    let img = image(&f)?;
    let out = out_dir()?;
    let media = out.join("media.img");
    image::build_media_image(
        &media,
        64,
        &[("CATALOG.CSV", E2E_CATALOG.as_bytes())],
        source_date_epoch(),
    )?;
    let qmp_path = out.join("qmp.sock");
    let human = Duration::from_millis(120);
    let opts = |reset_vars| qemu::QemuOpts {
        image: img.clone(),
        media: Some(media.clone()),
        out_dir: out.clone(),
        headless: true,
        qmp: Some(qmp_path.clone()),
        reset_vars,
        secure_vars: None,
    };
    let pause = |ms| std::thread::sleep(Duration::from_millis(ms));

    // ---------------- boot 1
    let mut child = qemu::spawn(&opts(true))?;
    let r1 = (|| -> Res<()> {
        let mut q = qemu::Qmp::connect(&qmp_path, Duration::from_secs(10))?;
        wait_ui(&mut q, &out, Duration::from_secs(90))?;
        q.type_str("Alice\n", human)?;
        q.type_str("1234\n", human)?;
        q.type_str("1234\n", human)?;
        pause(1500);
        q.type_str("1\n", human)?;
        q.type_str("1234\n", human)?;
        pause(1500);
        q.screendump(&out.join("e2e_0_login.ppm"))?;
        q.type_str("100.00\n", human)?;
        pause(500);
        q.key("f10", &[])?;
        pause(300);
        q.type_str("8", human)?; // import catalog
        pause(2000);
        q.screendump(&out.join("e2e_1_import.ppm"))?;
        q.type_str("4006381333931\n", human)?;
        pause(300);
        q.key("f1", &[])?;
        pause(300);
        q.key("ret", &[])?; // exact cash
        pause(2500);
        q.screendump(&out.join("e2e_2_sale.ppm"))?;
        q.key("esc", &[])?;
        pause(300);
        q.type_str("96385074\n", human)?;
        pause(300);
        q.key("f1", &[])?;
        pause(300);
        q.key("ret", &[])?;
        // Power cut: no grace period.
        q.quit()
    })();
    stop_qemu(&mut child);
    r1?;
    let printed = std::fs::read(out.join("printer.bin"))?;
    let has = |needle: &[u8]| printed.windows(needle.len()).any(|w| w == needle);
    if !has(b"Receipt #1") {
        return Err("e2e: receipt #1 not captured on the printer port".into());
    }
    if !has(&[0x1B, 0x70, 0x00, 25, 250]) {
        return Err("e2e: no cash-drawer kick on the printer port".into());
    }
    std::fs::copy(out.join("printer.bin"), out.join("printer_boot1.bin"))?;

    // ---------------- boot 2
    let mut child = qemu::spawn(&opts(false))?;
    let r2 = (|| -> Res<()> {
        let mut q = qemu::Qmp::connect(&qmp_path, Duration::from_secs(10))?;
        wait_ui(&mut q, &out, Duration::from_secs(90))?;
        q.type_str("1\n", human)?;
        q.type_str("1234\n", human)?;
        pause(1500);
        q.key("f10", &[])?;
        pause(300);
        q.type_str("7", human)?; // export
        pause(4000);
        q.screendump(&out.join("e2e_after_export.ppm"))?;
        pause(300);
        q.quit()
    })();
    stop_qemu(&mut child);
    r2?;

    // ---------------- host-side checks
    let journal = (1..100_000u64)
        .find_map(|seq| {
            image::read_media_file(&media, &format!("J{seq:08}.CSV"))
                .ok()
                .flatten()
        })
        .ok_or("e2e: no journal CSV exported (see out/e2e_after_export.ppm)")?;
    let text = String::from_utf8(journal)?;
    let mut receipts = Vec::new();
    let mut seqs = Vec::new();
    for line in text.lines().skip(1) {
        let cols: Vec<&str> = line.split(',').collect();
        if cols.len() < 8 {
            return Err(format!("e2e: malformed journal row {line}").into());
        }
        seqs.push(cols[0].parse::<u64>()?);
        if cols[2].is_empty() || cols[1].len() != 64 {
            return Err(format!("e2e: missing time/hash in {line}").into());
        }
        if cols[3] == "SALE" {
            receipts.push(cols[4].parse::<u64>()?);
        }
    }
    if seqs.windows(2).any(|w| w[1] != w[0] + 1) {
        return Err(format!("e2e: journal sequence has gaps: {seqs:?}").into());
    }
    if receipts.first() != Some(&1) || receipts.windows(2).any(|w| w[1] != w[0] + 1) {
        return Err(format!("e2e: receipt sequence wrong: {receipts:?}").into());
    }
    println!(
        "e2e: OK - {} journal records, sales {:?} (sale 2 {} the power cut), receipt + drawer kick captured",
        seqs.len(),
        receipts,
        if receipts.contains(&2) {
            "committed before"
        } else {
            "lost to"
        }
    );
    Ok(())
}

// ------------------------------------------------------------ Secure Boot

fn keys_dir(f: &Flags) -> Res<PathBuf> {
    Ok(match &f.keys {
        Some(k) => k.clone(),
        None => root()?.join("keys"),
    })
}

fn run_ok(cmd: &mut Command) -> Res<()> {
    sh(cmd.stdout(std::process::Stdio::null()))
}

/// Generate a self-contained Secure Boot hierarchy:
/// PK (platform owner) → KEK → db (signs BOOTX64.EFI). RSA-2048/SHA-256,
/// which every UEFI 2.3.1+ firmware accepts. Produces PEM keys/certs, DER
/// certs (for firmware setup UIs) and signed .auth updates (for
/// KeyTool/firmware "append from file" enrollment).
fn keys(dir: &Path) -> Res<()> {
    std::fs::create_dir_all(dir)?;
    if dir.join("db.key").exists() {
        println!(
            "keys: {} already contains keys; not overwriting",
            dir.display()
        );
        return Ok(());
    }
    let guid_path = dir.join("GUID.txt");
    let guid = match std::fs::read_to_string(&guid_path) {
        Ok(g) => g.trim().to_string(),
        Err(_) => {
            let out = Command::new("python3")
                .args(["-c", "import uuid;print(uuid.uuid4())"])
                .output()?;
            let g = String::from_utf8(out.stdout)?.trim().to_string();
            std::fs::write(&guid_path, format!("{g}\n"))?;
            g
        }
    };
    for (name, cn) in [
        ("PK", "POS Platform Key"),
        ("KEK", "POS Key Exchange Key"),
        ("db", "POS Signing Key"),
    ] {
        run_ok(Command::new("openssl").current_dir(dir).args([
            "req",
            "-new",
            "-x509",
            "-newkey",
            "rsa:2048",
            "-sha256",
            "-days",
            "3650",
            "-nodes",
            "-subj",
            &format!("/CN={cn}/"),
            "-keyout",
            &format!("{name}.key"),
            "-out",
            &format!("{name}.crt"),
        ]))?;
        run_ok(Command::new("openssl").current_dir(dir).args([
            "x509",
            "-in",
            &format!("{name}.crt"),
            "-outform",
            "DER",
            "-out",
            &format!("{name}.cer"),
        ]))?;
        run_ok(Command::new("cert-to-efi-sig-list").current_dir(dir).args([
            "-g",
            &guid,
            &format!("{name}.crt"),
            &format!("{name}.esl"),
        ]))?;
    }
    // PK and KEK are signed by PK; db by KEK.
    for (var, signer) in [("PK", "PK"), ("KEK", "PK"), ("db", "KEK")] {
        run_ok(Command::new("sign-efi-sig-list").current_dir(dir).args([
            "-g",
            &guid,
            "-k",
            &format!("{signer}.key"),
            "-c",
            &format!("{signer}.crt"),
            var,
            &format!("{var}.esl"),
            &format!("{var}.auth"),
        ]))?;
    }
    #[cfg(unix)]
    for k in ["PK.key", "KEK.key", "db.key"] {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.join(k), std::fs::Permissions::from_mode(0o600))?;
    }
    println!(
        "keys: generated PK/KEK/db in {} (owner GUID {guid})",
        dir.display()
    );
    println!("keys: keep *.key OFFLINE; only *.cer/*.auth go to the till firmware");
    Ok(())
}

/// Sign the release EFI with db and build out/pos-signed.img.
fn sign(dir: &Path) -> Res<PathBuf> {
    if !dir.join("db.key").exists() {
        return Err(format!("no keys in {}; run `cargo xtask keys` first", dir.display()).into());
    }
    let efi = build_efi(&Flags {
        release: true,
        ..Flags::default()
    })?;
    let out = out_dir()?;
    let signed = out.join("BOOTX64.signed.EFI");
    let _ = std::fs::remove_file(&signed);
    run_ok(Command::new("sbsign").args([
        "--key",
        &dir.join("db.key").display().to_string(),
        "--cert",
        &dir.join("db.crt").display().to_string(),
        "--output",
        &signed.display().to_string(),
        &efi.display().to_string(),
    ]))?;
    run_ok(Command::new("sbverify").args([
        "--cert",
        &dir.join("db.crt").display().to_string(),
        &signed.display().to_string(),
    ]))?;
    let img = out.join("pos-signed.img");
    make_image(&std::fs::read(&signed)?, &img)?;
    println!(
        "signed BOOTX64.EFI sha256 {}",
        sha256_hex(&std::fs::read(&signed)?)
    );
    println!(
        "pos-signed.img sha256 {}",
        sha256_hex(&std::fs::read(&img)?)
    );
    Ok(img)
}

/// Enroll our PK/KEK/db (no Microsoft keys) into a Secure Boot OVMF
/// varstore with enforcement on; the signed image must boot to the POS UI,
/// the unsigned one must be refused by the firmware.
fn sb_test(dir: &Path) -> Res<()> {
    keys(dir)?;
    let signed = sign(dir)?;
    let out = out_dir()?;
    let guid = std::fs::read_to_string(dir.join("GUID.txt"))?
        .trim()
        .to_string();
    let vars = out.join("sb_vars.fd");
    let _ = std::fs::remove_file(&vars);
    sh(Command::new("virt-fw-vars")
        .arg("--input")
        .arg(std::env::var("OVMF_SB_VARS").map_err(|_| "OVMF_SB_VARS not set (nix develop)")?)
        .args(["--set-pk", &guid, &dir.join("PK.crt").display().to_string()])
        .args([
            "--add-kek",
            &guid,
            &dir.join("KEK.crt").display().to_string(),
        ])
        .args(["--add-db", &guid, &dir.join("db.crt").display().to_string()])
        .args([
            "--no-microsoft",
            "--sb",
            "--output",
            &vars.display().to_string(),
        ]))?;
    let qmp_path = out.join("qmp.sock");
    let opts = |image: PathBuf| qemu::QemuOpts {
        image,
        media: None,
        out_dir: out.clone(),
        headless: true,
        qmp: Some(qmp_path.clone()),
        reset_vars: true,
        secure_vars: Some(vars.clone()),
    };
    // Signed: must reach the UI.
    let mut child = qemu::spawn(&opts(signed))?;
    let r = (|| -> Res<()> {
        let mut q = qemu::Qmp::connect(&qmp_path, Duration::from_secs(10))?;
        wait_ui(&mut q, &out, Duration::from_secs(120))?;
        q.quit()
    })();
    stop_qemu(&mut child);
    r.map_err(|e| format!("sb-test: signed image did not boot: {e}"))?;
    println!("sb-test: signed image boots with Secure Boot enforced");
    // Unsigned: must NOT reach the UI.
    let unsigned = out.join("pos-unsigned.img");
    make_image(
        &std::fs::read(build_efi(&Flags {
            release: true,
            ..Flags::default()
        })?)?,
        &unsigned,
    )?;
    let mut child = qemu::spawn(&opts(unsigned))?;
    let r = (|| -> Res<bool> {
        let mut q = qemu::Qmp::connect(&qmp_path, Duration::from_secs(10))?;
        let booted = wait_ui(&mut q, &out, Duration::from_secs(45)).is_ok();
        q.quit()?;
        Ok(booted)
    })();
    stop_qemu(&mut child);
    if r? {
        return Err("sb-test: UNSIGNED image booted with Secure Boot on".into());
    }
    let log = std::fs::read(out.join("console.log")).unwrap_or_default();
    let denied =
        log.windows(13).any(|w| w == b"Access Denied") || log.windows(9).any(|w| w == b"Violation");
    println!(
        "sb-test: unsigned image refused{}",
        if denied {
            " (firmware reported Access Denied / Security Violation)"
        } else {
            ""
        }
    );
    Ok(())
}
