//! Build orchestration. Run `cargo xtask help`.

mod image;
mod qemu;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use sha2::Digest;

pub type Res<T> = Result<T, Box<dyn std::error::Error>>;

const EFI_TARGET: &str = "x86_64-unknown-uefi";
const KERNEL_TARGET: &str = "x86_64-unknown-none";
const IMAGE_MIB: u64 = 256;
const ESP_MIB: u64 = 64;

#[derive(Debug, Default, Clone)]
struct Flags {
    release: bool,
    debug_console: bool,
    headless: bool,
    /// Tier B: Limine + pos-kernel instead of the UEFI application.
    kernel: bool,
    /// QEMU: all USB devices behind a hub.
    hub: bool,
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
            "--kernel" => f.kernel = true,
            "--hub" => f.hub = true,
            "--keys" => f.keys = Some(PathBuf::from(it.next().ok_or("--keys needs a dir")?)),
            other => return Err(format!("unknown flag {other}").into()),
        }
    }
    match cmd {
        "build" if f.kernel => build_kernel(&f).map(|_| ()),
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
                hub: f.hub,
                media_unplugged: false,
            })?;
            child.wait()?;
            Ok(())
        }
        "smoke" => smoke(&f),
        "shot" => shot(&f),
        "e2e" => e2e(f.kernel, f.hub),
        "keys" => keys(&keys_dir(&f)?),
        "sign" if f.kernel => sign_kernel(&keys_dir(&f)?).map(|_| ()),
        "sign" => sign(&keys_dir(&f)?).map(|_| ()),
        "sb-test" => sb_test(&keys_dir(&f)?, f.kernel),
        "repro" => repro(),
        "test" => host_tests(),
        "lint" => lint(),
        "tamper-test" => tamper_test(),
        "key-test" => key_test(),
        "ci" => {
            lint()?;
            host_tests()?;
            smoke(&Flags::default())?;
            smoke(&Flags {
                kernel: true,
                ..Flags::default()
            })?;
            tamper_test()?;
            key_test()?;
            e2e(false, false)?;
            e2e(true, true)?;
            // Ephemeral keys: never sign test images with keys/ (which may
            // hold the production db key).
            let ci_keys = out_dir()?.join("ci-keys");
            sb_test(&ci_keys, false)?;
            sb_test(&ci_keys, true)?;
            repro()
        }
        _ => {
            eprintln!(
                "usage: cargo xtask <cmd> [--release] [--debug-console] [--headless]\n\
                 \n\
                 --kernel (build/image/qemu/smoke/shot): Tier B, Limine + pos-kernel\n\
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
                 tamper-test  Tier B: modified kernel or limine.conf must not boot\n\
                 key-test Tier B: stage 0 creates the store key, then hands it over\n\
                 ci       lint, test, smoke/e2e/sb-test (A and B), tamper, key, repro"
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
    build_uefi_bin(f, target_dir, "platform-uefi", "pos-uefi", f.debug_console)
}

/// Build a UEFI application of this workspace (Tier A app or Tier B stage 0).
fn build_uefi_bin(
    f: &Flags,
    target_dir: &Path,
    pkg: &str,
    bin: &str,
    debug_console: bool,
) -> Res<PathBuf> {
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
        .args(["build", "--locked", "-p", pkg, "--target", EFI_TARGET]);
    if f.release {
        cmd.arg("--release");
    }
    if debug_console {
        cmd.args(["--features", "debug-console"]);
    }
    sh(&mut cmd)?;
    let prof = if f.release { "release" } else { "debug" };
    Ok(target_dir
        .join(EFI_TARGET)
        .join(prof)
        .join(format!("{bin}.efi")))
}

fn build_efi(f: &Flags) -> Res<PathBuf> {
    let efi = build_efi_in(f, &root()?.join("target"))?;
    let out = out_dir()?.join("BOOTX64.EFI");
    std::fs::copy(&efi, &out)?;
    println!("BOOTX64.EFI sha256 {}", sha256_hex(&std::fs::read(&out)?));
    Ok(out)
}

fn build_kernel(f: &Flags) -> Res<PathBuf> {
    let built = build_kernel_in(f, &root()?.join("target"))?;
    let out = out_dir()?.join("pos-kernel");
    std::fs::copy(&built, &out)?;
    println!("pos-kernel sha256 {}", sha256_hex(&std::fs::read(&out)?));
    Ok(out)
}

fn build_kernel_in(f: &Flags, target_dir: &Path) -> Res<PathBuf> {
    check_release_features(f)?;
    let root = root()?;
    let home = std::env::var("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|_| std::env::var("HOME").map(|h| PathBuf::from(h).join(".cargo")))?;
    let rustflags = format!(
        "--remap-path-prefix={}=/cargo --remap-path-prefix={}=/build \
         -C relocation-model=static -C code-model=kernel",
        home.display(),
        root.display()
    );
    let mut cmd = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()));
    cmd.current_dir(&root)
        .env(
            "CARGO_ENCODED_RUSTFLAGS",
            rustflags
                .split_whitespace()
                .collect::<Vec<_>>()
                .join("\x1f"),
        )
        .env("CARGO_TARGET_DIR", target_dir)
        .env("SOURCE_DATE_EPOCH", source_date_epoch().to_string())
        .args([
            "build",
            "--locked",
            "-p",
            "pos-kernel",
            "--target",
            KERNEL_TARGET,
        ]);
    if f.release {
        cmd.arg("--release");
    }
    if f.debug_console {
        cmd.args(["--features", "debug-console"]);
    }
    sh(&mut cmd)?;
    let prof = if f.release { "release" } else { "debug" };
    Ok(target_dir.join(KERNEL_TARGET).join(prof).join("pos-kernel"))
}

/// Limine config: boot the kernel immediately, no menu, no entry editor.
/// The kernel path is pinned by its BLAKE2B hash, and the config's own
/// hash is enrolled into LIMINE.EFI (`limine enroll-config`), so neither
/// can be changed without breaking the signature on LIMINE.EFI.
fn limine_conf(kernel_b2: &str) -> String {
    format!(
        "timeout: 0\neditor_enabled: no\n\n/POS appliance\n    protocol: limine\n    \
         path: boot():/boot/pos-kernel#{kernel_b2}\n"
    )
}

/// The Tier B ESP contents.
#[derive(Debug)]
struct KernelParts {
    stage0: Vec<u8>,
    limine: Vec<u8>,
    conf: String,
    kernel: Vec<u8>,
}

impl KernelParts {
    fn esp(&self) -> [(&str, &[u8]); 4] {
        [
            ("EFI/BOOT/BOOTX64.EFI", &self.stage0),
            ("EFI/BOOT/LIMINE.EFI", &self.limine),
            ("limine.conf", self.conf.as_bytes()),
            ("boot/pos-kernel", &self.kernel),
        ]
    }
}

/// Copy Limine's EFI image and enroll `conf`'s BLAKE2B hash into it.
fn enrolled_limine(conf: &str, out: &Path) -> Res<Vec<u8>> {
    let limine_dir = std::env::var_os("LIMINE_DIR")
        .map(PathBuf::from)
        .ok_or("LIMINE_DIR not set; run inside `nix develop` (see flake.nix)")?;
    let _ = std::fs::remove_file(out);
    std::fs::write(out, std::fs::read(limine_dir.join("BOOTX64.EFI"))?)?;
    run_ok(Command::new("limine").args([
        "enroll-config",
        "--quiet",
        &out.display().to_string(),
        &blake2b_hex(conf.as_bytes())?,
    ]))?;
    Ok(std::fs::read(out)?)
}

fn kernel_parts_in(f: &Flags, target_dir: &Path, work: &Path) -> Res<KernelParts> {
    let kernel = std::fs::read(build_kernel_in(f, target_dir)?)?;
    let stage0 = std::fs::read(build_uefi_bin(
        f,
        target_dir,
        "pos-stage0",
        "pos-stage0",
        false,
    )?)?;
    let conf = limine_conf(&blake2b_hex(&kernel)?);
    let limine = enrolled_limine(&conf, &work.join("LIMINE.EFI"))?;
    Ok(KernelParts {
        stage0,
        limine,
        conf,
        kernel,
    })
}

fn kernel_parts(f: &Flags) -> Res<KernelParts> {
    let p = kernel_parts_in(f, &root()?.join("target"), &out_dir()?)?;
    std::fs::write(out_dir()?.join("pos-kernel"), &p.kernel)?;
    println!("pos-kernel sha256 {}", sha256_hex(&p.kernel));
    Ok(p)
}

fn kernel_image(f: &Flags) -> Res<PathBuf> {
    let p = kernel_parts(f)?;
    let out = out_dir()?.join("pos-kernel.img");
    make_image_with(&p.esp(), &out)?;
    println!(
        "pos-kernel.img sha256 {}",
        sha256_hex(&std::fs::read(&out)?)
    );
    Ok(out)
}

/// Boot `esp` and require that the POS UI never appears.
fn expect_no_boot(esp: &[(&str, &[u8])], name: &str, secure_vars: Option<PathBuf>) -> Res<()> {
    let out = out_dir()?;
    let img = out.join(format!("{name}.img"));
    make_image_with(esp, &img)?;
    let qmp_path = out.join("qmp.sock");
    let mut child = qemu::spawn(&qemu::QemuOpts {
        image: img,
        media: None,
        out_dir: out.clone(),
        headless: true,
        qmp: Some(qmp_path.clone()),
        reset_vars: true,
        secure_vars,
        hub: false,
        media_unplugged: false,
    })?;
    let res = (|| -> Res<bool> {
        let mut q = qemu::Qmp::connect(&qmp_path, Duration::from_secs(10))?;
        let booted = wait_ui(&mut q, &out, Duration::from_secs(25)).is_ok();
        let _ = std::fs::copy(out.join("ui.ppm"), out.join(format!("{name}.ppm")));
        q.quit()?;
        Ok(booted)
    })();
    stop_qemu(&mut child);
    if res? {
        return Err(format!("{name}: booted to the POS UI but must not").into());
    }
    Ok(())
}

/// Tier B boot integrity without Secure Boot: Limine must refuse (a) a
/// kernel that differs by one byte from the hash in limine.conf and (b) a
/// limine.conf that differs from the hash enrolled in LIMINE.EFI.
fn tamper_test() -> Res<()> {
    let p = kernel_parts(&Flags::default())?;
    let mut bad_kernel = p.kernel.clone();
    let i = bad_kernel.len() / 2;
    bad_kernel[i] ^= 0x01;
    let mut esp = p.esp();
    esp[3].1 = &bad_kernel;
    expect_no_boot(&esp, "tamper-kernel", None)?;
    println!("tamper-test: modified kernel refused (kernel hash in limine.conf)");

    // An attacker who swaps the kernel and rewrites the hash in the config.
    let bad_conf = limine_conf(&blake2b_hex(&bad_kernel)?);
    let mut esp = p.esp();
    esp[2].1 = bad_conf.as_bytes();
    esp[3].1 = &bad_kernel;
    expect_no_boot(&esp, "tamper-conf", None)?;
    println!("tamper-test: OK - modified limine.conf refused (config hash enrolled in LIMINE.EFI)");
    Ok(())
}

/// Tier B key handover: stage 0 creates the NVRAM key on first boot and
/// reuses it on the next (debug build, read from the kernel log).
fn key_test() -> Res<()> {
    let f = Flags {
        kernel: true,
        debug_console: true,
        ..Flags::default()
    };
    let img = image(&f)?;
    let out = out_dir()?;
    let qmp_path = out.join("qmp.sock");
    let mut seen = Vec::new();
    for reset_vars in [true, false] {
        let mut child = qemu::spawn(&qemu::QemuOpts {
            image: img.clone(),
            media: None,
            out_dir: out.clone(),
            headless: true,
            qmp: Some(qmp_path.clone()),
            reset_vars,
            secure_vars: None,
            hub: false,
            media_unplugged: false,
        })?;
        let r = (|| -> Res<()> {
            let mut q = qemu::Qmp::connect(&qmp_path, Duration::from_secs(10))?;
            wait_ui(&mut q, &out, Duration::from_secs(90))?;
            q.quit()
        })();
        stop_qemu(&mut child);
        r?;
        let log = String::from_utf8_lossy(&std::fs::read(out.join("console.log"))?).into_owned();
        let src = [
            "key Generated",
            "key Existing",
            "key Fallback",
            "key Missing",
        ]
        .into_iter()
        .find(|k| log.contains(k))
        .ok_or("key-test: no key line in the kernel log")?;
        seen.push(src);
    }
    if seen != ["key Generated", "key Existing"] {
        return Err(format!("key-test: expected Generated then Existing, got {seen:?}").into());
    }
    println!("key-test: OK - stage 0 created the NVRAM key, then handed the same key over");
    Ok(())
}

/// BLAKE2B-512 via coreutils `b2sum` (in every Nix/Linux environment).
fn blake2b_hex(data: &[u8]) -> Res<String> {
    use std::io::Write;
    let mut child = Command::new("b2sum")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()?;
    child.stdin.take().ok_or("b2sum stdin")?.write_all(data)?;
    let out = child.wait_with_output()?;
    if !out.status.success() {
        return Err("b2sum failed".into());
    }
    let s = String::from_utf8(out.stdout)?;
    Ok(s.split_whitespace()
        .next()
        .ok_or("b2sum output")?
        .to_string())
}

/// Two blank, unbound superblocks (pos-store format). The device binds the
/// store to itself and assigns a random store id on first boot.
fn data_partition_init() -> Res<Vec<u8>> {
    let l = image::layout(IMAGE_MIB, ESP_MIB);
    pos_store::format_image(image::SECTOR as usize, l.data_sectors)
        .map_err(|e| format!("format data partition: {e:?}").into())
}

fn make_image(efi: &[u8], out: &Path) -> Res<()> {
    make_image_with(&[("EFI/BOOT/BOOTX64.EFI", efi)], out)
}

fn make_image_with(esp_files: &[(&str, &[u8])], out: &Path) -> Res<()> {
    let l = image::build_image(
        out,
        IMAGE_MIB,
        ESP_MIB,
        esp_files,
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
    if f.kernel {
        return kernel_image(f);
    }
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
        "--exclude",
        "pos-kernel",
        "--exclude",
        "pos-stage0",
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
        "--exclude",
        "pos-kernel",
        "--exclude",
        "pos-stage0",
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
    ]))?;
    for feats in [&[][..], &["--features", "debug-console"][..]] {
        sh(Command::new("cargo")
            .current_dir(&r)
            .args([
                "clippy",
                "--locked",
                "-p",
                "pos-kernel",
                "--target",
                KERNEL_TARGET,
            ])
            .args(feats)
            .args(["--", "-D", "warnings"]))?;
    }
    sh(Command::new("cargo").current_dir(&r).args([
        "clippy",
        "--locked",
        "-p",
        "pos-stage0",
        "--target",
        EFI_TARGET,
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
        hub: f.hub,
        media_unplugged: false,
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
        // Tier B: stage 0 + enrolled Limine + kernel.
        let parts = kernel_parts_in(&f, &td, &tmp)?;
        let kimg = tmp.join(format!("pos-kernel{i}.img"));
        make_image_with(&parts.esp(), &kimg)?;
        hashes.push((
            sha256_hex(&efi),
            sha256_hex(&std::fs::read(&img)?),
            sha256_hex(&std::fs::read(&kimg)?),
        ));
    }
    for (i, h) in hashes.iter().enumerate() {
        println!(
            "build {}: efi {} img {} kernel img {}",
            i + 1,
            h.0,
            h.1,
            h.2
        );
    }
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
        hub: f.hub,
        media_unplugged: false,
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
fn e2e(kernel: bool, hub: bool) -> Res<()> {
    // E2E_DEBUG=1 runs a debug build with the serial debug console.
    let f = if std::env::var_os("E2E_DEBUG").is_some() {
        Flags {
            debug_console: true,
            kernel,
            ..Flags::default()
        }
    } else {
        Flags {
            release: true,
            kernel,
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
    // Tier B: boot 1 starts without the export stick and hot-plugs it.
    let opts = |reset_vars: bool| qemu::QemuOpts {
        image: img.clone(),
        media: Some(media.clone()),
        out_dir: out.clone(),
        headless: true,
        qmp: Some(qmp_path.clone()),
        reset_vars,
        secure_vars: None,
        hub,
        media_unplugged: kernel && reset_vars,
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
        if kernel {
            q.plug_media(hub)?;
            pause(3000);
        }
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
/// sbsign `input` with the db key into `output`, then sbverify it.
fn sign_file(dir: &Path, input: &Path, output: &Path) -> Res<()> {
    let _ = std::fs::remove_file(output);
    run_ok(Command::new("sbsign").args([
        "--key",
        &dir.join("db.key").display().to_string(),
        "--cert",
        &dir.join("db.crt").display().to_string(),
        "--output",
        &output.display().to_string(),
        &input.display().to_string(),
    ]))?;
    run_ok(Command::new("sbverify").args([
        "--cert",
        &dir.join("db.crt").display().to_string(),
        &output.display().to_string(),
    ]))
}

/// Tier B release image with stage 0 and the config-enrolled Limine both
/// signed with the db key.
fn sign_kernel(dir: &Path) -> Res<(PathBuf, KernelParts)> {
    if !dir.join("db.key").exists() {
        return Err(format!("no keys in {}; run `cargo xtask keys` first", dir.display()).into());
    }
    let mut p = kernel_parts(&Flags {
        release: true,
        kernel: true,
        ..Flags::default()
    })?;
    let out = out_dir()?;
    let (s0, s0s) = (out.join("STAGE0.EFI"), out.join("STAGE0.signed.EFI"));
    let (li, lis) = (out.join("LIMINE.EFI"), out.join("LIMINE.signed.EFI"));
    std::fs::write(&s0, &p.stage0)?;
    std::fs::write(&li, &p.limine)?;
    sign_file(dir, &s0, &s0s)?;
    sign_file(dir, &li, &lis)?;
    let unsigned = KernelParts {
        stage0: std::mem::replace(&mut p.stage0, std::fs::read(&s0s)?),
        limine: std::mem::replace(&mut p.limine, std::fs::read(&lis)?),
        conf: p.conf.clone(),
        kernel: p.kernel.clone(),
    };
    let img = out.join("pos-kernel-signed.img");
    make_image_with(&p.esp(), &img)?;
    println!(
        "pos-kernel-signed.img sha256 {}",
        sha256_hex(&std::fs::read(&img)?)
    );
    Ok((img, unsigned))
}

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
    sign_file(dir, &efi, &signed)?;
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
fn sb_test(dir: &Path, kernel: bool) -> Res<()> {
    keys(dir)?;
    let mut kernel_unsigned = None;
    let signed = if kernel {
        let (img, unsigned) = sign_kernel(dir)?;
        kernel_unsigned = Some(unsigned);
        img
    } else {
        sign(dir)?
    };
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
        hub: false,
        media_unplugged: false,
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
    if let Some(u) = kernel_unsigned {
        // Signed stage 0 chaining to an unsigned Limine: the firmware must
        // refuse LoadImage. Then an unsigned stage 0.
        let signed_stage0 = std::fs::read(out.join("STAGE0.signed.EFI"))?;
        let mut esp = u.esp();
        esp[0].1 = &signed_stage0;
        expect_no_boot(&esp, "sb-unsigned-limine", Some(vars.clone()))?;
        println!("sb-test: signed stage 0 refused to run an unsigned LIMINE.EFI");
        expect_no_boot(&u.esp(), "sb-unsigned-stage0", Some(vars.clone()))?;
        println!("sb-test: unsigned stage 0 refused");
        return Ok(());
    }
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
