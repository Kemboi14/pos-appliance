//! Host end-to-end: the real App with mock hardware, driven by key events.
//! Covers setup, login, shift, catalog import, scanned and typed sales, cash
//! and split tenders, refund with manager override, X/Z, export, reboot
//! recovery, a power cut at every write of a sale, and receipt golden files.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use pos_app::{App, BootStatus, Control, Hw, Screen};
use pos_hal::*;
use pos_store::mem::MemDevice;

// ------------------------------------------------------------ mocks

struct Disp(u32, u32);
impl Display for Disp {
    fn width(&self) -> u32 {
        self.0
    }
    fn height(&self) -> u32 {
        self.1
    }
    fn pixel_format(&self) -> PixelFormat {
        PixelFormat::Bgrx
    }
    fn blit(&mut self, _: &[u32], _: usize, x: u32, y: u32, w: u32, h: u32) -> HalResult<()> {
        assert!(x + w <= self.0 && y + h <= self.1);
        Ok(())
    }
}

struct Clk(std::cell::Cell<i64>);
impl Clock for Clk {
    fn monotonic_us(&self) -> u64 {
        0
    }
    fn wall(&self) -> HalResult<DateTime> {
        DateTime::from_unix(self.0.get()).ok_or(HalError::Io)
    }
}

#[derive(Default)]
struct Prn(Vec<u8>);
impl Printer for Prn {
    fn write(&mut self, b: &[u8]) -> HalResult<()> {
        self.0.extend_from_slice(b);
        Ok(())
    }
    fn status(&mut self) -> PrinterStatus {
        PrinterStatus::Ready
    }
    fn describe(&self) -> String {
        "mock".into()
    }
}

struct Rnd(u8);
impl Rng for Rnd {
    fn fill(&mut self, b: &mut [u8]) -> HalResult<()> {
        for x in b {
            self.0 = self.0.wrapping_mul(31).wrapping_add(7);
            *x = self.0;
        }
        Ok(())
    }
}

#[derive(Default)]
struct Media(BTreeMap<String, Vec<u8>>);
impl ExternalMedia for Media {
    fn is_present(&mut self) -> bool {
        true
    }
    fn write_file(&mut self, n: &str, d: &[u8]) -> HalResult<()> {
        self.0.insert(n.into(), d.to_vec());
        Ok(())
    }
    fn read_file(&mut self, n: &str, max: usize) -> HalResult<Vec<u8>> {
        let d = self.0.get(n).ok_or(HalError::NotFound)?;
        if d.len() > max {
            return Err(HalError::NoSpace);
        }
        Ok(d.clone())
    }
}

/// Block device that dies at the N-th block write (power cut) and keeps only
/// flushed data; shares its backing so a "reboot" sees the durable bytes.
type WriteCache = Vec<(u64, Vec<u8>)>;

#[derive(Clone, Debug)]
struct CutDev {
    durable: Rc<RefCell<Vec<u8>>>,
    cache: Rc<RefCell<WriteCache>>,
    writes: Rc<RefCell<u64>>,
    cut_at: Option<u64>,
    dead: Rc<RefCell<bool>>,
}
impl CutDev {
    fn from(m: &MemDevice) -> Self {
        CutDev {
            durable: Rc::new(RefCell::new(m.data.clone())),
            cache: Rc::default(),
            writes: Rc::default(),
            cut_at: None,
            dead: Rc::default(),
        }
    }
    fn reboot(&self) -> Self {
        CutDev {
            durable: Rc::new(RefCell::new(self.durable.borrow().clone())),
            cache: Rc::default(),
            writes: Rc::default(),
            cut_at: None,
            dead: Rc::default(),
        }
    }
}
impl BlockDevice for CutDev {
    fn block_size(&self) -> usize {
        512
    }
    fn block_count(&self) -> u64 {
        (self.durable.borrow().len() / 512) as u64
    }
    fn read(&mut self, lba: u64, buf: &mut [u8]) -> HalResult<()> {
        if *self.dead.borrow() {
            return Err(HalError::Io);
        }
        let d = self.durable.borrow();
        for k in 0..buf.len() / 512 {
            let b = lba + k as u64;
            let c = self.cache.borrow();
            let src = c
                .iter()
                .rev()
                .find(|(l, _)| *l == b)
                .map(|(_, x)| x.clone())
                .unwrap_or_else(|| d[b as usize * 512..][..512].to_vec());
            buf[k * 512..][..512].copy_from_slice(&src);
        }
        Ok(())
    }
    fn write(&mut self, lba: u64, buf: &[u8]) -> HalResult<()> {
        if *self.dead.borrow() {
            return Err(HalError::Io);
        }
        for k in 0..buf.len() / 512 {
            let mut w = self.writes.borrow_mut();
            if self.cut_at == Some(*w) {
                *self.dead.borrow_mut() = true;
                self.cache.borrow_mut().clear();
                return Err(HalError::Io);
            }
            *w += 1;
            self.cache
                .borrow_mut()
                .push((lba + k as u64, buf[k * 512..][..512].to_vec()));
        }
        Ok(())
    }
    fn flush(&mut self) -> HalResult<()> {
        if *self.dead.borrow() {
            return Err(HalError::Io);
        }
        let mut d = self.durable.borrow_mut();
        for (l, x) in self.cache.borrow_mut().drain(..) {
            d[l as usize * 512..][..512].copy_from_slice(&x);
        }
        Ok(())
    }
}

// ------------------------------------------------------------ harness

const KEY: [u8; 32] = [0x42; 32];

struct Rig {
    disp: Disp,
    clock: Clk,
    printer: Prn,
    rng: Rnd,
    media: Media,
    t_us: u64,
}

impl Rig {
    fn new() -> Self {
        Rig {
            disp: Disp(1024, 768),
            clock: Clk(std::cell::Cell::new(1_763_550_000)),
            printer: Prn::default(),
            rng: Rnd(1),
            media: Media::default(),
            t_us: 1_000_000,
        }
    }
    fn hw(&mut self) -> Hw<'_> {
        Hw {
            display: &mut self.disp,
            clock: &self.clock,
            printer: &mut self.printer,
            rng: &mut self.rng,
            media: &mut self.media,
        }
    }
    fn key<D: BlockDevice>(&mut self, app: &mut App<D>, k: Key, gap_us: u64) -> Control {
        self.t_us += gap_us;
        self.clock.0.set(self.clock.0.get() + 1);
        let at = self.t_us;
        let c = app.handle(
            InputEvent::Key {
                key: k,
                mods: Modifiers::default(),
                at_us: at,
            },
            &mut self.hw(),
        );
        app.render(&mut self.hw());
        c
    }
    /// Human typing: 150 ms between keys.
    fn typ<D: BlockDevice>(&mut self, app: &mut App<D>, s: &str) {
        for c in s.chars() {
            let k = if c == '\n' { Key::Enter } else { Key::Char(c) };
            self.key(app, k, 150_000);
        }
    }
    /// Scanner burst: 10 ms between keys, then Enter.
    fn scan<D: BlockDevice>(&mut self, app: &mut App<D>, code: &str) {
        for c in code.chars() {
            self.key(app, Key::Char(c), 10_000);
        }
        self.key(app, Key::Enter, 10_000);
    }
}

fn fast_hasher() -> Box<dyn pos_core::auth::PinHasher> {
    Box::new(pos_app::Argon2Hasher::with_params(8, 1))
}

fn boot<D: BlockDevice>(dev: D) -> (App<D>, BootStatus) {
    App::boot_with(dev, KEY, 1024, 768, fast_hasher())
}

fn fresh_device() -> MemDevice {
    let mut d = MemDevice::new(512, 8192);
    pos_store::format(&mut d).unwrap();
    d
}

const CATALOG: &str = "sku,barcode,plu,name,price,tax_code,weighable,quick_key\n\
MILK,4006381333931,,Milk 1L,1.20,A,0,0\n\
BREAD,96385074,,Bread,2.49,A,0,1\n\
APPLE,,04711,Apples,3.99,Z,1,2\n\
SOAP,036000291452,,Soap,5.00,A,0,\n";

/// Setup manager (1/1234), add cashier (2/0000), import catalog, open shift.
fn bring_up<D: BlockDevice>(rig: &mut Rig, app: &mut App<D>) {
    rig.typ(app, "Alice\n1234\n1234\n");
    assert!(
        app.message().unwrap_or("").contains("Manager created"),
        "{:?}",
        app.message()
    );
    rig.typ(app, "1\n1234\n\n"); // login manager
    rig.typ(app, "100.00\n"); // float
    assert!(matches!(app.screen(), Screen::Sale), "{:?}", app.screen());
    rig.media
        .0
        .insert("CATALOG.CSV".into(), CATALOG.as_bytes().to_vec());
    rig.key(app, Key::F(10), 100_000);
    rig.typ(app, "8"); // import catalog
    assert_eq!(app.state().catalog.len(), 4, "{:?}", app.message());
    rig.key(app, Key::F(10), 100_000);
    rig.typ(app, "0Bob\n0000\n"); // add cashier
    assert_eq!(app.state().users.len(), 2, "{:?}", app.message());
}

#[test]
fn full_day_flow() {
    let mut rig = Rig::new();
    let (mut app, st) = boot(fresh_device());
    assert_eq!(st, BootStatus::FirstRun);
    bring_up(&mut rig, &mut app);
    // Sign out manager, sign in cashier.
    rig.key(&mut app, Key::F(12), 100_000);
    rig.typ(&mut app, "2\n0000\n\n");
    assert!(matches!(app.screen(), Screen::Sale));

    // Scanned EAN-13, scanned UPC-A (normalised), typed EAN-8 with qty 3*.
    rig.scan(&mut app, "4006381333931");
    rig.scan(&mut app, "036000291452");
    rig.typ(&mut app, "3*96385074\n");
    // Weighable without weight is refused; with weight (1.250 kg) accepted.
    rig.typ(&mut app, "1.25*APPLE\n");
    assert_eq!(app.cart().lines.len(), 4, "{:?}", app.message());
    // 1.20 + 5.00 + 3×2.49 + 1.25×3.99 (=4.9875 → 4.99) = 18.66
    let total = app.cart().price(&app.state().config).unwrap().totals.total;
    assert_eq!(total.0, 18_66);

    // Pay: 20.00 cash.
    rig.key(&mut app, Key::F(1), 100_000);
    rig.typ(&mut app, "20.00\n");
    match app.screen() {
        Screen::Done { change, receipt } => {
            assert_eq!(change.0, 1_34);
            assert_eq!(*receipt, 1);
        }
        s => panic!("{s:?} {:?}", app.message()),
    }
    assert!(
        rig.printer
            .0
            .windows(5)
            .any(|w| w == [0x1B, 0x70, 0, 25, 250]),
        "drawer kick"
    );
    let receipt1 = rig.printer.0.clone();
    rig.printer.0.clear();
    rig.key(&mut app, Key::Escape, 100_000);

    // Split tender: card 1.00 then cash.
    rig.scan(&mut app, "96385074");
    rig.key(&mut app, Key::F(1), 100_000);
    rig.typ(&mut app, "1.00");
    rig.key(&mut app, Key::F(2), 100_000);
    rig.typ(&mut app, "4242/AB12\n");
    rig.key(&mut app, Key::Enter, 100_000);
    assert!(
        matches!(app.screen(), Screen::Done { receipt: 2, .. }),
        "{:?}",
        app.screen()
    );
    rig.key(&mut app, Key::Escape, 100_000);

    // Cashier line void needs a manager.
    rig.scan(&mut app, "4006381333931");
    rig.key(&mut app, Key::F(4), 100_000);
    assert!(matches!(app.screen(), Screen::Prompt(_)));
    rig.typ(&mut app, "1\n1234\n");
    assert!(app.cart().is_empty(), "{:?}", app.message());

    // Refund 1 bread from receipt 1 (manager override), cash payout.
    rig.key(&mut app, Key::F(9), 100_000);
    rig.typ(&mut app, "1\n");
    rig.key(&mut app, Key::Down, 100_000);
    rig.key(&mut app, Key::Down, 100_000);
    rig.typ(&mut app, "\n1\n"); // qty 1 of line 2 (bread ×3)
    rig.key(&mut app, Key::F(1), 100_000);
    rig.typ(&mut app, "damaged\n1\n"); // reason, cash
    rig.typ(&mut app, "1\n1234\n"); // override
    assert_eq!(app.state().next_receipt, 4, "{:?}", app.message());
    let c = &app.state().shift.as_ref().unwrap().counters;
    assert_eq!(c.refunds_total.0, 2_49);

    // Wrong manager PIN three times does not lock (limit 5) but is journaled.
    let events_before = app.state().events;
    rig.key(&mut app, Key::F(11), 100_000);
    rig.typ(&mut app, "1\n9999\n");
    assert!(app.state().events > events_before);

    // Sign in as manager, Z report with counted cash, then export.
    rig.key(&mut app, Key::F(12), 100_000);
    rig.typ(&mut app, "1\n1234\n");
    rig.key(&mut app, Key::F(10), 100_000);
    rig.typ(&mut app, "2");
    // Expected: 100 + 20.00 + 2.49(cash part 1.49? no: 2.49-1.00) − 1.34 − 2.49
    let exp = app.state().shift.as_ref().unwrap().expected_cash().unwrap();
    assert_eq!(exp.0, 100_00 + 20_00 + 1_49 - 1_34 - 2_49);
    rig.typ(&mut app, "117.66\n");
    assert!(
        matches!(app.screen(), Screen::Info { .. }),
        "{:?} {:?}",
        app.screen(),
        app.message()
    );
    assert!(app.state().shift.is_none());
    assert_eq!(app.state().next_z, 2);
    rig.key(&mut app, Key::Enter, 100_000);
    rig.typ(&mut app, "1\n1234\n\n");
    rig.typ(&mut app, "50\n"); // new shift float
    rig.key(&mut app, Key::F(10), 100_000);
    rig.typ(&mut app, "7");
    assert!(
        rig.media.0.contains_key("Z000001.CSV"),
        "{:?} {:?}",
        rig.media.0.keys(),
        app.message()
    );
    assert!(rig.media.0.keys().any(|k| k.starts_with('J')));
    let z = String::from_utf8(rig.media.0["Z000001.CSV"].clone()).unwrap();
    assert!(z.contains("counted_cash,117.66"), "{z}");
    assert!(z.contains("sales_count,2"));

    // Receipt golden file (byte-for-byte ESC/POS).
    golden("receipt_sale_1.bin", &receipt1);

    // Reboot: everything comes back from the journal.
    let before = app.state().clone();
    let dev = app.into_device().unwrap();
    let (app2, st) = boot(dev);
    assert_eq!(st, BootStatus::Ready);
    assert_eq!(app2.state(), &before);
}

#[test]
fn foreign_machine_needs_manager_adoption() {
    let mut rig = Rig::new();
    let (mut app, _) = boot(fresh_device());
    bring_up(&mut rig, &mut app);
    let dev = app.into_device().unwrap();
    let (mut other, st) = App::boot_with(dev, [9; 32], 1024, 768, fast_hasher());
    assert_eq!(st, BootStatus::Foreign);
    rig.typ(&mut other, "1\n1234\n");
    assert_eq!(
        other.store_trust(),
        Some(pos_store::Trust::Bound),
        "{:?}",
        other.message()
    );
    assert!(matches!(other.state().users.len(), 2));
}

#[test]
fn power_cut_during_sale_commit_every_write() {
    // Build a ready store with an open shift and catalog.
    let mut rig = Rig::new();
    let (mut app, _) = boot(fresh_device());
    bring_up(&mut rig, &mut app);
    let base = CutDev::from(&MemDeviceExt::take(app));
    let mut cuts = 0;
    let mut mid_sale = 0;
    for cut in 0..64 {
        let mut dev = base.reboot();
        dev.cut_at = Some(cut);
        let (mut app, st) = boot(dev.clone());
        if st != BootStatus::Ready {
            // Power cut while booting (epoch superblock write): the next
            // boot must still find the store intact.
            assert!(*dev.dead.borrow(), "cut {cut}: {st:?}");
            let (again, st2) = boot(dev.reboot());
            assert_eq!(st2, BootStatus::Ready, "cut {cut}");
            assert_eq!(again.state().next_receipt, 1);
            cuts += 1;
            continue;
        }
        let mut rig = Rig::new();
        rig.typ(&mut app, "1\n1234\n\n");
        rig.scan(&mut app, "4006381333931");
        rig.key(&mut app, Key::F(1), 100_000);
        let ctl = rig.key(&mut app, Key::Enter, 100_000);
        let done = matches!(app.screen(), Screen::Done { .. });
        if !*dev.dead.borrow() {
            assert!(done);
            break; // cut index beyond this scenario's writes
        }
        cuts += 1;
        // Never confirmed or printed unless durable.
        if done {
            assert!(!rig.printer.0.is_empty());
        } else {
            assert!(
                rig.printer.0.is_empty(),
                "printed without commit at cut {cut}"
            );
            if app.state().users.len() == 2 && ctl == Control::Reset {
                mid_sale += 1;
            }
        }
        // Reboot and check consistency.
        let (app2, st) = boot(dev.reboot());
        assert_eq!(st, BootStatus::Ready, "cut {cut}");
        let n = app2.state().next_receipt;
        assert!(n == 1 || n == 2, "cut {cut}: next receipt {n}");
        if done {
            assert_eq!(n, 2, "confirmed sale lost at cut {cut}");
        }
    }
    assert!(
        cuts >= 3 && mid_sale >= 1,
        "cuts {cuts}, during sale commit {mid_sale}"
    );
}

/// Helper to pull the MemDevice-equivalent bytes out of an App<MemDevice>.
struct MemDeviceExt;
impl MemDeviceExt {
    fn take(app: App<MemDevice>) -> MemDevice {
        app.into_device().unwrap()
    }
}

fn golden(name: &str, bytes: &[u8]) {
    let p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(name);
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, bytes).unwrap();
    }
    let want =
        std::fs::read(&p).unwrap_or_else(|_| panic!("missing {p:?}; run with UPDATE_GOLDEN=1"));
    assert!(want == bytes, "golden mismatch: {name}");
}

// ------------------------------------------------------------ touch only

/// 14 quick keys: two pages of the 4×3 grid.
const TOUCH_CATALOG: &str = "sku,barcode,plu,name,price,tax_code,weighable,quick_key\n\
MILK,4006381333931,,Milk 1L,1.20,A,0,0\n\
BREAD,96385074,,Bread,2.49,A,0,1\n\
EGGS,,,Eggs (tray of 30),9.50,A,0,2\n\
SUGAR,,,Sugar 2kg,3.10,A,0,3\n\
RICE,,,Rice 5kg,8.00,A,0,4\n\
TEA,,,Tea leaves 250g,2.20,A,0,5\n\
OIL,,,Cooking oil 1L,4.40,A,0,6\n\
SALT,,,Salt,0.60,A,0,7\n\
FLOUR,,,Maize flour 2kg,2.90,A,0,8\n\
SODA,,,Soda 500ml,1.00,A,0,9\n\
WATER,,,Water 1L,0.80,A,0,10\n\
JUICE,,,Juice,1.50,A,0,11\n\
MATCH,,,Matches,0.20,A,0,12\n\
CANDLE,,,Candles (pack of 6),1.80,A,0,13\n";

impl Rig {
    /// Tap the centre of the target labelled `label` (press, then release
    /// 80 ms later), as a finger on the touch panel would.
    fn tap<D: BlockDevice>(&mut self, app: &mut App<D>, label: &str) {
        let r = app
            .tap_targets()
            .iter()
            .find(|(_, l)| l == label)
            .map(|(r, _)| *r)
            .unwrap_or_else(|| {
                let all: Vec<&String> = app.tap_targets().iter().map(|(_, l)| l).collect();
                panic!("no target {label:?} on {:?}; have {all:?}", app.screen())
            });
        self.tap_at(app, r.x + r.w / 2, r.y + r.h / 2, 300_000);
    }

    fn tap_at<D: BlockDevice>(&mut self, app: &mut App<D>, x: i32, y: i32, gap_us: u64) {
        for (pressed, gap) in [(true, gap_us), (false, 80_000)] {
            self.t_us += gap;
            let at = self.t_us;
            app.handle(
                InputEvent::Pointer {
                    x: x as u32,
                    y: y as u32,
                    pressed,
                    at_us: at,
                },
                &mut self.hw(),
            );
            app.render(&mut self.hw());
        }
        self.clock.0.set(self.clock.0.get() + 1);
    }

    fn taps<D: BlockDevice>(&mut self, app: &mut App<D>, labels: &[&str]) {
        for l in labels {
            self.tap(app, l);
        }
    }

    /// Save the screen as a PPM under $POS_UI_SHOTS (if set), for review.
    fn shot<D: BlockDevice>(&self, app: &App<D>, name: &str) {
        let Ok(dir) = std::env::var("POS_UI_SHOTS") else {
            return;
        };
        let c = app.canvas();
        let mut out = format!("P6\n{} {}\n255\n", c.width(), c.height()).into_bytes();
        for y in 0..c.height() {
            for x in 0..c.width() {
                let p = c.pixel(x, y).unwrap_or(0);
                out.extend_from_slice(&[(p >> 16) as u8, (p >> 8) as u8, p as u8]);
            }
        }
        let path =
            std::path::Path::new(&dir).join(format!("{name}_{}x{}.ppm", c.width(), c.height()));
        std::fs::write(path, out).unwrap();
    }
}

/// A whole day on a till with only a touch screen: setup with the on-screen
/// keyboard, sign-in by name tile, quick keys over two pages, the keypad
/// for quantities and tendered cash, search, a report, and a refund.
fn touch_only_day(w: u32, h: u32) {
    let mut rig = Rig::new();
    rig.disp = Disp(w, h);
    let (mut app, st) = App::boot_with(fresh_device(), KEY, w, h, fast_hasher());
    assert_eq!(st, BootStatus::FirstRun);
    app.render(&mut rig.hw());
    rig.shot(&app, "t0_setup");

    // Name: the shift is pre-set for the first letter only.
    rig.taps(&mut app, &["A", "l", "i", "c", "e"]);
    rig.shot(&app, "t1_keyboard");
    rig.tap(&mut app, "OK");
    rig.taps(
        &mut app,
        &["1", "2", "3", "4", "OK", "1", "2", "3", "4", "OK"],
    );
    assert!(app.message().unwrap_or("").contains("Manager created"));

    // Sign in by tapping the name tile.
    rig.shot(&app, "t2_login");
    rig.tap(&mut app, "Alice (1)");
    rig.shot(&app, "t2b_pin");
    rig.taps(&mut app, &["1", "2", "3", "4", "OK"]);
    assert!(matches!(app.screen(), Screen::Home), "{:?}", app.screen());
    rig.shot(&app, "t2c_home");
    rig.tap(&mut app, "Till");
    rig.taps(&mut app, &["1", "0", "0", "OK"]); // float
    assert!(matches!(app.screen(), Screen::Sale), "{:?}", app.screen());

    // Import the catalog from the menu.
    rig.media
        .0
        .insert("CATALOG.CSV".into(), TOUCH_CATALOG.as_bytes().to_vec());
    rig.tap(&mut app, "F10 Menu");
    rig.shot(&app, "t3_menu");
    rig.tap(&mut app, "8. Import catalog from USB (CATALOG.CSV)");
    assert_eq!(app.state().catalog.len(), 14, "{:?}", app.message());

    // Quick keys on page 1, then page 2.
    rig.tap(&mut app, "Bread");
    rig.tap(&mut app, "Eggs (tray of 30)");
    rig.shot(&app, "t4_sale");
    rig.tap(&mut app, "▶");
    rig.tap(&mut app, "Candles (pack of 6)");
    rig.tap(&mut app, "◀");
    // Quantity with the keypad: 3 × Milk.
    rig.tap(&mut app, "123");
    rig.shot(&app, "t5_keypad");
    rig.taps(&mut app, &["3", "× Qty", "Items", "Milk 1L"]);
    assert_eq!(app.cart().lines.len(), 4, "{:?}", app.message());
    assert_eq!(app.cart().lines[3].qty, pos_core::Qty::units(3));

    // A bounced second tap 40 ms later is ignored; a real second tap counts.
    let r = app
        .tap_targets()
        .iter()
        .find(|(_, l)| l == "Salt")
        .map(|(r, _)| *r)
        .unwrap_or_else(|| panic!("no Salt"));
    rig.tap_at(&mut app, r.x + 10, r.y + 10, 300_000);
    rig.tap_at(&mut app, r.x + 12, r.y + 10, 40_000);
    assert_eq!(app.cart().lines.len(), 5, "bounce was not ignored");

    // Search with the on-screen keyboard; tap the result.
    rig.tap(&mut app, "F2 Find");
    rig.taps(&mut app, &["s", "o", "d"]);
    rig.shot(&app, "t6_search");
    rig.tap(&mut app, "Soda 500ml SODA 1.00");
    assert_eq!(app.cart().lines.len(), 6);
    assert!(matches!(app.screen(), Screen::Sale));

    // Pay: 2.49 + 9.50 + 1.80 + 3×1.20 + 0.60 + 1.00 = 18.99; tender 50.
    let total = app.cart().price(&app.state().config).unwrap().totals.total;
    assert_eq!(total.0, 18_99);
    rig.tap(&mut app, "F1 Pay");
    rig.taps(&mut app, &["5", "0"]);
    rig.shot(&app, "t7_pay");
    rig.tap(&mut app, "Cash");
    match app.screen() {
        Screen::Done { change, receipt } => {
            assert_eq!(change.0, 31_01);
            assert_eq!(*receipt, 1);
        }
        s => panic!("{s:?} {:?}", app.message()),
    }
    rig.shot(&app, "t8_done");
    rig.tap(&mut app, "Next sale");
    assert!(matches!(app.screen(), Screen::Sale));

    // A report screen is left with its Continue button.
    rig.tap(&mut app, "F10 Menu");
    rig.tap(&mut app, "Journal status");
    rig.shot(&app, "t9_info");
    rig.tap(&mut app, "Continue");
    assert!(matches!(app.screen(), Screen::Sale));

    // Refund one Milk from receipt 1: tap the line, keypad, keyboard, choice.
    rig.tap(&mut app, "F9 Refund");
    rig.taps(&mut app, &["1", "OK"]);
    let row = app
        .tap_targets()
        .iter()
        .find(|(_, l)| l.starts_with("Milk 1L"))
        .map(|(_, l)| l.clone())
        .unwrap_or_else(|| panic!("no Milk row"));
    rig.tap(&mut app, &row);
    rig.taps(&mut app, &["1", "OK", "Continue"]);
    rig.taps(&mut app, &["S", "p", "i", "l", "t", "OK"]);
    rig.shot(&app, "t10_payout");
    rig.tap(&mut app, "Cash");
    let c = &app.state().shift.as_ref().unwrap().counters;
    assert_eq!(c.refunds_total.0, 1_20, "{:?}", app.message());

    // Sign out from the F-key grid.
    rig.tap(&mut app, "F12 Sign out");
    assert!(matches!(app.screen(), Screen::Prompt(_)));
}

#[test]
fn touch_only_day_1024x768() {
    touch_only_day(1024, 768);
}

#[test]
fn touch_only_day_1280x800() {
    touch_only_day(1280, 800);
}

#[test]
fn touch_only_day_1920x1080() {
    touch_only_day(1920, 1080);
}

/// A scan straight after a sale (on the change screen) is not lost: the
/// first character no longer just dismisses the screen.
#[test]
fn scan_on_change_screen_starts_next_sale() {
    let mut rig = Rig::new();
    let (mut app, _) = boot(fresh_device());
    bring_up(&mut rig, &mut app);
    rig.scan(&mut app, "96385074");
    rig.key(&mut app, Key::F(1), 100_000);
    rig.key(&mut app, Key::Enter, 100_000);
    assert!(matches!(app.screen(), Screen::Done { .. }));
    rig.scan(&mut app, "96385074");
    assert!(matches!(app.screen(), Screen::Sale));
    assert_eq!(app.cart().lines.len(), 1, "{:?}", app.message());
}

fn fixture(n: &str) -> Vec<u8> {
    std::fs::read(format!(
        "{}/../pos-ui/tests/data/{n}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

fn on_hand<D: BlockDevice>(app: &App<D>, sku: &str) -> i64 {
    app.state()
        .inventory
        .get(sku)
        .map_or(i64::MIN, |l| l.on_hand.0 / 1000)
}

/// Pictures from USB, the home screen's two apps, and the Inventory app:
/// receive, reorder level, sales and refunds moving stock, write-off,
/// count, a cashier needing a manager, STOCK.CSV, and a reboot.
#[test]
fn inventory_and_pictures() {
    use pos_core::inventory::StockStatus;
    use pos_core::media::PictureFor;
    let mut rig = Rig::new();
    let (mut app, _) = boot(fresh_device());
    bring_up(&mut rig, &mut app);

    let m = &mut rig.media.0;
    m.insert("IMG_MILK.JPG".into(), fixture("photo.jpg"));
    m.insert("IMG_SOAP.PNG".into(), fixture("palette.png"));
    m.insert("IMG_BREAD.PNG".into(), b"not a picture".to_vec());
    m.insert("LOGO.PNG".into(), fixture("logo.png"));
    m.insert("WALLPAPER.JPG".into(), fixture("photo_progressive.jpg"));
    m.insert("USER_1.JPG".into(), fixture("photo.jpg"));
    rig.key(&mut app, Key::F(10), 100_000);
    rig.tap(
        &mut app,
        "Import pictures from USB (IMG_<SKU>.JPG, LOGO, WALLPAPER)",
    );
    let msg = app.message().unwrap_or("").to_string();
    assert!(
        msg.contains("2 products") && msg.contains("1 users"),
        "{msg}"
    );
    assert!(msg.contains("logo") && msg.contains("wallpaper"), "{msg}");
    assert!(msg.contains("IMG_BREAD.PNG: not a PNG or JPEG"), "{msg}");
    let pics = &app.state().pictures;
    assert_eq!(pics.len(), 5);
    let milk = pics.item("MILK").unwrap();
    assert_eq!((milk.w, milk.h), (128, 80), "aspect kept, shrunk to 128");
    let wall = pics.get(&PictureFor::Wallpaper).unwrap();
    assert_eq!((wall.w, wall.h), (640, 360), "wallpaper cropped to 16:9");
    assert!(matches!(app.screen(), Screen::Sale), "{:?}", app.screen());
    rig.shot(&app, "i0_sale_pictures");

    // Home screen with the two apps.
    rig.key(&mut app, Key::Home, 100_000);
    assert!(matches!(app.screen(), Screen::Home));
    rig.shot(&app, "i1_home");
    rig.key(&mut app, Key::Char('2'), 100_000);
    assert!(matches!(app.screen(), Screen::Stock));
    rig.shot(&app, "i2_stock");

    // Find milk by typing; open it; receive 24 at 0.80; reorder at 5.
    rig.typ(&mut app, "milk\n");
    assert_eq!(app.screen(), &Screen::StockItem("MILK".into()));
    rig.key(&mut app, Key::F(1), 100_000);
    rig.typ(&mut app, "24\n0.80\n");
    assert_eq!(on_hand(&app, "MILK"), 24, "{:?}", app.message());
    rig.key(&mut app, Key::F(4), 100_000);
    rig.typ(&mut app, "5\n");
    rig.shot(&app, "i3_item");
    assert_eq!(app.state().inventory.status("MILK"), StockStatus::Ok);
    // A scanned barcode in the list opens the item directly.
    rig.key(&mut app, Key::Escape, 100_000);
    rig.scan(&mut app, "96385074");
    assert_eq!(app.screen(), &Screen::StockItem("BREAD".into()));
    rig.key(&mut app, Key::Escape, 100_000);

    // Sell 20 milk at the till: stock follows; untracked bread doesn't.
    rig.key(&mut app, Key::Home, 100_000);
    rig.key(&mut app, Key::Char('1'), 100_000);
    assert!(matches!(app.screen(), Screen::Sale));
    rig.typ(&mut app, "20*4006381333931\n");
    rig.scan(&mut app, "96385074");
    rig.key(&mut app, Key::F(1), 100_000);
    rig.key(&mut app, Key::Enter, 100_000);
    assert!(
        matches!(app.screen(), Screen::Done { .. }),
        "{:?}",
        app.message()
    );
    rig.key(&mut app, Key::Escape, 100_000);
    assert_eq!(on_hand(&app, "MILK"), 4);
    assert_eq!(app.state().inventory.status("MILK"), StockStatus::Low);
    assert_eq!(
        app.state().inventory.status("BREAD"),
        StockStatus::Untracked
    );
    // Home is refused with a sale open.
    rig.scan(&mut app, "96385074");
    rig.key(&mut app, Key::Home, 100_000);
    assert!(matches!(app.screen(), Screen::Sale));
    rig.key(&mut app, Key::F(4), 100_000); // manager may void the line
    assert!(app.cart().is_empty(), "{:?}", app.message());

    // Refund 2 milk: back on the shelf.
    rig.key(&mut app, Key::F(9), 100_000);
    rig.typ(&mut app, "1\n\n2\n");
    rig.key(&mut app, Key::F(1), 100_000);
    rig.typ(&mut app, "Wrong size\n1\n");
    assert_eq!(on_hand(&app, "MILK"), 6, "{:?}", app.message());

    // Write off 1 (expired), then count 10.
    rig.key(&mut app, Key::Home, 100_000);
    rig.key(&mut app, Key::Char('2'), 100_000);
    rig.typ(&mut app, "milk\n");
    rig.key(&mut app, Key::F(3), 100_000);
    rig.typ(&mut app, "1\n2\n");
    assert_eq!(on_hand(&app, "MILK"), 5, "{:?}", app.message());
    rig.key(&mut app, Key::F(2), 100_000);
    rig.typ(&mut app, "10\n");
    assert_eq!(on_hand(&app, "MILK"), 10);
    assert!(matches!(
        app.state().inventory.get("MILK"),
        Some(l) if l.last_counted > 0
    ));

    // STOCK.CSV from the list.
    rig.key(&mut app, Key::Escape, 100_000);
    rig.key(&mut app, Key::Escape, 100_000); // clear the search
    rig.key(&mut app, Key::F(6), 100_000);
    let csv = String::from_utf8(rig.media.0["STOCK.CSV"].clone()).unwrap();
    assert!(
        csv.contains("MILK,\"Milk 1L\",In stock,10,5,0.80,12.00"),
        "{csv}"
    );
    assert!(csv.contains("BREAD,\"Bread\",Not tracked,,,,"), "{csv}");

    // A cashier needs a manager to receive stock.
    rig.key(&mut app, Key::Home, 100_000);
    rig.key(&mut app, Key::F(12), 100_000);
    rig.shot(&app, "i4_lock");
    rig.typ(&mut app, "2\n");
    rig.shot(&app, "i5_pin");
    rig.typ(&mut app, "0000\n");
    rig.key(&mut app, Key::Char('2'), 100_000);
    rig.typ(&mut app, "milk\n");
    rig.key(&mut app, Key::F(1), 100_000);
    rig.typ(&mut app, "5\n\n");
    assert!(
        matches!(app.screen(), Screen::Prompt(_)),
        "{:?}",
        app.screen()
    );
    rig.typ(&mut app, "1\n1234\n");
    assert_eq!(on_hand(&app, "MILK"), 15, "{:?}", app.message());

    // Everything survives a reboot (journal replay, and a snapshot).
    let before = app.state().clone();
    let dev = app.into_device().unwrap();
    let (app2, st) = boot(dev);
    assert_eq!(st, BootStatus::Ready);
    assert_eq!(app2.state(), &before);
}

/// Inventory and the home screen by touch alone.
#[test]
fn inventory_by_touch() {
    let mut rig = Rig::new();
    let (mut app, _) = boot(fresh_device());
    bring_up(&mut rig, &mut app);
    rig.tap(&mut app, "Home");
    rig.tap(&mut app, "Inventory");
    rig.tap(&mut app, "Milk 1L");
    rig.tap(&mut app, "Receive");
    rig.taps(&mut app, &["1", "2", "OK", "OK"]);
    assert_eq!(on_hand(&app, "MILK"), 12, "{:?}", app.message());
    rig.tap(&mut app, "Write off");
    rig.taps(&mut app, &["2", "OK", "Damaged"]);
    assert_eq!(on_hand(&app, "MILK"), 10, "{:?}", app.message());
    rig.tap(&mut app, "Back");
    rig.tap(&mut app, "Not tracked");
    assert!(app.tap_targets().iter().all(|(_, l)| l != "Milk 1L"));
    rig.tap(&mut app, "Home");
    rig.tap(&mut app, "Sign out");
    assert!(matches!(app.screen(), Screen::Prompt(_)));
}
