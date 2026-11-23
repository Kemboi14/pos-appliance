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
use pos_ui::{ButtonStyle, Font, PointerFrame, Rect, Ui};

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
    /// On-screen keyboard shift state (text prompts only).
    pub shift: Shift,
}

/// On-screen keyboard shift: off, next letter only, or caps lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shift {
    Off,
    Once,
    Lock,
}

/// What a prompt's field holds, which decides the on-screen keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Input {
    /// Whole numbers: user numbers, PINs, receipt numbers, menu choices.
    Digits,
    /// Amounts and quantities (digits and a decimal point).
    Amount,
    /// Free text: names, codes, reasons.
    Text,
}

impl Next {
    pub(crate) fn input(&self) -> Input {
        match self {
            Next::FormatConfirm
            | Next::SetupName
            | Next::CardRef(_)
            | Next::VoucherCode
            | Next::RefundReason(..)
            | Next::RefundCardRef(..)
            | Next::AddUserName(_) => Input::Text,
            Next::Float | Next::VoucherAmount(_) | Next::ZCounted | Next::RefundQty(..) => {
                Input::Amount
            }
            Next::AdoptUser
            | Next::AdoptPin(_)
            | Next::SetupPin1(_)
            | Next::SetupPin2(..)
            | Next::AddUserPin(..)
            | Next::LoginUser
            | Next::LoginPin(_)
            | Next::OverrideUser(_)
            | Next::OverridePin(..)
            | Next::ReprintNo
            | Next::RefundReceipt
            | Next::RefundPayout(..)
            | Next::UnlockUser => Input::Digits,
        }
    }

    /// Does Escape leave this prompt (rather than just clear the field)?
    pub(crate) fn cancellable(&self) -> bool {
        !matches!(
            self,
            Next::FormatConfirm
                | Next::AdoptUser
                | Next::AdoptPin(_)
                | Next::SetupName
                | Next::SetupPin1(_)
                | Next::SetupPin2(..)
                | Next::LoginUser
        )
    }

    fn initial_shift(&self) -> Shift {
        match self {
            // Codes and the ERASE confirmation are upper case.
            Next::FormatConfirm
            | Next::CardRef(_)
            | Next::VoucherCode
            | Next::RefundCardRef(..) => Shift::Lock,
            // Names and reasons start with a capital.
            Next::SetupName | Next::AddUserName(_) | Next::RefundReason(..) => Shift::Once,
            _ => Shift::Off,
        }
    }
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
        shift: next.initial_shift(),
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
        &format!(
            "Tap your name, or type your user number ({})",
            users.join(", ")
        ),
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
            // Any key starts the next sale; anything but Enter/Escape (e.g.
            // the first character of a scan) is then handled as a sale key.
            app.message = None;
            if matches!(key, Key::Enter | Key::Escape) {
                Screen::Sale
            } else {
                sale_key(app, key, at, hw);
                if matches!(app.screen, Screen::Blank) {
                    Screen::Sale
                } else {
                    core::mem::replace(&mut app.screen, Screen::Blank)
                }
            }
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
            if c.is_alphabetic() && p.shift == Shift::Once {
                p.shift = Shift::Off;
            }
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
        Key::PageUp => {
            app.scan.other_key();
            app.qk_page = app.qk_page.saturating_sub(1);
        }
        Key::PageDown => {
            app.scan.other_key();
            app.qk_page = (app.qk_page + 1).min(quick_pages(app).saturating_sub(1));
        }
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
    app.last_sale = Some(sale.clone());
    // Drop the last "+ item" notice, but keep warnings (e.g. a printer error).
    if matches!(app.message, Some((_, false))) {
        app.message = None;
    }
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

// ------------------------------------------------------------------ touch

/// Quick keys per page: positions `p*12 .. p*12+11` of the catalog's
/// `quick_key` column fill page `p`, in a 4×3 grid, so the catalog decides
/// the layout and it is the same on every screen size.
pub(crate) const QK_PER_PAGE: usize = 12;

pub(crate) fn quick_pages<D: BlockDevice>(app: &App<D>) -> usize {
    app.state
        .catalog
        .quick_keys()
        .iter()
        .filter_map(|i| i.quick_key)
        .max()
        .map_or(1, |m| usize::from(m) / QK_PER_PAGE + 1)
}

/// Tappable actions. Most map onto keys, so behaviour stays in `on_key`.
#[derive(Debug, Clone)]
pub(crate) enum UiAction {
    Key(Key),
    /// Several characters at once (the "00" key).
    Type(&'static str),
    /// Add an item by SKU (quick key, search result).
    Code(String),
    /// Select a cart line.
    Select(usize),
    /// Fill the prompt with this value and submit it (user tiles, choices).
    Submit(String),
    /// Clear the field being typed.
    Clear,
    /// Cycle the on-screen keyboard's shift.
    Shift,
    Menu(usize),
    RefundRow(usize),
    Page(isize),
    TogglePad,
}

/// Run one frame with the tap so widgets can see it, then act on it.
pub(crate) fn on_click<D: BlockDevice>(
    app: &mut App<D>,
    tap: ((i32, i32), (i32, i32)),
    hw: &mut Hw<'_>,
) {
    let now = app.now(hw);
    let frame = PointerFrame {
        held: None,
        click: Some(tap),
    };
    let Some(action) = draw(app, frame, Some(tap.0), now) else {
        return;
    };
    let at = app.now_us;
    // Taps are never part of a scanner burst.
    app.scan.reset();
    let key = |app: &mut App<D>, k: Key, hw: &mut Hw<'_>| {
        on_key(app, k, Modifiers::default(), at, hw);
    };
    match action {
        UiAction::Key(k) => key(app, k, hw),
        UiAction::Type(s) => {
            for c in s.chars() {
                key(app, Key::Char(c), hw);
            }
        }
        UiAction::Code(code) => match app.screen {
            Screen::Sale => add_code(app, &code),
            Screen::Search { .. } => {
                add_code(app, &code);
                app.screen = Screen::Sale;
            }
            _ => {}
        },
        UiAction::Select(i) => app.sel = Some(i),
        UiAction::Submit(v) => {
            if let Screen::Prompt(p) = &mut app.screen {
                p.field = v;
                key(app, Key::Enter, hw);
            }
        }
        UiAction::Clear => match &mut app.screen {
            Screen::Prompt(p) => p.field.clear(),
            Screen::Search { query, sel } => {
                query.clear();
                *sel = 0;
            }
            _ => app.entry.clear(),
        },
        UiAction::Shift => {
            if let Screen::Prompt(p) = &mut app.screen {
                p.shift = match p.shift {
                    Shift::Off => Shift::Once,
                    Shift::Once => Shift::Lock,
                    Shift::Lock => Shift::Off,
                };
            }
        }
        UiAction::Menu(i) => {
            if matches!(app.screen, Screen::Menu { .. }) {
                app.screen = menu_select(app, i, hw);
            }
        }
        UiAction::RefundRow(i) => {
            if let Screen::RefundPick { sel, .. } = &mut app.screen {
                *sel = i;
                key(app, Key::Enter, hw);
            }
        }
        UiAction::Page(d) => {
            let n = quick_pages(app) as isize;
            app.qk_page = (app.qk_page as isize + d).clamp(0, (n - 1).max(0)) as usize;
        }
        UiAction::TogglePad => app.sale_pad = !app.sale_pad,
    }
}

// ------------------------------------------------------------------ drawing

/// Which number pad to show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pad {
    Digits,
    Amount,
    Sale,
    Pay,
}

type PadKey = (&'static str, Option<UiAction>, ButtonStyle);

fn numpad(ui: &mut Ui<'_>, r: Rect, kind: Pad) -> Option<UiAction> {
    use ButtonStyle as S;
    let ch = |c: char| Some(UiAction::Key(Key::Char(c)));
    let off: PadKey = ("", None, S::Disabled);
    let digit = |l: &'static str| -> PadKey { (l, ch(l.chars().next().unwrap_or('0')), S::Normal) };
    let (k3, k4, k5, ok): (PadKey, PadKey, PadKey, PadKey) = match kind {
        Pad::Digits => (
            ("Cancel", Some(UiAction::Key(Key::Escape)), S::Normal),
            off.clone(),
            off.clone(),
            ("OK", Some(UiAction::Key(Key::Enter)), S::Primary),
        ),
        Pad::Amount => (
            ("Cancel", Some(UiAction::Key(Key::Escape)), S::Normal),
            (".", ch('.'), S::Normal),
            ("00", Some(UiAction::Type("00")), S::Normal),
            ("OK", Some(UiAction::Key(Key::Enter)), S::Primary),
        ),
        Pad::Sale => (
            ("× Qty", ch('*'), S::Normal),
            (".", ch('.'), S::Normal),
            ("00", Some(UiAction::Type("00")), S::Normal),
            ("Enter", Some(UiAction::Key(Key::Enter)), S::Primary),
        ),
        Pad::Pay => (
            ("Back", Some(UiAction::Key(Key::Escape)), S::Normal),
            (".", ch('.'), S::Normal),
            ("00", Some(UiAction::Type("00")), S::Normal),
            ("Cash", Some(UiAction::Key(Key::Enter)), S::Ok),
        ),
    };
    let keys: [PadKey; 16] = [
        digit("7"),
        digit("8"),
        digit("9"),
        ("⌫", Some(UiAction::Key(Key::Backspace)), S::Danger),
        digit("4"),
        digit("5"),
        digit("6"),
        ("C", Some(UiAction::Clear), S::Normal),
        digit("1"),
        digit("2"),
        digit("3"),
        k3,
        k4,
        digit("0"),
        k5,
        ok,
    ];
    let u = ui.u();
    let cells = r.grid(4, 4, u / 2);
    let font = ui.grid_font(cells[0], keys.iter().map(|k| k.0));
    let mut action = None;
    for ((label, act, style), cell) in keys.into_iter().zip(cells) {
        if ui.button_font(cell, label, style, font) {
            action = act;
        }
    }
    action
}

/// On-screen keyboard for text prompts and search.
fn keyboard(ui: &mut Ui<'_>, r: Rect, shift: Shift, cancel: bool) -> Option<UiAction> {
    use ButtonStyle as S;
    let upper = shift != Shift::Off;
    let u = ui.u();
    let cells = r.grid(11, 5, u / 2);
    let mut action = None;
    let letters = ["1234567890", "qwertyuiop/", "asdfghjkl-.", "zxcvbnm,'&"];
    let font = ui.grid_font(cells[0], ["W", "⌫", "⇧"]);
    let char_key = |ui: &mut Ui<'_>, cell: Rect, c: char, action: &mut Option<UiAction>| {
        let c = if upper { c.to_ascii_uppercase() } else { c };
        let mut buf = [0u8; 4];
        if ui.button_font(cell, c.encode_utf8(&mut buf), S::Normal, font) {
            *action = Some(UiAction::Key(Key::Char(c)));
        }
    };
    for (row, keys) in letters.iter().enumerate() {
        let base = row * 11;
        // Row 3 starts with Shift.
        let skip = usize::from(row == 3);
        for (k, c) in keys.chars().enumerate() {
            char_key(ui, cells[base + skip + k], c, &mut action);
        }
    }
    if ui.button_font(cells[10], "⌫", S::Danger, font) {
        action = Some(UiAction::Key(Key::Backspace));
    }
    let (shift_label, shift_style) = match shift {
        Shift::Off => ("⇧", S::Normal),
        Shift::Once => ("⇧", S::Primary),
        Shift::Lock => ("CAPS", S::Ok),
    };
    let shift_font = if shift == Shift::Lock {
        ui.grid_font(cells[33], ["CAPS"])
    } else {
        font
    };
    if ui.button_font(cells[33], shift_label, shift_style, shift_font) {
        action = Some(UiAction::Shift);
    }
    // Bottom row: Cancel ×2, Clear ×2, Space ×5, OK ×2.
    let row = &cells[44..55];
    let bottom: [(Rect, &str, Option<UiAction>, ButtonStyle); 4] = [
        (
            Rect::span(row, 0, 2),
            "Cancel",
            Some(UiAction::Key(Key::Escape)),
            if cancel { S::Normal } else { S::Disabled },
        ),
        (
            Rect::span(row, 2, 2),
            "Clear",
            Some(UiAction::Clear),
            S::Normal,
        ),
        (
            Rect::span(row, 4, 5),
            "Space",
            Some(UiAction::Key(Key::Char(' '))),
            S::Normal,
        ),
        (
            Rect::span(row, 9, 2),
            "OK",
            Some(UiAction::Key(Key::Enter)),
            S::Primary,
        ),
    ];
    for (cell, label, act, style) in bottom {
        if ui.button_font(cell, label, style, Font::Bold) {
            action = act;
        }
    }
    action
}

/// A grid of choice buttons (user tiles, payout kinds).
fn choices(ui: &mut Ui<'_>, r: Rect, items: &[(String, String)]) -> Option<UiAction> {
    let u = ui.u();
    let n = items.len().max(1) as i32;
    let cols = n.min(4);
    let rows = (n + cols - 1) / cols;
    let cell_h = (12 * u).min((r.h - (rows - 1) * u) / rows.max(1));
    let cell_w = (r.w - 3 * u) / 4;
    let area = r.centered(
        cols * cell_w + (cols - 1) * u,
        rows * cell_h + (rows - 1) * u,
    );
    let cells = area.grid(cols, rows, u);
    let font = ui.grid_font(cells[0], items.iter().map(|i| i.0.as_str()));
    let mut action = None;
    for ((label, value), cell) in items.iter().zip(cells) {
        if ui.button_font(cell, label, ButtonStyle::Primary, font) {
            action = Some(UiAction::Submit(value.clone()));
        }
    }
    action
}

/// Data the frame needs, copied out of the app before the canvas is borrowed.
struct View {
    status: String,
    message: Option<(String, bool)>,
    screen: Screen,
    cfg: pos_core::config::Config,
    priced: Option<pos_core::cart::Priced>,
    cart: pos_core::cart::Cart,
    sel: Option<usize>,
    entry: String,
    pending_qty: Option<Qty>,
    payment: Option<Payment>,
    quick: Vec<(usize, String, String)>,
    qk_page: usize,
    qk_pages: usize,
    sale_pad: bool,
    search: Vec<(String, String, String)>,
    refund_lines: Vec<String>,
    last_sale: Option<pos_core::sale::Sale>,
    choices: Vec<(String, String)>,
}

fn prompt_choices<D: BlockDevice>(app: &App<D>, next: &Next, now: i64) -> Vec<(String, String)> {
    let users = |f: &dyn Fn(&User) -> bool| -> Vec<(String, String)> {
        app.state
            .users
            .values()
            .filter(|u| u.active && f(u))
            .map(|u| (format!("{} ({})", u.name, u.id), u.id.to_string()))
            .collect()
    };
    let v = match next {
        Next::LoginUser => users(&|_| true),
        Next::AdoptUser => users(&|u| u.role == Role::Manager),
        Next::OverrideUser(p) => users(&|u| app.state.allows(u.id, p.permission())),
        Next::UnlockUser => users(&|u| app.state.is_locked(u.id, now)),
        Next::RefundPayout(..) => vec![
            ("Cash".into(), "1".into()),
            ("Back to card".into(), "2".into()),
            ("Voucher".into(), "3".into()),
        ],
        _ => Vec::new(),
    };
    // More tiles than fit comfortably: fall back to typing the number.
    if v.len() > 12 { Vec::new() } else { v }
}

fn view<D: BlockDevice>(app: &App<D>, now: i64) -> View {
    let cfg = app.state.config.clone();
    let money = |m: Money| format!("{}", m.display(cfg.decimals));
    let screen = app.screen.clone();
    let search = match &screen {
        Screen::Search { query, .. } => app
            .state
            .catalog
            .search(query, 50)
            .into_iter()
            .map(|i| (i.sku.clone(), i.name.clone(), money(i.price)))
            .collect(),
        _ => Vec::new(),
    };
    let refund_lines = match &screen {
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
                            .map(|(_, q)| format!("  → refund {q}"))
                            .unwrap_or_default();
                        format!(
                            "{}   sold {}, refunded {}   {}{}",
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
    let choices = match &screen {
        Screen::Prompt(p) => prompt_choices(app, &p.next, now),
        _ => Vec::new(),
    };
    View {
        status: status_line(app, now),
        message: app.message.clone(),
        priced: app.cart.price(&cfg).ok(),
        cart: app.cart.clone(),
        sel: app.sel,
        entry: app.entry.clone(),
        pending_qty: app.pending_qty,
        payment: app.payment.clone(),
        quick: app
            .state
            .catalog
            .quick_keys()
            .into_iter()
            .filter_map(|i| Some((usize::from(i.quick_key?), i.sku.clone(), i.name.clone())))
            .collect(),
        qk_page: app.qk_page,
        qk_pages: quick_pages(app),
        sale_pad: app.sale_pad,
        search,
        refund_lines,
        last_sale: app.last_sale.clone(),
        choices,
        screen,
        cfg,
    }
}

/// Draw one frame. `ptr` carries the pointer state; `held_from` is where
/// the current (or just-finished) press began. Returns the tapped action.
pub(crate) fn draw<D: BlockDevice>(
    app: &mut App<D>,
    ptr: PointerFrame,
    held_from: Option<(i32, i32)>,
    now: i64,
) -> Option<UiAction> {
    let v = view(app, now);
    let mut ui = Ui::new(&mut app.canvas, ptr, held_from);
    let mut action = None;
    ui.clear();
    let u = ui.u();
    let full = ui.c.bounds();
    let status_h = ui.c.line_height(Font::Normal) + 2 * u;
    let (top, rest) = full.split_top(status_h);
    let (body, bottom) = rest.split_bottom(ui.c.line_height(Font::Bold) + 2 * u);
    ui.c.fill_rect(top, ui.theme.panel);
    ui.label(top.inset(u), &v.status, Font::Normal, 0);
    if let Some((m, err)) = &v.message {
        let col = if *err { ui.theme.danger } else { ui.theme.ok };
        ui.c.fill_rect(bottom, col);
        ui.label_color(bottom.inset(u), m, Font::Bold, 0, ui.theme.button_text);
    }
    let body = body.inset(u);
    let money = |m: Money| format!("{}", m.display(v.cfg.decimals));

    match &v.screen {
        Screen::Blank => {}
        Screen::Fatal(m) => {
            ui.c.fill_rect(body, ui.theme.danger);
            let (t, r) = body.split_top(10 * u);
            ui.label(t, "FATAL", Font::Large, 1);
            ui.c.text_wrapped(r.inset(2 * u), m, Font::Normal, ui.theme.text);
        }
        Screen::Prompt(p) => action = draw_prompt(&mut ui, body, p, &v.choices),
        Screen::Sale | Screen::Pay | Screen::Done { .. } => {
            action = draw_sale(&mut ui, body, &v, &money);
        }
        Screen::Search { query, sel } => {
            let (t, r) = body.split_top(7 * u);
            ui.panel(t);
            ui.label(t.inset(u), &format!("Find: {query}_"), Font::Large, 0);
            let (list, kb) = r.split_bottom(r.h * 11 / 20);
            let list = Rect::new(list.x, list.y + u, list.w, list.h - 2 * u);
            let row_h = ui.c.line_height(Font::Normal) + 2 * u;
            if v.search.is_empty() {
                ui.label_color(list, "No matches", Font::Normal, 1, ui.theme.muted);
            }
            let rows = (list.h / row_h).max(1) as usize;
            let first = sel.saturating_sub(rows - 1);
            for (k, (sku, name, price)) in v.search.iter().enumerate().skip(first).take(rows) {
                let r = Rect::new(list.x, list.y + (k - first) as i32 * row_h, list.w, row_h);
                let cols = [
                    (name.as_str(), list.w * 55 / 100, 0u8),
                    (sku.as_str(), list.w * 25 / 100, 0),
                    (price.as_str(), list.w * 20 / 100, 2),
                ];
                if ui.row(r, &cols, k == *sel) {
                    action = Some(UiAction::Code(sku.clone()));
                }
            }
            if let Some(a) = keyboard(&mut ui, kb, Shift::Off, true) {
                action = Some(a);
            }
        }
        Screen::Menu { sel } => {
            let (t, r) = body.split_top(7 * u);
            ui.label(t, "Manager menu", Font::Large, 1);
            // Two columns of buttons; digits 1-9, 0 still pick from the keyboard.
            let rows = (MENU.len() as i32 + 1) / 2;
            let cell_h = (9 * u).min((r.h - u) / rows);
            let grid = r.centered(r.w.min(120 * u), rows * cell_h);
            let cells = grid.grid(2, rows, u / 2);
            let labels: Vec<String> = MENU
                .iter()
                .enumerate()
                .map(|(k, item)| match k {
                    0..=8 => format!("{}. {item}", k + 1),
                    9 => format!("0. {item}"),
                    _ => (*item).to_string(),
                })
                .collect();
            let font = ui
                .grid_font(cells[0], labels.iter().map(String::as_str))
                .min_bold();
            for (k, (label, cell)) in labels.iter().zip(cells).enumerate() {
                let style = if k == *sel {
                    ButtonStyle::Primary
                } else {
                    ButtonStyle::Normal
                };
                if ui.button_font(cell, label, style, font) {
                    action = Some(UiAction::Menu(k));
                }
            }
        }
        Screen::Info { title, lines } => {
            let (t, r) = body.split_top(7 * u);
            ui.label(t, title, Font::Large, 1);
            let (list, btn) = r.split_bottom(9 * u);
            let lh = ui.c.line_height(Font::Normal) + u / 2;
            let h = (lines.len() as i32 * lh + 2 * u).min(list.h);
            let list = Rect::new(list.x + list.w / 6, list.y, list.w * 4 / 6, h);
            ui.panel(list);
            let inner = list.inset(u);
            for (k, l) in lines.iter().enumerate() {
                let y = inner.y + k as i32 * lh;
                if y + lh > inner.y + inner.h {
                    break;
                }
                ui.label(Rect::new(inner.x, y, inner.w, lh), l, Font::Normal, 0);
            }
            let b = btn.centered(30 * u, 7 * u);
            if ui.button(b, "Continue", ButtonStyle::Primary) {
                action = Some(UiAction::Key(Key::Enter));
            }
        }
        Screen::RefundPick {
            receipt,
            sel,
            picks,
        } => {
            let (t, r) = body.split_top(7 * u);
            ui.label(
                t,
                &format!("Refund from receipt #{receipt}"),
                Font::Large,
                1,
            );
            let (list, btns) = r.split_bottom(9 * u);
            ui.label_color(
                list.split_top(4 * u).0,
                "Tap a line (or Enter) to set its refund quantity",
                Font::Normal,
                1,
                ui.theme.muted,
            );
            let list = list.split_top(4 * u).1;
            let row_h = ui.c.line_height(Font::Normal) + 2 * u;
            for (k, l) in v.refund_lines.iter().enumerate() {
                let r = Rect::new(list.x, list.y + k as i32 * row_h, list.w, row_h);
                if r.y + r.h > list.y + list.h {
                    break;
                }
                if ui.row(r, &[(l.as_str(), list.w, 0)], k == *sel) {
                    action = Some(UiAction::RefundRow(k));
                }
            }
            let cells = btns.centered(btns.w * 3 / 4, 7 * u).grid(3, 1, u);
            let cont = if picks.is_empty() {
                ButtonStyle::Disabled
            } else {
                ButtonStyle::Ok
            };
            let font = ui.grid_font(cells[0], ["Set quantity", "Continue", "Cancel"]);
            if ui.button_font(cells[0], "Set quantity", ButtonStyle::Primary, font) {
                action = Some(UiAction::Key(Key::Enter));
            }
            if ui.button_font(cells[1], "Continue", cont, font) {
                action = Some(UiAction::Key(Key::F(1)));
            }
            if ui.button_font(cells[2], "Cancel", ButtonStyle::Normal, font) {
                action = Some(UiAction::Key(Key::Escape));
            }
        }
    }
    let targets = core::mem::take(&mut ui.targets);
    app.targets = targets;
    action
}

fn draw_prompt(
    ui: &mut Ui<'_>,
    body: Rect,
    p: &Prompt,
    choice_items: &[(String, String)],
) -> Option<UiAction> {
    let u = ui.u();
    let input = p.next.input();
    let (t, r) = body.split_top(7 * u);
    ui.label(t, &p.title, Font::Large, 1);
    // Hints can be long (the damaged-store message): up to three lines.
    let hint_h = 3 * ui.c.line_height(Font::Normal);
    let (h, r) = r.split_top(hint_h);
    let muted = ui.theme.muted;
    ui.c.text_wrapped(h.centered(h.w * 4 / 5, h.h), &p.hint, Font::Normal, muted);
    let (f, r) = r.split_top(ui.c.line_height(Font::Large) + 2 * u);
    let fr = f.centered(f.w / 2, f.h);
    ui.panel(fr);
    let shown: String = if p.secret {
        p.field.chars().map(|_| '•').collect()
    } else {
        p.field.clone()
    };
    ui.label(fr.inset(u), &format!("{shown}_"), Font::Large, 1);
    let r = Rect::new(r.x, r.y + u, r.w, r.h - u);
    if !choice_items.is_empty() {
        let (grid, foot) = r.split_bottom(8 * u);
        let mut action = choices(ui, grid, choice_items);
        if p.next.cancellable() {
            let b = foot.centered(24 * u, 6 * u);
            if ui.button(b, "Cancel", ButtonStyle::Normal) {
                action = Some(UiAction::Key(Key::Escape));
            }
        }
        return action;
    }
    match input {
        Input::Text => {
            let kb = r.centered(r.w.min(110 * u), r.h);
            keyboard(ui, kb, p.shift, p.next.cancellable())
        }
        Input::Digits | Input::Amount => {
            let side = r.h.min(r.w / 2);
            let pad = r.centered(side * 4 / 3, side);
            let kind = if input == Input::Digits {
                Pad::Digits
            } else {
                Pad::Amount
            };
            numpad(ui, pad, kind)
        }
    }
}

fn draw_sale(
    ui: &mut Ui<'_>,
    body: Rect,
    v: &View,
    money: &dyn Fn(Money) -> String,
) -> Option<UiAction> {
    let mut action = None;
    let u = ui.u();
    let done = match v.screen {
        Screen::Done { change, receipt } => Some((change, receipt)),
        _ => None,
    };
    let (left, right) = body.split_left(body.w * 3 / 5);
    let left = Rect::new(left.x, left.y, left.w - u, left.h);

    // ---- left: lines (the finished sale's lines on the Done screen)
    ui.panel(left);
    let (list, entry_r) = left.split_bottom(ui.c.line_height(Font::Large) + 3 * u);
    let list = list.inset(u / 2);
    let row_h = ui.c.line_height(Font::Normal) + u;
    let rows = (list.h / row_h).max(1) as usize;
    let mut lines: Vec<(String, String, String)> = Vec::new();
    match (&done, &v.last_sale, &v.priced) {
        (Some(_), Some(s), _) => {
            for l in &s.lines {
                lines.push((
                    l.line.name.clone(),
                    format!("{}", l.line.qty),
                    money(l.priced.total),
                ));
            }
        }
        (None, _, Some(p)) => {
            for (l, pl) in v.cart.lines.iter().zip(&p.lines) {
                let mut name = l.name.clone();
                if l.discount.is_some() {
                    name.push_str(" (disc)");
                }
                if l.price_override {
                    name.push_str(" (ovr)");
                }
                lines.push((name, format!("{}", l.qty), money(pl.total)));
            }
        }
        _ => {}
    }
    let sel = if done.is_some() { None } else { v.sel };
    let first = sel
        .unwrap_or(lines.len().saturating_sub(1))
        .saturating_sub(rows.saturating_sub(1));
    for (k, (name, qty, total)) in lines.iter().enumerate().skip(first).take(rows) {
        let r = Rect::new(list.x, list.y + (k - first) as i32 * row_h, list.w, row_h);
        let cols = [
            (name.as_str(), list.w * 6 / 10, 0u8),
            (qty.as_str(), list.w * 15 / 100, 2),
            (total.as_str(), list.w * 25 / 100, 2),
        ];
        if ui.row(r, &cols, sel == Some(k)) && done.is_none() {
            action = Some(UiAction::Select(k));
        }
    }
    if lines.is_empty() && done.is_none() {
        ui.label_color(
            list,
            "Scan an item, tap a quick key, or type a code",
            Font::Normal,
            1,
            ui.theme.muted,
        );
    }
    ui.c.fill_rect(entry_r, ui.theme.bg);
    match done {
        Some((_, receipt)) => {
            let ok = ui.theme.ok;
            ui.label_color(
                entry_r.inset(u),
                &format!("✓ Receipt #{receipt} saved"),
                Font::Large,
                0,
                ok,
            );
        }
        None => {
            let q = v.pending_qty.map(|q| format!("{q} × ")).unwrap_or_default();
            ui.label(
                entry_r.inset(u),
                &format!("› {q}{}_", v.entry),
                Font::Large,
                0,
            );
        }
    }

    // ---- right: totals
    let (tot, rest) = right.split_top(14 * u);
    ui.panel(tot);
    let totals = match (&done, &v.last_sale) {
        (Some(_), Some(s)) => Some((s.lines.len(), s.totals.clone())),
        (None, _) => v
            .priced
            .as_ref()
            .map(|p| (v.cart.lines.len(), p.totals.clone())),
        _ => None,
    };
    if let Some((n, t)) = totals {
        let (a, b) = tot.inset(u).split_top(4 * u);
        let muted = ui.theme.muted;
        ui.label_color(
            a,
            &format!("Items {n}   Tax {}", money(t.tax)),
            Font::Normal,
            0,
            muted,
        );
        if !t.discount.is_zero() {
            ui.label_color(
                a,
                &format!("Disc −{}", money(t.discount)),
                Font::Normal,
                2,
                muted,
            );
        }
        ui.label(
            b,
            &format!("{} {}", v.cfg.currency, money(t.total)),
            Font::Large,
            2,
        );
    }
    let rest = Rect::new(rest.x, rest.y + u, rest.w, rest.h - u);

    match &v.screen {
        Screen::Pay => {
            let Some(pay) = &v.payment else {
                return action;
            };
            let lh = ui.c.line_height(Font::Normal) + u / 2;
            let n_lines = 2 + pay.tenders.len() as i32;
            let (info, rest) = rest.split_top(n_lines * lh + 2 * u);
            ui.panel(info);
            let due = pay.cash_due(&v.cfg).unwrap_or(Money::ZERO);
            let remaining = pay.remaining().unwrap_or(Money::ZERO);
            let mut rows = vec![
                ("Remaining".to_string(), money(remaining)),
                ("Cash due (rounded)".to_string(), money(due)),
            ];
            for t in &pay.tenders {
                rows.push((
                    format!("{} {}", t.kind.label(), t.reference),
                    money(t.amount),
                ));
            }
            let inner = info.inset(u);
            for (k, (l, a)) in rows.iter().enumerate() {
                let r = Rect::new(inner.x, inner.y + k as i32 * lh, inner.w, lh);
                ui.label(r, l, Font::Normal, 0);
                ui.label(r, a, Font::Bold, 2);
            }
            let rest = Rect::new(rest.x, rest.y + u, rest.w, rest.h - u);
            let (pad, tenders) = rest.split_bottom(8 * u);
            if let Some(a) = numpad(ui, Rect::new(pad.x, pad.y, pad.w, pad.h - u), Pad::Pay) {
                action = Some(a);
            }
            // Cash is the keypad's green key (Enter / F1).
            let cells = tenders.grid(3, 1, u / 2);
            let btns = [
                ("Card (F2)", Key::F(2), ButtonStyle::Primary),
                ("Voucher (F3)", Key::F(3), ButtonStyle::Primary),
                ("Void sale (F8)", Key::F(8), ButtonStyle::Danger),
            ];
            let font = ui.grid_font(cells[0], btns.iter().map(|b| b.0));
            for ((label, k, style), cell) in btns.into_iter().zip(cells) {
                if ui.button_font(cell, label, style, font) {
                    action = Some(UiAction::Key(k));
                }
            }
        }
        Screen::Done { change, .. } => {
            ui.c.fill_round_rect(rest, ui.c.px(6), ui.theme.ok);
            let inner = rest.inset(2 * u);
            let white = ui.theme.button_text;
            let lh = ui.c.line_height(Font::Bold) + u;
            let big = ui.c.line_height(Font::Huge);
            let mut rows: Vec<(String, String)> = Vec::new();
            if let Some(s) = &v.last_sale {
                rows.push(("Total".into(), money(s.totals.total)));
                if !s.rounding.is_zero() {
                    rows.push(("Cash rounding".into(), money(s.rounding)));
                }
                for t in &s.tenders {
                    rows.push((t.kind.label().into(), money(t.amount)));
                }
            }
            // Paid lines, then CHANGE and the amount, centred as one block.
            let block_h = rows.len() as i32 * lh + 2 * u + lh + big;
            let block = inner.centered(inner.w.min(50 * u), block_h);
            let mut y = block.y;
            for (l, a) in &rows {
                let r = Rect::new(block.x, y, block.w, lh);
                ui.label_color(r, l, Font::Normal, 0, white);
                ui.label_color(r, a, Font::Bold, 2, white);
                y += lh;
            }
            y += u;
            ui.c.fill_rect(Rect::new(block.x, y, block.w, ui.c.px(2).max(1)), white);
            y += u;
            ui.label_color(
                Rect::new(block.x, y, block.w, lh),
                "CHANGE",
                Font::Bold,
                1,
                white,
            );
            y += lh;
            ui.label_color(
                Rect::new(block.x, y, block.w, big),
                &money(*change),
                Font::Huge,
                1,
                white,
            );
            let hint = Rect::new(inner.x, inner.y + inner.h - lh, inner.w, lh);
            ui.label_color(
                hint,
                "Tap or press any key for the next sale",
                Font::Small,
                1,
                white,
            );
            if ui.target(rest, "Next sale") {
                action = Some(UiAction::Key(Key::Escape));
            }
        }
        _ => {
            // Header: page arrows, page number, keypad toggle.
            let (head, rest) = rest.split_top(6 * u);
            let hc = head.grid(4, 1, u / 2);
            let paged = v.qk_pages > 1 && !v.sale_pad;
            let st = |on: bool| {
                if on {
                    ButtonStyle::Normal
                } else {
                    ButtonStyle::Disabled
                }
            };
            let hf = ui.grid_font(hc[0], ["◀", "▶", "123", "Items"]);
            if ui.button_font(hc[0], "◀", st(paged && v.qk_page > 0), hf) {
                action = Some(UiAction::Page(-1));
            }
            let page_txt = if v.sale_pad {
                "Keypad".to_string()
            } else {
                format!("Page {}/{}", v.qk_page + 1, v.qk_pages)
            };
            let muted = ui.theme.muted;
            ui.label_color(hc[1], &page_txt, Font::Normal, 1, muted);
            if ui.button_font(hc[2], "▶", st(paged && v.qk_page + 1 < v.qk_pages), hf) {
                action = Some(UiAction::Page(1));
            }
            let (tl, ts) = if v.sale_pad {
                ("Items", ButtonStyle::Primary)
            } else {
                ("123", ButtonStyle::Primary)
            };
            if ui.button_font(hc[3], tl, ts, hf) {
                action = Some(UiAction::TogglePad);
            }
            let rest = Rect::new(rest.x, rest.y + u / 2, rest.w, rest.h - u / 2);
            let (mid, fk) = rest.split_top(rest.h * 52 / 100);
            let mid = Rect::new(mid.x, mid.y, mid.w, mid.h - u / 2);
            if v.sale_pad {
                if let Some(a) = numpad(ui, mid, Pad::Sale) {
                    action = Some(a);
                }
            } else {
                let cells = mid.grid(4, 3, u / 2);
                let page: Vec<&(usize, String, String)> = v
                    .quick
                    .iter()
                    .filter(|(pos, ..)| pos / QK_PER_PAGE == v.qk_page)
                    .collect();
                let font = ui.grid_font(cells[0], page.iter().map(|q| q.2.as_str()));
                for (pos, sku, name) in page {
                    let cell = cells[pos % QK_PER_PAGE];
                    if ui.button_font(cell, name, ButtonStyle::Normal, font) {
                        action = Some(UiAction::Code(sku.clone()));
                    }
                }
            }
            let fns = [
                ("F1 Pay", ButtonStyle::Ok),
                ("F2 Find", ButtonStyle::Normal),
                ("F3 Qty", ButtonStyle::Normal),
                ("F4 Void line", ButtonStyle::Danger),
                ("F5 Line %", ButtonStyle::Normal),
                ("F6 Sale %", ButtonStyle::Normal),
                ("F7 Price", ButtonStyle::Normal),
                ("F8 Void sale", ButtonStyle::Danger),
                ("F9 Refund", ButtonStyle::Normal),
                ("F10 Menu", ButtonStyle::Primary),
                ("F11 Drawer", ButtonStyle::Normal),
                ("F12 Sign out", ButtonStyle::Normal),
            ];
            let cells = fk.grid(4, 3, u / 2);
            let font = ui.grid_font(cells[0], fns.iter().map(|f| f.0));
            for (n, ((label, style), cell)) in fns.iter().zip(cells).enumerate() {
                if ui.button_font(cell, label, *style, font) {
                    action = Some(UiAction::Key(Key::F(n as u8 + 1)));
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
    let printer = if app.printer_ok { "" } else { "   PRINTER!" };
    let full = if pct >= 80 { "   EXPORT NEEDED" } else { "" };
    format!(
        "{}   •   {}   •   {}   •   {}   •   Journal {}%{}{}",
        app.state.config.store_name,
        user,
        shift,
        receipt::fmt_time(now),
        pct,
        full,
        printer
    )
}
