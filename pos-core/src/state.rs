//! Event-sourced POS state.
//!
//! Every durable change is an [`Event`]. The live system validates a command,
//! builds the event, has `pos-store` commit it (write + flush), and only then
//! calls [`PosState::apply`]. Recovery replays the same events through the
//! same `apply`, so live and recovered state cannot diverge. `apply` re-checks
//! every invariant and rejects events that would violate them; on replay a
//! rejection means the journal is not one this code produced.

use crate::auth::{
    self, LockState, Permission, PinHasher, Role, User, UserId, manager_lockout_secs,
};
use crate::cart::{Cart, CartError};
use crate::catalog::{Catalog, CatalogError, Item};
use crate::config::Config;
use crate::money::{Money, MoneyError, Qty};
use crate::sale::{Refund, RefundLine, Refundable, RefundableLine, Sale, SoldLine};
use crate::shift::{Counters, Report, ReportKind, Shift};
use crate::tender::{Payment, Tender, TenderKind};
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Event {
    ConfigSet {
        by: UserId,
        at: i64,
        config: Config,
    },
    /// `by == None` only for the very first user (initial setup).
    UserSet {
        by: Option<UserId>,
        at: i64,
        user: User,
    },
    LoginOk {
        user: UserId,
        at: i64,
    },
    LoginFailed {
        user: UserId,
        at: i64,
    },
    Logout {
        user: UserId,
        at: i64,
    },
    UserUnlocked {
        user: UserId,
        by: UserId,
        at: i64,
    },
    CatalogUpsert {
        by: UserId,
        at: i64,
        item: Item,
    },
    CatalogDelete {
        by: UserId,
        at: i64,
        sku: String,
    },
    /// Atomic catalog replacement (CSV import).
    CatalogReplace {
        by: UserId,
        at: i64,
        items: Vec<Item>,
        source: String,
    },
    ShiftOpened {
        shift_id: u32,
        by: UserId,
        at: i64,
        float: Money,
    },
    SaleCompleted(Sale),
    RefundCompleted(Refund),
    LineVoided {
        by: UserId,
        authorized_by: UserId,
        at: i64,
        sku: String,
        qty: Qty,
        amount: Money,
    },
    SaleVoided {
        by: UserId,
        authorized_by: UserId,
        at: i64,
        lines: u16,
        amount: Money,
    },
    Override {
        cashier: UserId,
        manager: UserId,
        permission: Permission,
        at: i64,
    },
    NoSale {
        by: UserId,
        at: i64,
    },
    Reprint {
        by: UserId,
        receipt_no: u64,
        at: i64,
    },
    Report(Report),
    Exported {
        by: UserId,
        at: i64,
        what: String,
        bytes: u64,
    },
}

impl Event {
    pub fn at(&self) -> i64 {
        match self {
            Event::ConfigSet { at, .. }
            | Event::UserSet { at, .. }
            | Event::LoginOk { at, .. }
            | Event::LoginFailed { at, .. }
            | Event::Logout { at, .. }
            | Event::UserUnlocked { at, .. }
            | Event::CatalogUpsert { at, .. }
            | Event::CatalogDelete { at, .. }
            | Event::CatalogReplace { at, .. }
            | Event::ShiftOpened { at, .. }
            | Event::LineVoided { at, .. }
            | Event::SaleVoided { at, .. }
            | Event::Override { at, .. }
            | Event::NoSale { at, .. }
            | Event::Reprint { at, .. }
            | Event::Exported { at, .. } => *at,
            Event::SaleCompleted(s) => s.at,
            Event::RefundCompleted(r) => r.at,
            Event::Report(r) => r.at,
        }
    }

    /// Short audit label.
    pub fn kind(&self) -> &'static str {
        match self {
            Event::ConfigSet { .. } => "CONFIG",
            Event::UserSet { .. } => "USER",
            Event::LoginOk { .. } => "LOGIN",
            Event::LoginFailed { .. } => "LOGIN_FAIL",
            Event::Logout { .. } => "LOGOUT",
            Event::UserUnlocked { .. } => "UNLOCK",
            Event::CatalogUpsert { .. } => "ITEM",
            Event::CatalogDelete { .. } => "ITEM_DEL",
            Event::CatalogReplace { .. } => "IMPORT",
            Event::ShiftOpened { .. } => "SHIFT_OPEN",
            Event::SaleCompleted(_) => "SALE",
            Event::RefundCompleted(_) => "REFUND",
            Event::LineVoided { .. } => "LINE_VOID",
            Event::SaleVoided { .. } => "SALE_VOID",
            Event::Override { .. } => "OVERRIDE",
            Event::NoSale { .. } => "NO_SALE",
            Event::Reprint { .. } => "REPRINT",
            Event::Report(r) if r.kind == ReportKind::Z => "Z_REPORT",
            Event::Report(_) => "X_REPORT",
            Event::Exported { .. } => "EXPORT",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DomainError {
    Money(MoneyError),
    Cart(CartError),
    Catalog(CatalogError),
    Tender(crate::tender::TenderError),
    Fiscal(String),
    Config(&'static str),
    UnknownUser,
    UserInactive,
    Locked,
    BadPin,
    Forbidden(Permission),
    NoShift,
    ShiftAlreadyOpen,
    ShiftMismatch,
    ReceiptSequence { expected: u64, got: u64 },
    ZSequence { expected: u32, got: u32 },
    InconsistentTotals,
    NotPaid,
    UnknownReceipt,
    RefundExceedsSale,
    EmptyRefund,
    SetupDone,
    LastManager,
}

impl From<MoneyError> for DomainError {
    fn from(e: MoneyError) -> Self {
        DomainError::Money(e)
    }
}
impl From<CartError> for DomainError {
    fn from(e: CartError) -> Self {
        DomainError::Cart(e)
    }
}
impl From<CatalogError> for DomainError {
    fn from(e: CatalogError) -> Self {
        DomainError::Catalog(e)
    }
}
impl From<crate::tender::TenderError> for DomainError {
    fn from(e: crate::tender::TenderError) -> Self {
        DomainError::Tender(e)
    }
}

pub type DResult<T> = Result<T, DomainError>;

/// Materialised state. Serialized whole as a snapshot by `pos-store`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PosState {
    pub config: Config,
    pub catalog: Catalog,
    pub users: BTreeMap<UserId, User>,
    pub locks: BTreeMap<UserId, LockState>,
    pub shift: Option<Shift>,
    pub next_receipt: u64,
    pub next_z: u32,
    pub next_shift: u32,
    pub grand_total: Money,
    pub refundable: BTreeMap<u64, Refundable>,
    /// Number of events applied (sanity check against the journal).
    pub events: u64,
    pub last_event_at: i64,
}

impl Default for PosState {
    fn default() -> Self {
        PosState {
            config: Config::default(),
            catalog: Catalog::default(),
            users: BTreeMap::new(),
            locks: BTreeMap::new(),
            shift: None,
            next_receipt: 1,
            next_z: 1,
            next_shift: 1,
            grand_total: Money::ZERO,
            refundable: BTreeMap::new(),
            events: 0,
            last_event_at: 0,
        }
    }
}

impl PosState {
    pub fn needs_setup(&self) -> bool {
        !self
            .users
            .values()
            .any(|u| u.role == Role::Manager && u.active)
    }

    pub fn user(&self, id: UserId) -> DResult<&User> {
        self.users.get(&id).ok_or(DomainError::UnknownUser)
    }

    fn require(&self, id: UserId, p: Permission) -> DResult<()> {
        let u = self.user(id)?;
        if !u.active {
            return Err(DomainError::UserInactive);
        }
        if !auth::role_allows(u.role, p) {
            return Err(DomainError::Forbidden(p));
        }
        Ok(())
    }

    /// May `user` do `p` alone? Otherwise a manager override is required.
    pub fn allows(&self, user: UserId, p: Permission) -> bool {
        self.require(user, p).is_ok()
    }

    pub fn is_locked(&self, user: UserId, now: i64) -> bool {
        match (self.users.get(&user), self.locks.get(&user)) {
            (Some(u), Some(l)) if u.role == Role::Manager => l.locked && now < l.locked_until,
            (_, Some(l)) => l.locked,
            _ => false,
        }
    }

    // ------------------------------------------------------------ commands

    /// Check a PIN and return the event to journal (`LoginOk` or
    /// `LoginFailed`). Failed attempts are journaled so lockout survives a
    /// reboot; the caller must commit the returned event either way.
    pub fn login(
        &self,
        user: UserId,
        pin: &[u8],
        hasher: &dyn PinHasher,
        at: i64,
    ) -> DResult<(Event, bool)> {
        let u = self.user(user)?;
        if !u.active {
            return Err(DomainError::UserInactive);
        }
        if self.is_locked(user, at) {
            return Err(DomainError::Locked);
        }
        if auth::verify_pin(hasher, pin, &u.pin) {
            Ok((Event::LoginOk { user, at }, true))
        } else {
            Ok((Event::LoginFailed { user, at }, false))
        }
    }

    /// Verify a manager's PIN for an override (not journaled as a login).
    pub fn check_override(
        &self,
        manager: UserId,
        pin: &[u8],
        hasher: &dyn PinHasher,
        p: Permission,
        at: i64,
    ) -> DResult<()> {
        self.require(manager, p)?;
        if self.is_locked(manager, at) {
            return Err(DomainError::Locked);
        }
        if !auth::verify_pin(hasher, pin, &self.user(manager)?.pin) {
            return Err(DomainError::BadPin);
        }
        Ok(())
    }

    /// Build the `SaleCompleted` event for a paid cart.
    pub fn build_sale(
        &self,
        cart: &Cart,
        payment: &Payment,
        cashier: UserId,
        at: i64,
    ) -> DResult<Sale> {
        let shift = self.shift.as_ref().ok_or(DomainError::NoShift)?;
        let priced = cart.price(&self.config)?;
        if priced.totals.total != payment.due {
            return Err(DomainError::InconsistentTotals);
        }
        if !payment.is_complete() {
            return Err(DomainError::NotPaid);
        }
        let lines = cart
            .lines
            .iter()
            .cloned()
            .zip(priced.lines)
            .map(|(line, priced)| SoldLine { line, priced })
            .collect();
        Ok(Sale {
            receipt_no: self.next_receipt,
            shift_id: shift.id,
            cashier,
            at,
            lines,
            totals: priced.totals,
            tenders: payment.tenders.clone(),
            rounding: payment.rounding,
            change: payment.change,
        })
    }

    /// Build a refund of `(line index, qty)` pairs from receipt `original`,
    /// paid out with `payout` (cash or back to card/voucher, by reference).
    #[allow(clippy::too_many_arguments)]
    pub fn build_refund(
        &self,
        original: u64,
        picks: &[(u16, Qty)],
        payout_kind: TenderKind,
        payout_ref: &str,
        cashier: UserId,
        authorized_by: UserId,
        reason: &str,
        at: i64,
    ) -> DResult<Refund> {
        let shift = self.shift.as_ref().ok_or(DomainError::NoShift)?;
        let rf = self
            .refundable
            .get(&original)
            .ok_or(DomainError::UnknownReceipt)?;
        if picks.is_empty() {
            return Err(DomainError::EmptyRefund);
        }
        let mut lines = Vec::new();
        for &(idx, qty) in picks {
            let l = rf
                .lines
                .get(usize::from(idx))
                .ok_or(DomainError::UnknownReceipt)?;
            if qty.0 <= 0 {
                return Err(DomainError::EmptyRefund);
            }
            let already: i64 = lines
                .iter()
                .filter(|x: &&RefundLine| x.index == idx)
                .map(|x| x.qty.0)
                .sum();
            let left_qty = l.qty.0 - l.refunded_qty.0 - already;
            if qty.0 > left_qty {
                return Err(DomainError::RefundExceedsSale);
            }
            let left_total = l.total.checked_sub(l.refunded_total)?;
            let left_tax = l.tax.checked_sub(l.refunded_tax)?;
            let (total, tax) = if qty.0 == left_qty {
                (left_total, left_tax)
            } else {
                (
                    Money(
                        l.total
                            .mul_div(qty.0, l.qty.0, self.config.rounding)?
                            .0
                            .min(left_total.0),
                    ),
                    Money(
                        l.tax
                            .mul_div(qty.0, l.qty.0, self.config.rounding)?
                            .0
                            .min(left_tax.0),
                    ),
                )
            };
            lines.push(RefundLine {
                index: idx,
                sku: l.sku.clone(),
                qty,
                total,
                tax,
            });
        }
        let total = Money::sum(lines.iter().map(|l| l.total))?;
        let tax = Money::sum(lines.iter().map(|l| l.tax))?;
        Ok(Refund {
            receipt_no: self.next_receipt,
            original_receipt: original,
            shift_id: shift.id,
            cashier,
            authorized_by,
            at,
            lines,
            total,
            tax,
            payout: alloc::vec![Tender {
                kind: payout_kind,
                amount: total,
                reference: payout_ref.into(),
            }],
            reason: reason.into(),
        })
    }

    pub fn build_report(
        &self,
        kind: ReportKind,
        by: UserId,
        at: i64,
        counted_cash: Option<Money>,
    ) -> DResult<Report> {
        let shift = self.shift.clone().ok_or(DomainError::NoShift)?;
        Ok(Report {
            kind,
            z_no: (kind == ReportKind::Z).then_some(self.next_z),
            at,
            by,
            expected_cash: shift.expected_cash()?,
            shift,
            counted_cash,
            grand_total: self.grand_total,
            extra: Vec::new(),
        })
    }

    // ------------------------------------------------------------ apply

    /// Apply a committed event. Fails without modifying `self` if the event
    /// violates an invariant.
    pub fn apply(&mut self, e: &Event) -> DResult<()> {
        let mut next = self.clone_for(e);
        next.apply_inner(e)?;
        next.events += 1;
        next.last_event_at = next.last_event_at.max(e.at());
        *self = next;
        Ok(())
    }

    /// Cheap path: most events only touch small parts of the state, but
    /// cloning the whole state keeps `apply` transactional. The catalog and
    /// refund index are the large parts; they are cloned regardless, which is
    /// fine at POS event rates (a few per minute).
    fn clone_for(&self, _e: &Event) -> PosState {
        self.clone()
    }

    fn apply_inner(&mut self, e: &Event) -> DResult<()> {
        match e {
            Event::ConfigSet { by, config, .. } => {
                self.require(*by, Permission::Configure)?;
                config.validate().map_err(DomainError::Config)?;
                for it in self.catalog.items() {
                    if config.tax_class(it.tax_class).is_none() {
                        return Err(DomainError::Config("tax class in use"));
                    }
                }
                self.config = config.clone();
            }
            Event::UserSet { by, user, .. } => {
                match by {
                    None => {
                        if !self.needs_setup() || user.role != Role::Manager {
                            return Err(DomainError::SetupDone);
                        }
                    }
                    Some(b) => self.require(*b, Permission::ManageUsers)?,
                }
                if user.name.trim().is_empty() || user.name.chars().count() > 24 {
                    return Err(DomainError::UnknownUser);
                }
                let mut after = self.users.clone();
                after.insert(user.id, user.clone());
                if !after.values().any(|u| u.role == Role::Manager && u.active) {
                    return Err(DomainError::LastManager);
                }
                self.users = after;
            }
            Event::LoginOk { user, at } => {
                let u = self.user(*user)?;
                if !u.active {
                    return Err(DomainError::UserInactive);
                }
                if self.is_locked(*user, *at) {
                    return Err(DomainError::Locked);
                }
                let l = self.locks.entry(*user).or_default();
                l.failures = 0;
                l.locked = false;
            }
            Event::LoginFailed { user, at } => {
                let role = self.user(*user)?.role;
                let max = self.config.max_pin_failures;
                let l = self.locks.entry(*user).or_default();
                l.failures = l.failures.saturating_add(1);
                if l.failures >= max {
                    l.locked = true;
                    l.failures = 0;
                    if role == Role::Manager {
                        l.locked_until = at.saturating_add(manager_lockout_secs(l.lockouts));
                    }
                    l.lockouts = l.lockouts.saturating_add(1);
                }
            }
            Event::Logout { user, .. } => {
                self.user(*user)?;
            }
            Event::UserUnlocked { user, by, .. } => {
                self.require(*by, Permission::UnlockUser)?;
                self.user(*user)?;
                self.locks.insert(*user, LockState::default());
            }
            Event::CatalogUpsert { by, item, .. } => {
                self.require(*by, Permission::ManageCatalog)?;
                let v = Catalog::validate_item(item, &self.config)?;
                self.catalog.upsert(v)?;
            }
            Event::CatalogDelete { by, sku, .. } => {
                self.require(*by, Permission::ManageCatalog)?;
                self.catalog.remove(sku).ok_or(CatalogError::NotFound)?;
            }
            Event::CatalogReplace { by, items, .. } => {
                self.require(*by, Permission::Import)?;
                self.catalog = Catalog::from_items(items, &self.config)
                    .map_err(|(_, e)| DomainError::Catalog(e))?;
            }
            Event::ShiftOpened {
                shift_id,
                by,
                at,
                float,
            } => {
                self.require(*by, Permission::OpenShift)?;
                if self.shift.is_some() {
                    return Err(DomainError::ShiftAlreadyOpen);
                }
                if *shift_id != self.next_shift || float.is_negative() {
                    return Err(DomainError::ShiftMismatch);
                }
                self.next_shift += 1;
                self.shift = Some(Shift {
                    id: *shift_id,
                    opened_by: *by,
                    opened_at: *at,
                    float: *float,
                    counters: Counters::default(),
                });
            }
            Event::SaleCompleted(s) => self.apply_sale(s)?,
            Event::RefundCompleted(r) => self.apply_refund(r)?,
            Event::LineVoided {
                by, authorized_by, ..
            } => {
                self.require(*authorized_by, Permission::LineVoid)?;
                self.user(*by)?;
                self.shift_mut()?.counters.line_voids += 1;
            }
            Event::SaleVoided {
                by, authorized_by, ..
            } => {
                self.require(*authorized_by, Permission::VoidSale)?;
                self.user(*by)?;
                self.shift_mut()?.counters.sale_voids += 1;
            }
            Event::Override {
                cashier,
                manager,
                permission,
                ..
            } => {
                self.require(*manager, *permission)?;
                self.user(*cashier)?;
                if let Some(s) = self.shift.as_mut() {
                    s.counters.overrides += 1;
                }
            }
            Event::NoSale { by, .. } => {
                self.require(*by, Permission::NoSale)?;
                self.shift_mut()?.counters.no_sales += 1;
            }
            Event::Reprint { by, receipt_no, .. } => {
                self.require(*by, Permission::Reprint)?;
                if *receipt_no == 0 || *receipt_no >= self.next_receipt {
                    return Err(DomainError::UnknownReceipt);
                }
                if let Some(s) = self.shift.as_mut() {
                    s.counters.reprints += 1;
                }
            }
            Event::Report(r) => self.apply_report(r)?,
            Event::Exported { by, .. } => {
                self.require(*by, Permission::Export)?;
            }
        }
        Ok(())
    }

    fn shift_mut(&mut self) -> DResult<&mut Shift> {
        self.shift.as_mut().ok_or(DomainError::NoShift)
    }

    fn apply_sale(&mut self, s: &Sale) -> DResult<()> {
        self.require(s.cashier, Permission::Sell)?;
        if s.receipt_no != self.next_receipt {
            return Err(DomainError::ReceiptSequence {
                expected: self.next_receipt,
                got: s.receipt_no,
            });
        }
        let shift = self.shift.as_ref().ok_or(DomainError::NoShift)?;
        if shift.id != s.shift_id || s.lines.is_empty() {
            return Err(DomainError::ShiftMismatch);
        }
        // Re-price from the journaled lines: totals must match exactly.
        let cart = Cart {
            lines: s.lines.iter().map(|l| l.line.clone()).collect(),
            basket_discount: None,
        };
        let line_sum = Money::sum(s.lines.iter().map(|l| l.priced.total))?;
        if line_sum != s.totals.total {
            return Err(DomainError::InconsistentTotals);
        }
        let tax_sum = Money::sum(s.lines.iter().map(|l| l.priced.tax))?;
        if tax_sum != s.totals.tax {
            return Err(DomainError::InconsistentTotals);
        }
        for (l, sl) in cart.lines.iter().zip(&s.lines) {
            if l.gross(&self.config)? != sl.priced.gross || sl.priced.total.is_negative() {
                return Err(DomainError::InconsistentTotals);
            }
        }
        let paid = Money::sum(s.tenders.iter().map(|t| t.amount))?.checked_sub(s.change)?;
        if paid != s.totals.total.checked_add(s.rounding)? || s.change.is_negative() {
            return Err(DomainError::NotPaid);
        }
        let collected = s.totals.total;
        let shift = self.shift_mut()?;
        shift.counters.add_sale(s)?;
        self.grand_total = self.grand_total.checked_add(collected)?;
        self.next_receipt += 1;
        self.refundable.insert(
            s.receipt_no,
            Refundable {
                receipt_no: s.receipt_no,
                lines: s
                    .lines
                    .iter()
                    .map(|l| RefundableLine {
                        sku: l.line.sku.clone(),
                        name: l.line.name.clone(),
                        qty: l.line.qty,
                        total: l.priced.total,
                        tax: l.priced.tax,
                        refunded_qty: Qty(0),
                        refunded_total: Money::ZERO,
                        refunded_tax: Money::ZERO,
                    })
                    .collect(),
            },
        );
        while self.refundable.len() > self.config.refund_window as usize {
            self.refundable.pop_first();
        }
        Ok(())
    }

    fn apply_refund(&mut self, r: &Refund) -> DResult<()> {
        self.require(r.authorized_by, Permission::Refund)?;
        self.user(r.cashier)?;
        if r.receipt_no != self.next_receipt {
            return Err(DomainError::ReceiptSequence {
                expected: self.next_receipt,
                got: r.receipt_no,
            });
        }
        if self.shift.as_ref().map(|s| s.id) != Some(r.shift_id) {
            return Err(DomainError::ShiftMismatch);
        }
        if r.lines.is_empty() {
            return Err(DomainError::EmptyRefund);
        }
        let mut rf = self
            .refundable
            .get(&r.original_receipt)
            .cloned()
            .ok_or(DomainError::UnknownReceipt)?;
        for rl in &r.lines {
            let l = rf
                .lines
                .get_mut(usize::from(rl.index))
                .ok_or(DomainError::UnknownReceipt)?;
            if rl.qty.0 <= 0 || rl.total.is_negative() || rl.tax.is_negative() || rl.sku != l.sku {
                return Err(DomainError::RefundExceedsSale);
            }
            l.refunded_qty = Qty(l
                .refunded_qty
                .0
                .checked_add(rl.qty.0)
                .ok_or(MoneyError::Overflow)?);
            l.refunded_total = l.refunded_total.checked_add(rl.total)?;
            l.refunded_tax = l.refunded_tax.checked_add(rl.tax)?;
            if l.refunded_qty > l.qty || l.refunded_total > l.total || l.refunded_tax > l.tax {
                return Err(DomainError::RefundExceedsSale);
            }
        }
        if Money::sum(r.lines.iter().map(|l| l.total))? != r.total
            || Money::sum(r.lines.iter().map(|l| l.tax))? != r.tax
            || Money::sum(r.payout.iter().map(|t| t.amount))? != r.total
        {
            return Err(DomainError::InconsistentTotals);
        }
        self.refundable.insert(r.original_receipt, rf);
        self.shift_mut()?.counters.add_refund(r)?;
        self.grand_total = self.grand_total.checked_sub(r.total)?;
        self.next_receipt += 1;
        Ok(())
    }

    fn apply_report(&mut self, r: &Report) -> DResult<()> {
        let perm = match r.kind {
            ReportKind::X => Permission::XReport,
            ReportKind::Z => Permission::ZReport,
        };
        self.require(r.by, perm)?;
        let shift = self.shift.as_ref().ok_or(DomainError::NoShift)?;
        if *shift != r.shift {
            return Err(DomainError::ShiftMismatch);
        }
        if r.kind == ReportKind::Z {
            if r.z_no != Some(self.next_z) {
                return Err(DomainError::ZSequence {
                    expected: self.next_z,
                    got: r.z_no.unwrap_or(0),
                });
            }
            self.next_z += 1;
            self.shift = None;
        }
        Ok(())
    }
}
