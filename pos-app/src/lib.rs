//! The POS application, generic over the HAL. Both Tier A (UEFI) and Tier B
//! (own kernel) run this same code; they differ only in the HAL objects they
//! pass in.
//!
//! Durability rule: every state change is an [`Event`] committed through
//! [`App::commit`] (write + flush via pos-store) *before* the UI confirms it
//! or the printer prints.
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;
#[cfg(test)]
extern crate std;

mod admin;
mod desktop;
pub mod export;
pub mod hasher;
pub mod pictures;
pub mod receipt;
mod reports;
mod screens;
pub mod settings;
pub mod stock;
pub mod sync;
mod wifi;

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use pos_core::Money;
use pos_core::auth::UserId;
use pos_core::cart::Cart;
use pos_core::fiscal::{FiscalPolicy, GenericPolicy};
use pos_core::sale::{Refund, Sale};
use pos_core::state::{DomainError, Event, PosState};
use pos_core::tender::Payment;
use pos_hal::{BlockDevice, Clock, Display, ExternalMedia, InputEvent, Printer, Rng};
use pos_store::{Committed, Store, StoreError, Trust};
use pos_ui::Canvas;
use pos_ui::scan::{ScanConfig, ScanDetector};

pub use hasher::Argon2Hasher;
pub use screens::Screen;

/// Hardware the app uses while handling one event.
pub struct Hw<'a> {
    pub display: &'a mut dyn Display,
    pub clock: &'a dyn Clock,
    pub printer: &'a mut dyn Printer,
    pub rng: &'a mut dyn Rng,
    pub media: &'a mut dyn ExternalMedia,
    pub wifi: &'a mut dyn pos_hal::Wifi,
    pub sound: &'a mut dyn pos_hal::Sound,
}

impl core::fmt::Debug for Hw<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("Hw")
    }
}

/// What the platform should do after an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    Continue,
    /// Cold reset (after a storage fault, or on request).
    Reset,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppError {
    Domain(DomainError),
    Store(StoreError),
    Encode,
    Fiscal(String),
}

impl From<DomainError> for AppError {
    fn from(e: DomainError) -> Self {
        AppError::Domain(e)
    }
}
impl From<StoreError> for AppError {
    fn from(e: StoreError) -> Self {
        AppError::Store(e)
    }
}

pub fn describe(e: &AppError) -> String {
    match e {
        AppError::Domain(d) => match d {
            DomainError::Forbidden(p) => format!("Not permitted: {}", p.label()),
            DomainError::Locked => "User locked - ask a manager".into(),
            DomainError::BadPin => "Wrong PIN".into(),
            DomainError::NoShift => "No shift open".into(),
            DomainError::RefundExceedsSale => "Refund exceeds what was sold".into(),
            DomainError::UnknownReceipt => "Unknown or expired receipt".into(),
            DomainError::Tender(t) => format!("Tender rejected: {t:?}"),
            DomainError::ManagedByHead => {
                "Managed by the head till (catalog, users, settings, pictures)".into()
            }
            DomainError::Sync(s) => format!("Store network: {s}"),
            DomainError::Stock(s) => format!("Stock: {s}"),
            other => format!("{other:?}"),
        },
        AppError::Store(StoreError::Full) => {
            "JOURNAL FULL - manager must export (F10 > Export)".into()
        }
        AppError::Store(s) => format!("Storage error: {s:?}"),
        AppError::Encode => "Encoding error".into(),
        AppError::Fiscal(s) => format!("Fiscal policy: {s}"),
    }
}

/// Stable record kind codes stored in the record header (for tooling; the
/// payload is self-describing).
pub fn kind_code(e: &Event) -> u16 {
    match e {
        Event::ConfigSet { .. } => 1,
        Event::UserSet { .. } => 2,
        Event::LoginOk { .. } => 3,
        Event::LoginFailed { .. } => 4,
        Event::Logout { .. } => 5,
        Event::UserUnlocked { .. } => 6,
        Event::CatalogUpsert { .. } => 7,
        Event::CatalogDelete { .. } => 8,
        Event::CatalogReplace { .. } => 9,
        Event::ShiftOpened { .. } => 10,
        Event::SaleCompleted(_) => 11,
        Event::RefundCompleted(_) => 12,
        Event::LineVoided { .. } => 13,
        Event::SaleVoided { .. } => 14,
        Event::Override { .. } => 15,
        Event::NoSale { .. } => 16,
        Event::Reprint { .. } => 17,
        Event::Report(_) => 18,
        Event::Exported { .. } => 19,
        Event::StoreAdopted { .. } => 20,
        Event::StockReceived { .. } => 21,
        Event::StockCounted { .. } => 22,
        Event::StockWrittenOff { .. } => 23,
        Event::ReorderLevelSet { .. } => 24,
        Event::PictureSet { .. } => 25,
        Event::NetConfigured { .. } => 26,
        Event::SyncReceived { .. } => 27,
        Event::SyncAcked { .. } => 28,
        Event::SyncRelayed { .. } => 29,
        Event::SyncResynced { .. } => 30,
        Event::SyncSection { .. } => 31,
        Event::PolicySet { .. } => 32,
        Event::UiSet { .. } => 33,
        Event::WifiSet { .. } => 34,
    }
}

/// A recently committed sale/refund kept in memory for reprints.
#[derive(Debug, Clone)]
pub enum Journaled {
    Sale(Sale, [u8; 32]),
    Refund(Refund, [u8; 32]),
}

const RECENT: usize = 64;

/// Sounds the app asks for; played (or not, per the till's settings)
/// once the current event is handled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Chirp {
    /// A scan or code added an item.
    Found,
    /// Nothing matched.
    Missing,
    /// Key or tap feedback.
    Click,
}

/// Which app the signed-in user is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Place {
    Home,
    Till,
    Inventory,
    Settings,
    Reports,
}

pub struct App<D: BlockDevice> {
    pub(crate) store: Option<Store<D>>,
    pub(crate) state: PosState,
    pub(crate) policy: Box<dyn FiscalPolicy>,
    pub(crate) hasher: Box<dyn pos_core::auth::PinHasher>,
    pub(crate) canvas: Canvas,
    pub(crate) screen: Screen,
    pub(crate) user: Option<UserId>,
    pub(crate) cart: Cart,
    pub(crate) sel: Option<usize>,
    pub(crate) payment: Option<Payment>,
    pub(crate) entry: String,
    pub(crate) pending_qty: Option<pos_core::Qty>,
    pub(crate) scan: ScanDetector,
    pub(crate) message: Option<(String, bool)>,
    pub(crate) recent: VecDeque<Journaled>,
    pub(crate) key: [u8; 32],
    /// Where the current press began, and where the pointer is now.
    pub(crate) press_from: Option<(i32, i32)>,
    pub(crate) press_now: Option<(i32, i32)>,
    /// Last accepted tap (time, position), for debouncing.
    pub(crate) last_tap: Option<(u64, i32, i32)>,
    /// Quick-key page shown, and whether the sale keypad replaces it.
    pub(crate) qk_page: usize,
    pub(crate) sale_pad: bool,
    /// The sale just completed, for the change screen.
    pub(crate) last_sale: Option<Sale>,
    /// Tappable areas of the last frame drawn.
    pub(crate) targets: Vec<(pos_ui::Rect, String)>,
    pub(crate) dirty: bool,
    pub(crate) fatal: bool,
    pub(crate) printer_ok: bool,
    pub(crate) now_us: u64,
    /// Device kept while the store cannot be opened (format/recovery flow).
    pub(crate) pending_dev: Option<D>,
    pub(crate) place: Place,
    pub(crate) stock: stock::StockList,
    pub(crate) gallery: pictures::Gallery,
    pub(crate) net_rt: sync::SyncRuntime,
    /// The signed-in user's record changed (from the head) during a sale.
    pub(crate) stale_user: bool,
    /// Monotonic time of the last key or tap (automatic sign-out).
    pub(crate) last_input_us: Option<u64>,
    pub(crate) chirps: Vec<Chirp>,
    /// The clock screen saver is showing (any key or tap wakes it).
    pub(crate) saver: bool,
    pub(crate) wifi_rt: wifi::WifiRuntime,
}

impl<D: BlockDevice> core::fmt::Debug for App<D> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("App").field("screen", &self.screen).finish()
    }
}

/// Outcome of opening the store at boot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BootStatus {
    Ready,
    FirstRun,
    Foreign,
    Unformatted,
    Corrupt(String),
}

impl<D: BlockDevice> App<D> {
    /// Open the store, recover state, and choose the first screen.
    /// `key` is this machine's store-binding key.
    pub fn boot(dev: D, key: [u8; 32], width: u32, height: u32) -> (App<D>, BootStatus) {
        Self::boot_with(dev, key, width, height, Box::new(Argon2Hasher::new()))
    }

    pub fn boot_with(
        dev: D,
        key: [u8; 32],
        width: u32,
        height: u32,
        hasher: Box<dyn pos_core::auth::PinHasher>,
    ) -> (App<D>, BootStatus) {
        let mut app = App {
            store: None,
            state: PosState::default(),
            policy: Box::new(GenericPolicy),
            hasher,
            canvas: Canvas::new(width, height),
            screen: Screen::Blank,
            user: None,
            cart: Cart::default(),
            sel: None,
            payment: None,
            entry: String::new(),
            pending_qty: None,
            scan: ScanDetector::new(ScanConfig::default()),
            message: None,
            recent: VecDeque::new(),
            key,
            press_from: None,
            press_now: None,
            last_tap: None,
            qk_page: 0,
            sale_pad: false,
            last_sale: None,
            targets: Vec::new(),
            dirty: true,
            fatal: false,
            printer_ok: true,
            now_us: 0,
            pending_dev: None,
            place: Place::Home,
            stock: stock::StockList::default(),
            gallery: pictures::Gallery::default(),
            net_rt: sync::SyncRuntime::default(),
            stale_user: false,
            last_input_us: None,
            chirps: Vec::new(),
            saver: false,
            wifi_rt: wifi::WifiRuntime::default(),
        };
        let status = match Store::open(dev, &key) {
            Ok(rec) => {
                let trust = rec.store.trust();
                let truncated = rec.truncated;
                match app.recover(rec.snapshot.as_deref(), &rec.records) {
                    Ok(()) => {
                        app.store = Some(rec.store);
                        match trust {
                            Trust::Unbound => BootStatus::FirstRun,
                            Trust::Foreign => BootStatus::Foreign,
                            Trust::Bound if app.state.needs_setup() => BootStatus::FirstRun,
                            Trust::Bound => {
                                if truncated {
                                    app.message = Some((
                                        "Recovered after power loss: an unfinished write was discarded".into(),
                                        true,
                                    ));
                                }
                                BootStatus::Ready
                            }
                        }
                    }
                    Err(e) => {
                        app.pending_dev = Some(rec.store.into_device());
                        BootStatus::Corrupt(e)
                    }
                }
            }
            Err(e) => {
                let st = match e.error {
                    StoreError::Unformatted => BootStatus::Unformatted,
                    other => BootStatus::Corrupt(format!("{other:?}")),
                };
                app.pending_dev = Some(e.dev);
                st
            }
        };
        app.screen = screens::initial_screen(&app, &status);
        (app, status)
    }

    fn recover(
        &mut self,
        snapshot: Option<&[u8]>,
        records: &[pos_store::Record],
    ) -> Result<(), String> {
        let mut st = match snapshot {
            Some(b) => PosState::from_snapshot(b).ok_or_else(|| String::from("snapshot decode"))?,
            None => PosState::default(),
        };
        for r in records {
            let e: Event =
                postcard::from_bytes(&r.payload).map_err(|_| format!("record {} decode", r.seq))?;
            st.apply(&e)
                .map_err(|err| format!("record {} rejected: {err:?}", r.seq))?;
            self.remember(&e, &r.hash);
        }
        self.state = st;
        self.scan.set_config(ScanConfig {
            max_gap_us: u64::from(self.state.config.scanner_max_gap_ms) * 1000,
            min_len: usize::from(self.state.config.scanner_min_len),
        });
        Ok(())
    }

    fn remember(&mut self, e: &Event, hash: &[u8; 32]) {
        let j = match e {
            Event::SaleCompleted(s) => Journaled::Sale(s.clone(), *hash),
            Event::RefundCompleted(r) => Journaled::Refund(r.clone(), *hash),
            _ => return,
        };
        if self.recent.len() >= RECENT {
            self.recent.pop_front();
        }
        self.recent.push_back(j);
    }

    pub fn state(&self) -> &PosState {
        &self.state
    }
    pub fn screen(&self) -> &Screen {
        &self.screen
    }
    pub fn place(&self) -> Place {
        self.place
    }
    pub fn cart(&self) -> &Cart {
        &self.cart
    }
    pub fn message(&self) -> Option<&str> {
        self.message.as_ref().map(|(m, _)| m.as_str())
    }
    /// Show a message in the status bar (platform warnings).
    pub fn notice(&mut self, msg: &str, error: bool) {
        self.set_msg(msg, error);
    }

    pub fn store_trust(&self) -> Option<Trust> {
        self.store.as_ref().map(Store::trust)
    }

    /// Validate events against a copy of the state, commit them durably as
    /// one batch, then apply them. Nothing is applied if anything fails.
    pub fn commit(&mut self, events: Vec<Event>) -> Result<Vec<Committed>, AppError> {
        let mut trial = self.state.clone();
        let mut payloads = Vec::with_capacity(events.len());
        for e in &events {
            trial.apply(e)?;
            payloads.push((
                kind_code(e),
                postcard::to_allocvec(e).map_err(|_| AppError::Encode)?,
            ));
        }
        let store = self
            .store
            .as_mut()
            .ok_or(AppError::Store(StoreError::NotTrusted))?;
        let recs: Vec<(u16, &[u8])> = payloads.iter().map(|(k, p)| (*k, p.as_slice())).collect();
        let committed = match store.append_batch(&recs) {
            Ok(c) => c,
            Err(e) => {
                if store.is_poisoned() {
                    self.fatal = true;
                }
                return Err(e.into());
            }
        };
        self.state = trial;
        for (e, c) in events.iter().zip(&committed) {
            self.remember(e, &c.hash);
        }
        if events.iter().any(|e| matches!(e, Event::PictureSet { .. })) {
            self.gallery.clear();
        }
        // Periodic snapshot; failure only costs a longer replay next boot.
        let every = u64::from(self.state.config.snapshot_every);
        let Some(store) = self.store.as_mut() else {
            return Ok(committed);
        };
        if store.records_since_snapshot() >= every
            && let Ok(bytes) = postcard::to_allocvec(&self.state)
            && store.write_snapshot(&bytes).is_err()
            && store.is_poisoned()
        {
            self.fatal = true;
        }
        Ok(committed)
    }

    pub(crate) fn now(&self, hw: &Hw<'_>) -> i64 {
        // Never let journal time go backwards even if the RTC is wrong.
        let t = hw.clock.wall().ok().and_then(|d| d.to_unix()).unwrap_or(0);
        t.max(self.state.last_event_at)
    }

    pub(crate) fn print(&mut self, hw: &mut Hw<'_>, bytes: &[u8]) {
        match hw.printer.write(bytes) {
            Ok(()) => self.printer_ok = true,
            Err(e) => {
                self.printer_ok = false;
                self.message = Some((format!("Printer: {e} - reprint from manager menu"), true));
            }
        }
    }

    pub(crate) fn user_name(&self, id: UserId) -> String {
        self.state
            .users
            .get(&id)
            .map(|u| u.name.clone())
            .unwrap_or_else(|| id.to_string())
    }

    pub(crate) fn set_msg(&mut self, m: impl Into<String>, err: bool) {
        self.message = Some((m.into(), err));
        self.dirty = true;
    }

    pub(crate) fn err(&mut self, e: &AppError) {
        let s = describe(e);
        self.set_msg(s, true);
    }

    pub(crate) fn chirp(&mut self, c: Chirp) {
        self.chirps.push(c);
    }

    fn play(&mut self, hw: &mut Hw<'_>) {
        let p = &self.state.ui;
        for c in core::mem::take(&mut self.chirps) {
            match c {
                Chirp::Found if p.beep_on_scan => hw.sound.beep(1800, 45),
                Chirp::Missing if p.beep_on_scan => hw.sound.beep(320, 160),
                Chirp::Click if p.key_click => hw.sound.beep(2600, 4),
                _ => {}
            }
        }
    }

    /// Feed one input event. Returns what the platform should do next.
    pub fn handle(&mut self, ev: InputEvent, hw: &mut Hw<'_>) -> Control {
        self.dirty = true;
        // The screen saver swallows the input that wakes it.
        if self.saver {
            let wake = match ev {
                InputEvent::Key { at_us, .. } => Some(at_us),
                InputEvent::Pointer {
                    pressed: false,
                    at_us,
                    ..
                } => Some(at_us),
                InputEvent::Pointer { .. } => None,
            };
            if let Some(at) = wake {
                self.saver = false;
                self.last_input_us = Some(at);
                self.press_from = None;
                self.press_now = None;
                self.canvas.invalidate();
            }
            return Control::Continue;
        }
        match ev {
            InputEvent::Key { .. } | InputEvent::Pointer { pressed: false, .. } => {
                self.chirp(Chirp::Click);
            }
            _ => {}
        }
        match ev {
            InputEvent::Key { key, mods, at_us } => {
                self.now_us = at_us;
                self.last_input_us = Some(at_us);
                screens::on_key(self, key, mods, at_us, hw);
            }
            InputEvent::Pointer {
                x,
                y,
                pressed,
                at_us,
            } => {
                self.now_us = at_us;
                self.last_input_us = Some(at_us);
                let p = (x as i32, y as i32);
                // A tap is a press and a release; the widget acts only if
                // both land on it. While held, the widget under the finger
                // is drawn pressed.
                if pressed {
                    if self.press_from.is_none() {
                        self.press_from = Some(p);
                    }
                    self.press_now = Some(p);
                } else if let Some(from) = self.press_from.take() {
                    self.press_now = None;
                    if !self.bounce(at_us, p) {
                        self.last_tap = Some((at_us, p.0, p.1));
                        screens::on_click(self, (from, p), hw);
                    }
                }
            }
        }
        if self.stale_user && self.cart.is_empty() && !matches!(self.screen, Screen::Pay) {
            self.stale_user = false;
            self.user = None;
            self.place = Place::Home;
            self.screen = screens::login_screen(self);
        }
        self.play(hw);
        if self.fatal {
            Control::Reset
        } else {
            Control::Continue
        }
    }

    /// Resistive panels and shaky fingers can report a second tap a few
    /// tens of milliseconds after the first; drop a tap that close in time
    /// and place to the last accepted one.
    fn bounce(&self, at_us: u64, p: (i32, i32)) -> bool {
        const WINDOW_US: u64 = 150_000;
        let near = self.canvas.px(24);
        matches!(self.last_tap, Some((t, x, y))
            if at_us.saturating_sub(t) < WINDOW_US
                && (x - p.0).abs() <= near
                && (y - p.1).abs() <= near)
    }

    /// Periodic tick (clock in the status bar, automatic sign-out).
    pub fn tick(&mut self, hw: &mut Hw<'_>) {
        self.dirty = true;
        self.canvas.warm(48);
        self.idle_signout(hw);
        self.screen_saver(hw);
        wifi::tick(self, hw);
    }

    /// Start the clock screen saver after the idle time, unless a sale or
    /// payment is open.
    fn screen_saver(&mut self, hw: &mut Hw<'_>) {
        let mins = self.state.ui.saver_mins;
        if mins == 0 || self.saver || !self.cart.is_empty() || self.payment.is_some() {
            return;
        }
        let now = hw.clock.monotonic_us();
        let last = *self.last_input_us.get_or_insert(now);
        if now.saturating_sub(last) >= u64::from(mins) * 60_000_000 {
            self.saver = true;
        }
    }

    /// Is the screen saver showing? (tests)
    pub fn saver_on(&self) -> bool {
        self.saver
    }

    /// Sign out after the store's idle limit, never with a sale open.
    fn idle_signout(&mut self, hw: &mut Hw<'_>) {
        let mins = self.state.policy.idle_signout_mins;
        if mins == 0 || self.user.is_none() || !self.cart.is_empty() || self.payment.is_some() {
            return;
        }
        if matches!(self.screen, Screen::Pay) {
            return;
        }
        let now = hw.clock.monotonic_us();
        let last = self.last_input_us.get_or_insert(now);
        if now.saturating_sub(*last) >= u64::from(mins) * 60_000_000 {
            self.last_input_us = None;
            self.screen = screens::sign_out(self, hw);
            self.set_msg(
                format!(
                    "Signed out after {mins} minute{} without use",
                    if mins == 1 { "" } else { "s" }
                ),
                false,
            );
        }
    }

    /// May the signed-in user open this app?
    pub(crate) fn can_use(&self, place: Place) -> bool {
        use pos_core::auth::Permission as P;
        let Some(u) = self.user else { return false };
        let may = |p| self.state.allows(u, p);
        match place {
            // Everyone may look stock up; changing it needs `Stock`.
            Place::Home | Place::Settings | Place::Inventory => true,
            Place::Till => may(P::Sell),
            Place::Reports => may(P::XReport),
        }
    }

    /// Wall-clock time without a clock (pages drawn between events).
    pub(crate) fn now_unix(&self) -> i64 {
        self.state.last_event_at
    }

    pub(crate) fn store_usage(&self) -> (u64, u64) {
        self.store.as_ref().map(|s| s.usage()).unwrap_or((0, 1))
    }

    /// Redraw if needed and present to the display.
    pub fn render(&mut self, hw: &mut Hw<'_>) {
        if !self.dirty {
            // Idle: get a few glyphs ready for screens not yet shown.
            self.canvas.warm(4);
            return;
        }
        self.dirty = false;
        let now = self.now(hw);
        let ptr = pos_ui::PointerFrame {
            held: self.press_now,
            click: None,
        };
        let from = self.press_from;
        screens::draw(self, ptr, from, now);
        let _ = self.canvas.present(hw.display);
    }

    /// Shut down and hand back the storage device (tests, reformat).
    pub fn into_device(self) -> Option<D> {
        self.store.map(Store::into_device).or(self.pending_dev)
    }

    /// Tappable areas of the last frame, with their labels (tests).
    pub fn tap_targets(&self) -> &[(pos_ui::Rect, String)] {
        &self.targets
    }

    /// Bytes of the current back buffer (tests).
    pub fn canvas(&self) -> &Canvas {
        &self.canvas
    }

    /// Format an unformatted or corrupt partition (manager-confirmed flow).
    pub(crate) fn reformat(&mut self, mut dev: D) -> Result<(), StoreError> {
        pos_store::format(&mut dev)?;
        let rec = Store::open(dev, &self.key).map_err(|e| e.error)?;
        self.store = Some(rec.store);
        self.state = PosState::default();
        Ok(())
    }

    pub(crate) fn money(&self, m: Money) -> String {
        format!("{}", m.display(self.state.config.decimals))
    }
}
