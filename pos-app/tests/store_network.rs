//! Three tills on one (simulated) Ethernet: a head and two lanes. Setup by
//! the manager menu and a USB key file, catalog/users/pictures from the
//! head, store-wide stock from sales and stock changes on any till, a lane
//! that was offline catching up, a reboot, and lanes refusing local
//! catalog changes.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, VecDeque};
use std::rc::Rc;

use pos_app::sync::Network;
use pos_app::{App, Hw, Screen};
use pos_hal::*;
use pos_store::mem::MemDevice;

struct Disp;
impl Display for Disp {
    fn width(&self) -> u32 {
        1024
    }
    fn height(&self) -> u32 {
        768
    }
    fn pixel_format(&self) -> PixelFormat {
        PixelFormat::Bgrx
    }
    fn blit(&mut self, _: &[u32], _: usize, _: u32, _: u32, _: u32, _: u32) -> HalResult<()> {
        Ok(())
    }
}

struct Clk(Rc<Cell<i64>>);
impl Clock for Clk {
    fn monotonic_us(&self) -> u64 {
        0
    }
    fn wall(&self) -> HalResult<DateTime> {
        DateTime::from_unix(self.0.get()).ok_or(HalError::Io)
    }
}

#[derive(Default)]
struct Prn;
impl Printer for Prn {
    fn write(&mut self, _: &[u8]) -> HalResult<()> {
        Ok(())
    }
    fn status(&mut self) -> PrinterStatus {
        PrinterStatus::Ready
    }
    fn describe(&self) -> String {
        "mock".into()
    }
}

struct Rnd(u64);
impl Rng for Rnd {
    fn fill(&mut self, b: &mut [u8]) -> HalResult<()> {
        for x in b {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            *x = (self.0 >> 33) as u8;
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

/// Ethernet hub; a port can be unplugged.
#[derive(Default)]
struct Hub {
    ports: Vec<(VecDeque<Vec<u8>>, bool)>,
}

struct Port {
    hub: Rc<RefCell<Hub>>,
    n: usize,
}

impl Nic for Port {
    fn mac(&self) -> [u8; 6] {
        [0x02, 0, 0, 0, 0, self.n as u8 + 1]
    }
    fn link_up(&mut self) -> bool {
        self.hub.borrow().ports[self.n].1
    }
    fn transmit(&mut self, f: &[u8]) -> HalResult<()> {
        let mut h = self.hub.borrow_mut();
        if !h.ports[self.n].1 {
            return Err(HalError::Io);
        }
        for (k, (q, up)) in h.ports.iter_mut().enumerate() {
            if k != self.n && *up {
                q.push_back(f.to_vec());
            }
        }
        Ok(())
    }
    fn receive(&mut self, buf: &mut [u8]) -> Option<usize> {
        let f = self.hub.borrow_mut().ports[self.n].0.pop_front()?;
        buf[..f.len()].copy_from_slice(&f);
        Some(f.len())
    }
}

struct Till {
    app: App<MemDevice>,
    net: Network<Port>,
    media: Media,
    rng: Rnd,
    clock: Clk,
    t_us: u64,
}

const CATALOG: &str = "sku,barcode,plu,name,price,tax_code,weighable,quick_key\n\
MILK,4006381333931,,Milk 1L,1.20,A,0,0\n\
BREAD,96385074,,Bread,2.49,A,0,1\n";

fn hasher() -> Box<dyn pos_core::auth::PinHasher> {
    Box::new(pos_app::Argon2Hasher::with_params(8, 1))
}

impl Till {
    fn new(hub: &Rc<RefCell<Hub>>, n: usize, clock: &Rc<Cell<i64>>) -> Till {
        let mut d = MemDevice::new(512, 16384);
        pos_store::format(&mut d).unwrap();
        hub.borrow_mut().ports.push((VecDeque::new(), true));
        let (app, _) = App::boot_with(d, [n as u8 + 1; 32], 1024, 768, hasher());
        Till {
            app,
            net: Network::new(Port {
                hub: hub.clone(),
                n,
            }),
            media: Media::default(),
            rng: Rnd(n as u64 * 7919 + 1),
            clock: Clk(clock.clone()),
            t_us: 1_000_000,
        }
    }

    fn with_hw<R>(
        &mut self,
        f: impl FnOnce(&mut App<MemDevice>, &mut Network<Port>, &mut Hw<'_>) -> R,
    ) -> R {
        let mut disp = Disp;
        let mut prn = Prn;
        let mut hw = Hw {
            display: &mut disp,
            clock: &self.clock,
            printer: &mut prn,
            rng: &mut self.rng,
            media: &mut self.media,
            wifi: &mut pos_hal::NoWifi,
            sound: &mut pos_hal::NoSound,
        };
        f(&mut self.app, &mut self.net, &mut hw)
    }

    fn key(&mut self, k: Key) {
        self.t_us += 150_000;
        let at = self.t_us;
        self.with_hw(|app, _, hw| {
            app.handle(
                InputEvent::Key {
                    key: k,
                    mods: Modifiers::default(),
                    at_us: at,
                },
                hw,
            );
            app.render(hw);
        });
    }

    fn typ(&mut self, s: &str) {
        for c in s.chars() {
            self.key(if c == '\n' { Key::Enter } else { Key::Char(c) });
        }
    }

    fn tap_menu(&mut self, label: &str) {
        self.key(Key::F(10));
        let r = self
            .app
            .tap_targets()
            .iter()
            .find(|(_, l)| l == label)
            .map(|(r, _)| *r)
            .unwrap_or_else(|| panic!("no {label}"));
        for pressed in [true, false] {
            self.t_us += 100_000;
            let at = self.t_us;
            self.with_hw(|app, _, hw| {
                app.handle(
                    InputEvent::Pointer {
                        x: (r.x + r.w / 2) as u32,
                        y: (r.y + r.h / 2) as u32,
                        pressed,
                        at_us: at,
                    },
                    hw,
                );
                app.render(hw);
            });
        }
    }

    fn on_hand(&self, sku: &str) -> i64 {
        self.app
            .state()
            .inventory
            .get(sku)
            .map_or(i64::MIN, |l| l.on_hand.0 / 1000)
    }

    fn msg(&self) -> String {
        self.app.message().unwrap_or("").to_string()
    }
}

/// Run every plugged-in till's network for `ms` of simulated time.
fn run(tills: &mut [&mut Till], clock: &Rc<Cell<i64>>, now_ms: &mut u64, ms: u64) {
    for _ in 0..ms / 20 {
        *now_ms += 20;
        if now_ms.is_multiple_of(1000) {
            clock.set(clock.get() + 1);
        }
        for t in tills.iter_mut() {
            let n = *now_ms;
            t.with_hw(|app, net, hw| net.step(app, hw, n));
        }
    }
}

fn setup(t: &mut Till, name: &str, pin: &str) {
    t.typ(&format!("{name}\n{pin}\n{pin}\n"));
    t.typ("\n"); // signed in; keep the default store name
    assert!(
        matches!(t.app.screen(), Screen::Home),
        "{:?} {}",
        t.app.screen(),
        t.msg()
    );
}

#[test]
fn head_and_two_lanes() {
    let clock = Rc::new(Cell::new(1_763_550_000));
    let hub = Rc::new(RefCell::new(Hub::default()));
    let mut head = Till::new(&hub, 0, &clock);
    let mut l2 = Till::new(&hub, 1, &clock);
    let mut l3 = Till::new(&hub, 2, &clock);
    let mut now_ms = 0u64;

    // Head: manager, catalog, a delivery, then "Network setup" as head.
    setup(&mut head, "Alice", "1234");
    head.media
        .0
        .insert("CATALOG.CSV".into(), CATALOG.as_bytes().to_vec());
    head.tap_menu("8. Import catalog from USB (CATALOG.CSV)");
    assert_eq!(head.app.state().catalog.len(), 2, "{}", head.msg());
    head.media.0.insert(
        "IMG_MILK.PNG".into(),
        std::fs::read(format!(
            "{}/../pos-ui/tests/data/logo.png",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap(),
    );
    head.tap_menu("Import pictures from USB (IMG_<SKU>.JPG, LOGO, WALLPAPER)");
    head.key(Key::Char('2'));
    head.typ("milk\n");
    head.key(Key::F(1));
    head.typ("50\n\n");
    assert_eq!(head.on_hand("MILK"), 50, "{}", head.msg());
    head.key(Key::Home);
    head.tap_menu("Network setup (head / lane till)");
    head.typ("2\n1\n10.0.0.1/24\n");
    let lines = match head.app.screen() {
        Screen::Info { lines, .. } => lines.clone(),
        s => panic!("{s:?} {}", head.msg()),
    };
    assert!(
        lines.iter().any(|l| l.contains("STORENET.KEY written")),
        "{lines:?}"
    );
    let keyfile = head.media.0["STORENET.KEY"].clone();
    let code = String::from_utf8(keyfile.clone()).unwrap();
    head.key(Key::Enter);

    // Lanes: their own first manager, then join: key from the stick (lane
    // 2) or typed (lane 3), static addresses, head found automatically.
    setup(&mut l2, "Bob", "1111");
    setup(&mut l3, "Carol", "2222");
    l2.media.0.insert("STORENET.KEY".into(), keyfile);
    l2.tap_menu("Network setup (head / lane till)");
    l2.typ("3\n2\n\n10.0.0.2/24\n\n");
    assert!(
        matches!(l2.app.screen(), Screen::Info { .. }),
        "{}",
        l2.msg()
    );
    l3.tap_menu("Network setup (head / lane till)");
    l3.typ(&format!("3\n3\n{}\n10.0.0.3/24\n10.0.0.1\n", code.trim()));
    assert!(
        matches!(l3.app.screen(), Screen::Info { .. }),
        "{}",
        l3.msg()
    );
    l2.key(Key::Enter);
    l3.key(Key::Enter);

    run(
        &mut [&mut head, &mut l2, &mut l3],
        &clock,
        &mut now_ms,
        8000,
    );
    for l in [&l2, &l3] {
        let st = l.app.state();
        assert_eq!(
            st.catalog.len(),
            2,
            "catalog from head; {:?}",
            l.app.net_status()
        );
        assert_eq!(st.users.len(), 1);
        assert_eq!(st.users[&1].name, "Alice", "users from head");
        assert_eq!(l.on_hand("MILK"), 50, "absolute stock on joining");
        assert!(st.pictures.item("MILK").is_some(), "pictures from head");
    }
    assert_eq!(head.app.state().sync.origins.len(), 0);
    assert!(
        head.app.net_status().contains("2 tills online"),
        "{}",
        head.app.net_status()
    );
    // The lane's own manager was replaced by the head's users: signed out.
    assert!(
        matches!(l2.app.screen(), Screen::Prompt(_)),
        "{:?}",
        l2.app.screen()
    );

    // Lane 2 sells 3 milk (Alice signs in with the head's PIN).
    l2.typ("1\n1234\n\n100\n");
    assert!(
        matches!(l2.app.screen(), Screen::Sale),
        "{:?} {}",
        l2.app.screen(),
        l2.msg()
    );
    l2.typ("3*4006381333931\n");
    l2.key(Key::F(1));
    l2.key(Key::Enter);
    assert!(
        matches!(l2.app.screen(), Screen::Done { .. }),
        "{}",
        l2.msg()
    );
    l2.key(Key::Escape);
    assert_eq!(l2.on_hand("MILK"), 47);
    run(
        &mut [&mut head, &mut l2, &mut l3],
        &clock,
        &mut now_ms,
        4000,
    );
    assert_eq!(head.on_hand("MILK"), 47);
    assert_eq!(l3.on_hand("MILK"), 47);
    assert_eq!(l2.app.state().sync.outbox.len(), 0, "head acknowledged");
    let (n, total, _) = head.app.state().sync.store_today(clock.get());
    assert_eq!((n, total.0), (1, 3_60));

    // Lane 3 is unplugged; meanwhile lane 2 sells 2 more and the head
    // receives 10. Lane 3 writes off 1 while offline.
    hub.borrow_mut().ports[2].1 = false;
    l2.typ("2*4006381333931\n");
    l2.key(Key::F(1));
    l2.key(Key::Enter);
    l2.key(Key::Escape);
    head.key(Key::Char('2'));
    head.typ("milk\n");
    head.key(Key::F(1));
    head.typ("10\n\n");
    l3.typ("1\n1234\n2");
    l3.typ("milk\n");
    l3.key(Key::F(3));
    l3.typ("1\n2\n");
    assert_eq!(l3.on_hand("MILK"), 46, "{}", l3.msg());
    run(
        &mut [&mut head, &mut l2, &mut l3],
        &clock,
        &mut now_ms,
        3000,
    );
    assert_eq!(head.on_hand("MILK"), 55);
    assert_eq!(l2.on_hand("MILK"), 55);
    // Plugged back in: everyone converges on 50 + 10 − 3 − 2 − 1 = 54.
    hub.borrow_mut().ports[2].1 = true;
    run(
        &mut [&mut head, &mut l2, &mut l3],
        &clock,
        &mut now_ms,
        30_000,
    );
    for (name, t) in [("head", &head), ("lane 2", &l2), ("lane 3", &l3)] {
        assert_eq!(t.on_hand("MILK"), 54, "{name}: {}", t.app.net_status());
    }

    // A price change on the head reaches the lanes.
    head.key(Key::Home);
    head.media.0.insert(
        "CATALOG.CSV".into(),
        CATALOG.replace("1.20", "1.35").as_bytes().to_vec(),
    );
    head.tap_menu("8. Import catalog from USB (CATALOG.CSV)");
    run(
        &mut [&mut head, &mut l2, &mut l3],
        &clock,
        &mut now_ms,
        4000,
    );
    for l in [&l2, &l3] {
        assert_eq!(l.app.state().catalog.get("MILK").unwrap().price.0, 1_35);
    }
    // Lanes may not change the catalog themselves.
    l2.key(Key::Home);
    l2.media
        .0
        .insert("CATALOG.CSV".into(), CATALOG.as_bytes().to_vec());
    l2.tap_menu("8. Import catalog from USB (CATALOG.CSV)");
    assert!(
        l2.msg().contains("Managed by the head till"),
        "{}",
        l2.msg()
    );

    // Lane 2 reboots: state and sync position come back; nothing is sent twice.
    let before = l2.app.state().clone();
    let dev = l2.app.into_device().unwrap();
    let (app, _) = App::boot_with(dev, [2; 32], 1024, 768, hasher());
    l2.app = app;
    assert_eq!(l2.app.state(), &before);
    l2.net = Network::new(Port {
        hub: hub.clone(),
        n: 1,
    });
    run(
        &mut [&mut head, &mut l2, &mut l3],
        &clock,
        &mut now_ms,
        8000,
    );
    assert_eq!(head.on_hand("MILK"), 54);
    assert_eq!(l2.on_hand("MILK"), 54);
    let (n, _, tills) = head.app.state().sync.store_today(clock.get());
    assert_eq!((n, tills), (2, 1), "two sales on lane 2, counted once");
}
