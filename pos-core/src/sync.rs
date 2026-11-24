//! Store network bookkeeping (see `docs/NETWORK.md`).
//!
//! Everything here is derived inside [`crate::PosState::apply`], so a
//! reboot recovers exactly the sync position it had: which movements the
//! head has acknowledged, which relayed movements and which catalog/users/
//! settings/picture versions a lane has applied.

use alloc::collections::{BTreeMap, VecDeque};
use alloc::string::String;
use alloc::vec::Vec;
use serde::{Deserialize, Serialize};

use crate::auth::User;
use crate::catalog::Item;
use crate::config::Config;
use crate::inventory::StockLevel;
use crate::media::{Picture, PictureFor};
use crate::money::Money;
use crate::policy::Policy;

pub type TillNo = u8;

/// Movements the head keeps for lanes that are catching up.
pub const RELAY_WINDOW: usize = 20_000;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum NetRole {
    #[default]
    Standalone,
    Head,
    Lane,
}

impl NetRole {
    pub fn label(self) -> &'static str {
        match self {
            NetRole::Standalone => "Standalone",
            NetRole::Head => "Head till",
            NetRole::Lane => "Lane till",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StaticIp {
    pub ip: [u8; 4],
    pub prefix: u8,
    pub gateway: Option<[u8; 4]>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetSettings {
    pub role: NetRole,
    /// This till's number in the store, 1–99.
    pub till: TillNo,
    /// The store network key (never printed or exported).
    pub key: Option<[u8; 32]>,
    /// `None` = DHCP.
    pub static_ip: Option<StaticIp>,
    /// Lanes: the head's address; `None` = found by announcement.
    pub head_ip: Option<[u8; 4]>,
}

impl NetSettings {
    pub fn networked(&self) -> bool {
        self.role != NetRole::Standalone && self.key.is_some()
    }
    pub fn valid(&self) -> bool {
        match self.role {
            NetRole::Standalone => true,
            _ => (1..=99).contains(&self.till) && self.key.is_some(),
        }
    }
}

/// One stock change as shared between tills.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StockMove {
    pub sku: String,
    /// Change in thousandths of a unit.
    pub delta: i64,
    /// Deliveries, counts and write-offs start tracking an item; sales and
    /// refunds only move items that are already tracked.
    pub track: bool,
}

/// A sale or refund total, for the head's store-wide figures.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SaleNote {
    pub receipt: u64,
    /// Negative for refunds.
    pub total: Money,
}

/// One till's activity, numbered per origin from 1.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Movement {
    pub origin: TillNo,
    pub seq: u64,
    pub at: i64,
    pub moves: Vec<StockMove>,
    pub sale: Option<SaleNote>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum SectionKey {
    Catalog,
    Users,
    Config,
    Picture(PictureFor),
    Policy,
}

/// State the head hands to lanes whole.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Section {
    Catalog(Vec<Item>),
    Users(Vec<User>),
    Config(Config),
    Picture(PictureFor, Option<Picture>),
    Policy(Policy),
}

impl Section {
    pub fn key(&self) -> SectionKey {
        match self {
            Section::Catalog(_) => SectionKey::Catalog,
            Section::Users(_) => SectionKey::Users,
            Section::Config(_) => SectionKey::Config,
            Section::Picture(k, _) => SectionKey::Picture(k.clone()),
            Section::Policy(_) => SectionKey::Policy,
        }
    }
}

/// Sales per till for one day (head).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TillDay {
    /// Days since 1970 (UTC) the figures are for.
    pub day: i64,
    pub sales: u32,
    pub total: Money,
    pub last_at: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncState {
    /// Own movements numbered so far.
    pub next_seq: u64,
    /// Lane: own movements the head has not acknowledged yet.
    pub outbox: VecDeque<Movement>,
    /// Lane: highest own movement the head acknowledged.
    pub acked: u64,
    /// Lane: highest head relay number applied.
    pub cursor: u64,
    /// Lane: section versions applied.
    pub have: BTreeMap<SectionKey, u64>,
    /// Head: highest movement received per origin.
    pub origins: BTreeMap<TillNo, u64>,
    /// Head: recent movements from every till, by relay number.
    pub relay: VecDeque<(u64, Movement)>,
    pub relay_seq: u64,
    /// Head: current section versions.
    pub versions: BTreeMap<SectionKey, u64>,
    /// Head: today's sales per till (the head's own included).
    pub tills: BTreeMap<TillNo, TillDay>,
}

impl SyncState {
    pub(crate) fn note_sale(&mut self, origin: TillNo, at: i64, note: &SaleNote) {
        let day = at.div_euclid(86_400);
        let d = self.tills.entry(origin).or_default();
        if d.day != day {
            *d = TillDay {
                day,
                ..TillDay::default()
            };
        }
        if !note.total.is_negative() {
            d.sales = d.sales.saturating_add(1);
        }
        d.total = d.total.checked_add(note.total).unwrap_or(d.total);
        d.last_at = d.last_at.max(at);
    }

    pub(crate) fn push_relay(&mut self, m: Movement) {
        self.relay_seq += 1;
        self.relay.push_back((self.relay_seq, m));
        while self.relay.len() > RELAY_WINDOW {
            self.relay.pop_front();
        }
    }

    /// Head: can a lane at `cursor` be served from the window?
    pub fn relay_from(&self, cursor: u64) -> Option<impl Iterator<Item = &(u64, Movement)>> {
        let first = self.relay.front().map_or(self.relay_seq + 1, |r| r.0);
        if cursor + 1 < first && cursor < self.relay_seq {
            return None;
        }
        Some(self.relay.iter().filter(move |r| r.0 > cursor))
    }

    /// Store sales today (head view).
    pub fn store_today(&self, now: i64) -> (u32, Money, usize) {
        let day = now.div_euclid(86_400);
        let mut n = 0;
        let mut t = Money::ZERO;
        let mut tills = 0;
        for d in self.tills.values().filter(|d| d.day == day) {
            n += d.sales;
            t = t.checked_add(d.total).unwrap_or(t);
            tills += 1;
        }
        (n, t, tills)
    }
}

/// Absolute stock levels for a lane that fell out of the relay window.
pub type Levels = Vec<(String, StockLevel)>;
