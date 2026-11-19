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
            })?;
            child.wait()?;
            Ok(())
        }
        "smoke" => smoke(&f),
        "repro" => repro(),
        "test" => host_tests(),
        "lint" => lint(),
        "ci" => {
            lint()?;
            host_tests()?;
            smoke(&Flags::default())?;
            repro()
        }
        _ => {
            eprintln!(
                "usage: cargo xtask <cmd> [--release] [--debug-console] [--headless]\n\
                 \n\
                 build    build BOOTX64.EFI into out/\n\
                 image    build out/pos.img (GPT: ESP + POS data partition)\n\
                 qemu     boot out/pos.img in QEMU/OVMF (printer bytes -> out/printer.bin)\n\
                 smoke    M0 acceptance: headless boot, screendump, check test pattern\n\
                 repro    build the release image twice from clean and compare hashes\n\
                 test     host test suite (core/store/ui/escpos/app)\n\
                 lint     rustfmt --check + clippy -D warnings (host and UEFI)\n\
                 ci       lint + test + smoke + repro"
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

fn data_partition_init() -> Vec<u8> {
    Vec::new()
}

fn make_image(efi: &[u8], out: &Path) -> Res<()> {
    let l = image::build_image(
        out,
        IMAGE_MIB,
        ESP_MIB,
        efi,
        &data_partition_init(),
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
        "--exclude",
        "kernel",
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
        "kernel",
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

/// M0 acceptance: boot headless, take a screendump, verify the test pattern.
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
    })?;
    let res = (|| -> Res<()> {
        let mut q = qemu::Qmp::connect(&qmp_path, Duration::from_secs(10))?;
        let shot = out.join("smoke.ppm");
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        loop {
            std::thread::sleep(Duration::from_secs(2));
            q.screendump(&shot)?;
            std::thread::sleep(Duration::from_millis(300));
            if let Ok(img) = qemu::read_ppm(&shot) {
                let (w, h) = (img.0, img.1);
                let center = qemu::pixel(&img, w / 2, h / 2);
                let corner = qemu::pixel(&img, w - 5, h - 5);
                if center == 0x00F0_A000 && corner == 0x001E_3A5F {
                    println!("smoke: OK - GOP mode {w}x{h}, test pattern verified");
                    q.quit()?;
                    return Ok(());
                }
            }
            if std::time::Instant::now() > deadline {
                q.quit()?;
                return Err("smoke: test pattern not seen within 60 s (see out/smoke.ppm)".into());
            }
        }
    })();
    stop_qemu(&mut child);
    res
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
