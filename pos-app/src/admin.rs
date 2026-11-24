//! The Settings app: users and their roles, what each role may do, the
//! shift sales goal, automatic sign-out, the store name, "my PIN", and the
//! existing settings import and network setup.
//!
//! Anyone signed in can open it. Each row that changes something needs a
//! permission (users: `ManageUsers`, everything else: `Configure`). A user
//! without it gets the usual override prompt. Whoever authorised it is then
//! the `by` on every change made on the pages below.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use pos_core::Money;
use pos_core::auth::{self, Permission, Role, UserId};
use pos_core::csv::parse_money;
use pos_core::policy::{MAX_IDLE_MINS, Policy};
use pos_core::state::Event;
use pos_hal::{BlockDevice, Key};
use pos_ui::{Font, Rect, Ui, mix};

use crate::screens::{self, Next, Pending, Prompt, Screen, UiAction, prompt};
use crate::{App, Hw, Place};

/// A page of the Settings app.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetPage {
    Main,
    Users,
    User(UserId),
    Roles,
    Role(Role),
    Display,
    Wifi,
}

/// Settings actions that need a permission (run through `authorize`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdminAct {
    Users,
    AddUser,
    Roles,
    Goal,
    IdleSignout,
    StoreName,
    Display,
    Wifi,
}

impl AdminAct {
    pub(crate) fn permission(self) -> Permission {
        match self {
            AdminAct::Users | AdminAct::AddUser => Permission::ManageUsers,
            _ => Permission::Configure,
        }
    }
}

/// What tapping a row does.
#[derive(Debug, Clone)]
enum Act {
    Nothing,
    Back,
    Admin(AdminAct),
    Other(Pending),
    MyPin,
    About,
    NetInfo,
    OpenUser(UserId),
    Rename(UserId),
    CycleRole(UserId),
    ResetPin(UserId),
    ToggleActive(UserId),
    Unlock(UserId),
    OpenRole(Role),
    Toggle(Role, Permission),
    DefaultRoles,
    // Display & sound
    ToggleTheme,
    CycleAccent,
    CycleText,
    ToggleHand,
    TogglePictures,
    ToggleBeep,
    ToggleClick,
    Saver,
    TestSound,
    // Wi-Fi
    WifiScan,
    WifiPick(String),
    WifiForget,
    WifiDisconnect,
    WifiReconnect,
}

struct Row {
    label: String,
    value: String,
    act: Act,
    /// Drawn greyed out (informational, or the user lacks the right).
    dim: bool,
}

fn row(label: impl Into<String>, value: impl Into<String>, act: Act) -> Row {
    Row {
        label: label.into(),
        value: value.into(),
        act,
        dim: false,
    }
}

/// Open the Settings app as the signed-in user.
pub(crate) fn open<D: BlockDevice>(app: &mut App<D>) -> Screen {
    let Some(me) = app.user else {
        return screens::login_screen(app);
    };
    app.place = Place::Settings;
    page(SetPage::Main, me)
}

pub(crate) fn page(page: SetPage, by: UserId) -> Screen {
    Screen::Settings { page, sel: 0, by }
}

fn title(app: &App<impl BlockDevice>, p: SetPage) -> String {
    match p {
        SetPage::Main => "Settings".into(),
        SetPage::Users => "Users".into(),
        SetPage::User(id) => format!("User: {}", app.user_name(id)),
        SetPage::Roles => "Roles & permissions".into(),
        SetPage::Role(r) => format!("What a {} may do", r.label()),
        SetPage::Display => "Display & sound".into(),
        SetPage::Wifi => "Wi-Fi".into(),
    }
}

fn parent(p: SetPage) -> Option<SetPage> {
    match p {
        SetPage::Main => None,
        SetPage::Users | SetPage::Roles | SetPage::Display | SetPage::Wifi => Some(SetPage::Main),
        SetPage::User(_) => Some(SetPage::Users),
        SetPage::Role(_) => Some(SetPage::Roles),
    }
}

fn mins(m: u16) -> String {
    match m {
        0 => "Never".into(),
        1 => "After 1 minute idle".into(),
        m => format!("After {m} minutes idle"),
    }
}

fn rows<D: BlockDevice>(app: &App<D>, p: SetPage, by: UserId) -> Vec<Row> {
    let st = &app.state;
    let can = |perm: Permission| st.allows(by, perm);
    let gated = |mut r: Row, perm: Permission| {
        r.dim = !can(perm);
        r
    };
    let mut v = Vec::new();
    match p {
        SetPage::Main => {
            v.push(row("My PIN", "Change", Act::MyPin));
            let active = st.users.values().filter(|u| u.active).count();
            v.push(gated(
                row(
                    "Users",
                    format!("{active} active"),
                    Act::Admin(AdminAct::Users),
                ),
                Permission::ManageUsers,
            ));
            v.push(gated(
                row(
                    "Roles & permissions",
                    format!("{} roles", Role::ALL.len()),
                    Act::Admin(AdminAct::Roles),
                ),
                Permission::Configure,
            ));
            let goal = st.policy.shift_goal;
            v.push(gated(
                row(
                    "Shift sales goal",
                    if goal.is_zero() {
                        "Off".into()
                    } else {
                        app.money(goal)
                    },
                    Act::Admin(AdminAct::Goal),
                ),
                Permission::Configure,
            ));
            v.push(gated(
                row(
                    "Automatic sign-out",
                    mins(st.policy.idle_signout_mins),
                    Act::Admin(AdminAct::IdleSignout),
                ),
                Permission::Configure,
            ));
            let ui = &st.ui;
            v.push(gated(
                row(
                    "Display & sound",
                    format!(
                        "{} · {} · text {}",
                        if ui.theme == pos_core::ui::ThemeMode::Dark {
                            "Dark"
                        } else {
                            "Light"
                        },
                        ui.accent.label(),
                        ui.text.label().to_lowercase()
                    ),
                    Act::Admin(AdminAct::Display),
                ),
                Permission::Configure,
            ));
            let wifi = match (&app.wifi_rt.connected, &st.wifi, app.wifi_rt.present) {
                (Some(n), ..) => format!("Connected: {n}"),
                (None, Some(w), true) => format!("Not connected ({})", w.ssid),
                (None, None, true) => "Not set up".into(),
                (None, _, false) => "No adapter".into(),
            };
            v.push(gated(
                row("Wi-Fi", wifi, Act::Admin(AdminAct::Wifi)),
                Permission::Configure,
            ));
            v.push(gated(
                row(
                    "Store name",
                    st.config.store_name.clone(),
                    Act::Admin(AdminAct::StoreName),
                ),
                Permission::Configure,
            ));
            v.push(gated(
                row(
                    "Import settings (SETTINGS.TXT)",
                    "USB",
                    Act::Other(Pending::ImportSettings),
                ),
                Permission::Configure,
            ));
            v.push(gated(
                row(
                    "Store network",
                    st.net.role.label(),
                    Act::Other(Pending::NetworkSetup),
                ),
                Permission::Configure,
            ));
            v.push(row("Network status", "", Act::NetInfo));
            v.push(row("About this till", "", Act::About));
        }
        SetPage::Users => {
            for u in st.users.values() {
                let mut status = String::from(u.role.label());
                if !u.active {
                    status.push_str(" · disabled");
                } else if st.is_locked(u.id, app.now_unix()) {
                    status.push_str(" · LOCKED");
                }
                let mut r = row(
                    format!("{}  (#{})", u.name, u.id),
                    status,
                    Act::OpenUser(u.id),
                );
                r.dim = !u.active;
                v.push(r);
            }
            v.push(row("+ Add user", "", Act::Admin(AdminAct::AddUser)));
        }
        SetPage::User(id) => {
            let Some(u) = st.users.get(&id) else {
                return v;
            };
            v.push(row("Name", u.name.clone(), Act::Rename(id)));
            v.push(row(
                "Role (tap to change)",
                u.role.label(),
                Act::CycleRole(id),
            ));
            v.push(row("PIN", "Reset", Act::ResetPin(id)));
            v.push(row(
                "Status",
                if u.active {
                    "Active (tap to disable)"
                } else {
                    "Disabled (tap to enable)"
                },
                Act::ToggleActive(id),
            ));
            if st.is_locked(id, app.now_unix()) {
                v.push(row("Locked after wrong PINs", "Unlock", Act::Unlock(id)));
            }
            let mut info = row(
                "Can do",
                format!(
                    "{} of {} things",
                    st.policy.count(u.role),
                    Permission::ALL.len()
                ),
                Act::OpenRole(u.role),
            );
            info.dim = u.role == Role::Manager;
            if u.role == Role::Manager {
                info.act = Act::Nothing;
                info.value = "Everything (managers)".into();
            }
            v.push(info);
        }
        SetPage::Roles => {
            let mut m = row("Manager", "Everything (fixed)", Act::Nothing);
            m.dim = true;
            v.push(m);
            for r in Role::ALL.into_iter().filter(|r| *r != Role::Manager) {
                let users = st
                    .users
                    .values()
                    .filter(|u| u.active && u.role == r)
                    .count();
                v.push(row(
                    r.label(),
                    format!(
                        "{} of {} · {users} user{}",
                        st.policy.count(r),
                        Permission::ALL.len(),
                        if users == 1 { "" } else { "s" }
                    ),
                    Act::OpenRole(r),
                ));
            }
            v.push(row(
                "Restore the default permissions",
                "",
                Act::DefaultRoles,
            ));
        }
        SetPage::Display => {
            use pos_core::ui::ThemeMode;
            let ui = &st.ui;
            let on = |b: bool| if b { "On" } else { "Off" };
            v.push(row(
                "Theme",
                if ui.theme == ThemeMode::Dark {
                    "Dark (tap for light)"
                } else {
                    "Light (tap for dark)"
                },
                Act::ToggleTheme,
            ));
            v.push(row("Accent colour", ui.accent.label(), Act::CycleAccent));
            v.push(row("Text size", ui.text.label(), Act::CycleText));
            v.push(row(
                "Sale screen",
                if ui.left_handed {
                    "Left-handed: keys on the left"
                } else {
                    "Right-handed: keys on the right"
                },
                Act::ToggleHand,
            ));
            v.push(row(
                "Pictures on quick keys",
                on(ui.key_pictures),
                Act::TogglePictures,
            ));
            v.push(row("Beep on scan", on(ui.beep_on_scan), Act::ToggleBeep));
            v.push(row("Key click", on(ui.key_click), Act::ToggleClick));
            v.push(row(
                "Screen saver",
                match ui.saver_mins {
                    0 => String::from("Off"),
                    m => format!("Clock after {m} min idle"),
                },
                Act::Saver,
            ));
            v.push(row("Test the speaker", "Beep", Act::TestSound));
        }
        SetPage::Wifi => {
            let rt = &app.wifi_rt;
            if !rt.present {
                let mut r = row("Adapter", "None found", Act::Nothing);
                r.dim = true;
                v.push(r);
                let mut why = row(
                    "Wi-Fi needs a firmware (UEFI) Wi-Fi driver",
                    "Use Ethernet",
                    Act::Nothing,
                );
                why.dim = true;
                v.push(why);
            } else {
                let status = match (rt.busy(), &rt.connected) {
                    (Some(b), _) => b.to_string(),
                    (None, Some(n)) => format!("Connected to {n}"),
                    (None, None) => rt
                        .last_error
                        .clone()
                        .unwrap_or_else(|| "Not connected".into()),
                };
                let mut s = row("Status", status, Act::Nothing);
                s.dim = true;
                v.push(s);
                if let Some(w) = &st.wifi {
                    v.push(row("Saved network", w.ssid.clone(), Act::Nothing));
                    if rt.connected.is_some() {
                        v.push(row("Disconnect", "", Act::WifiDisconnect));
                    } else {
                        v.push(row("Connect now", "", Act::WifiReconnect));
                    }
                    v.push(row("Forget this network", "", Act::WifiForget));
                }
                v.push(row(
                    "Scan for networks",
                    if rt.scanned {
                        format!("{} found", rt.networks.len())
                    } else {
                        String::new()
                    },
                    Act::WifiScan,
                ));
                for n in rt.networks.iter().take(8) {
                    let mut r = row(
                        n.ssid.clone(),
                        format!(
                            "{}% {}{}",
                            n.quality,
                            crate::wifi::signal(n.quality),
                            if n.secured {
                                ""
                            } else {
                                " · open (not allowed)"
                            }
                        ),
                        Act::WifiPick(n.ssid.clone()),
                    );
                    r.dim = !n.secured;
                    v.push(r);
                }
            }
            let mut d = row("Driver", rt.describe.clone(), Act::Nothing);
            d.dim = true;
            v.push(d);
        }
        SetPage::Role(r) => {
            for perm in Permission::ALL {
                let mut x = row(
                    cap(perm.label()),
                    if perm.manager_only() {
                        "Managers only"
                    } else if st.policy.allows(r, perm) {
                        "Allowed"
                    } else {
                        "Needs override"
                    },
                    Act::Toggle(r, perm),
                );
                x.dim = perm.manager_only() || r == Role::Manager;
                v.push(x);
            }
        }
    }
    v.push(row(
        if p == SetPage::Main { "Done" } else { "Back" },
        "",
        Act::Back,
    ));
    v
}

fn cap(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().chain(c).collect(),
        None => String::new(),
    }
}

pub(crate) fn key<D: BlockDevice>(
    app: &mut App<D>,
    p: SetPage,
    sel: usize,
    by: UserId,
    key: Key,
    hw: &mut Hw<'_>,
) -> Screen {
    let n = rows(app, p, by).len();
    match key {
        Key::Up => Screen::Settings {
            page: p,
            sel: sel.saturating_sub(1),
            by,
        },
        Key::Down => Screen::Settings {
            page: p,
            sel: (sel + 1).min(n.saturating_sub(1)),
            by,
        },
        Key::Enter => select(app, p, sel, by, hw),
        Key::Escape => back(app, p, by),
        Key::Home => screens::go_home(app),
        Key::F(12) => screens::sign_out(app, hw),
        _ => Screen::Settings { page: p, sel, by },
    }
}

fn back<D: BlockDevice>(app: &mut App<D>, p: SetPage, by: UserId) -> Screen {
    match parent(p) {
        Some(up) => page(up, by),
        None => screens::go_home(app),
    }
}

/// The settings page to return to from an admin prompt (Escape or done).
pub(crate) fn return_to<D: BlockDevice>(app: &mut App<D>, next: &Next) -> Option<Screen> {
    let to = match *next {
        Next::NewUserName(by)
        | Next::NewUserRole(_, by)
        | Next::NewUserPin(_, _, by)
        | Next::NewUserPin2(_, _, _, by) => {
            if app.place == Place::Settings {
                page(SetPage::Users, by)
            } else {
                screens::back_to_work(app)
            }
        }
        Next::RenameUser(id, by) | Next::ResetPin(id, by) | Next::ResetPin2(id, _, by) => {
            page(SetPage::User(id), by)
        }
        Next::Goal(by) | Next::IdleMins(by) => page(SetPage::Main, by),
        Next::SaverMins(by) => page(SetPage::Display, by),
        Next::WifiPassword(_, by) => page(SetPage::Wifi, by),
        Next::StoreName(by) => {
            if app.place == Place::Settings {
                page(SetPage::Main, by)
            } else {
                // First-time setup.
                app.place = Place::Home;
                Screen::Home
            }
        }
        Next::MyPinOld | Next::MyPinNew | Next::MyPinNew2(_) => page(SetPage::Main, app.user?),
        _ => return None,
    };
    Some(to)
}

pub(crate) fn select<D: BlockDevice>(
    app: &mut App<D>,
    p: SetPage,
    sel: usize,
    by: UserId,
    hw: &mut Hw<'_>,
) -> Screen {
    let list = rows(app, p, by);
    let Some(r) = list.get(sel) else {
        return Screen::Settings { page: p, sel, by };
    };
    let stay = Screen::Settings { page: p, sel, by };
    let now = app.now(hw);
    match r.act.clone() {
        Act::Nothing => stay,
        Act::Back => back(app, p, by),
        Act::Admin(a) => {
            if app.state.allows(by, a.permission()) {
                run(app, a, by)
            } else {
                screens::authorize(app, Pending::Admin(a), hw)
            }
        }
        Act::Other(pending) => screens::authorize(app, pending, hw),
        Act::MyPin => {
            if app.state.net.role == pos_core::sync::NetRole::Lane {
                app.set_msg("Change PINs on the head till", true);
                return stay;
            }
            prompt("Change my PIN", "Your current PIN", true, Next::MyPinOld)
        }
        Act::About => about(app),
        Act::NetInfo => screens::network_info(app),
        Act::OpenUser(id) => page(SetPage::User(id), by),
        Act::OpenRole(r) => page(SetPage::Role(r), by),
        Act::Rename(id) => prompt(
            "Rename user",
            &format!("New name for {} (1-24 characters)", app.user_name(id)),
            false,
            Next::RenameUser(id, by),
        ),
        Act::ResetPin(id) => prompt(
            "Reset PIN",
            &format!("New 4-8 digit PIN for {}", app.user_name(id)),
            true,
            Next::ResetPin(id, by),
        ),
        Act::CycleRole(id) => {
            let Some(mut u) = app.state.users.get(&id).cloned() else {
                return stay;
            };
            let k = Role::ALL.iter().position(|r| *r == u.role).unwrap_or(0);
            u.role = Role::ALL[(k + 1) % Role::ALL.len()];
            let msg = format!("{} is now a {}", u.name, u.role.label());
            // Rights are checked on every action, so the change takes
            // effect at once, also for a user who is signed in.
            save_user(app, by, now, u, &msg);
            stay
        }
        Act::ToggleActive(id) => {
            if Some(id) == app.user {
                app.set_msg("You cannot disable yourself", true);
                return stay;
            }
            let Some(mut u) = app.state.users.get(&id).cloned() else {
                return stay;
            };
            u.active = !u.active;
            let msg = format!(
                "{} {}",
                u.name,
                if u.active { "enabled" } else { "disabled" }
            );
            save_user(app, by, now, u, &msg);
            stay
        }
        Act::Unlock(id) => {
            match app.commit(vec![Event::UserUnlocked {
                user: id,
                by,
                at: now,
            }]) {
                Ok(_) => app.set_msg(format!("{} unlocked", app.user_name(id)), false),
                Err(e) => app.err(&e),
            }
            stay
        }
        Act::Toggle(role, perm) => {
            if role == Role::Manager || perm.manager_only() {
                app.set_msg("Only managers can do this - it cannot be granted", true);
                return stay;
            }
            let mut pol = app.state.policy.clone();
            let on = !pol.allows(role, perm);
            pol.set(role, perm, on);
            let msg = format!(
                "{}: {} {}",
                role.label(),
                perm.label(),
                if on { "allowed" } else { "needs an override" }
            );
            save_policy(app, by, now, pol, &msg);
            stay
        }
        Act::ToggleTheme
        | Act::CycleAccent
        | Act::CycleText
        | Act::ToggleHand
        | Act::TogglePictures
        | Act::ToggleBeep
        | Act::ToggleClick => {
            use pos_core::ui::{Accent, TextSize, ThemeMode};
            let mut ui = app.state.ui.clone();
            let next = |i: usize, n: usize| (i + 1) % n;
            match r.act {
                Act::ToggleTheme => {
                    ui.theme = match ui.theme {
                        ThemeMode::Dark => ThemeMode::Light,
                        ThemeMode::Light => ThemeMode::Dark,
                    }
                }
                Act::CycleAccent => {
                    let i = Accent::ALL
                        .iter()
                        .position(|a| *a == ui.accent)
                        .unwrap_or(0);
                    ui.accent = Accent::ALL[next(i, Accent::ALL.len())];
                }
                Act::CycleText => {
                    let i = TextSize::ALL
                        .iter()
                        .position(|a| *a == ui.text)
                        .unwrap_or(0);
                    ui.text = TextSize::ALL[next(i, TextSize::ALL.len())];
                }
                Act::ToggleHand => ui.left_handed = !ui.left_handed,
                Act::TogglePictures => ui.key_pictures = !ui.key_pictures,
                Act::ToggleBeep => ui.beep_on_scan = !ui.beep_on_scan,
                _ => ui.key_click = !ui.key_click,
            }
            let beep = matches!(r.act, Act::ToggleBeep) && ui.beep_on_scan;
            save_ui(app, by, now, ui, "Display & sound saved");
            if beep {
                hw.sound.beep(1800, 45);
            }
            stay
        }
        Act::Saver => prompt(
            "Screen saver",
            &format!(
                "Show a clock after this many idle minutes (not during a sale). \
                 0 = off, at most {}.",
                pos_core::ui::MAX_SAVER_MINS
            ),
            false,
            Next::SaverMins(by),
        ),
        Act::TestSound => {
            hw.sound.beep(1800, 45);
            hw.sound.beep(1200, 45);
            app.set_msg(
                "Beeped. Nothing heard? Many PCs have no built-in speaker",
                false,
            );
            stay
        }
        Act::WifiScan => {
            app.wifi_request(crate::wifi::Job::Scan);
            stay
        }
        Act::WifiPick(ssid) => {
            if !app
                .wifi_rt
                .networks
                .iter()
                .any(|n| n.ssid == ssid && n.secured)
            {
                app.set_msg(
                    "Open networks are not allowed: use a WPA2/WPA3 password",
                    true,
                );
                return stay;
            }
            prompt(
                "Wi-Fi password",
                &format!("Password for \"{ssid}\" (8-63 characters)"),
                true,
                Next::WifiPassword(ssid, by),
            )
        }
        Act::WifiDisconnect => {
            app.wifi_request(crate::wifi::Job::Disconnect);
            stay
        }
        Act::WifiReconnect => {
            if let Some(w) = app.state.wifi.clone() {
                app.wifi_request(crate::wifi::Job::Connect {
                    ssid: w.ssid,
                    password: w.password,
                    by,
                });
            }
            stay
        }
        Act::WifiForget => {
            match app.commit(vec![Event::WifiSet {
                by,
                at: now,
                wifi: None,
            }]) {
                Ok(_) => {
                    app.wifi_request(crate::wifi::Job::Disconnect);
                    app.set_msg("Wi-Fi network forgotten", false);
                }
                Err(e) => app.err(&e),
            }
            stay
        }
        Act::DefaultRoles => {
            let mut pol = app.state.policy.clone();
            pol.roles = Policy::default().roles;
            save_policy(app, by, now, pol, "Default permissions restored");
            stay
        }
    }
}

/// Run an admin action that `by` is allowed to do.
pub(crate) fn run<D: BlockDevice>(app: &mut App<D>, a: AdminAct, by: UserId) -> Screen {
    // Adding a user also runs from the till menu, and goes back there.
    if a != AdminAct::AddUser {
        app.place = Place::Settings;
    }
    let money = |app: &App<D>, m: Money| app.money(m);
    match a {
        AdminAct::Users => page(SetPage::Users, by),
        AdminAct::Roles => page(SetPage::Roles, by),
        AdminAct::Display => page(SetPage::Display, by),
        AdminAct::Wifi => page(SetPage::Wifi, by),
        AdminAct::AddUser => prompt(
            "New user",
            "Their name (1-24 characters)",
            false,
            Next::NewUserName(by),
        ),
        AdminAct::Goal => prompt(
            "Shift sales goal",
            &format!(
                "Sales total to aim for in one shift (now {}). The home screen shows \
                 progress. 0 turns it off.",
                if app.state.policy.shift_goal.is_zero() {
                    "off".into()
                } else {
                    money(app, app.state.policy.shift_goal)
                }
            ),
            false,
            Next::Goal(by),
        ),
        AdminAct::IdleSignout => prompt(
            "Automatic sign-out",
            &format!(
                "Minutes without a touch before the till signs out (not during a \
                 sale). 0 = never, at most {MAX_IDLE_MINS}."
            ),
            false,
            Next::IdleMins(by),
        ),
        AdminAct::StoreName => prompt(
            "Store name",
            &format!(
                "Shown on the home screen and receipts (now \"{}\")",
                app.state.config.store_name
            ),
            false,
            Next::StoreName(by),
        ),
    }
}

fn save_user<D: BlockDevice>(
    app: &mut App<D>,
    by: UserId,
    at: i64,
    user: pos_core::auth::User,
    msg: &str,
) -> bool {
    match app.commit(vec![Event::UserSet {
        by: Some(by),
        at,
        user,
    }]) {
        Ok(_) => {
            app.set_msg(msg, false);
            true
        }
        Err(e) => {
            app.err(&e);
            false
        }
    }
}

fn save_policy<D: BlockDevice>(app: &mut App<D>, by: UserId, at: i64, policy: Policy, msg: &str) {
    match app.commit(vec![Event::PolicySet { by, at, policy }]) {
        Ok(_) => app.set_msg(msg, false),
        Err(e) => app.err(&e),
    }
}

fn save_ui<D: BlockDevice>(
    app: &mut App<D>,
    by: UserId,
    at: i64,
    prefs: pos_core::ui::UiPrefs,
    msg: &str,
) {
    match app.commit(vec![Event::UiSet { by, at, prefs }]) {
        Ok(_) => app.set_msg(msg, false),
        Err(e) => app.err(&e),
    }
}

fn valid_name(s: &str) -> bool {
    let n = s.trim().chars().count();
    (1..=24).contains(&n)
}

/// Enter on an admin prompt.
pub(crate) fn submit<D: BlockDevice>(
    app: &mut App<D>,
    p: Prompt,
    field: String,
    now: i64,
    hw: &mut Hw<'_>,
) -> Screen {
    let again = |p: Prompt, app: &mut App<D>, msg: &str| {
        app.set_msg(msg, true);
        Screen::Prompt(p)
    };
    let done = |app: &mut App<D>, next: &Next| {
        return_to(app, next).unwrap_or_else(|| screens::login_screen(app))
    };
    match p.next.clone() {
        Next::NewUserName(by) => {
            if !valid_name(&field) {
                return again(p, app, "Name: 1-24 characters");
            }
            if name_taken(app, &field, None) {
                return again(p, app, "Someone already has that name");
            }
            prompt(
                "New user",
                &format!("{}'s role", field.trim()),
                false,
                Next::NewUserRole(field.trim().into(), by),
            )
        }
        Next::NewUserRole(name, by) => {
            let k: usize = field.trim().parse().unwrap_or(0);
            let Some(role) = Role::ALL.get(k.wrapping_sub(1)).copied() else {
                return again(p, app, "Pick a role");
            };
            prompt(
                "New user",
                &format!("Choose a 4-8 digit PIN for {name}"),
                true,
                Next::NewUserPin(name, role, by),
            )
        }
        Next::NewUserPin(name, role, by) => {
            if !auth::valid_pin(field.as_bytes()) {
                return again(p, app, "PIN must be 4-8 digits");
            }
            prompt(
                "New user",
                "Repeat the PIN",
                true,
                Next::NewUserPin2(name, role, field, by),
            )
        }
        Next::NewUserPin2(name, role, pin, by) => {
            if field != pin {
                app.set_msg("PINs differ - choose it again", true);
                return prompt(
                    "New user",
                    &format!("Choose a 4-8 digit PIN for {name}"),
                    true,
                    Next::NewUserPin(name, role, by),
                );
            }
            let id = app.state.users.keys().max().copied().unwrap_or(0) + 1;
            let Some(user) = screens::make_user(app, hw, id, &name, role, &pin) else {
                app.set_msg("No entropy source: cannot create users", true);
                return done(app, &p.next);
            };
            save_user(
                app,
                by,
                now,
                user,
                &format!(
                    "{name} added as {} - they sign in by tapping their name (or typing {id})",
                    role.label()
                ),
            );
            done(app, &p.next)
        }
        Next::RenameUser(id, by) => {
            if !valid_name(&field) {
                return again(p, app, "Name: 1-24 characters");
            }
            if name_taken(app, &field, Some(id)) {
                return again(p, app, "Someone already has that name");
            }
            if let Some(mut u) = app.state.users.get(&id).cloned() {
                u.name = field.trim().into();
                let msg = format!("Renamed to {}", u.name);
                save_user(app, by, now, u, &msg);
            }
            done(app, &p.next)
        }
        Next::ResetPin(id, by) => {
            if !auth::valid_pin(field.as_bytes()) {
                return again(p, app, "PIN must be 4-8 digits");
            }
            prompt(
                "Reset PIN",
                "Repeat the PIN",
                true,
                Next::ResetPin2(id, field, by),
            )
        }
        Next::ResetPin2(id, pin, by) => {
            if field != pin {
                app.set_msg("PINs differ - choose it again", true);
                return prompt(
                    "Reset PIN",
                    &format!("New 4-8 digit PIN for {}", app.user_name(id)),
                    true,
                    Next::ResetPin(id, by),
                );
            }
            let Some(old) = app.state.users.get(&id).cloned() else {
                return done(app, &p.next);
            };
            match screens::make_user(app, hw, id, &old.name, old.role, &pin) {
                Some(mut u) => {
                    u.active = old.active;
                    save_user(app, by, now, u, &format!("New PIN set for {}", old.name));
                }
                None => app.set_msg("No entropy source: cannot set PINs", true),
            }
            done(app, &p.next)
        }
        Next::MyPinOld => {
            let Some(me) = app.user else {
                return screens::login_screen(app);
            };
            let ok = app
                .state
                .users
                .get(&me)
                .is_some_and(|u| auth::verify_pin(&*app.hasher, field.as_bytes(), &u.pin));
            if !ok {
                // Counts towards lockout like any wrong PIN.
                if let Ok((ev, false)) = app.state.login(me, field.as_bytes(), &*app.hasher, now) {
                    let _ = app.commit(vec![ev]);
                }
                app.set_msg("Wrong PIN", true);
                return done(app, &p.next);
            }
            prompt(
                "Change my PIN",
                "Your new 4-8 digit PIN",
                true,
                Next::MyPinNew,
            )
        }
        Next::MyPinNew => {
            if !auth::valid_pin(field.as_bytes()) {
                return again(p, app, "PIN must be 4-8 digits");
            }
            prompt(
                "Change my PIN",
                "Repeat the new PIN",
                true,
                Next::MyPinNew2(field),
            )
        }
        Next::MyPinNew2(pin) => {
            let Some(me) = app.user else {
                return screens::login_screen(app);
            };
            if field != pin {
                app.set_msg("PINs differ - choose it again", true);
                return prompt(
                    "Change my PIN",
                    "Your new 4-8 digit PIN",
                    true,
                    Next::MyPinNew,
                );
            }
            let Some(old) = app.state.users.get(&me).cloned() else {
                return screens::login_screen(app);
            };
            match screens::make_user(app, hw, me, &old.name, old.role, &pin) {
                Some(mut u) => {
                    u.active = old.active;
                    save_user(app, me, now, u, "Your PIN is changed");
                }
                None => app.set_msg("No entropy source: cannot set PINs", true),
            }
            done(app, &p.next)
        }
        Next::Goal(by) => {
            let goal = if field.trim().is_empty() {
                Some(Money::ZERO)
            } else {
                parse_money(&field, app.state.config.decimals).filter(|m| !m.is_negative())
            };
            let Some(goal) = goal else {
                return again(p, app, "Enter an amount (0 = off)");
            };
            let mut pol = app.state.policy.clone();
            pol.shift_goal = goal;
            let msg = if goal.is_zero() {
                String::from("Sales goal off")
            } else {
                format!("Shift sales goal set to {}", app.money(goal))
            };
            save_policy(app, by, now, pol, &msg);
            done(app, &p.next)
        }
        Next::IdleMins(by) => {
            let m = match field.trim() {
                "" => Some(0),
                s => s.parse::<u16>().ok().filter(|m| *m <= MAX_IDLE_MINS),
            };
            let Some(m) = m else {
                return again(p, app, "Minutes: 0 to 240");
            };
            let mut pol = app.state.policy.clone();
            pol.idle_signout_mins = m;
            save_policy(
                app,
                by,
                now,
                pol,
                &format!("Automatic sign-out: {}", mins(m)),
            );
            done(app, &p.next)
        }
        Next::SaverMins(by) => {
            let m = match field.trim() {
                "" => Some(0),
                s => s
                    .parse::<u16>()
                    .ok()
                    .filter(|m| *m <= pos_core::ui::MAX_SAVER_MINS),
            };
            let Some(m) = m else {
                return again(p, app, "Minutes: 0 to 120");
            };
            let mut ui = app.state.ui.clone();
            ui.saver_mins = m;
            save_ui(app, by, now, ui, "Display & sound saved");
            done(app, &p.next)
        }
        Next::WifiPassword(ssid, by) => {
            let w = WifiSettingsCheck {
                ssid: &ssid,
                password: &field,
            };
            if let Err(e) = w.check() {
                return again(p, app, e);
            }
            app.wifi_request(crate::wifi::Job::Connect {
                ssid,
                password: field,
                by,
            });
            done(app, &p.next)
        }
        Next::StoreName(by) => {
            let name = field.trim();
            if name.is_empty() {
                return done(app, &p.next);
            }
            if name.chars().count() > 40 {
                return again(p, app, "At most 40 characters");
            }
            let mut cfg = app.state.config.clone();
            cfg.store_name = name.into();
            match app.commit(vec![Event::ConfigSet {
                by,
                at: now,
                config: cfg,
            }]) {
                Ok(_) => app.set_msg(format!("Store name: {name}"), false),
                Err(e) => app.err(&e),
            }
            done(app, &p.next)
        }
        _ => done(app, &p.next),
    }
}

struct WifiSettingsCheck<'a> {
    ssid: &'a str,
    password: &'a str,
}

impl WifiSettingsCheck<'_> {
    fn check(&self) -> Result<(), &'static str> {
        pos_core::ui::WifiSettings {
            ssid: self.ssid.into(),
            password: self.password.into(),
        }
        .validate()
    }
}

fn name_taken<D: BlockDevice>(app: &App<D>, name: &str, except: Option<UserId>) -> bool {
    let n = name.trim();
    app.state
        .users
        .values()
        .any(|u| Some(u.id) != except && u.active && u.name.eq_ignore_ascii_case(n))
}

/// Pick choices for admin prompts.
pub(crate) fn choices(next: &Next) -> Vec<(String, String)> {
    match next {
        Next::NewUserRole(..) => Role::ALL
            .iter()
            .enumerate()
            .map(|(k, r)| (r.label().to_string(), format!("{}", k + 1)))
            .collect(),
        Next::SaverMins(_) => [
            ("Off", "0"),
            ("2 min", "2"),
            ("5 min", "5"),
            ("10 min", "10"),
            ("30 min", "30"),
            ("60 min", "60"),
        ]
        .iter()
        .map(|(a, b)| (String::from(*a), String::from(*b)))
        .collect(),
        Next::IdleMins(_) => [
            ("Never", "0"),
            ("2 min", "2"),
            ("5 min", "5"),
            ("10 min", "10"),
            ("15 min", "15"),
            ("30 min", "30"),
            ("60 min", "60"),
        ]
        .iter()
        .map(|(a, b)| (String::from(*a), String::from(*b)))
        .collect(),
        _ => Vec::new(),
    }
}

fn about<D: BlockDevice>(app: &App<D>) -> Screen {
    let st = &app.state;
    let (used, total) = app.store_usage();
    let mut lines = vec![
        format!("POS appliance {}", env!("CARGO_PKG_VERSION")),
        format!("Store: {}", st.config.store_name),
        format!(
            "Network: {}{}",
            st.net.role.label(),
            if st.net.networked() {
                format!(", till {}", st.net.till)
            } else {
                String::new()
            }
        ),
        format!(
            "Users: {} active ({} managers)",
            st.users.values().filter(|u| u.active).count(),
            st.users
                .values()
                .filter(|u| u.active && u.role == Role::Manager)
                .count()
        ),
        format!(
            "Products: {}   Pictures: {}",
            st.catalog.len(),
            st.pictures.len()
        ),
        format!(
            "Journal: {} records, {}% of the data partition used",
            st.events,
            used * 100 / total.max(1)
        ),
        format!("Receipts issued: {}", st.next_receipt.saturating_sub(1)),
    ];
    if let Some(me) = app.user.and_then(|id| st.users.get(&id)) {
        lines.push(String::new());
        lines.push(format!(
            "Signed in: {} (#{}), {} - may do {} of {} things",
            me.name,
            me.id,
            me.role.label(),
            st.policy.count(me.role),
            Permission::ALL.len()
        ));
    }
    Screen::Info {
        title: "About this till".into(),
        lines,
    }
}

// ------------------------------------------------------------------ drawing

pub(crate) struct SettingsView {
    title: String,
    rows: Vec<Row>,
    sel: usize,
    hint: String,
}

pub(crate) fn view<D: BlockDevice>(app: &App<D>) -> Option<SettingsView> {
    let Screen::Settings { page, sel, by } = app.screen else {
        return None;
    };
    let hint = match page {
        SetPage::Role(_) => "Tap a line to allow it or require an override".into(),
        SetPage::Users => "Tap a user to change their role, PIN or status".into(),
        SetPage::Display => "Changes apply to this till only, straight away".into(),
        SetPage::Wifi if app.wifi_rt.present => {
            "Tap a network to join it; the till rejoins it at every start".into()
        }
        _ if Some(by) != app.user => format!("Authorised by {}", app.user_name(by)),
        _ => String::new(),
    };
    Some(SettingsView {
        title: title(app, page),
        rows: rows(app, page, by),
        sel,
        hint,
    })
}

pub(crate) fn draw(ui: &mut Ui<'_>, body: Rect, v: &SettingsView) -> Option<UiAction> {
    let u = ui.u();
    let (t, r) = body.split_top(7 * u);
    ui.label(t, &v.title, Font::Large, 1);
    let (h, r) = if v.hint.is_empty() {
        (Rect::new(r.x, r.y, r.w, 0), r)
    } else {
        r.split_top(ui.c.line_height(Font::Normal) + u)
    };
    if h.h > 0 {
        ui.label_color(h, &v.hint, Font::Normal, 1, ui.theme.muted);
    }
    let n = v.rows.len() as i32;
    // One column while rows stay finger-sized, else two.
    let min_h = ui.c.line_height(Font::Bold) + 3 * u;
    let cols = if n * min_h > r.h { 2 } else { 1 };
    let per_col = (n + cols - 1) / cols;
    let row_h = ((r.h - u) / per_col.max(1)).min(9 * u);
    let width = r.w.min(if cols == 1 { 110 * u } else { 150 * u });
    let area = r.centered(width, per_col * row_h);
    let col_w = (area.w - (cols - 1) * 2 * u) / cols;
    let mut action = None;
    for (k, rw) in v.rows.iter().enumerate() {
        let k = k as i32;
        let cell = Rect::new(
            area.x + (k / per_col) * (col_w + 2 * u),
            area.y + (k % per_col) * row_h,
            col_w,
            row_h,
        )
        .inset(u / 4);
        let selected = k as usize == v.sel;
        let back = matches!(rw.act, Act::Back);
        let t = ui.theme;
        let (bg, fg) = if selected {
            (t.accent, t.on_accent)
        } else if back {
            (t.ok, t.on_accent)
        } else if rw.dim {
            (t.panel, t.muted)
        } else {
            (t.button, t.button_text)
        };
        let bg = if ui.pressed_in(cell) {
            mix(bg, 0x00FF_FFFF, 90)
        } else {
            bg
        };
        let rad = ui.c.px(6);
        ui.c.fill_round_rect(cell, rad, bg);
        let inner = cell.inset(2 * u);
        if back {
            ui.label_color(inner, &rw.label, Font::Bold, 1, fg);
        } else {
            ui.label_color(inner, &rw.label, Font::Bold, 0, fg);
            let vw = ui.c.text_width(&rw.value, Font::Normal).min(inner.w / 2);
            let vr = Rect::new(inner.x + inner.w - vw, inner.y, vw, inner.h);
            ui.label_color(vr, &rw.value, Font::Normal, 2, fg);
        }
        if ui.target(cell, &rw.label) {
            action = Some(UiAction::Menu(k as usize));
        }
    }
    action
}
