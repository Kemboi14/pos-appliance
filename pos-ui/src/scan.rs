//! Keyboard-wedge barcode scanner detection by inter-key timing.
//!
//! Scanners "type" a whole code within a few milliseconds per character and
//! end it with Enter. Humans are far slower. Characters are always delivered
//! to the focused input immediately (no added latency for typing); when Enter
//! arrives, the detector decides whether the characters typed since the last
//! Enter/clear formed a scanner burst.
//!
//! Note for UEFI: the firmware's USB keyboard driver polls at its own
//! interval (≈8–16 ms per character), so the default threshold is 30 ms,
//! not the 5 ms one might use with a native driver.

use alloc::string::String;

#[derive(Debug, Clone, Copy)]
pub struct ScanConfig {
    /// Maximum gap between consecutive characters (and before Enter).
    pub max_gap_us: u64,
    /// Minimum code length.
    pub min_len: usize,
}

impl Default for ScanConfig {
    fn default() -> Self {
        ScanConfig {
            max_gap_us: 30_000,
            min_len: 6,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Enter {
    /// The buffered characters were a scanner burst; `code` is the scan.
    Scan(String),
    /// Human typing (or too short): treat Enter normally.
    Typed,
}

#[derive(Debug, Clone, Default)]
pub struct ScanDetector {
    cfg: Option<ScanConfig>,
    buf: String,
    last_at: Option<u64>,
    /// All gaps so far were ≤ max_gap.
    fast: bool,
}

impl ScanDetector {
    pub fn new(cfg: ScanConfig) -> Self {
        ScanDetector {
            cfg: Some(cfg),
            buf: String::new(),
            last_at: None,
            fast: true,
        }
    }

    pub fn set_config(&mut self, cfg: ScanConfig) {
        self.cfg = Some(cfg);
    }

    fn cfg(&self) -> ScanConfig {
        self.cfg.unwrap_or_default()
    }

    /// Feed a printable character observed at `at_us`.
    pub fn char(&mut self, c: char, at_us: u64) {
        let gap_ok = match self.last_at {
            Some(t) => at_us.saturating_sub(t) <= self.cfg().max_gap_us,
            None => true,
        };
        if !gap_ok {
            // A slow gap restarts burst tracking at this character: a scan
            // can follow human typing without an Enter in between.
            self.buf.clear();
            self.fast = true;
        }
        self.buf.push(c);
        self.last_at = Some(at_us);
    }

    /// Any other key (Backspace, arrows, …) means a human is editing.
    pub fn other_key(&mut self) {
        self.fast = false;
        self.last_at = None;
    }

    /// Enter pressed at `at_us`: classify and reset.
    pub fn enter(&mut self, at_us: u64) -> Enter {
        let cfg = self.cfg();
        let timely = self
            .last_at
            .is_some_and(|t| at_us.saturating_sub(t) <= cfg.max_gap_us);
        let res = if self.fast && timely && self.buf.chars().count() >= cfg.min_len {
            Enter::Scan(core::mem::take(&mut self.buf))
        } else {
            Enter::Typed
        };
        self.reset();
        res
    }

    pub fn reset(&mut self) {
        self.buf.clear();
        self.last_at = None;
        self.fast = true;
    }

    /// Length of the current burst (characters the UI should replace with
    /// the scanned code if Enter turns this into a scan).
    pub fn burst_len(&self) -> usize {
        self.buf.chars().count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(d: &mut ScanDetector, s: &str, start: u64, gap: u64) -> u64 {
        let mut t = start;
        for c in s.chars() {
            d.char(c, t);
            t += gap;
        }
        t
    }

    #[test]
    fn fast_burst_is_scan() {
        let mut d = ScanDetector::new(ScanConfig::default());
        let t = feed(&mut d, "4006381333931", 1_000_000, 12_000);
        assert_eq!(d.enter(t), Enter::Scan("4006381333931".into()));
    }

    #[test]
    fn human_typing_is_not_scan() {
        let mut d = ScanDetector::new(ScanConfig::default());
        let t = feed(&mut d, "4006381333931", 0, 150_000);
        assert_eq!(d.enter(t), Enter::Typed);
    }

    #[test]
    fn short_burst_is_not_scan() {
        let mut d = ScanDetector::new(ScanConfig::default());
        let t = feed(&mut d, "123", 0, 5_000);
        assert_eq!(d.enter(t), Enter::Typed);
    }

    #[test]
    fn scan_after_human_typing_without_enter() {
        let mut d = ScanDetector::new(ScanConfig::default());
        let t = feed(&mut d, "12", 0, 200_000);
        let t = feed(&mut d, "96385074", t + 500_000, 10_000);
        assert_eq!(d.burst_len(), 8);
        assert_eq!(d.enter(t), Enter::Scan("96385074".into()));
    }

    #[test]
    fn editing_cancels_scan() {
        let mut d = ScanDetector::new(ScanConfig::default());
        let t = feed(&mut d, "4006381333931", 0, 5_000);
        d.other_key();
        assert_eq!(d.enter(t), Enter::Typed);
    }

    #[test]
    fn slow_enter_is_not_scan() {
        let mut d = ScanDetector::new(ScanConfig::default());
        let t = feed(&mut d, "4006381333931", 0, 5_000);
        assert_eq!(d.enter(t + 400_000), Enter::Typed);
    }
}
