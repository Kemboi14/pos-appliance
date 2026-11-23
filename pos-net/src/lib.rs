//! Store network: IPv4 (DHCP or static) over an Ethernet [`Nic`], and
//! encrypted links between tills (see `docs/NETWORK.md`).
//!
//! The head till listens on TCP [`PORT`] and announces itself by UDP
//! broadcast on [`ANNOUNCE_PORT`]; lanes connect to it. Every byte on the
//! wire is inside a [`frame`] sealed with the store network key, so only
//! tills of the same store can read or inject anything. The node knows
//! nothing about POS data: it moves opaque messages between tills, keyed
//! by the sender's till number (authenticated by the frame).
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;
#[cfg(test)]
extern crate std;

pub mod frame;

use alloc::collections::{BTreeMap, VecDeque};
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use pos_hal::{Nic, Rng};
use smoltcp::iface::{Config as IfConfig, Interface, SocketHandle, SocketSet};
use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::socket::{dhcpv4, tcp, udp};
use smoltcp::time::{Duration, Instant};
use smoltcp::wire::{EthernetAddress, IpAddress, IpCidr, IpEndpoint, Ipv4Address, Ipv4Cidr};

pub use frame::{Cipher, FrameError, StoreKey};

pub const PORT: u16 = 47100;
pub const ANNOUNCE_PORT: u16 = 47101;
/// Lanes a head serves at once.
pub const MAX_LANES: usize = 32;
const TCP_BUF: usize = 64 * 1024;
const ANNOUNCE_EVERY_MS: u64 = 2000;
const RECONNECT_MS: u64 = 3000;
/// A link with no frame for this long is dropped (the peer sends a
/// keep-alive every few seconds).
const IDLE_MS: u64 = 20_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Head,
    Lane,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Addressing {
    Dhcp,
    Static {
        ip: [u8; 4],
        prefix: u8,
        gateway: Option<[u8; 4]>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeConfig {
    pub role: Role,
    pub till: u8,
    pub key: StoreKey,
    pub addressing: Addressing,
    /// Lanes: the head's address; `None` = learn it from announcements.
    pub head: Option<[u8; 4]>,
}

/// What the status bar shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub link: bool,
    pub ip: Option<([u8; 4], u8)>,
    /// Till numbers with an open, authenticated link.
    pub peers: Vec<u8>,
    pub head: Option<[u8; 4]>,
    /// Frames refused (wrong key or tampered) since start.
    pub rejected: u32,
}

impl Status {
    pub fn summary(&self, role: Role) -> String {
        use core::fmt::Write;
        let mut s = String::new();
        if !self.link {
            return "Network: cable unplugged".into();
        }
        let Some((ip, _)) = self.ip else {
            return "Network: waiting for an address".into();
        };
        let _ = write!(s, "{}.{}.{}.{}", ip[0], ip[1], ip[2], ip[3]);
        match role {
            Role::Head => {
                let n = self.peers.len();
                let _ = write!(
                    s,
                    " · head · {n} till{} online",
                    if n == 1 { "" } else { "s" }
                );
            }
            Role::Lane if self.peers.is_empty() => s.push_str(" · head not reachable"),
            Role::Lane => s.push_str(" · online"),
        }
        s
    }
}

/// smoltcp device over a [`Nic`].
struct Dev<'a> {
    nic: &'a mut dyn Nic,
    rx: Vec<u8>,
}

struct Rx(Vec<u8>);
struct Tx<'a>(&'a mut dyn Nic);

impl RxToken for Rx {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        f(&self.0)
    }
}

impl TxToken for Tx<'_> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let mut buf = vec![0u8; len];
        let r = f(&mut buf);
        let _ = self.0.transmit(&buf);
        r
    }
}

impl Device for Dev<'_> {
    type RxToken<'b>
        = Rx
    where
        Self: 'b;
    type TxToken<'b>
        = Tx<'b>
    where
        Self: 'b;

    fn receive(&mut self, _t: Instant) -> Option<(Rx, Tx<'_>)> {
        self.rx.resize(1600, 0);
        let n = self.nic.receive(&mut self.rx)?;
        Some((Rx(self.rx[..n].to_vec()), Tx(&mut *self.nic)))
    }

    fn transmit(&mut self, _t: Instant) -> Option<Tx<'_>> {
        Some(Tx(&mut *self.nic))
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut c = DeviceCapabilities::default();
        c.medium = Medium::Ethernet;
        c.max_transmission_unit = 1514;
        c.max_burst_size = Some(1);
        c
    }
}

/// One TCP connection carrying frames.
#[derive(Debug)]
struct Link {
    handle: SocketHandle,
    /// Authenticated sender, once its first frame arrived.
    till: Option<u8>,
    rx: Vec<u8>,
    tx: VecDeque<u8>,
    last_rx_ms: u64,
}

pub struct Node {
    cfg: NodeConfig,
    cipher: Cipher,
    iface: Interface,
    sockets: SocketSet<'static>,
    dhcp: Option<SocketHandle>,
    udp: SocketHandle,
    links: Vec<Link>,
    ip: Option<Ipv4Cidr>,
    head: Option<[u8; 4]>,
    inbox: VecDeque<(u8, Vec<u8>)>,
    last_announce: u64,
    last_connect: u64,
    rejected: u32,
    link_up: bool,
    port_seq: u16,
}

impl core::fmt::Debug for Node {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Node").field("cfg", &self.cfg).finish()
    }
}

fn tcp_socket() -> tcp::Socket<'static> {
    let mut s = tcp::Socket::new(
        tcp::SocketBuffer::new(vec![0u8; TCP_BUF]),
        tcp::SocketBuffer::new(vec![0u8; TCP_BUF]),
    );
    s.set_nagle_enabled(false);
    s.set_keep_alive(Some(Duration::from_millis(5000)));
    s.set_timeout(Some(Duration::from_millis(30_000)));
    s
}

impl Node {
    pub fn new(cfg: NodeConfig, nic: &mut dyn Nic, now_ms: u64, seed: u64) -> Node {
        let mac = nic.mac();
        let mut ic = IfConfig::new(EthernetAddress(mac).into());
        ic.random_seed = seed;
        let mut dev = Dev {
            nic,
            rx: Vec::new(),
        };
        let mut iface = Interface::new(ic, &mut dev, Instant::from_millis(now_ms as i64));
        let mut sockets = SocketSet::new(Vec::new());
        let mut ip = None;
        let dhcp = match cfg.addressing {
            Addressing::Dhcp => Some(sockets.add(dhcpv4::Socket::new())),
            Addressing::Static {
                ip: a,
                prefix,
                gateway,
            } => {
                let cidr = Ipv4Cidr::new(Ipv4Address::from_octets(a), prefix);
                iface.update_ip_addrs(|addrs| {
                    let _ = addrs.push(IpCidr::Ipv4(cidr));
                });
                if let Some(g) = gateway {
                    let _ = iface
                        .routes_mut()
                        .add_default_ipv4_route(Ipv4Address::from_octets(g));
                }
                ip = Some(cidr);
                None
            }
        };
        let mut u = udp::Socket::new(
            udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 8], vec![0u8; 4096]),
            udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 8], vec![0u8; 4096]),
        );
        let _ = u.bind(ANNOUNCE_PORT);
        let udp = sockets.add(u);
        let cipher = Cipher::new(&cfg.key);
        let head = cfg.head;
        let mut n = Node {
            cfg,
            cipher,
            iface,
            sockets,
            dhcp,
            udp,
            links: Vec::new(),
            ip,
            head,
            inbox: VecDeque::new(),
            last_announce: 0,
            last_connect: 0,
            rejected: 0,
            link_up: false,
            port_seq: (seed as u16) | 0xC000,
        };
        if n.cfg.role == Role::Head {
            for _ in 0..MAX_LANES {
                let mut s = tcp_socket();
                let _ = s.listen(PORT);
                let h = n.sockets.add(s);
                n.links.push(Link {
                    handle: h,
                    till: None,
                    rx: Vec::new(),
                    tx: VecDeque::new(),
                    last_rx_ms: now_ms,
                });
            }
        }
        n
    }

    pub fn config(&self) -> &NodeConfig {
        &self.cfg
    }

    /// Run the stack: move packets, keep links up, decrypt what arrived.
    pub fn poll(&mut self, nic: &mut dyn Nic, rng: &mut dyn Rng, now_ms: u64) {
        self.link_up = nic.link_up();
        let now = Instant::from_millis(now_ms as i64);
        {
            let mut dev = Dev {
                nic,
                rx: Vec::new(),
            };
            self.iface.poll(now, &mut dev, &mut self.sockets);
        }
        self.dhcp_events();
        if self.ip.is_some() {
            match self.cfg.role {
                Role::Head => self.announce(rng, now_ms),
                Role::Lane => {
                    self.listen_announce();
                    self.connect(now_ms);
                }
            }
        }
        self.pump(now_ms);
        // Flush what pump queued.
        let mut dev = Dev {
            nic,
            rx: Vec::new(),
        };
        self.iface.poll(now, &mut dev, &mut self.sockets);
    }

    fn dhcp_events(&mut self) {
        let Some(h) = self.dhcp else { return };
        let ev = self.sockets.get_mut::<dhcpv4::Socket>(h).poll();
        match ev {
            Some(dhcpv4::Event::Configured(c)) => {
                let cidr = c.address;
                let router = c.router;
                self.iface.update_ip_addrs(|a| {
                    a.clear();
                    let _ = a.push(IpCidr::Ipv4(cidr));
                });
                match router {
                    Some(r) => {
                        let _ = self.iface.routes_mut().add_default_ipv4_route(r);
                    }
                    None => {
                        self.iface.routes_mut().remove_default_ipv4_route();
                    }
                }
                self.ip = Some(cidr);
            }
            Some(dhcpv4::Event::Deconfigured) => {
                self.iface.update_ip_addrs(|a| a.clear());
                self.iface.routes_mut().remove_default_ipv4_route();
                self.ip = None;
                for l in &mut self.links {
                    self.sockets.get_mut::<tcp::Socket>(l.handle).abort();
                }
            }
            None => {}
        }
    }

    fn announce(&mut self, rng: &mut dyn Rng, now_ms: u64) {
        if now_ms.saturating_sub(self.last_announce) < ANNOUNCE_EVERY_MS && self.last_announce != 0
        {
            return;
        }
        let Some(ip) = self.ip else { return };
        self.last_announce = now_ms;
        let mut nonce = [0u8; 24];
        if rng.fill(&mut nonce).is_err() {
            return;
        }
        let a = ip.address().octets();
        let mut body = b"HEAD".to_vec();
        body.extend_from_slice(&a);
        let Ok(f) = self.cipher.seal(self.cfg.till, nonce, &body) else {
            return;
        };
        let s = self.sockets.get_mut::<udp::Socket>(self.udp);
        let to = IpEndpoint::new(IpAddress::Ipv4(Ipv4Address::BROADCAST), ANNOUNCE_PORT);
        let _ = s.send_slice(&f, to);
    }

    fn listen_announce(&mut self) {
        let mut buf = [0u8; 256];
        loop {
            let s = self.sockets.get_mut::<udp::Socket>(self.udp);
            let Ok((n, _)) = s.recv_slice(&mut buf) else {
                break;
            };
            let mut v = buf[..n].to_vec();
            match self.cipher.open(&mut v) {
                Ok(Some((_, body))) if body.len() == 8 && &body[..4] == b"HEAD" => {
                    if self.cfg.head.is_none() {
                        self.head = Some([body[4], body[5], body[6], body[7]]);
                    }
                }
                Ok(_) => {}
                Err(_) => self.rejected = self.rejected.saturating_add(1),
            }
        }
    }

    fn connect(&mut self, now_ms: u64) {
        let Some(head) = self.head else { return };
        let open = self
            .links
            .iter()
            .any(|l| self.sockets.get::<tcp::Socket>(l.handle).is_open());
        let tried = !self.links.is_empty();
        if open || (tried && now_ms.saturating_sub(self.last_connect) < RECONNECT_MS) {
            return;
        }
        self.last_connect = now_ms;
        for l in self.links.drain(..) {
            self.sockets.remove(l.handle);
        }
        let mut s = tcp_socket();
        self.port_seq = self.port_seq.wrapping_add(1) | 0xC000;
        let remote = IpEndpoint::new(IpAddress::Ipv4(Ipv4Address::from_octets(head)), PORT);
        if s.connect(self.iface.context(), remote, self.port_seq)
            .is_ok()
        {
            let h = self.sockets.add(s);
            self.links.push(Link {
                handle: h,
                till: None,
                rx: Vec::new(),
                tx: VecDeque::new(),
                last_rx_ms: now_ms,
            });
        }
    }

    /// Move bytes between sockets and link buffers; decrypt frames.
    fn pump(&mut self, now_ms: u64) {
        for l in &mut self.links {
            let s = self.sockets.get_mut::<tcp::Socket>(l.handle);
            if s.state() == tcp::State::Listen || s.state() == tcp::State::SynSent {
                l.last_rx_ms = now_ms;
                continue;
            }
            let mut buf = [0u8; 4096];
            while s.can_recv() {
                match s.recv_slice(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => l.rx.extend_from_slice(&buf[..n]),
                }
            }
            let mut bad = false;
            loop {
                match self.cipher.open(&mut l.rx) {
                    Ok(Some((till, payload))) => {
                        // A link speaks for one till only.
                        if l.till.is_some_and(|t| t != till) || till == self.cfg.till {
                            bad = true;
                            break;
                        }
                        l.till = Some(till);
                        l.last_rx_ms = now_ms;
                        if !payload.is_empty() {
                            self.inbox.push_back((till, payload));
                        }
                    }
                    Ok(None) => break,
                    Err(_) => {
                        bad = true;
                        break;
                    }
                }
            }
            if bad {
                self.rejected = self.rejected.saturating_add(1);
                s.abort();
            }
            while !l.tx.is_empty() && s.can_send() {
                let (a, _) = l.tx.as_slices();
                match s.send_slice(a) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        l.tx.drain(..n);
                    }
                }
            }
            let idle = s.state() == tcp::State::Established
                && now_ms.saturating_sub(l.last_rx_ms) > IDLE_MS;
            let dead = !s.is_open() || idle;
            if dead {
                if idle {
                    s.abort();
                }
                l.till = None;
                l.rx.clear();
                l.tx.clear();
                if self.cfg.role == Role::Head && !s.is_open() {
                    let _ = s.listen(PORT);
                    l.last_rx_ms = now_ms;
                }
            }
        }
    }

    /// Next message received: (sender till, payload).
    pub fn recv(&mut self) -> Option<(u8, Vec<u8>)> {
        self.inbox.pop_front()
    }

    /// Queue `payload` for till `to`. A lane may also send before it knows
    /// the head's number by passing `None` (its only link). False if there
    /// is no open link to that till or the frame cannot be built.
    pub fn send(&mut self, to: Option<u8>, payload: &[u8], rng: &mut dyn Rng) -> bool {
        let mut nonce = [0u8; 24];
        if rng.fill(&mut nonce).is_err() {
            return false;
        }
        let Ok(f) = self.cipher.seal(self.cfg.till, nonce, payload) else {
            return false;
        };
        let link = self.links.iter_mut().find(|l| {
            let st = self.sockets.get::<tcp::Socket>(l.handle).state();
            st == tcp::State::Established
                && match to {
                    Some(t) => l.till == Some(t),
                    None => true,
                }
        });
        match link {
            Some(l) if l.tx.len() + f.len() <= 32 * 1024 * 1024 => {
                l.tx.extend(f);
                true
            }
            _ => false,
        }
    }

    /// Bytes still waiting to be sent to `to` (flow control for big syncs).
    pub fn backlog(&self, to: Option<u8>) -> usize {
        self.links
            .iter()
            .filter(|l| to.is_none() || l.till == to)
            .map(|l| l.tx.len())
            .sum()
    }

    /// Links that are open but have not yet heard from their peer (a lane
    /// says hello first, so the head can learn who it is).
    pub fn unidentified(&self) -> usize {
        self.links
            .iter()
            .filter(|l| {
                l.till.is_none()
                    && self.sockets.get::<tcp::Socket>(l.handle).state() == tcp::State::Established
            })
            .count()
    }

    pub fn status(&self) -> Status {
        let mut peers: Vec<u8> = self.links.iter().filter_map(|l| l.till).collect();
        peers.sort_unstable();
        peers.dedup();
        Status {
            link: self.link_up,
            ip: self.ip.map(|c| (c.address().octets(), c.prefix_len())),
            peers,
            head: self.head,
            rejected: self.rejected,
        }
    }

    /// Per-peer last-heard times, for the network screen.
    pub fn last_heard(&self) -> BTreeMap<u8, u64> {
        self.links
            .iter()
            .filter_map(|l| l.till.map(|t| (t, l.last_rx_ms)))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::rc::Rc;
    use core::cell::RefCell;
    use pos_hal::HalResult;

    /// An Ethernet hub: every frame goes to every other port.
    #[derive(Default)]
    struct Hub {
        ports: Vec<VecDeque<Vec<u8>>>,
    }

    struct Port {
        hub: Rc<RefCell<Hub>>,
        n: usize,
        mac: [u8; 6],
    }

    impl Nic for Port {
        fn mac(&self) -> [u8; 6] {
            self.mac
        }
        fn link_up(&mut self) -> bool {
            true
        }
        fn transmit(&mut self, f: &[u8]) -> HalResult<()> {
            let mut h = self.hub.borrow_mut();
            for (k, q) in h.ports.iter_mut().enumerate() {
                if k != self.n {
                    q.push_back(f.to_vec());
                }
            }
            Ok(())
        }
        fn receive(&mut self, buf: &mut [u8]) -> Option<usize> {
            let f = self.hub.borrow_mut().ports[self.n].pop_front()?;
            buf[..f.len()].copy_from_slice(&f);
            Some(f.len())
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

    fn port(hub: &Rc<RefCell<Hub>>) -> Port {
        let mut h = hub.borrow_mut();
        h.ports.push(VecDeque::new());
        let n = h.ports.len() - 1;
        Port {
            hub: hub.clone(),
            n,
            mac: [0x02, 0, 0, 0, 0, n as u8 + 1],
        }
    }

    fn cfg(role: Role, till: u8, key: u8, ip: u8) -> NodeConfig {
        NodeConfig {
            role,
            till,
            key: StoreKey([key; 32]),
            addressing: Addressing::Static {
                ip: [10, 0, 0, ip],
                prefix: 24,
                gateway: None,
            },
            head: None,
        }
    }

    #[test]
    fn head_and_lanes_talk_and_strangers_cannot() {
        let hub = Rc::new(RefCell::new(Hub::default()));
        let mut nics: Vec<Port> = (0..4).map(|_| port(&hub)).collect();
        let mut rng = Rnd(1);
        let mut nodes = vec![
            Node::new(cfg(Role::Head, 1, 7, 1), &mut nics[0], 0, 11),
            Node::new(cfg(Role::Lane, 2, 7, 2), &mut nics[1], 0, 22),
            Node::new(cfg(Role::Lane, 3, 7, 3), &mut nics[2], 0, 33),
            // Same LAN, wrong store key.
            Node::new(cfg(Role::Lane, 4, 9, 4), &mut nics[3], 0, 44),
        ];
        let mut t = 0u64;
        let mut run = |nodes: &mut Vec<Node>, nics: &mut Vec<Port>, rng: &mut Rnd, ms: u64| {
            for _ in 0..ms / 10 {
                t += 10;
                for (n, nic) in nodes.iter_mut().zip(nics.iter_mut()) {
                    n.poll(nic, rng, t);
                }
            }
        };
        run(&mut nodes, &mut nics, &mut rng, 6000);
        // Lanes learned the head's address from its announcement and say hello.
        for (k, n) in nodes.iter_mut().enumerate().skip(1) {
            let want = if k == 3 { None } else { Some([10, 0, 0, 1]) };
            assert_eq!(n.status().head, want);
            n.send(None, b"hello", &mut rng);
        }
        run(&mut nodes, &mut nics, &mut rng, 1000);
        let mut got = Vec::new();
        while let Some(m) = nodes[0].recv() {
            got.push(m);
        }
        got.sort();
        assert_eq!(got, vec![(2, b"hello".to_vec()), (3, b"hello".to_vec())]);
        assert_eq!(nodes[0].status().peers, vec![2, 3]);
        // The stranger never got an address for the head (announcements
        // are sealed), so it never connected.
        assert!(nodes[3].status().peers.is_empty());
        assert!(nodes[3].status().rejected > 0);
        // Head replies to one lane only; a big message crosses many segments.
        let big: Vec<u8> = (0..300_000u32).map(|i| i as u8).collect();
        assert!(nodes[0].send(Some(3), &big, &mut rng));
        assert!(nodes[0].send(Some(2), b"just you", &mut rng));
        run(&mut nodes, &mut nics, &mut rng, 3000);
        assert_eq!(nodes[2].recv(), Some((1, big)));
        assert_eq!(nodes[1].recv(), Some((1, b"just you".to_vec())));
        assert_eq!(nodes[1].recv(), None);
        assert_eq!(nodes[1].status().summary(Role::Lane), "10.0.0.2 · online");
    }

    #[test]
    fn stranger_with_known_address_is_refused() {
        let hub = Rc::new(RefCell::new(Hub::default()));
        let mut nics: Vec<Port> = (0..2).map(|_| port(&hub)).collect();
        let mut rng = Rnd(5);
        let mut bad = cfg(Role::Lane, 9, 9, 9);
        bad.head = Some([10, 0, 0, 1]);
        let mut nodes = [
            Node::new(cfg(Role::Head, 1, 7, 1), &mut nics[0], 0, 1),
            Node::new(bad, &mut nics[1], 0, 2),
        ];
        let mut t = 0;
        for i in 0..400 {
            t += 10;
            if i == 100 {
                assert!(nodes[1].send(None, b"let me in", &mut rng));
            }
            for (n, nic) in nodes.iter_mut().zip(nics.iter_mut()) {
                n.poll(nic, &mut rng, t);
            }
        }
        assert_eq!(nodes[0].recv(), None);
        assert!(nodes[0].status().peers.is_empty());
        assert!(nodes[0].status().rejected >= 1);
    }
}
