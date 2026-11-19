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
    rig.typ(app, "1\n1234\n"); // login manager
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
    rig.typ(&mut app, "2\n0000\n");
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
    rig.typ(&mut app, "1\n1234\n");
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
        rig.typ(&mut app, "1\n1234\n");
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
