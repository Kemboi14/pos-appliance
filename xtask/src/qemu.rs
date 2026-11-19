//! QEMU + OVMF runner and a minimal QMP client (no serde, line-oriented JSON).

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::Res;

#[derive(Debug, Clone)]
pub struct QemuOpts {
    pub image: PathBuf,
    pub media: Option<PathBuf>,
    pub out_dir: PathBuf,
    pub headless: bool,
    pub qmp: Option<PathBuf>,
    /// Fresh copy of OVMF_VARS for each run unless false.
    pub reset_vars: bool,
}

fn env_path(var: &str) -> Res<PathBuf> {
    let p = std::env::var_os(var)
        .map(PathBuf::from)
        .ok_or_else(|| format!("{var} not set; run inside `nix develop` (see flake.nix)"))?;
    if !p.exists() {
        return Err(format!("{var}={} does not exist", p.display()).into());
    }
    Ok(p)
}

pub fn spawn(o: &QemuOpts) -> Res<Child> {
    let code = env_path("OVMF_CODE")?;
    let vars_src = env_path("OVMF_VARS")?;
    let vars = o.out_dir.join("ovmf_vars.fd");
    if o.reset_vars || !vars.exists() {
        std::fs::copy(&vars_src, &vars)?;
        let mut perm = std::fs::metadata(&vars)?.permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        perm.set_readonly(false);
        std::fs::set_permissions(&vars, perm)?;
    }
    let printer = o.out_dir.join("printer.bin");
    let console = o.out_dir.join("console.log");

    let mut cmd = Command::new("qemu-system-x86_64");
    cmd.args(["-machine", "q35", "-m", "512M", "-smp", "1"]);
    if Path::new("/dev/kvm").exists() && std::fs::File::open("/dev/kvm").is_ok() {
        cmd.args(["-enable-kvm", "-cpu", "host"]);
    } else {
        cmd.args(["-cpu", "max"]);
    }
    cmd.arg("-drive").arg(format!(
        "if=pflash,format=raw,unit=0,readonly=on,file={}",
        code.display()
    ));
    cmd.arg("-drive").arg(format!(
        "if=pflash,format=raw,unit=1,file={}",
        vars.display()
    ));
    // No network devices at all: the appliance is offline by design.
    cmd.args(["-nic", "none"]);
    cmd.args(["-device", "qemu-xhci,id=xhci"]);
    cmd.args(["-device", "usb-kbd,bus=xhci.0"]);
    cmd.args(["-device", "usb-tablet,bus=xhci.0"]);
    cmd.arg("-drive").arg(format!(
        "if=none,id=stick,format=raw,file={}",
        o.image.display()
    ));
    cmd.args([
        "-device",
        "usb-storage,bus=xhci.0,drive=stick,bootindex=0,removable=on",
    ]);
    if let Some(m) = &o.media {
        cmd.arg("-drive")
            .arg(format!("if=none,id=media,format=raw,file={}", m.display()));
        cmd.args(["-device", "usb-storage,bus=xhci.0,drive=media,removable=on"]);
    }
    // COM1: firmware console / debug log. COM2: ESC/POS printer capture.
    cmd.arg("-serial")
        .arg(format!("file:{}", console.display()));
    cmd.arg("-serial")
        .arg(format!("file:{}", printer.display()));
    if let Some(q) = &o.qmp {
        let _ = std::fs::remove_file(q);
        cmd.arg("-qmp")
            .arg(format!("unix:{},server=on,wait=off", q.display()));
    }
    if o.headless {
        cmd.args(["-display", "none"]);
    }
    cmd.stdin(Stdio::null());
    eprintln!("+ {cmd:?}");
    Ok(cmd.spawn()?)
}

#[derive(Debug)]
pub struct Qmp {
    w: UnixStream,
    r: BufReader<UnixStream>,
}

impl Qmp {
    pub fn connect(path: &Path, timeout: Duration) -> Res<Self> {
        let start = Instant::now();
        let s = loop {
            match UnixStream::connect(path) {
                Ok(s) => break s,
                Err(e) if start.elapsed() < timeout => {
                    let _ = e;
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(e) => return Err(e.into()),
            }
        };
        s.set_read_timeout(Some(Duration::from_secs(30)))?;
        let mut q = Qmp {
            r: BufReader::new(s.try_clone()?),
            w: s,
        };
        let greeting = q.line()?;
        if !greeting.contains("QMP") {
            return Err(format!("bad QMP greeting: {greeting}").into());
        }
        q.cmd(r#"{"execute":"qmp_capabilities"}"#)?;
        Ok(q)
    }

    fn line(&mut self) -> Res<String> {
        let mut l = String::new();
        if self.r.read_line(&mut l)? == 0 {
            return Err("QMP connection closed".into());
        }
        Ok(l)
    }

    /// Send a command and wait for its `return` (skipping async events).
    pub fn cmd(&mut self, json: &str) -> Res<String> {
        self.w.write_all(json.as_bytes())?;
        self.w.write_all(b"\n")?;
        loop {
            let l = self.line()?;
            if l.contains("\"return\"") {
                return Ok(l);
            }
            if l.contains("\"error\"") {
                return Err(format!("QMP error for {json}: {l}").into());
            }
        }
    }

    pub fn screendump(&mut self, file: &Path) -> Res<()> {
        let _ = std::fs::remove_file(file);
        self.cmd(&format!(
            r#"{{"execute":"screendump","arguments":{{"filename":"{}"}}}}"#,
            file.display()
        ))?;
        Ok(())
    }

    /// Press and release one key given as a QEMU qcode (e.g. "a", "ret",
    /// "shift"), optionally with modifiers held.
#[allow(dead_code)] // used by the M5 end-to-end test
    pub fn key(&mut self, qcode: &str, mods: &[&str]) -> Res<()> {
        let mut keys: Vec<String> = mods
            .iter()
            .map(|m| format!(r#"{{"type":"qcode","data":"{m}"}}"#))
            .collect();
        keys.push(format!(r#"{{"type":"qcode","data":"{qcode}"}}"#));
        self.cmd(&format!(
            r#"{{"execute":"send-key","arguments":{{"keys":[{}],"hold-time":20}}}}"#,
            keys.join(",")
        ))?;
        Ok(())
    }

    /// Type an ASCII string with `delay` between characters.
#[allow(dead_code)] // used by the M5 end-to-end test
    pub fn type_str(&mut self, s: &str, delay: Duration) -> Res<()> {
        for c in s.chars() {
            let (code, shift) = qcode_for(c).ok_or_else(|| format!("no qcode for {c:?}"))?;
            if shift {
                self.key(code, &["shift"])?;
            } else {
                self.key(code, &[])?;
            }
            std::thread::sleep(delay);
        }
        Ok(())
    }

    pub fn quit(&mut self) -> Res<()> {
        let _ = self.w.write_all(br#"{"execute":"quit"}"#);
        let _ = self.w.write_all(b"\n");
        Ok(())
    }
}

#[allow(dead_code)] // used by the M5 end-to-end test
pub fn qcode_for(c: char) -> Option<(&'static str, bool)> {
    const LOWER: [&str; 26] = [
        "a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k", "l", "m", "n", "o", "p", "q", "r",
        "s", "t", "u", "v", "w", "x", "y", "z",
    ];
    const DIGITS: [&str; 10] = ["0", "1", "2", "3", "4", "5", "6", "7", "8", "9"];
    Some(match c {
        'a'..='z' => (LOWER[(c as u8 - b'a') as usize], false),
        'A'..='Z' => (LOWER[(c as u8 - b'A') as usize], true),
        '0'..='9' => (DIGITS[(c as u8 - b'0') as usize], false),
        ' ' => ("spc", false),
        '\n' => ("ret", false),
        '.' => ("dot", false),
        ',' => ("comma", false),
        '-' => ("minus", false),
        '*' => ("kp_multiply", false),
        '+' => ("kp_add", false),
        '/' => ("slash", false),
        _ => return None,
    })
}

/// Parse a binary PPM (P6) screendump into (width, height, rgb bytes).
pub fn read_ppm(path: &Path) -> Res<(usize, usize, Vec<u8>)> {
    let data = std::fs::read(path)?;
    let mut fields = Vec::new();
    let mut i = 0;
    while fields.len() < 4 {
        while i < data.len() && data[i].is_ascii_whitespace() {
            i += 1;
        }
        if data.get(i) == Some(&b'#') {
            while i < data.len() && data[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        let s = i;
        while i < data.len() && !data[i].is_ascii_whitespace() {
            i += 1;
        }
        fields.push(std::str::from_utf8(&data[s..i])?.to_string());
    }
    i += 1;
    if fields[0] != "P6" || fields[3] != "255" {
        return Err("unsupported PPM".into());
    }
    let w: usize = fields[1].parse()?;
    let h: usize = fields[2].parse()?;
    let px = data[i..].to_vec();
    if px.len() < w * h * 3 {
        return Err("truncated PPM".into());
    }
    Ok((w, h, px))
}

pub fn pixel(img: &(usize, usize, Vec<u8>), x: usize, y: usize) -> u32 {
    let o = (y * img.0 + x) * 3;
    (u32::from(img.2[o]) << 16) | (u32::from(img.2[o + 1]) << 8) | u32::from(img.2[o + 2])
}
