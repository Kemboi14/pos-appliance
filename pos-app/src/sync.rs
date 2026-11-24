//! Store network sync: the messages tills exchange and what the app does
//! with them (see `docs/NETWORK.md`).
//!
//! Lanes speak first: every second a lane sends `Want` (its position) and
//! `Push` (own movements the head has not confirmed). The head answers
//! with `Ack`, `Relay` (other tills' movements) or `Resync`, and any
//! `Section` (catalog, users, settings, pictures) newer than the lane has.
//! Every received change is committed as an event before it takes effect,
//! like any other change.

use alloc::string::String;
use alloc::vec::Vec;

use pos_core::state::Event;
use pos_core::sync::{Levels, Movement, NetRole, Section, SectionKey, TillNo};
use pos_hal::{BlockDevice, Nic, Rng};
use serde::{Deserialize, Serialize};

use crate::{App, Hw};

pub const PROTOCOL: u32 = 2;
/// Movements per message.
const BATCH: usize = 200;
/// Don't queue more sections while this much is unsent to a lane.
const BACKLOG_LIMIT: usize = 2 * 1024 * 1024;
const WANT_EVERY_MS: u64 = 1000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Msg {
    Want {
        protocol: u32,
        cursor: u64,
        have: Vec<(SectionKey, u64)>,
    },
    Push {
        items: Vec<Movement>,
    },
    Ack {
        upto: u64,
    },
    Relay {
        items: Vec<(u64, Movement)>,
    },
    Resync {
        relay_seq: u64,
        acked: u64,
        levels: Levels,
    },
    Section {
        version: u64,
        section: Section,
    },
    /// The peer speaks another protocol version.
    Refused {
        protocol: u32,
    },
}

/// What the app needs from the network node.
pub trait Link {
    fn recv(&mut self) -> Option<(TillNo, Vec<u8>)>;
    fn send(&mut self, to: Option<TillNo>, data: &[u8], rng: &mut dyn Rng) -> bool;
    fn peers(&self) -> Vec<TillNo>;
    fn backlog(&self, to: Option<TillNo>) -> usize;
    fn summary(&self) -> String;
}

impl Link for pos_net::Node {
    fn recv(&mut self) -> Option<(TillNo, Vec<u8>)> {
        pos_net::Node::recv(self)
    }
    fn send(&mut self, to: Option<TillNo>, data: &[u8], rng: &mut dyn Rng) -> bool {
        pos_net::Node::send(self, to, data, rng)
    }
    fn peers(&self) -> Vec<TillNo> {
        self.status().peers
    }
    fn backlog(&self, to: Option<TillNo>) -> usize {
        pos_net::Node::backlog(self, to)
    }
    fn summary(&self) -> String {
        let role = self.config().role;
        self.status().summary(role)
    }
}

/// Sync progress kept in memory (not journaled).
#[derive(Debug, Default)]
pub struct SyncRuntime {
    last_want_ms: u64,
    pub status: String,
    pub peers: Vec<TillNo>,
    pub last_error: Option<String>,
}

fn send(link: &mut dyn Link, to: Option<TillNo>, m: &Msg, rng: &mut dyn Rng) -> bool {
    match postcard::to_allocvec(m) {
        Ok(b) => link.send(to, &b, rng),
        Err(_) => false,
    }
}

impl<D: BlockDevice> App<D> {
    /// Exchange sync messages. Call often (every main-loop pass).
    pub fn sync(&mut self, link: &mut dyn Link, hw: &mut Hw<'_>, now_ms: u64) {
        if !self.state.net.networked() {
            return;
        }
        let status = link.summary();
        let peers = link.peers();
        if status != self.net_rt.status || peers != self.net_rt.peers {
            self.net_rt.status = status;
            self.net_rt.peers = peers;
            self.dirty = true;
        }
        while let Some((from, bytes)) = link.recv() {
            let Ok(msg) = postcard::from_bytes::<Msg>(&bytes) else {
                continue;
            };
            match self.state.net.role {
                NetRole::Head => self.head_msg(link, hw, from, msg),
                NetRole::Lane => self.lane_msg(hw, msg),
                NetRole::Standalone => {}
            }
        }
        if self.state.net.role == NetRole::Lane
            && now_ms.saturating_sub(self.net_rt.last_want_ms) >= WANT_EVERY_MS
        {
            self.net_rt.last_want_ms = now_ms;
            let s = &self.state.sync;
            let want = Msg::Want {
                protocol: PROTOCOL,
                cursor: s.cursor,
                have: s.have.iter().map(|(k, v)| (k.clone(), *v)).collect(),
            };
            let push: Vec<Movement> = s
                .outbox
                .iter()
                .filter(|m| m.seq > s.acked)
                .take(BATCH)
                .cloned()
                .collect();
            send(link, None, &want, hw.rng);
            if !push.is_empty() {
                send(link, None, &Msg::Push { items: push }, hw.rng);
            }
        }
    }

    fn sync_commit(&mut self, e: Event) -> bool {
        match self.commit(alloc::vec![e]) {
            Ok(_) => {
                self.net_rt.last_error = None;
                true
            }
            Err(err) => {
                self.net_rt.last_error = Some(crate::describe(&err));
                false
            }
        }
    }

    fn head_msg(&mut self, link: &mut dyn Link, hw: &mut Hw<'_>, from: TillNo, msg: Msg) {
        let at = self.now(hw);
        match msg {
            Msg::Push { items } => {
                let have = self.state.sync.origins.get(&from).copied().unwrap_or(0);
                let fresh: Vec<Movement> = items.into_iter().filter(|m| m.seq > have).collect();
                if !fresh.is_empty() {
                    self.sync_commit(Event::SyncReceived {
                        at,
                        from,
                        items: fresh,
                    });
                }
                let upto = self.state.sync.origins.get(&from).copied().unwrap_or(0);
                send(link, Some(from), &Msg::Ack { upto }, hw.rng);
            }
            Msg::Want {
                protocol,
                cursor,
                have,
            } => {
                if protocol != PROTOCOL {
                    send(
                        link,
                        Some(from),
                        &Msg::Refused { protocol: PROTOCOL },
                        hw.rng,
                    );
                    return;
                }
                let s = &self.state.sync;
                let acked = s.origins.get(&from).copied().unwrap_or(0);
                // A lane that has never synced starts from absolute levels.
                let fresh = cursor == 0 && s.relay_seq > 0;
                match s.relay_from(cursor).filter(|_| !fresh) {
                    Some(it) => {
                        let items: Vec<(u64, Movement)> = it.take(BATCH).cloned().collect();
                        if !items.is_empty() {
                            send(link, Some(from), &Msg::Relay { items }, hw.rng);
                        }
                    }
                    None => {
                        let levels: Levels = self
                            .state
                            .inventory
                            .levels()
                            .map(|(k, v)| (k.clone(), v.clone()))
                            .collect();
                        let m = Msg::Resync {
                            relay_seq: s.relay_seq,
                            acked,
                            levels,
                        };
                        send(link, Some(from), &m, hw.rng);
                    }
                }
                self.send_sections(link, hw, from, &have);
            }
            _ => {}
        }
    }

    /// Sections newer than the lane has, settings first so a new catalog
    /// finds its tax classes.
    fn send_sections(
        &mut self,
        link: &mut dyn Link,
        hw: &mut Hw<'_>,
        to: TillNo,
        have: &[(SectionKey, u64)],
    ) {
        let lane_has = |k: &SectionKey| have.iter().find(|(x, _)| x == k).map_or(0, |(_, v)| *v);
        let mut todo: Vec<(SectionKey, u64)> = self
            .state
            .sync
            .versions
            .iter()
            .filter(|(k, v)| lane_has(k) < **v)
            .map(|(k, v)| (k.clone(), *v))
            .collect();
        let rank = |k: &SectionKey| match k {
            SectionKey::Config => 0,
            SectionKey::Users | SectionKey::Policy => 1,
            SectionKey::Catalog => 2,
            SectionKey::Picture(_) => 3,
        };
        todo.sort_by_key(|(k, _)| rank(k));
        for (k, version) in todo {
            if link.backlog(Some(to)) > BACKLOG_LIMIT {
                break;
            }
            let st = &self.state;
            let section = match &k {
                SectionKey::Config => Section::Config(st.config.clone()),
                SectionKey::Users => Section::Users(st.users.values().cloned().collect()),
                SectionKey::Policy => Section::Policy(st.policy.clone()),
                SectionKey::Catalog => Section::Catalog(st.catalog.items().cloned().collect()),
                SectionKey::Picture(p) => {
                    Section::Picture(p.clone(), st.pictures.get(p).map(|a| (**a).clone()))
                }
            };
            send(link, Some(to), &Msg::Section { version, section }, hw.rng);
        }
    }

    fn lane_msg(&mut self, hw: &mut Hw<'_>, msg: Msg) {
        let at = self.now(hw);
        match msg {
            Msg::Ack { upto } => {
                if upto > self.state.sync.acked && upto <= self.state.sync.next_seq {
                    self.sync_commit(Event::SyncAcked { at, upto });
                }
            }
            Msg::Relay { items } => {
                let cursor = self.state.sync.cursor;
                let items: Vec<(u64, Movement)> =
                    items.into_iter().filter(|(hs, _)| *hs > cursor).collect();
                if !items.is_empty() {
                    self.sync_commit(Event::SyncRelayed { at, items });
                }
            }
            Msg::Resync {
                relay_seq,
                acked,
                levels,
            } => {
                if relay_seq != self.state.sync.cursor && acked <= self.state.sync.next_seq {
                    self.sync_commit(Event::SyncResynced {
                        at,
                        relay_seq,
                        acked,
                        levels,
                    });
                }
            }
            Msg::Section { version, section } => {
                let key = section.key();
                if self
                    .state
                    .sync
                    .have
                    .get(&key)
                    .is_some_and(|v| *v >= version)
                {
                    return;
                }
                let me_before = self.user.and_then(|u| self.state.users.get(&u).cloned());
                if self.sync_commit(Event::SyncSection {
                    at,
                    version,
                    section,
                }) {
                    self.after_section(&key, me_before);
                }
            }
            Msg::Refused { protocol } => {
                self.net_rt.last_error = Some(alloc::format!(
                    "Head till runs network protocol {protocol}, this till {PROTOCOL}: update"
                ));
            }
            _ => {}
        }
    }

    /// Users replaced from the head: sign out unless the signed-in user's
    /// record (name, role, PIN) is unchanged. A sale in progress can be
    /// finished or voided first; the user is signed out right after.
    fn after_section(&mut self, key: &SectionKey, me_before: Option<pos_core::auth::User>) {
        if *key == SectionKey::Users
            && let Some(u) = self.user
            && (self.state.users.get(&u) != me_before.as_ref()
                || !self.state.users.get(&u).is_some_and(|x| x.active))
        {
            if !self.cart.is_empty() {
                self.stale_user = true;
                self.set_msg("Users changed on the head till: finish this sale", true);
                return;
            }
            self.user = None;
            self.place = crate::Place::Home;
            self.screen = crate::screens::login_screen(self);
        }
        self.dirty = true;
    }

    /// The node configuration the settings call for, if networked.
    pub fn node_config(&self) -> Option<pos_net::NodeConfig> {
        let n = &self.state.net;
        let key = n.key?;
        let role = match n.role {
            NetRole::Head => pos_net::Role::Head,
            NetRole::Lane => pos_net::Role::Lane,
            NetRole::Standalone => return None,
        };
        Some(pos_net::NodeConfig {
            role,
            till: n.till,
            key: pos_net::StoreKey(key),
            addressing: match n.static_ip {
                Some(s) => pos_net::Addressing::Static {
                    ip: s.ip,
                    prefix: s.prefix,
                    gateway: s.gateway,
                },
                None => pos_net::Addressing::Dhcp,
            },
            head: n.head_ip,
        })
    }

    /// Status line text for the network ("" when standalone).
    pub fn net_status(&self) -> &str {
        &self.net_rt.status
    }
}

/// A platform's network: its NIC and the node built from the settings.
/// Platforms call [`Network::step`] every main-loop pass.
pub struct Network<N: Nic> {
    pub nic: N,
    node: Option<pos_net::Node>,
}

impl<N: Nic> core::fmt::Debug for Network<N> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Network").field("node", &self.node).finish()
    }
}

impl<N: Nic> Network<N> {
    pub fn new(nic: N) -> Self {
        Network { nic, node: None }
    }

    pub fn step<D: BlockDevice>(&mut self, app: &mut App<D>, hw: &mut Hw<'_>, now_ms: u64) {
        let want = app.node_config();
        if want.as_ref() != self.node.as_ref().map(|n| n.config()) {
            self.node = want.map(|cfg| {
                let mut seed = [0u8; 8];
                let _ = hw.rng.fill(&mut seed);
                pos_net::Node::new(cfg, &mut self.nic, now_ms, u64::from_le_bytes(seed))
            });
            if self.node.is_none() {
                app.net_rt = SyncRuntime::default();
                app.dirty = true;
            }
        }
        if let Some(node) = &mut self.node {
            node.poll(&mut self.nic, hw.rng, now_ms);
            app.sync(node, hw, now_ms);
            node.poll(&mut self.nic, hw.rng, now_ms);
        }
    }
}
