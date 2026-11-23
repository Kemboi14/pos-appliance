//! Pictures attached to products, users and the store (logo, wallpaper).
//!
//! The core only stores them: an opaque, already-shrunk, losslessly
//! compressed image (QOI, produced by `pos-ui`) with its size. Pictures are
//! shared with `Arc`, so cloning the state for a trial commit stays cheap.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use serde::{Deserialize, Serialize};

use crate::auth::UserId;

/// Size limits per kind, in pixels (the importer shrinks to fit).
pub const ITEM_MAX: u16 = 128;
pub const USER_MAX: u16 = 128;
pub const LOGO_MAX_W: u16 = 512;
pub const LOGO_MAX_H: u16 = 256;
pub const WALL_MAX_W: u16 = 640;
pub const WALL_MAX_H: u16 = 360;
/// Hard cap on one encoded picture.
pub const MAX_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Picture {
    pub w: u16,
    pub h: u16,
    /// QOI-encoded pixels.
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum PictureFor {
    Item(String),
    User(UserId),
    Logo,
    Wallpaper,
}

impl PictureFor {
    /// Largest width and height allowed for this kind.
    pub fn max(&self) -> (u16, u16) {
        match self {
            PictureFor::Item(_) => (ITEM_MAX, ITEM_MAX),
            PictureFor::User(_) => (USER_MAX, USER_MAX),
            PictureFor::Logo => (LOGO_MAX_W, LOGO_MAX_H),
            PictureFor::Wallpaper => (WALL_MAX_W, WALL_MAX_H),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pictures {
    map: BTreeMap<PictureFor, Arc<Picture>>,
}

impl Pictures {
    pub fn get(&self, k: &PictureFor) -> Option<&Arc<Picture>> {
        self.map.get(k)
    }
    pub fn item(&self, sku: &str) -> Option<&Arc<Picture>> {
        self.map.get(&PictureFor::Item(sku.into()))
    }
    pub fn keys(&self) -> impl Iterator<Item = &PictureFor> {
        self.map.keys()
    }
    pub fn len(&self) -> usize {
        self.map.len()
    }
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
    /// Total encoded bytes (kept well inside a snapshot slot).
    pub fn bytes(&self) -> usize {
        self.map.values().map(|p| p.data.len()).sum()
    }
    pub(crate) fn set(&mut self, k: PictureFor, p: Option<Picture>) {
        match p {
            Some(p) => {
                self.map.insert(k, Arc::new(p));
            }
            None => {
                self.map.remove(&k);
            }
        }
    }
    pub(crate) fn retain_items(&mut self, keep: impl Fn(&str) -> bool) {
        self.map.retain(|k, _| match k {
            PictureFor::Item(s) => keep(s),
            _ => true,
        });
    }

    /// Check a picture before it is journaled.
    pub fn valid(k: &PictureFor, p: &Picture) -> bool {
        let (mw, mh) = k.max();
        p.w > 0
            && p.h > 0
            && p.w <= mw
            && p.h <= mh
            && p.data.len() <= MAX_BYTES
            && p.data.starts_with(b"qoif")
    }
}
