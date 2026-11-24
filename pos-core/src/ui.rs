//! Per-till preferences: how this till looks and sounds, and its Wi-Fi.
//! They belong to the machine (screen, speaker, radio), so they are never
//! handed from the head to the lanes.

use alloc::string::String;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ThemeMode {
    Dark,
    Light,
}

/// Accent colours offered in Settings (all readable with white text).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Accent {
    Blue,
    Green,
    Purple,
    Orange,
    Red,
    Teal,
}

impl Accent {
    pub const ALL: [Accent; 6] = [
        Accent::Blue,
        Accent::Green,
        Accent::Purple,
        Accent::Orange,
        Accent::Red,
        Accent::Teal,
    ];
    pub fn label(self) -> &'static str {
        match self {
            Accent::Blue => "Blue",
            Accent::Green => "Green",
            Accent::Purple => "Purple",
            Accent::Orange => "Orange",
            Accent::Red => "Red",
            Accent::Teal => "Teal",
        }
    }
    /// RGB888, chosen for ≥ 4.5:1 contrast with white text.
    pub fn rgb(self) -> u32 {
        match self {
            Accent::Blue => 0x0025_63EB,
            Accent::Green => 0x0015_803D,
            Accent::Purple => 0x0075_3FD4,
            Accent::Orange => 0x00C2_410C,
            Accent::Red => 0x00BE_123C,
            Accent::Teal => 0x000F_766E,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TextSize {
    Normal,
    Large,
    ExtraLarge,
}

impl TextSize {
    pub const ALL: [TextSize; 3] = [TextSize::Normal, TextSize::Large, TextSize::ExtraLarge];
    pub fn label(self) -> &'static str {
        match self {
            TextSize::Normal => "Normal",
            TextSize::Large => "Large",
            TextSize::ExtraLarge => "Extra large",
        }
    }
    /// Text scale in thousandths.
    pub fn permille(self) -> i32 {
        match self {
            TextSize::Normal => 1000,
            TextSize::Large => 1150,
            TextSize::ExtraLarge => 1300,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiPrefs {
    pub theme: ThemeMode,
    pub accent: Accent,
    pub text: TextSize,
    /// Sale screen mirrored: keys on the left, the basket on the right.
    pub left_handed: bool,
    /// Product pictures on the quick keys.
    pub key_pictures: bool,
    /// Beep when a scan adds an item (low tone when nothing matched).
    pub beep_on_scan: bool,
    /// Short click on every key and tap.
    pub key_click: bool,
    /// Clock screen saver after this many idle minutes; 0 = off.
    pub saver_mins: u16,
}

pub const MAX_SAVER_MINS: u16 = 120;

impl Default for UiPrefs {
    fn default() -> Self {
        UiPrefs {
            theme: ThemeMode::Dark,
            accent: Accent::Blue,
            text: TextSize::Normal,
            left_handed: false,
            key_pictures: true,
            beep_on_scan: true,
            key_click: false,
            saver_mins: 0,
        }
    }
}

impl UiPrefs {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.saver_mins > MAX_SAVER_MINS {
            return Err("screen saver delay too long");
        }
        Ok(())
    }
}

/// The Wi-Fi network this till joins at start-up (Tier A, firmware driver).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WifiSettings {
    pub ssid: String,
    /// WPA2/WPA3-Personal passphrase. Kept like the store network key: in
    /// the journal on the till's own stick, never exported or synced.
    pub password: String,
}

impl WifiSettings {
    pub fn validate(&self) -> Result<(), &'static str> {
        let n = self.ssid.len();
        if n == 0 || n > 32 {
            return Err("network name must be 1-32 bytes");
        }
        let p = self.password.len();
        if !(8..=63).contains(&p) || !self.password.bytes().all(|b| (0x20..0x7F).contains(&b)) {
            return Err("password must be 8-63 printable characters");
        }
        Ok(())
    }
}
