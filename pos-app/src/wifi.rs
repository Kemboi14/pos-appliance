//! Wi-Fi for this till (Tier A: the firmware's Wi-Fi driver).
//!
//! Scans and connections block in the firmware for seconds, so the UI
//! only *asks* for them; they run on the next tick, after a frame saying
//! "Scanning…" or "Connecting…" has been drawn. At start-up the saved
//! network is joined before the store network opens its adapter (see
//! [`App::wifi_settled`]); a lost connection is retried every few minutes
//! while no sale is open.

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use pos_core::auth::UserId;
use pos_core::state::Event;
use pos_core::ui::WifiSettings;
use pos_hal::{BlockDevice, HalError, WifiNetwork};

use crate::{App, Hw};

const RETRY_US: u64 = 5 * 60 * 1_000_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Job {
    Scan,
    Connect {
        ssid: String,
        password: String,
        by: UserId,
    },
    Disconnect,
}

#[derive(Debug, Default)]
pub(crate) struct WifiRuntime {
    pub job: Option<Job>,
    /// Last scan, strongest first.
    pub networks: Vec<WifiNetwork>,
    pub scanned: bool,
    /// From the platform, refreshed every tick.
    pub present: bool,
    pub describe: String,
    pub connected: Option<String>,
    /// The start-up connection attempt has been made (or is not needed).
    pub settled: bool,
    pub last_try_us: Option<u64>,
    pub last_error: Option<String>,
}

impl WifiRuntime {
    pub fn busy(&self) -> Option<&'static str> {
        match self.job {
            Some(Job::Scan) => Some("Scanning for networks..."),
            Some(Job::Connect { .. }) => Some("Connecting..."),
            Some(Job::Disconnect) => Some("Disconnecting..."),
            None => None,
        }
    }
}

fn explain(e: HalError) -> &'static str {
    match e {
        HalError::NotPresent => "no Wi-Fi adapter with a firmware driver",
        HalError::Refused => "the network refused the connection (check the password)",
        HalError::Timeout => "no answer from the network (out of range?)",
        HalError::NotFound => "network not found",
        _ => "the Wi-Fi driver reported an error",
    }
}

impl<D: BlockDevice> App<D> {
    /// May the platform open the network adapter? Not before the saved
    /// Wi-Fi network has been tried, or the wired card would be chosen.
    pub fn wifi_settled(&self) -> bool {
        self.wifi_rt.settled || self.state.wifi.is_none()
    }

    /// Should the store network run over Wi-Fi rather than Ethernet?
    pub fn wifi_preferred(&self) -> bool {
        self.state.wifi.is_some() && self.wifi_rt.connected.is_some()
    }

    pub(crate) fn wifi_request(&mut self, job: Job) {
        self.wifi_rt.job = Some(job);
        self.dirty = true;
    }
}

/// Run on every tick: refresh status, connect at start-up, run a job.
pub(crate) fn tick<D: BlockDevice>(app: &mut App<D>, hw: &mut Hw<'_>) {
    let rt = &mut app.wifi_rt;
    rt.present = hw.wifi.present();
    if rt.describe.is_empty() {
        rt.describe = hw.wifi.describe();
    }
    let before = rt.connected.clone();
    rt.connected = hw.wifi.connected();
    if before.is_some() && rt.connected.is_none() {
        app.set_msg("Wi-Fi connection lost - will retry", true);
    }
    let now = hw.clock.monotonic_us();

    // Start-up, and retries while idle.
    if let Some(w) = app.state.wifi.clone()
        && app.wifi_rt.present
        && app.wifi_rt.connected.is_none()
        && app.wifi_rt.job.is_none()
    {
        let due = match app.wifi_rt.last_try_us {
            None => true,
            Some(t) => now.saturating_sub(t) >= RETRY_US && app.cart.is_empty(),
        };
        if due {
            app.wifi_rt.last_try_us = Some(now);
            match hw.wifi.connect(&w.ssid, &w.password) {
                Ok(()) => {
                    app.wifi_rt.connected = Some(w.ssid.clone());
                    app.wifi_rt.last_error = None;
                    app.set_msg(format!("Wi-Fi: connected to {}", w.ssid), false);
                }
                Err(e) => {
                    app.wifi_rt.last_error = Some(explain(e).into());
                    app.set_msg(format!("Wi-Fi {}: {}", w.ssid, explain(e)), true);
                }
            }
        }
    }
    app.wifi_rt.settled = true;

    let Some(job) = app.wifi_rt.job.take() else {
        return;
    };
    app.dirty = true;
    match job {
        Job::Scan => match hw.wifi.scan() {
            Ok(mut v) => {
                v.sort_by(|a, b| b.quality.cmp(&a.quality).then(a.ssid.cmp(&b.ssid)));
                v.dedup_by(|a, b| a.ssid == b.ssid);
                v.retain(|n| !n.ssid.is_empty());
                app.wifi_rt.networks = v;
                app.wifi_rt.scanned = true;
                app.wifi_rt.last_error = None;
            }
            Err(e) => {
                app.wifi_rt.last_error = Some(explain(e).into());
                app.set_msg(format!("Wi-Fi scan: {}", explain(e)), true);
            }
        },
        Job::Connect { ssid, password, by } => {
            let settings = WifiSettings {
                ssid: ssid.clone(),
                password: password.clone(),
            };
            if let Err(e) = settings.validate() {
                app.set_msg(format!("Wi-Fi: {e}"), true);
                return;
            }
            match hw.wifi.connect(&ssid, &password) {
                Ok(()) => {
                    app.wifi_rt.connected = Some(ssid.clone());
                    app.wifi_rt.last_error = None;
                    let at = app.now(hw);
                    match app.commit(vec![Event::WifiSet {
                        by,
                        at,
                        wifi: Some(settings),
                    }]) {
                        Ok(_) => app.set_msg(
                            format!("Wi-Fi: connected to {ssid}; it is joined at every start"),
                            false,
                        ),
                        Err(e) => app.err(&e),
                    }
                }
                Err(e) => {
                    app.wifi_rt.last_error = Some(explain(e).into());
                    app.set_msg(format!("Wi-Fi {ssid}: {}", explain(e)), true);
                }
            }
        }
        Job::Disconnect => {
            let _ = hw.wifi.disconnect();
            app.wifi_rt.connected = None;
            app.set_msg("Wi-Fi disconnected", false);
        }
    }
}

/// Signal strength in words.
pub(crate) fn signal(q: u8) -> &'static str {
    match q {
        75.. => "excellent",
        50..=74 => "good",
        25..=49 => "fair",
        _ => "weak",
    }
}
