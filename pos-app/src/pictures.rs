//! Pictures: importing them from a USB stick, and a cache of decoded
//! bitmaps for drawing.
//!
//! Files are looked up in the stick's root folder (the Tier B FAT driver
//! reads only the root), by name:
//!
//! | File                         | Picture                  |
//! |------------------------------|--------------------------|
//! | `IMG_<SKU>.JPG` / `.PNG`     | product                  |
//! | `USER_<number>.JPG` / `.PNG` | user (sign-in avatar)    |
//! | `LOGO.PNG` / `.JPG`          | store logo               |
//! | `WALLPAPER.JPG` / `.PNG`     | sign-in and home screens |
//!
//! Each is shrunk to its kind's size limit, stored losslessly (QOI) in the
//! journal, and so survives reboots and is covered by the hash chain.

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use pos_core::auth::UserId;
use pos_core::media::{Picture, PictureFor, Pictures};
use pos_core::state::Event;
use pos_hal::{BlockDevice, HalError};
use pos_ui::image::{self, Bitmap};

use crate::{App, Hw};

const MAX_FILE: usize = 16 * 1024 * 1024;
/// Pictures committed per journal batch.
const BATCH: usize = 16;
const CACHE_MAX: usize = 400;

/// Characters a SKU may contain that a FAT file name may not.
fn file_safe(sku: &str) -> String {
    sku.chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c => c,
        })
        .collect()
}

fn candidates(t: &PictureFor) -> Vec<String> {
    let stem = match t {
        PictureFor::Item(s) => format!("IMG_{}", file_safe(s)),
        PictureFor::User(u) => format!("USER_{u}"),
        PictureFor::Logo => "LOGO".into(),
        PictureFor::Wallpaper => "WALLPAPER".into(),
    };
    ["JPG", "PNG", "JPEG"]
        .iter()
        .map(|e| format!("{stem}.{e}"))
        .collect()
}

/// Decode, shrink and encode a picture file for `t`.
pub fn prepare(t: &PictureFor, file: &[u8]) -> Result<Picture, image::ImageError> {
    let b = image::decode(file)?;
    let (mw, mh) = t.max();
    let (w, h) = match t {
        // Wallpapers are cropped to the screen's shape; the rest keep theirs.
        PictureFor::Wallpaper => (i32::from(mw), i32::from(mh)),
        _ => b.fit_size(i32::from(mw), i32::from(mh)),
    };
    let small = match t {
        PictureFor::Wallpaper => b.cover(w, h),
        _ => b.resized(w, h),
    };
    Ok(Picture {
        w: w as u16,
        h: h as u16,
        data: image::pack(&small),
    })
}

/// Look for every known picture name on the stick and journal what is
/// found. Returns a summary for the status line.
pub fn import<D: BlockDevice>(
    app: &mut App<D>,
    hw: &mut Hw<'_>,
    by: UserId,
    now: i64,
    mut events: Vec<Event>,
) -> Result<String, String> {
    if !hw.media.is_present() {
        return Err("Insert the USB stick with the pictures".into());
    }
    let mut targets = vec_targets(app);
    targets.dedup();
    let budget = budget(app);
    let mut used = app.state.pictures.bytes();
    let (mut items, mut users, mut other) = (0usize, 0usize, Vec::new());
    let mut problems: Vec<String> = Vec::new();
    let mut pending: Vec<Event> = Vec::new();
    for t in targets {
        let mut found = None;
        for name in candidates(&t) {
            match hw.media.read_file(&name, MAX_FILE) {
                Ok(d) => {
                    found = Some((name, d));
                    break;
                }
                Err(HalError::NotFound) => {}
                Err(HalError::NoSpace) => {
                    problems.push(format!("{name}: over 16 MB"));
                    break;
                }
                Err(e) => return Err(format!("{name}: {e}")),
            }
        }
        let Some((name, data)) = found else { continue };
        let pic = match prepare(&t, &data) {
            Ok(p) => p,
            Err(e) => {
                problems.push(format!("{name}: {}", describe(e)));
                continue;
            }
        };
        let old = app.state.pictures.get(&t).map_or(0, |p| p.data.len());
        if used - old + pic.data.len() > budget {
            problems.push(format!("{name}: picture storage full"));
            break;
        }
        used = used - old + pic.data.len();
        match &t {
            PictureFor::Item(_) => items += 1,
            PictureFor::User(_) => users += 1,
            PictureFor::Logo => other.push("logo"),
            PictureFor::Wallpaper => other.push("wallpaper"),
        }
        pending.push(Event::PictureSet {
            by,
            at: now,
            target: t,
            picture: Some(pic),
        });
        if pending.len() >= BATCH {
            events.append(&mut pending);
            app.commit(core::mem::take(&mut events))
                .map_err(|e| crate::describe(&e))?;
        }
    }
    events.append(&mut pending);
    if items + users + other.len() == 0 {
        // Still journal a manager override if there was one.
        if !events.is_empty() {
            app.commit(events).map_err(|e| crate::describe(&e))?;
        }
        return Err(problems.first().cloned().unwrap_or_else(|| {
            "No pictures found (IMG_<SKU>.JPG, USER_<n>.JPG, LOGO.PNG, WALLPAPER.JPG)".into()
        }));
    }
    if !events.is_empty() {
        app.commit(events).map_err(|e| crate::describe(&e))?;
    }
    let mut parts = Vec::new();
    if items > 0 {
        parts.push(format!("{items} products"));
    }
    if users > 0 {
        parts.push(format!("{users} users"));
    }
    for o in other {
        parts.push(o.into());
    }
    let mut s = format!("Pictures imported: {}", parts.join(", "));
    if !problems.is_empty() {
        s.push_str(&format!(
            " ({} skipped: {})",
            problems.len(),
            problems.join("; ")
        ));
    }
    Ok(s)
}

fn vec_targets<D: BlockDevice>(app: &App<D>) -> Vec<PictureFor> {
    let mut v = vec_from([PictureFor::Logo, PictureFor::Wallpaper]);
    v.extend(
        app.state
            .users
            .values()
            .filter(|u| u.active)
            .map(|u| PictureFor::User(u.id)),
    );
    v.extend(
        app.state
            .catalog
            .items()
            .map(|i| PictureFor::Item(i.sku.clone())),
    );
    v
}

fn vec_from<const N: usize>(a: [PictureFor; N]) -> Vec<PictureFor> {
    a.into_iter().collect()
}

/// Bytes pictures may use: just over half a snapshot slot, so a snapshot
/// with every picture still fits beside the rest of the state.
fn budget<D: BlockDevice>(app: &App<D>) -> usize {
    app.store.as_ref().map_or(0, |s| {
        let g = s.geometry();
        (g.snap_blocks as usize * g.block_size) * 55 / 100
    })
}

fn describe(e: image::ImageError) -> &'static str {
    match e {
        image::ImageError::Format => "not a PNG or JPEG",
        image::ImageError::Corrupt => "damaged file",
        image::ImageError::TooLarge => "over 12 megapixels - resize it",
    }
}

/// Decoded pictures, kept between frames.
#[derive(Debug, Default)]
pub struct Gallery {
    map: BTreeMap<PictureFor, Option<Arc<Bitmap>>>,
    wall: Option<Arc<Bitmap>>,
}

impl Gallery {
    pub fn clear(&mut self) {
        self.map.clear();
        self.wall = None;
    }

    /// Decode `keys` that are not cached yet.
    pub fn prepare(&mut self, pics: &Pictures, keys: &[PictureFor]) {
        if self.map.len() + keys.len() > CACHE_MAX {
            self.map.clear();
        }
        for k in keys {
            if !self.map.contains_key(k) {
                let b = pics
                    .get(k)
                    .and_then(|p| image::unpack(&p.data))
                    .map(Arc::new);
                self.map.insert(k.clone(), b);
            }
        }
    }

    pub fn get(&self, k: &PictureFor) -> Option<Arc<Bitmap>> {
        self.map.get(k).cloned().flatten()
    }

    /// The wallpaper at exactly `w`×`h` (uploaded, or the built-in one).
    pub fn wallpaper(&mut self, pics: &Pictures, w: i32, h: i32) -> Arc<Bitmap> {
        if let Some(b) = &self.wall
            && b.w == w
            && b.h == h
        {
            return b.clone();
        }
        let b = pics
            .get(&PictureFor::Wallpaper)
            .and_then(|p| image::unpack(&p.data))
            .map(|b| b.cover(w, h))
            .unwrap_or_else(|| image::default_wallpaper(w, h));
        let b = Arc::new(b);
        self.wall = Some(b.clone());
        b
    }
}
