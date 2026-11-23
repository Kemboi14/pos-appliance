//! Decoding real PNG/JPEG files (made with Pillow; see tests/data).

use pos_ui::image::{decode, pack, unpack};

fn load(n: &str) -> Vec<u8> {
    std::fs::read(format!("{}/tests/data/{n}", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

fn rgb(p: u32) -> (i32, i32, i32) {
    (
        ((p >> 16) & 0xFF) as i32,
        ((p >> 8) & 0xFF) as i32,
        (p & 0xFF) as i32,
    )
}

fn close(a: (i32, i32, i32), b: (i32, i32, i32)) -> bool {
    (a.0 - b.0).abs() < 24 && (a.1 - b.1).abs() < 24 && (a.2 - b.2).abs() < 24
}

#[test]
fn jpeg_baseline_and_progressive() {
    for f in ["photo.jpg", "photo_progressive.jpg"] {
        let b = decode(&load(f)).unwrap_or_else(|e| panic!("{f}: {e:?}"));
        assert_eq!((b.w, b.h), (320, 200));
        // Sky, sun, grass.
        assert!(
            close(rgb(b.px[(10 * 320 + 10) as usize]), (30, 120, 200)),
            "{f}"
        );
        assert!(
            close(rgb(b.px[(100 * 320 + 160) as usize]), (250, 200, 40)),
            "{f}"
        );
        assert!(
            close(rgb(b.px[(190 * 320 + 10) as usize]), (20, 160, 60)),
            "{f}"
        );
        assert!(b.px.iter().all(|p| p >> 24 == 0xFF));
        let t = b.resized(96, 60);
        assert_eq!(unpack(&pack(&t)).unwrap(), t);
    }
}

#[test]
fn png_alpha_grey_palette() {
    let logo = decode(&load("logo.png")).unwrap();
    assert_eq!((logo.w, logo.h), (64, 48));
    assert_eq!(logo.px[0] >> 24, 0, "transparent corner");
    assert_eq!(logo.px[(24 * 64 + 32) as usize], 0xFFC8_1E3C);
    let g = decode(&load("gray.png")).unwrap();
    let (r, gg, bb) = rgb(g.px[0]);
    assert!(r == gg && gg == bb);
    let p = decode(&load("palette.png")).unwrap();
    assert_eq!((p.w, p.h), (320, 200));
    assert!(close(rgb(p.px[(100 * 320 + 160) as usize]), (250, 200, 40)));
}

#[test]
fn truncated_files_fail_cleanly() {
    for f in ["photo.jpg", "logo.png", "palette.png"] {
        let d = load(f);
        for cut in (0..d.len()).step_by(37) {
            let _ = decode(&d[..cut]);
        }
    }
}
