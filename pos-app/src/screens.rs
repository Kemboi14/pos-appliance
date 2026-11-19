//! Screens: drawing and key handling. Pointer clicks are translated into the
//! same key actions, so all behaviour lives in `on_key`.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use pos_core::auth::{self, PIN_KDF_VERSION, Permission, PinHash, Role, User, UserId};
use pos_core::barcode::{self, EmbedKind, Scan};
use pos_core::cart::{Discount, Line};
use pos_core::csv::parse_money;
use pos_core::shift::ReportKind;
use pos_core::state::Event;
use pos_core::tender::{Payment, Tender, TenderKind};
use pos_core::{Money, Qty, Rate};
use pos_hal::{BlockDevice, Key, Modifiers};
use pos_store::Trust;
use pos_ui::scan::Enter;
use pos_ui::{ButtonStyle, Font, Rect, Ui};

use crate::receipt;
use crate::{App, AppError, BootStatus, Hw, Journaled};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pending {
    LineVoid(usize),
    VoidSale,
    PriceOverride(usize, Money),
    LineDiscount(usize, Rate),
    BasketDiscount(Rate),
    Refund {
        receipt: u64,
        picks: Vec<(u16, Qty)>,
        reason: String,
        kind: TenderKind,
        reference: String,
    },
    NoSale,
    XReport,
    ZReport(Option<Money>),
    Export,
    ImportCatalog,
    ImportSettings,
    AddUser(Role),
    Unlock,
}

impl Pending {
    fn permission(&self) -> Permission {
        match self {
            Pending::LineVoid(_) => Permission::LineVoid,
            Pending::VoidSale => Permission::VoidSale,
            Pending::PriceOverride(..) => Permission::PriceOverride,
            Pending::LineDiscount(..) | Pending::BasketDiscount(_) => Permission::Discount,
            Pending::Refund { .. } => Permission::Refund,
            Pending::NoSale => Permission::NoSale,
            Pending::XReport => Permission::XReport,
            Pending::ZReport(_) => Permission::ZReport,
            Pending::Export => Permission::Export,
            Pending::ImportCatalog => Permission::Import,
            Pending::ImportSettings => Permission::Configure,
            Pending::AddUser(_) => Permission::ManageUsers,
            Pending::Unlock => Permission::UnlockUser,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Next {
    FormatConfirm,
    AdoptUser,
    AdoptPin(UserId),
    SetupName,
    SetupPin1(String),
    SetupPin2(String, String),
    LoginUser,
    LoginPin(UserId),
    Float,
    CardRef(Money),
    VoucherCode,
    VoucherAmount(String),
    OverrideUser(Pending),
    OverridePin(Pending, UserId),
    ZCounted,
    ReprintNo,
    RefundReceipt,
    RefundQty(u64, u16, Vec<(u16, Qty)>),
    RefundReason(u64, Vec<(u16, Qty)>),
    RefundPayout(u64, Vec<(u16, Qty)>, String),
    RefundCardRef(u64, Vec<(u16, Qty)>, String),
    AddUserName(Role),
    AddUserPin(Role, String),
    UnlockUser,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prompt {
    pub title: String,
    pub hint: String,
    pub field: String,
    pub secret: bool,
    pub next: Next,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Screen {
    Blank,
    Fatal(String),
    Prompt(Prompt),
    Sale,
    Search {
        query: String,
        sel: usize,
    },
    Pay,
    Done {
        change: Money,
        receipt: u64,
    },
    Menu {
        sel: usize,
    },
    Info {
        title: String,
        lines: Vec<String>,
    },
    RefundPick {
        receipt: u64,
        sel: usize,
        picks: Vec<(u16, Qty)>,
    },
}

fn prompt(title: &str, hint: &str, secret: bool, next: Next) -> Screen {
    Screen::Prompt(Prompt {
        title: title.into(),
        hint: hint.into(),
        field: String::new(),
        secret,
        next,
    })
}

pub(crate) fn initial_screen<D: BlockDevice>(app: &App<D>, st: &BootStatus) -> Screen {
    match st {
        BootStatus::Ready => login_prompt(app),
        BootStatus::FirstRun => prompt(
            "First-time setup",
            "Enter the manager's name (this creates the first manager account)",
            false,
            Next::SetupName,
        ),
        BootStatus::Foreign => prompt(
            "STORE NOT BOUND TO THIS MACHINE",
            "The data partition was bound to another machine or was modified. \
             A manager must authorise adopting it. Manager user number:",
            false,
            Next::AdoptUser,
        ),
        BootStatus::Unformatted => prompt(
            "Data partition is blank",
            "Type ERASE and press Enter to initialise a new, empty store",
            false,
            Next::FormatConfirm,
        ),
        BootStatus::Corrupt(e) => prompt(
            "STORE DAMAGED",
            &format!(
                "{e}. Power off and image the stick for recovery, or type ERASE to start an EMPTY store (all data lost)"
            ),
            false,
            Next::FormatConfirm,
        ),
    }
}

fn login_prompt<D: BlockDevice>(app: &App<D>) -> Screen {
    let users: Vec<String> = app
        .state
        .users
        .values()
        .filter(|u| u.active)
        .map(|u| format!("{}={}", u.id, u.name))
        .collect();
    prompt(
        "Sign in",
        &format!("User number ({})", users.join(", ")),
        false,
        Next::LoginUser,
    )
}

// ------------------------------------------------------------------ keys

pub(crate) fn on_key<D: BlockDevice>(
    app: &mut App<D>,
    key: Key,
    mods: Modifiers,
    at: u64,
    hw: &mut Hw<'_>,
) {
    let _ = mods;
    let screen = core::mem::replace(&mut app.screen, Screen::Blank);
    app.screen = match screen {
        Screen::Prompt(p) => prompt_key(app, p, key, hw),
        Screen::Sale => {
            sale_key(app, key, at, hw);
            if matches!(app.screen, Screen::Blank) {
                Screen::Sale
            } else {
                core::mem::replace(&mut app.screen, Screen::Blank)
            }
        }
        Screen::Search { query, sel } => search_key(app, query, sel, key),
        Screen::Pay => {
            pay_key(app, key, hw);
            if matches!(app.screen, Screen::Blank) {
                Screen::Pay
            } else {
                core::mem::replace(&mut app.screen, Screen::Blank)
            }
        }
        Screen::Done { .. } => {
            app.message = None;
            Screen::Sale
        }
        Screen::Menu { sel } => menu_key(app, sel, key, hw),
        Screen::Info { title, lines } => match key {
            Key::Enter | Key::Escape => back_to_work(app),
            _ => Screen::Info { title, lines },
        },
        Screen::RefundPick {
            receipt,
            sel,
            picks,
        } => refund_pick_key(app, receipt, sel, picks, key),
        Screen::Fatal(m) => Screen::Fatal(m),
        Screen::Blank => back_to_work(app),
    };
}

fn back_to_work<D: BlockDevice>(app: &App<D>) -> Screen {
    match app.user {
        None => login_prompt(app),
        Some(_) if app.state.shift.is_none() => prompt(
            "Open shift",
            "Opening cash float (e.g. 100.00), Enter",
            false,
            Next::Float,
        ),
        Some(_) => Screen::Sale,
    }
}

fn prompt_key<D: BlockDevice>(
    app: &mut App<D>,
    mut p: Prompt,
    key: Key,
    hw: &mut Hw<'_>,
) -> Screen {
    match key {
        Key::Char(c) if p.field.chars().count() < 40 => {
            p.field.push(c);
            Screen::Prompt(p)
        }
        Key::Backspace => {
            p.field.pop();
            Screen::Prompt(p)
        }
        Key::Escape => match p.next {
            Next::FormatConfirm
            | Next::AdoptUser
            | Next::AdoptPin(_)
            | Next::SetupName
            | Next::SetupPin1(_)
            | Next::SetupPin2(..)
            | Next::LoginUser => {
                p.field.clear();
                Screen::Prompt(p)
            }
            Next::LoginPin(_) => login_prompt(app),
            Next::CardRef(_) | Next::VoucherCode | Next::VoucherAmount(_) => Screen::Pay,
            _ => back_to_work(app),
        },
        Key::Enter => {
            let field = core::mem::take(&mut p.field);
            submit(app, p, field, hw)
        }
        _ => Screen::Prompt(p),
    }
}

fn random<const N: usize>(hw: &mut Hw<'_>) -> Option<[u8; N]> {
    let mut b = [0u8; N];
    hw.rng.fill(&mut b).ok().map(|_| b)
}

fn make_user<D: BlockDevice>(
    app: &App<D>,
    hw: &mut Hw<'_>,
    id: UserId,
    name: &str,
    role: Role,
    pin: &str,
) -> Option<User> {
    let salt = random::<16>(hw)?;
    Some(User {
        id,
        name: name.trim().into(),
        role,
        pin: PinHash {
            version: PIN_KDF_VERSION,
            salt,
            hash: app.hasher.hash(pin.as_bytes(), &salt),
        },
        active: true,
    })
}

fn parse_user(s: &str) -> Option<UserId> {
    s.trim().parse().ok()
}

fn parse_qty(s: &str) -> Option<Qty> {
    // "3" → 3 units; "1.250" → 1.25 units (kg for weighables).
    let m = parse_money(s, 3)?;
    (m.0 > 0).then_some(Qty(m.0))
}

fn parse_rate(s: &str) -> Option<Rate> {
    // Percent with up to 4 decimals: "12.5" → 125000 = ppm (1 % = 10 000 ppm).
    let ppm = parse_money(s, 4)?.0;
    (0..=1_000_000).contains(&ppm).then_some(Rate(ppm as u32))
}

fn submit<D: BlockDevice>(app: &mut App<D>, p: Prompt, field: String, hw: &mut Hw<'_>) -> Screen {
    let now = app.now(hw);
    let again = |p: Prompt, app: &mut App<D>, msg: &str| {
        app.set_msg(msg, true);
        Screen::Prompt(p)
    };
    match p.next.clone() {
        Next::FormatConfirm => {
            if field.trim() != "ERASE" {
                return again(p, app, "Type ERASE exactly");
            }
            let dev = match app.pending_dev.take() {
                Some(d) => d,
                None => match app.store.take() {
                    Some(s) => s.into_device(),
                    None => return Screen::Fatal("no storage device".into()),
                },
            };
            match app.reformat(dev) {
                Ok(()) => prompt(
                    "First-time setup",
                    "Enter the manager's name",
                    false,
                    Next::SetupName,
                ),
                Err(e) => Screen::Fatal(format!("Format failed: {e:?}")),
            }
        }
        Next::AdoptUser => match parse_user(&field) {
            Some(u)
                if app
                    .state
                    .users
                    .get(&u)
                    .is_some_and(|x| x.role == Role::Manager) =>
            {
                prompt(
                    "Adopt store",
                    &format!("PIN for {}", app.user_name(u)),
                    true,
                    Next::AdoptPin(u),
                )
            }
            _ => again(p, app, "Not a manager"),
        },
        Next::AdoptPin(u) => {
            match app.state.check_override(
                u,
                field.as_bytes(),
                &*app.hasher,
                Permission::Configure,
                now,
            ) {
                Ok(()) => {
                    let key = app.key;
                    let bound = app.store.as_mut().map(|s| s.bind(&key, None));
                    match bound {
                        Some(Ok(())) => {
                            match app.commit(vec![Event::StoreAdopted { by: u, at: now }]) {
                                Ok(_) => {
                                    app.set_msg("Store adopted by this machine", false);
                                    login_prompt(app)
                                }
                                Err(e) => {
                                    app.err(&e);
                                    login_prompt(app)
                                }
                            }
                        }
                        _ => Screen::Fatal("Could not re-bind store".into()),
                    }
                }
                Err(e) => again(p, app, &crate::describe(&e.into())),
            }
        }
        Next::SetupName => {
            if field.trim().is_empty() || field.chars().count() > 24 {
                return again(p, app, "Name: 1-24 characters");
            }
            prompt(
                "First-time setup",
                "Choose a 4-8 digit manager PIN",
                true,
                Next::SetupPin1(field),
            )
        }
        Next::SetupPin1(name) => {
            if !auth::valid_pin(field.as_bytes()) {
                return again(p, app, "PIN must be 4-8 digits");
            }
            prompt(
                "First-time setup",
                "Repeat the PIN",
                true,
                Next::SetupPin2(name, field),
            )
        }
        Next::SetupPin2(name, pin1) => {
            if field != pin1 {
                app.set_msg("PINs differ - start again", true);
                return prompt(
                    "First-time setup",
                    "Choose a 4-8 digit manager PIN",
                    true,
                    Next::SetupPin1(name),
                );
            }
            // Bind a factory-fresh store to this machine with a random id.
            if app.store.as_ref().map(|s| s.trust()) == Some(Trust::Unbound) {
                let Some(id) = random::<16>(hw) else {
                    return Screen::Fatal("No entropy source - cannot initialise".into());
                };
                let key = app.key;
                if let Some(Err(e)) = app.store.as_mut().map(|s| s.bind(&key, Some(id))) {
                    return Screen::Fatal(format!("Cannot bind store: {e:?}"));
                }
            }
            let Some(user) = make_user(app, hw, 1, &name, Role::Manager, &pin1) else {
                return Screen::Fatal("No entropy source".into());
            };
            match app.commit(vec![Event::UserSet {
                by: None,
                at: now,
                user,
            }]) {
                Ok(_) => {
                    app.set_msg("Manager created. Sign in.", false);
                    login_prompt(app)
                }
                Err(e) => {
                    app.err(&e);
                    Screen::Fatal(crate::describe(&e))
                }
            }
        }
        Next::LoginUser => match parse_user(&field) {
            Some(u) if app.state.users.get(&u).is_some_and(|x| x.active) => prompt(
                "Sign in",
                &format!("PIN for {}", app.user_name(u)),
                true,
                Next::LoginPin(u),
            ),
            _ => again(p, app, "Unknown user"),
        },
        Next::LoginPin(u) => match app.state.login(u, field.as_bytes(), &*app.hasher, now) {
            Ok((ev, ok)) => match app.commit(vec![ev]) {
                Ok(_) if ok => {
                    app.user = Some(u);
                    app.message = None;
                    back_to_work(app)
                }
                Ok(_) => {
                    let left = app.state.locks.get(&u).map_or(0, |l| l.failures);
                    app.set_msg(
                        if app.state.is_locked(u, now) {
                            String::from("Wrong PIN - user is now LOCKED")
                        } else {
                            format!("Wrong PIN ({left} failed)")
                        },
                        true,
                    );
                    login_prompt(app)
                }
                Err(e) => {
                    app.err(&e);
                    login_prompt(app)
                }
            },
            Err(e) => {
                app.err(&e.into());
                login_prompt(app)
            }
        },
        Next::Float => {
            let Some(f) =
                parse_money(&field, app.state.config.decimals).filter(|m| !m.is_negative())
            else {
                return again(p, app, "Enter an amount");
            };
            let Some(user) = app.user else {
                return login_prompt(app);
            };
            let ev = Event::ShiftOpened {
                shift_id: app.state.next_shift,
                by: user,
                at: now,
                float: f,
            };
            match app.commit(vec![ev]) {
                Ok(_) => Screen::Sale,
                Err(e) => {
                    app.err(&e);
                    Screen::Prompt(p)
                }
            }
        }
        Next::CardRef(amount) => add_tender(
            app,
            Tender {
                kind: TenderKind::Card,
                amount,
                reference: field.trim().into(),
            },
            hw,
        ),
        Next::VoucherCode => {
            if field.trim().is_empty() {
                return again(p, app, "Voucher code required");
            }
            prompt(
                "Voucher",
                "Voucher value",
                false,
                Next::VoucherAmount(field.trim().into()),
            )
        }
        Next::VoucherAmount(code) => match parse_money(&field, app.state.config.decimals) {
            Some(a) => add_tender(
                app,
                Tender {
                    kind: TenderKind::Voucher,
                    amount: a,
                    reference: code,
                },
                hw,
            ),
            None => again(p, app, "Enter an amount"),
        },
        Next::OverrideUser(pending) => match parse_user(&field) {
            Some(m) if app.state.allows(m, pending.permission()) => prompt(
                "Manager override",
                &format!(
                    "{}: PIN for {}",
                    pending.permission().label(),
                    app.user_name(m)
                ),
                true,
                Next::OverridePin(pending, m),
            ),
            _ => again(p, app, "That user cannot authorise this"),
        },
        Next::OverridePin(pending, m) => {
            match app.state.check_override(
                m,
                field.as_bytes(),
                &*app.hasher,
                pending.permission(),
                now,
            ) {
                Ok(()) => execute(app, pending, m, true, hw),
                Err(e) => {
                    // Failed override PINs count towards the manager's lockout.
                    if let Ok((ev, false)) = app.state.login(m, field.as_bytes(), &*app.hasher, now)
                    {
                        let _ = app.commit(vec![ev]);
                    }
                    app.err(&e.into());
                    back_to_work(app)
                }
            }
        }
        Next::ZCounted => {
            let counted = if field.trim().is_empty() {
                None
            } else {
                match parse_money(&field, app.state.config.decimals) {
                    Some(m) => Some(m),
                    None => return again(p, app, "Enter an amount or leave empty"),
                }
            };
            authorize(app, Pending::ZReport(counted), hw)
        }
        Next::ReprintNo => match field.trim().parse::<u64>() {
            Ok(n) => reprint(app, n, hw),
            Err(_) => again(p, app, "Receipt number"),
        },
        Next::RefundReceipt => match field.trim().parse::<u64>() {
            Ok(n) if app.state.refundable.contains_key(&n) => Screen::RefundPick {
                receipt: n,
                sel: 0,
                picks: Vec::new(),
            },
            _ => again(p, app, "Unknown receipt (or outside the refund window)"),
        },
        Next::RefundQty(receipt, line, mut picks) => match parse_qty(&field) {
            Some(q) => {
                picks.retain(|(l, _)| *l != line);
                picks.push((line, q));
                Screen::RefundPick {
                    receipt,
                    sel: usize::from(line),
                    picks,
                }
            }
            None => again(p, app, "Quantity"),
        },
        Next::RefundReason(receipt, picks) => {
            if field.trim().is_empty() {
                return again(p, app, "A reason is required");
            }
            prompt(
                "Refund payout",
                "1 = cash, 2 = back to card, 3 = voucher",
                false,
                Next::RefundPayout(receipt, picks, field.trim().into()),
            )
        }
        Next::RefundPayout(receipt, picks, reason) => match field.trim() {
            "1" => authorize(
                app,
                Pending::Refund {
                    receipt,
                    picks,
                    reason,
                    kind: TenderKind::Cash,
                    reference: String::new(),
                },
                hw,
            ),
            "2" => prompt(
                "Card refund",
                "Last 4 digits / approval code from the card terminal (e.g. 4242/A1B2C3)",
                false,
                Next::RefundCardRef(receipt, picks, reason),
            ),
            "3" => authorize(
                app,
                Pending::Refund {
                    receipt,
                    picks,
                    reason,
                    kind: TenderKind::Voucher,
                    reference: "REFUND".into(),
                },
                hw,
            ),
            _ => again(p, app, "1, 2 or 3"),
        },
        Next::RefundCardRef(receipt, picks, reason) => {
            if pos_core::tender::validate_card_ref(field.trim()).is_err() {
                return again(p, app, "Format: 1234/APPROVAL");
            }
            authorize(
                app,
                Pending::Refund {
                    receipt,
                    picks,
                    reason,
                    kind: TenderKind::Card,
                    reference: field.trim().into(),
                },
                hw,
            )
        }
        Next::AddUserName(role) => {
            if field.trim().is_empty() || field.chars().count() > 24 {
                return again(p, app, "Name: 1-24 characters");
            }
            prompt(
                "New user",
                "4-8 digit PIN",
                true,
                Next::AddUserPin(role, field),
            )
        }
        Next::AddUserPin(role, name) => {
            if !auth::valid_pin(field.as_bytes()) {
                return again(p, app, "PIN must be 4-8 digits");
            }
            let id = app.state.users.keys().max().copied().unwrap_or(0) + 1;
            let (Some(by), Some(user)) = (app.user, make_user(app, hw, id, &name, role, &field))
            else {
                return back_to_work(app);
            };
            match app.commit(vec![Event::UserSet {
                by: Some(by),
                at: now,
                user,
            }]) {
                Ok(_) => app.set_msg(format!("User {id} ({name}) created"), false),
                Err(e) => app.err(&e),
            }
            back_to_work(app)
        }
        Next::UnlockUser => match (parse_user(&field), app.user) {
            (Some(u), Some(by)) => {
                match app.commit(vec![Event::UserUnlocked {
                    user: u,
                    by,
                    at: now,
                }]) {
                    Ok(_) => app.set_msg(format!("User {u} unlocked"), false),
                    Err(e) => app.err(&e),
                }
                back_to_work(app)
            }
            _ => again(p, app, "User number"),
        },
    }
}

/// Run `pending` if the signed-in user may; otherwise ask for a manager.
fn authorize<D: BlockDevice>(app: &mut App<D>, pending: Pending, hw: &mut Hw<'_>) -> Screen {
    match app.user {
        Some(u) if app.state.allows(u, pending.permission()) => execute(app, pending, u, false, hw),
        Some(_) => prompt(
            "Manager override",
            &format!(
                "{} needs a manager. Manager user number:",
                pending.permission().label()
            ),
            false,
            Next::OverrideUser(pending),
        ),
        None => login_prompt(app),
    }
}

fn execute<D: BlockDevice>(
    app: &mut App<D>,
    pending: Pending,
    by: UserId,
    overridden: bool,
    hw: &mut Hw<'_>,
) -> Screen {
    let now = app.now(hw);
    let Some(cashier) = app.user else {
        return login_prompt(app);
    };
    let mut events = Vec::new();
    if overridden {
        events.push(Event::Override {
            cashier,
            manager: by,
            permission: pending.permission(),
            at: now,
        });
    }
    let commit =
        |app: &mut App<D>, evs: Vec<Event>| -> Result<Vec<pos_store::Committed>, AppError> {
            if evs.is_empty() {
                return Ok(Vec::new());
            }
            app.commit(evs)
        };
    match pending {
        Pending::LineVoid(i) => {
            let Some(line) = app.cart.lines.get(i).cloned() else {
                return Screen::Sale;
            };
            let amount = line.gross(&app.state.config).unwrap_or(Money::ZERO);
            events.push(Event::LineVoided {
                by: cashier,
                authorized_by: by,
                at: now,
                sku: line.sku,
                qty: line.qty,
                amount,
            });
            match commit(app, events) {
                Ok(_) => {
                    let _ = app.cart.remove(i);
                    app.sel = app.cart.lines.len().checked_sub(1);
                    app.set_msg("Line voided", false);
                }
                Err(e) => app.err(&e),
            }
            Screen::Sale
        }
        Pending::VoidSale => {
            let amount = app
                .cart
                .price(&app.state.config)
                .map(|p| p.totals.total)
                .unwrap_or(Money::ZERO);
            let lines = app.cart.lines.len() as u16;
            events.push(Event::SaleVoided {
                by: cashier,
                authorized_by: by,
                at: now,
                lines,
                amount,
            });
            match commit(app, events) {
                Ok(_) => {
                    app.cart = Default::default();
                    app.payment = None;
                    app.sel = None;
                    app.set_msg("Sale voided", false);
                }
                Err(e) => app.err(&e),
            }
            Screen::Sale
        }
        Pending::PriceOverride(i, price) => {
            if let Err(e) = commit(app, events) {
                app.err(&e);
            } else if let Some(l) = app.cart.lines.get_mut(i) {
                l.unit_price = price;
                l.price_override = true;
            }
            Screen::Sale
        }
        Pending::LineDiscount(i, r) => {
            if let Err(e) = commit(app, events) {
                app.err(&e);
            } else if let Some(l) = app.cart.lines.get_mut(i) {
                l.discount = Some(Discount::Percent(r));
            }
            Screen::Sale
        }
        Pending::BasketDiscount(r) => {
            if let Err(e) = commit(app, events) {
                app.err(&e);
            } else {
                app.cart.basket_discount = Some(Discount::Percent(r));
            }
            Screen::Sale
        }
        Pending::Refund {
            receipt,
            picks,
            reason,
            kind,
            reference,
        } => {
            let r = app
                .state
                .build_refund(receipt, &picks, kind, &reference, cashier, by, &reason, now);
            let r = match r {
                Ok(r) => r,
                Err(e) => {
                    app.err(&e.into());
                    return back_to_work(app);
                }
            };
            if let Err(f) = app.policy.validate_refund(&app.state.config, &r) {
                app.err(&AppError::Fiscal(f.0));
                return back_to_work(app);
            }
            events.push(Event::RefundCompleted(r.clone()));
            match commit(app, events) {
                Ok(c) => {
                    let hash = c.last().map(|x| x.hash).unwrap_or([0; 32]);
                    let name = app.user_name(cashier);
                    let bytes = receipt::refund_receipt(
                        &app.state.config,
                        &*app.policy,
                        &r,
                        &name,
                        &hash,
                        false,
                    );
                    app.print(hw, &bytes);
                    if kind == TenderKind::Cash {
                        app.print(hw, &receipt::drawer_kick());
                    }
                    app.set_msg(
                        format!("Refund #{} recorded: {}", r.receipt_no, app.money(r.total)),
                        false,
                    );
                }
                Err(e) => app.err(&e),
            }
            back_to_work(app)
        }
        Pending::NoSale => {
            events.push(Event::NoSale {
                by: cashier,
                at: now,
            });
            match commit(app, events) {
                Ok(_) => app.print(hw, &receipt::drawer_kick()),
                Err(e) => app.err(&e),
            }
            back_to_work(app)
        }
        Pending::XReport | Pending::ZReport(_) => {
            let (kind, counted) = match pending {
                Pending::ZReport(c) => (ReportKind::Z, c),
                _ => (ReportKind::X, None),
            };
            let mut rep = match app.state.build_report(kind, by, now, counted) {
                Ok(r) => r,
                Err(e) => {
                    app.err(&e.into());
                    return back_to_work(app);
                }
            };
            rep.extra = app.policy.report_trailer(&app.state.config, &rep);
            events.push(Event::Report(rep.clone()));
            match commit(app, events) {
                Ok(c) => {
                    let hash = c.last().map(|x| x.hash);
                    let bytes = receipt::report_receipt(
                        &app.state.config,
                        &*app.policy,
                        &rep,
                        hash.as_ref(),
                    );
                    app.print(hw, &bytes);
                    let lines = receipt::report_lines(&app.state.config, &*app.policy, &rep);
                    if kind == ReportKind::Z {
                        app.user = None;
                        app.set_msg(
                            "Z report done; shift closed. Export to USB from the menu.",
                            false,
                        );
                    }
                    Screen::Info {
                        title: lines[0].clone(),
                        lines: lines[1..].to_vec(),
                    }
                }
                Err(e) => {
                    app.err(&e);
                    back_to_work(app)
                }
            }
        }
        Pending::Export => {
            if !events.is_empty()
                && let Err(e) = app.commit(events)
            {
                app.err(&e);
                return back_to_work(app);
            }
            match crate::export::export(app, hw, by, now) {
                Ok(summary) => app.set_msg(summary, false),
                Err(e) => app.set_msg(e, true),
            }
            back_to_work(app)
        }
        Pending::ImportCatalog => {
            match crate::export::import_catalog(app, hw, by, now, events) {
                Ok(n) => app.set_msg(format!("Catalog imported: {n} items"), false),
                Err(e) => app.set_msg(e, true),
            }
            back_to_work(app)
        }
        Pending::ImportSettings => {
            match crate::export::import_settings(app, hw, by, now, events) {
                Ok(()) => app.set_msg("Settings imported", false),
                Err(e) => app.set_msg(e, true),
            }
            back_to_work(app)
        }
        Pending::AddUser(role) => {
            if !events.is_empty()
                && let Err(e) = app.commit(events)
            {
                app.err(&e);
                return back_to_work(app);
            }
            prompt("New user", "Name", false, Next::AddUserName(role))
        }
        Pending::Unlock => {
            if !events.is_empty()
                && let Err(e) = app.commit(events)
            {
                app.err(&e);
                return back_to_work(app);
            }
            prompt(
                "Unlock user",
                "User number to unlock",
                false,
                Next::UnlockUser,
            )
        }
    }
}

fn reprint<D: BlockDevice>(app: &mut App<D>, n: u64, hw: &mut Hw<'_>) -> Screen {
    let now = app.now(hw);
    let Some(user) = app.user else {
        return login_prompt(app);
    };
    let found = app.recent.iter().rev().find(|j| match j {
        Journaled::Sale(s, _) => s.receipt_no == n,
        Journaled::Refund(r, _) => r.receipt_no == n,
    });
    let found = found
        .cloned()
        .or_else(|| crate::export::find_receipt(app, n));
    let Some(j) = found else {
        app.set_msg("Receipt not available for reprint", true);
        return back_to_work(app);
    };
    if let Err(e) = app.commit(vec![Event::Reprint {
        by: user,
        receipt_no: n,
        at: now,
    }]) {
        app.err(&e);
        return back_to_work(app);
    }
    let bytes = match &j {
        Journaled::Sale(s, h) => {
            let name = app.user_name(s.cashier);
            receipt::sale_receipt(&app.state.config, &*app.policy, s, &name, h, true)
        }
        Journaled::Refund(r, h) => {
            let name = app.user_name(r.cashier);
            receipt::refund_receipt(&app.state.config, &*app.policy, r, &name, h, true)
        }
    };
    app.print(hw, &bytes);
    app.set_msg(format!("Receipt #{n} reprinted (COPY)"), false);
    back_to_work(app)
}

// ------------------------------------------------------------------ sale

fn add_code<D: BlockDevice>(app: &mut App<D>, code: &str) {
    let cfg = &app.state.config;
    let scan = match barcode::classify(code, cfg.price_embed.as_ref()) {
        Ok(s) => s,
        Err(_) => Scan::Code(code.trim().into()),
    };
    let qty = app.pending_qty.take();
    let line = match scan {
        Scan::Embedded { plu, kind, value } => match app.state.catalog.by_plu(&plu) {
            Some(item) => {
                let mut l = Line::from_item(item, Qty::ONE);
                match kind {
                    EmbedKind::Price => l.fixed_amount = Some(Money(i64::from(value))),
                    EmbedKind::Weight => l.qty = Qty(i64::from(value)),
                }
                Some(l)
            }
            None => None,
        },
        Scan::Code(c) => {
            let item = app
                .state
                .catalog
                .by_barcode(&c)
                .or_else(|| app.state.catalog.get(&c))
                .or_else(|| app.state.catalog.get(&c.to_ascii_uppercase()));
            match item {
                Some(it) if it.weighable && qty.is_none() => {
                    app.set_msg(format!("{}: enter weight (kg) then * first", it.name), true);
                    return;
                }
                Some(it) => Some(Line::from_item(it, qty.unwrap_or(Qty::ONE))),
                None => None,
            }
        }
    };
    match line {
        Some(l) => {
            let name = l.name.clone();
            match app.cart.add(l) {
                Ok(i) => {
                    app.sel = Some(i);
                    app.message = Some((format!("+ {name}"), false));
                }
                Err(e) => app.set_msg(format!("{e:?}"), true),
            }
        }
        None => app.set_msg(format!("Not found: {code}"), true),
    }
}

fn start_payment<D: BlockDevice>(app: &mut App<D>) {
    if app.cart.is_empty() {
        return;
    }
    match app.cart.price(&app.state.config) {
        Ok(p) => {
            app.payment = Some(Payment::new(p.totals.total));
            app.entry.clear();
            app.screen = Screen::Pay;
        }
        Err(e) => app.set_msg(format!("{e:?}"), true),
    }
}

fn sale_key<D: BlockDevice>(app: &mut App<D>, key: Key, at: u64, hw: &mut Hw<'_>) {
    match key {
        Key::Char('*') => {
            app.scan.other_key();
            match parse_qty(&app.entry) {
                Some(q) => {
                    app.pending_qty = Some(q);
                    app.entry.clear();
                }
                None => app.set_msg("Type a quantity, then *", true),
            }
        }
        Key::Char(c) => {
            app.scan.char(c, at);
            if app.entry.chars().count() < 40 {
                app.entry.push(c);
            }
        }
        Key::Enter => match app.scan.enter(at) {
            Enter::Scan(code) => {
                // The burst also landed in the entry line; take it out again.
                let keep = app
                    .entry
                    .chars()
                    .count()
                    .saturating_sub(code.chars().count());
                app.entry = app.entry.chars().take(keep).collect();
                add_code(app, &code);
            }
            Enter::Typed => {
                let e = core::mem::take(&mut app.entry);
                if e.trim().is_empty() {
                    start_payment(app);
                } else {
                    add_code(app, &e);
                }
            }
        },
        Key::Backspace => {
            app.scan.other_key();
            app.entry.pop();
        }
        Key::Escape => {
            app.scan.reset();
            app.entry.clear();
            app.pending_qty = None;
            app.message = None;
        }
        Key::Up => {
            app.scan.other_key();
            app.sel = app.sel.map(|s| s.saturating_sub(1));
        }
        Key::Down => {
            app.scan.other_key();
            let n = app.cart.lines.len();
            app.sel = app.sel.map(|s| (s + 1).min(n.saturating_sub(1)));
        }
        Key::F(1) => start_payment(app),
        Key::F(2) => {
            app.screen = Screen::Search {
                query: core::mem::take(&mut app.entry),
                sel: 0,
            };
        }
        Key::F(3) => {
            let q = parse_qty(&app.entry);
            match (q, app.sel) {
                (Some(q), Some(i)) => match app.cart.set_qty(i, q) {
                    Ok(()) => app.entry.clear(),
                    Err(e) => app.set_msg(format!("{e:?}"), true),
                },
                _ => app.set_msg("Select a line, type the quantity, F3", true),
            }
        }
        Key::F(4) => {
            if let Some(i) = app.sel.filter(|&i| i < app.cart.lines.len()) {
                app.screen = authorize(app, Pending::LineVoid(i), hw);
            }
        }
        Key::F(5) => match (parse_rate(&app.entry), app.sel) {
            (Some(r), Some(i)) if i < app.cart.lines.len() => {
                app.entry.clear();
                app.screen = authorize(app, Pending::LineDiscount(i, r), hw);
            }
            _ => app.set_msg("Select a line, type percent, F5", true),
        },
        Key::F(6) => match parse_rate(&app.entry) {
            Some(r) if !app.cart.is_empty() => {
                app.entry.clear();
                app.screen = authorize(app, Pending::BasketDiscount(r), hw);
            }
            _ => app.set_msg("Type percent, F6", true),
        },
        Key::F(7) => match (parse_money(&app.entry, app.state.config.decimals), app.sel) {
            (Some(m), Some(i)) if !m.is_negative() && i < app.cart.lines.len() => {
                app.entry.clear();
                app.screen = authorize(app, Pending::PriceOverride(i, m), hw);
            }
            _ => app.set_msg("Select a line, type new price, F7", true),
        },
        Key::F(8) => {
            if !app.cart.is_empty() {
                app.screen = authorize(app, Pending::VoidSale, hw);
            }
        }
        Key::F(9) => {
            app.screen = prompt(
                "Refund",
                "Original receipt number",
                false,
                Next::RefundReceipt,
            );
        }
        Key::F(10) => app.screen = Screen::Menu { sel: 0 },
        Key::F(11) => {
            if app.cart.is_empty() {
                app.screen = authorize(app, Pending::NoSale, hw);
            }
        }
        Key::F(12) => {
            if app.cart.is_empty() {
                if let Some(u) = app.user.take() {
                    let now = app.now(hw);
                    let _ = app.commit(vec![Event::Logout { user: u, at: now }]);
                }
                app.screen = login_prompt(app);
            } else {
                app.set_msg("Finish or void the sale before signing out", true);
            }
        }
        _ => app.scan.other_key(),
    }
}

fn search_key<D: BlockDevice>(app: &mut App<D>, mut query: String, sel: usize, key: Key) -> Screen {
    let n = app.state.catalog.search(&query, 50).len();
    match key {
        Key::Char(c) => {
            query.push(c);
            Screen::Search { query, sel: 0 }
        }
        Key::Backspace => {
            query.pop();
            Screen::Search { query, sel: 0 }
        }
        Key::Up => Screen::Search {
            query,
            sel: sel.saturating_sub(1),
        },
        Key::Down => Screen::Search {
            query,
            sel: (sel + 1).min(n.saturating_sub(1)),
        },
        Key::Enter => {
            let sku = app
                .state
                .catalog
                .search(&query, 50)
                .get(sel)
                .map(|i| i.sku.clone());
            if let Some(sku) = sku {
                add_code(app, &sku);
            }
            Screen::Sale
        }
        Key::Escape => Screen::Sale,
        _ => Screen::Search { query, sel },
    }
}

fn add_tender<D: BlockDevice>(app: &mut App<D>, t: Tender, hw: &mut Hw<'_>) -> Screen {
    let Some(mut pay) = app.payment.take() else {
        return Screen::Sale;
    };
    if let Err(e) = pay.add(t, &app.state.config) {
        app.payment = Some(pay);
        app.err(&AppError::Domain(e.into()));
        return Screen::Pay;
    }
    if pay.is_complete() {
        finish_sale(app, pay, hw)
    } else {
        app.payment = Some(pay);
        Screen::Pay
    }
}

fn finish_sale<D: BlockDevice>(app: &mut App<D>, pay: Payment, hw: &mut Hw<'_>) -> Screen {
    let now = app.now(hw);
    let Some(user) = app.user else {
        return login_prompt(app);
    };
    let sale = match app.state.build_sale(&app.cart, &pay, user, now) {
        Ok(s) => s,
        Err(e) => {
            app.payment = Some(pay);
            app.err(&e.into());
            return Screen::Pay;
        }
    };
    if let Err(f) = app.policy.validate_sale(&app.state.config, &sale) {
        app.payment = Some(pay);
        app.err(&AppError::Fiscal(f.0));
        return Screen::Pay;
    }
    let committed = match app.commit(vec![Event::SaleCompleted(sale.clone())]) {
        Ok(c) => c,
        Err(e) => {
            // Not committed: nothing printed, nothing confirmed.
            app.payment = Some(pay);
            app.err(&e);
            return Screen::Pay;
        }
    };
    // Durable: now confirm, print and open the drawer.
    let hash = committed[0].hash;
    let name = app.user_name(user);
    let bytes = receipt::sale_receipt(&app.state.config, &*app.policy, &sale, &name, &hash, false);
    app.print(hw, &bytes);
    if sale.tenders.iter().any(|t| t.kind == TenderKind::Cash) {
        app.print(hw, &receipt::drawer_kick());
    }
    app.cart = Default::default();
    app.sel = None;
    app.entry.clear();
    Screen::Done {
        change: sale.change,
        receipt: sale.receipt_no,
    }
}

fn pay_key<D: BlockDevice>(app: &mut App<D>, key: Key, hw: &mut Hw<'_>) {
    let Some(pay) = app.payment.clone() else {
        app.screen = Screen::Sale;
        return;
    };
    let dec = app.state.config.decimals;
    let amount = || parse_money(&app.entry, dec);
    match key {
        Key::Char(c) if c.is_ascii_digit() || c == '.' => app.entry.push(c),
        Key::Backspace => {
            app.entry.pop();
        }
        Key::Enter | Key::F(1) => {
            let due = pay.cash_due(&app.state.config).unwrap_or(Money::ZERO);
            let a = amount().unwrap_or(due);
            app.entry.clear();
            if a.is_zero() && due.is_zero() {
                let s = finish_sale(app, pay, hw);
                app.screen = s;
                return;
            }
            app.screen = add_tender(
                app,
                Tender {
                    kind: TenderKind::Cash,
                    amount: a,
                    reference: String::new(),
                },
                hw,
            );
        }
        Key::F(2) => {
            let a = amount().unwrap_or(pay.remaining().unwrap_or(Money::ZERO));
            app.entry.clear();
            app.screen = prompt(
                "Card (record only)",
                &format!(
                    "Amount {} - last 4 digits / approval code, e.g. 4242/A1B2C3",
                    app.money(a)
                ),
                false,
                Next::CardRef(a),
            );
        }
        Key::F(3) => {
            app.entry.clear();
            app.screen = prompt("Voucher", "Voucher code", false, Next::VoucherCode);
        }
        Key::F(8) => app.screen = authorize(app, Pending::VoidSale, hw),
        Key::Escape => {
            if pay.tenders.is_empty() {
                app.payment = None;
                app.entry.clear();
                app.screen = Screen::Sale;
            } else {
                app.set_msg("Tenders recorded: complete payment or void (F8)", true);
            }
        }
        _ => {}
    }
}

// ------------------------------------------------------------------ menu

const MENU: &[&str] = &[
    "X report",
    "Z report (close day)",
    "Reprint last receipt",
    "Reprint receipt by number",
    "Refund",
    "No sale (open drawer)",
    "Export to USB (journal, reports, catalog)",
    "Import catalog from USB (CATALOG.CSV)",
    "Import settings from USB (SETTINGS.TXT)",
    "Add cashier",
    "Add manager",
    "Unlock user",
    "Journal status",
    "Back",
];

fn menu_key<D: BlockDevice>(app: &mut App<D>, sel: usize, key: Key, hw: &mut Hw<'_>) -> Screen {
    match key {
        Key::Up => Screen::Menu {
            sel: sel.saturating_sub(1),
        },
        Key::Down => Screen::Menu {
            sel: (sel + 1).min(MENU.len() - 1),
        },
        Key::Escape => back_to_work(app),
        Key::Char(c) if c.is_ascii_digit() => {
            let i = (c as u8 - b'0') as usize;
            let i = if i == 0 { 9 } else { i - 1 };
            menu_select(app, i.min(MENU.len() - 1), hw)
        }
        Key::Enter => menu_select(app, sel, hw),
        _ => Screen::Menu { sel },
    }
}

fn menu_select<D: BlockDevice>(app: &mut App<D>, i: usize, hw: &mut Hw<'_>) -> Screen {
    if app.state.shift.is_none() && matches!(i, 0 | 1 | 4 | 5) {
        app.set_msg("No shift open", true);
        return Screen::Menu { sel: i };
    }
    match i {
        0 => authorize(app, Pending::XReport, hw),
        1 => prompt(
            "Z report",
            "Counted cash in drawer (or empty to skip)",
            false,
            Next::ZCounted,
        ),
        2 => {
            let last = app.state.next_receipt.saturating_sub(1);
            if last == 0 {
                app.set_msg("No receipts yet", true);
                return Screen::Menu { sel: i };
            }
            reprint(app, last, hw)
        }
        3 => prompt("Reprint", "Receipt number", false, Next::ReprintNo),
        4 => prompt(
            "Refund",
            "Original receipt number",
            false,
            Next::RefundReceipt,
        ),
        5 => authorize(app, Pending::NoSale, hw),
        6 => authorize(app, Pending::Export, hw),
        7 => authorize(app, Pending::ImportCatalog, hw),
        8 => authorize(app, Pending::ImportSettings, hw),
        9 => authorize(app, Pending::AddUser(Role::Cashier), hw),
        10 => authorize(app, Pending::AddUser(Role::Manager), hw),
        11 => authorize(app, Pending::Unlock, hw),
        12 => {
            let (used, total) = app.store.as_ref().map(|s| s.usage()).unwrap_or((0, 1));
            let st = &app.state;
            Screen::Info {
                title: "Journal status".into(),
                lines: vec![
                    format!("Records applied: {}", st.events),
                    format!("Next receipt: {}   Next Z: {}", st.next_receipt, st.next_z),
                    format!(
                        "Log usage: {} / {} blocks ({}%)",
                        used,
                        total,
                        used * 100 / total.max(1)
                    ),
                    format!(
                        "Head hash: {}",
                        receipt::hex(
                            &app.store.as_ref().map(|s| s.head_hash()).unwrap_or([0; 32])[..8]
                        )
                    ),
                    format!("Catalog items: {}", st.catalog.len()),
                ],
            }
        }
        _ => back_to_work(app),
    }
}

fn refund_pick_key<D: BlockDevice>(
    app: &mut App<D>,
    receipt: u64,
    sel: usize,
    picks: Vec<(u16, Qty)>,
    key: Key,
) -> Screen {
    let n = app
        .state
        .refundable
        .get(&receipt)
        .map_or(0, |r| r.lines.len());
    match key {
        Key::Up => Screen::RefundPick {
            receipt,
            sel: sel.saturating_sub(1),
            picks,
        },
        Key::Down => Screen::RefundPick {
            receipt,
            sel: (sel + 1).min(n.saturating_sub(1)),
            picks,
        },
        Key::Enter => prompt(
            "Refund quantity",
            "Quantity to refund for the selected line",
            false,
            Next::RefundQty(receipt, sel as u16, picks),
        ),
        Key::F(1) if !picks.is_empty() => prompt(
            "Refund reason",
            "Reason (required)",
            false,
            Next::RefundReason(receipt, picks),
        ),
        Key::Escape => back_to_work(app),
        _ => Screen::RefundPick {
            receipt,
            sel,
            picks,
        },
    }
}

// ------------------------------------------------------------------ drawing

/// Clickable actions are mapped to keys.
pub(crate) fn on_click<D: BlockDevice>(app: &mut App<D>, hw: &mut Hw<'_>) {
    let now = app.now(hw);
    let click = app.click;
    if let Some(k) = draw(app, click, now) {
        let at = app.now_us;
        match k {
            UiAction::Key(key) => on_key(app, key, Modifiers::default(), at, hw),
            UiAction::Code(code) => {
                if matches!(app.screen, Screen::Sale) {
                    add_code(app, &code);
                }
            }
            UiAction::Select(i) => app.sel = Some(i),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) enum UiAction {
    Key(Key),
    Code(String),
    Select(usize),
}

pub(crate) fn draw<D: BlockDevice>(
    app: &mut App<D>,
    click: Option<(i32, i32)>,
    now: i64,
) -> Option<UiAction> {
    let mut action = None;
    let status = status_line(app, now);
    let message = app.message.clone();
    let screen = app.screen.clone();
    let cfg = app.state.config.clone();
    let priced = app.cart.price(&cfg).ok();
    let quick: Vec<(String, String)> = app
        .state
        .catalog
        .quick_keys()
        .into_iter()
        .take(12)
        .map(|i| (i.sku.clone(), i.name.clone()))
        .collect();
    let money = |m: Money| format!("{}", m.display(cfg.decimals));
    let search: Vec<(String, String, String)> = match &screen {
        Screen::Search { query, .. } => app
            .state
            .catalog
            .search(query, 50)
            .into_iter()
            .map(|i| (i.sku.clone(), i.name.clone(), money(i.price)))
            .collect(),
        _ => Vec::new(),
    };
    let refund_lines: Vec<String> = match &screen {
        Screen::RefundPick { receipt, picks, .. } => app
            .state
            .refundable
            .get(receipt)
            .map(|r| {
                r.lines
                    .iter()
                    .enumerate()
                    .map(|(i, l)| {
                        let pick = picks
                            .iter()
                            .find(|(x, _)| usize::from(*x) == i)
                            .map(|(_, q)| format!(" -> refund {q}"))
                            .unwrap_or_default();
                        format!(
                            "{:<20} sold {} refunded {} {}{}",
                            l.name,
                            l.qty,
                            l.refunded_qty,
                            money(l.total),
                            pick
                        )
                    })
                    .collect()
            })
            .unwrap_or_default(),
        _ => Vec::new(),
    };
    let payment = app.payment.clone();
    let cart = app.cart.clone();
    let sel = app.sel;
    let entry = app.entry.clone();
    let pending_qty = app.pending_qty;

    let mut ui = Ui::new(&mut app.canvas, click);
    ui.clear();
    let u = ui.u();
    let full = ui.c.bounds();
    let (top, rest) = full.split_top(3 * u + 4);
    let (body, bottom) = rest.split_bottom(4 * u);
    ui.panel(top);
    ui.label(top.inset(u), &status, Font::Normal, 0);
    if let Some((m, err)) = &message {
        let col = if *err { ui.theme.danger } else { ui.theme.ok };
        ui.c.fill_rect(bottom, col);
        ui.label_color(bottom.inset(u), m, Font::Normal, 0, ui.theme.button_text);
    }
    let body = body.inset(u);

    match &screen {
        Screen::Blank => {}
        Screen::Fatal(m) => {
            ui.c.fill_rect(body, ui.theme.danger);
            ui.label(body.split_top(6 * u).0, "FATAL", Font::Large, 1);
            ui.label(body, m, Font::Normal, 1);
        }
        Screen::Prompt(p) => {
            let (t, r) = body.split_top(8 * u);
            ui.label(t, &p.title, Font::Large, 1);
            let (h, r) = r.split_top(5 * u);
            ui.label_color(h, &p.hint, Font::Normal, 1, ui.theme.muted);
            let (f, r) = r.split_top(7 * u);
            let fr = Rect::new(f.x + f.w / 4, f.y, f.w / 2, f.h);
            ui.panel(fr);
            let shown: String = if p.secret {
                p.field.chars().map(|_| '*').collect()
            } else {
                p.field.clone()
            };
            ui.label(fr.inset(u), &format!("{shown}_"), Font::Large, 1);
            // On-screen keypad for touch use.
            let pad = Rect::new(r.x + r.w / 3, r.y + u, r.w / 3, r.h - u);
            let keys = ["7", "8", "9", "4", "5", "6", "1", "2", "3", "<", "0", "OK"];
            for (k, cell) in keys.iter().zip(pad.grid(3, 4, u)) {
                let style = match *k {
                    "OK" => ButtonStyle::Primary,
                    "<" => ButtonStyle::Danger,
                    _ => ButtonStyle::Normal,
                };
                if ui.button(cell, k, style) {
                    action = Some(UiAction::Key(match *k {
                        "OK" => Key::Enter,
                        "<" => Key::Backspace,
                        d => Key::Char(d.chars().next().unwrap_or('0')),
                    }));
                }
            }
        }
        Screen::Sale | Screen::Pay | Screen::Done { .. } => {
            let (left, right) = body.split_left(body.w * 3 / 5);
            let left = Rect::new(left.x, left.y, left.w - u, left.h);
            // Cart lines.
            ui.panel(left);
            let (list, entry_r) = left.split_bottom(6 * u);
            let row_h = ui.c.line_height(Font::Normal) + u;
            let rows = (list.h / row_h).max(1) as usize;
            let first = sel.unwrap_or(0).saturating_sub(rows.saturating_sub(1));
            if let Some(p) = &priced {
                for (k, (l, pl)) in cart
                    .lines
                    .iter()
                    .zip(&p.lines)
                    .enumerate()
                    .skip(first)
                    .take(rows)
                {
                    let r = Rect::new(list.x, list.y + (k - first) as i32 * row_h, list.w, row_h);
                    let mut name = l.name.clone();
                    if l.discount.is_some() {
                        name.push_str(" (disc)");
                    }
                    if l.price_override {
                        name.push_str(" (ovr)");
                    }
                    let cols = [
                        (name.as_str(), list.w * 6 / 10, 0u8),
                        (&*format!("{}", l.qty), list.w * 15 / 100, 2),
                        (&*money(pl.total), list.w * 25 / 100, 2),
                    ];
                    if ui.row(r, &cols, sel == Some(k)) {
                        action = Some(UiAction::Select(k));
                    }
                }
            }
            ui.c.fill_rect(entry_r, ui.theme.bg);
            let q = pending_qty.map(|q| format!("{q} x ")).unwrap_or_default();
            ui.label(entry_r.inset(u), &format!("> {q}{entry}_"), Font::Large, 0);

            // Totals.
            let (tot, rest) = right.split_top(14 * u);
            ui.panel(tot);
            if let Some(p) = &priced {
                let t = &p.totals;
                let (a, b) = tot.inset(u).split_top(4 * u);
                ui.label(
                    a,
                    &format!("Items {}   Tax {}", cart.lines.len(), money(t.tax)),
                    Font::Normal,
                    0,
                );
                if !t.discount.is_zero() {
                    ui.label(a, &format!("Disc -{}", money(t.discount)), Font::Normal, 2);
                }
                ui.label(
                    b,
                    &format!("{} {}", cfg.currency, money(t.total)),
                    Font::Large,
                    2,
                );
            }
            let rest = Rect::new(rest.x, rest.y + u, rest.w, rest.h - u);
            match &screen {
                Screen::Pay => {
                    if let Some(pay) = &payment {
                        let (info, keys) = rest.split_top(16 * u);
                        ui.panel(info);
                        let mut y = info.y + u;
                        let lh = ui.c.line_height(Font::Normal) + 4;
                        let due = pay.cash_due(&cfg).unwrap_or(Money::ZERO);
                        let mut lines = vec![
                            format!(
                                "Remaining: {}",
                                money(pay.remaining().unwrap_or(Money::ZERO))
                            ),
                            format!("Cash due (rounded): {}", money(due)),
                        ];
                        for t in &pay.tenders {
                            lines.push(format!(
                                "{} {} {}",
                                t.kind.label(),
                                money(t.amount),
                                t.reference
                            ));
                        }
                        for l in lines {
                            ui.label(
                                Rect::new(info.x + u, y, info.w - 2 * u, lh),
                                &l,
                                Font::Normal,
                                0,
                            );
                            y += lh;
                        }
                        let btns = [
                            ("Cash F1", Key::F(1)),
                            ("Card F2", Key::F(2)),
                            ("Voucher F3", Key::F(3)),
                            ("Back Esc", Key::Escape),
                        ];
                        for ((label, k), cell) in btns.iter().zip(keys.grid(2, 2, u)) {
                            if ui.button(cell, label, ButtonStyle::Primary) {
                                action = Some(UiAction::Key(*k));
                            }
                        }
                    }
                }
                Screen::Done { change, receipt } => {
                    ui.c.fill_rect(rest, ui.theme.ok);
                    let (a, b) = rest.split_top(rest.h / 2);
                    ui.label_color(
                        a,
                        &format!("Receipt #{receipt} saved"),
                        Font::Normal,
                        1,
                        ui.theme.button_text,
                    );
                    ui.label_color(
                        b,
                        &format!("CHANGE {}", money(*change)),
                        Font::Large,
                        1,
                        ui.theme.button_text,
                    );
                }
                _ => {
                    let (qk, fk) = rest.split_top(rest.h / 2);
                    for ((sku, name), cell) in quick.iter().zip(qk.grid(4, 3, u / 2)) {
                        if ui.button(cell, name, ButtonStyle::Normal) {
                            action = Some(UiAction::Code(sku.clone()));
                        }
                    }
                    let fns = [
                        ("F1 Pay", ButtonStyle::Ok),
                        ("F2 Find", ButtonStyle::Normal),
                        ("F3 Qty", ButtonStyle::Normal),
                        ("F4 Void ln", ButtonStyle::Danger),
                        ("F5 Disc%", ButtonStyle::Normal),
                        ("F6 Bskt%", ButtonStyle::Normal),
                        ("F7 Price", ButtonStyle::Normal),
                        ("F8 Void", ButtonStyle::Danger),
                        ("F9 Refund", ButtonStyle::Normal),
                        ("F10 Menu", ButtonStyle::Primary),
                        ("F11 Drawer", ButtonStyle::Normal),
                        ("F12 Exit", ButtonStyle::Normal),
                    ];
                    for (n, ((label, style), cell)) in
                        fns.iter().zip(fk.grid(4, 3, u / 2)).enumerate()
                    {
                        if ui.button(cell, label, *style) {
                            action = Some(UiAction::Key(Key::F(n as u8 + 1)));
                        }
                    }
                }
            }
        }
        Screen::Search { query, sel } => {
            let (t, list) = body.split_top(6 * u);
            ui.label(t, &format!("Search: {query}_"), Font::Large, 0);
            let row_h = ui.c.line_height(Font::Normal) + u;
            for (k, (sku, name, price)) in search.iter().enumerate().take((list.h / row_h) as usize)
            {
                let r = Rect::new(list.x, list.y + k as i32 * row_h, list.w, row_h);
                let cols = [
                    (sku.as_str(), list.w / 4, 0u8),
                    (name.as_str(), list.w / 2, 0),
                    (price.as_str(), list.w / 4, 2),
                ];
                if ui.row(r, &cols, k == *sel) {
                    action = Some(UiAction::Code(sku.clone()));
                }
            }
        }
        Screen::Menu { sel } => {
            let (t, list) = body.split_top(6 * u);
            ui.label(t, "Manager menu", Font::Large, 1);
            let row_h = ui.c.line_height(Font::Normal) + u;
            let list = Rect::new(list.x + list.w / 4, list.y, list.w / 2, list.h);
            for (k, item) in MENU.iter().enumerate() {
                let r = Rect::new(list.x, list.y + k as i32 * row_h, list.w, row_h);
                let label = format!(
                    "{}. {item}",
                    if k < 9 {
                        (k + 1).to_string()
                    } else if k == 9 {
                        "0".into()
                    } else {
                        " ".into()
                    }
                );
                if ui.row(r, &[(label.as_str(), list.w, 0)], k == *sel) {
                    action = Some(UiAction::Key(Key::Char(char::from(
                        b'0' + ((k + 1) % 10) as u8,
                    ))));
                }
            }
        }
        Screen::Info { title, lines } => {
            let (t, list) = body.split_top(6 * u);
            ui.label(t, title, Font::Large, 1);
            let lh = ui.c.line_height(Font::Normal) + 2;
            let list = Rect::new(list.x + list.w / 5, list.y, list.w * 3 / 5, list.h);
            for (k, l) in lines.iter().enumerate() {
                ui.label(
                    Rect::new(list.x, list.y + k as i32 * lh, list.w, lh),
                    l,
                    Font::Normal,
                    0,
                );
            }
            ui.label_color(
                Rect::new(list.x, list.y + list.h - lh, list.w, lh),
                "Enter to continue",
                Font::Normal,
                1,
                ui.theme.muted,
            );
        }
        Screen::RefundPick { receipt, sel, .. } => {
            let (t, list) = body.split_top(6 * u);
            ui.label(
                t,
                &format!("Refund from receipt #{receipt}: Enter = set qty, F1 = continue"),
                Font::Normal,
                1,
            );
            let row_h = ui.c.line_height(Font::Normal) + u;
            for (k, l) in refund_lines.iter().enumerate() {
                let r = Rect::new(list.x, list.y + k as i32 * row_h, list.w, row_h);
                if ui.row(r, &[(l.as_str(), list.w, 0)], k == *sel) {
                    action = Some(UiAction::Key(Key::Enter));
                }
            }
        }
    }
    action
}

fn status_line<D: BlockDevice>(app: &App<D>, now: i64) -> String {
    let user = app
        .user
        .map(|u| app.user_name(u))
        .unwrap_or_else(|| "-".into());
    let shift = app
        .state
        .shift
        .as_ref()
        .map(|s| format!("Shift {}", s.id))
        .unwrap_or_else(|| "No shift".into());
    let (used, total) = app.store.as_ref().map(|s| s.usage()).unwrap_or((0, 1));
    let pct = used * 100 / total.max(1);
    let printer = if app.printer_ok { "" } else { "  PRINTER!" };
    let full = if pct >= 80 { "  EXPORT NEEDED" } else { "" };
    format!(
        "{}  |  {}  |  {}  |  {}  |  Journal {}%{}{}",
        app.state.config.store_name,
        user,
        shift,
        receipt::fmt_time(now),
        pct,
        full,
        printer
    )
}
