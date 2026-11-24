//! The "desktop": the sign-in (lock) screen and the home screen with the
//! two apps, drawn over the wallpaper with frosted panels.

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use pos_hal::Key;
use pos_hal::Rgb888;
use pos_ui::image::Bitmap;
use pos_ui::{Font, Rect, Ui, mix};

use crate::screens::{Prompt, UiAction};

const WHITE: Rgb888 = 0x00FF_FFFF;
const GLASS: Rgb888 = 0x000A_0F1C;

/// Data for the desktop screens.
#[derive(Debug, Clone)]
pub(crate) struct DeskView {
    pub wall: Arc<Bitmap>,
    pub logo: Option<Arc<Bitmap>>,
    pub store: String,
    pub time: String,
    pub date: String,
    pub greeting: String,
    /// (id, name, role label, picture)
    pub users: Vec<(u16, String, &'static str, Option<Arc<Bitmap>>)>,
    pub me: Option<(u16, String, Option<Arc<Bitmap>>)>,
    pub till_sub: String,
    pub stock_sub: String,
    pub stock_alert: bool,
    pub net: String,
    /// Apps this user's role may open: (till, inventory).
    pub can_till: bool,
    pub can_stock: bool,
    pub can_reports: bool,
    pub reports_sub: String,
    pub settings_sub: String,
    /// Shift goal: caption and percent reached (None when off).
    pub goal: Option<(String, u32)>,
    /// One line of shift figures under the greeting.
    pub today: String,
}

fn glass(ui: &mut Ui<'_>, r: Rect, alpha: u8) {
    let rad = ui.u() * 2;
    ui.c.fill_round_rect_alpha(r, rad, GLASS, alpha);
}

/// A round avatar: the user's picture, or initials on a colour.
pub(crate) fn avatar(
    ui: &mut Ui<'_>,
    cx: i32,
    cy: i32,
    radius: i32,
    name: &str,
    pic: Option<&Bitmap>,
) {
    let r = Rect::new(cx - radius, cy - radius, 2 * radius, 2 * radius);
    match pic {
        Some(b) => ui.c.draw_bitmap(r, b, radius, 255),
        None => {
            let c = crate::stock::tile_color(name);
            ui.c.fill_gradient(r, radius, mix(c, WHITE, 50), mix(c, 0, 40));
            let initials: String = name
                .split_whitespace()
                .filter_map(|w| w.chars().next())
                .take(2)
                .collect::<String>()
                .to_uppercase();
            let f = if radius * 2 >= ui.c.line_height(Font::Huge) {
                Font::Huge
            } else if radius * 2 >= ui.c.line_height(Font::Large) {
                Font::Large
            } else {
                Font::Bold
            };
            ui.label_color(r, &initials, f, 1, WHITE);
        }
    }
}

fn top_bar(ui: &mut Ui<'_>, v: &DeskView, show_clock: bool) -> Rect {
    let u = ui.u();
    let full = ui.c.bounds();
    let h = ui.c.line_height(Font::Normal) + 2 * u;
    let bar = Rect::new(0, 0, full.w, h);
    ui.c.fill_round_rect_alpha(bar, 0, GLASS, 110);
    let inner = bar.inset(u);
    let mut x = inner.x;
    if let Some(l) = &v.logo {
        let lh = inner.h;
        let lw = (l.w * lh / l.h.max(1)).min(lh * 4);
        ui.c.draw_bitmap(Rect::new(x, inner.y, lw, lh), l, 0, 255);
        x += lw + u;
    }
    ui.label_color(
        Rect::new(x, inner.y, inner.w / 3, inner.h),
        &v.store,
        Font::Bold,
        0,
        WHITE,
    );
    if show_clock {
        ui.label_color(inner, &v.time, Font::Bold, 1, WHITE);
    }
    ui.label_color(inner, &v.net, Font::Small, 2, mix(WHITE, 0, 60));
    bar
}

fn message(ui: &mut Ui<'_>, msg: &Option<(String, bool)>, y: i32) {
    let Some((m, err)) = msg else { return };
    let u = ui.u();
    let full = ui.c.bounds();
    let w = (ui.c.text_width(m, Font::Bold) + 4 * u).min(full.w - 4 * u);
    let h = ui.c.line_height(Font::Bold) + 2 * u;
    let r = Rect::new((full.w - w) / 2, y, w, h);
    let c = if *err { ui.theme.danger } else { ui.theme.ok };
    ui.c.fill_round_rect_alpha(r, h / 2, c, 235);
    ui.label_color(r, m, Font::Bold, 1, WHITE);
}

/// Sign-in: big clock, user tiles; after picking a user, their avatar and
/// a PIN pad.
pub(crate) fn draw_lock(
    ui: &mut Ui<'_>,
    v: &DeskView,
    p: &Prompt,
    msg: &Option<(String, bool)>,
    pad: &mut dyn FnMut(&mut Ui<'_>, Rect) -> Option<UiAction>,
) -> Option<UiAction> {
    let u = ui.u();
    let full = ui.c.bounds();
    ui.c.blit_full(&v.wall);
    let bar = top_bar(ui, v, false);
    let mut action = None;
    let pin_user = match p.next {
        crate::screens::Next::LoginPin(id) => Some(id),
        _ => None,
    };
    // Clock block.
    let huge = ui.c.line_height(Font::Huge);
    let mut y = bar.h + 3 * u;
    if pin_user.is_none() {
        ui.label_color(Rect::new(0, y, full.w, huge), &v.time, Font::Huge, 1, WHITE);
        y += huge;
        let nh = ui.c.line_height(Font::Normal);
        ui.label_color(
            Rect::new(0, y, full.w, nh),
            &v.date,
            Font::Normal,
            1,
            mix(WHITE, 0, 40),
        );
        y += nh + 3 * u;
    }
    match pin_user {
        None => {
            let n = v.users.len() as i32;
            let bottom = full.h - 3 * u;
            if n == 0 || n > 12 {
                // Too many tiles: type the number.
                let w = full.w.min(60 * u);
                let r = Rect::new((full.w - w) / 2, y, w, bottom - y);
                glass(ui, r, 150);
                let inner = r.inset(2 * u);
                let (t, rest) = inner.split_top(ui.c.line_height(Font::Large) + u);
                ui.label_color(
                    t,
                    &format!("User number: {}_", p.field),
                    Font::Large,
                    1,
                    WHITE,
                );
                action = pad(ui, rest);
            } else {
                let cols = n.min(6);
                let rows = (n + cols - 1) / cols;
                let avail_h = bottom - y - 6 * u;
                let cell_w = ((full.w - 8 * u) / cols).min(28 * u);
                let cell_h = (avail_h / rows).min(26 * u);
                let grid = Rect::new(
                    (full.w - cols * cell_w) / 2,
                    y,
                    cols * cell_w,
                    rows * cell_h,
                );
                let nh = ui.c.line_height(Font::Bold);
                let sh = ui.c.line_height(Font::Small);
                for (k, (id, name, role, pic)) in v.users.iter().enumerate() {
                    let k = k as i32;
                    let cell = Rect::new(
                        grid.x + (k % cols) * cell_w,
                        grid.y + (k / cols) * cell_h,
                        cell_w,
                        cell_h,
                    )
                    .inset(u / 2);
                    let pressed = ui.pressed_in(cell);
                    glass(ui, cell, if pressed { 200 } else { 120 });
                    let rad = ((cell.h - nh - sh - 3 * u) / 2).min(cell.w / 2 - u).max(u);
                    let cx = cell.x + cell.w / 2;
                    let cy = cell.y + u + rad;
                    avatar(ui, cx, cy, rad, name, pic.as_deref());
                    let ny = cy + rad + u / 2;
                    ui.label_color(
                        Rect::new(cell.x, ny, cell.w, nh),
                        name,
                        Font::Bold,
                        1,
                        WHITE,
                    );
                    ui.label_color(
                        Rect::new(cell.x, ny + nh, cell.w, sh),
                        role,
                        Font::Small,
                        1,
                        mix(WHITE, 0, 70),
                    );
                    if ui.target(cell, &format!("{name} ({id})")) {
                        action = Some(UiAction::Submit(format!("{id}")));
                    }
                }
                let hint_y = grid.y + grid.h + u;
                let hint = if p.field.is_empty() {
                    String::from("Tap your name, or type your name or number and press Enter")
                } else {
                    format!("User number: {}_", p.field)
                };
                ui.label_color(
                    Rect::new(0, hint_y, full.w, ui.c.line_height(Font::Normal)),
                    &hint,
                    Font::Normal,
                    1,
                    mix(WHITE, 0, 50),
                );
            }
        }
        Some(id) => {
            let (name, pic) = v
                .users
                .iter()
                .find(|x| x.0 == id)
                .map(|x| (x.1.clone(), x.3.clone()))
                .unwrap_or_else(|| (format!("{id}"), None));
            let w = full.w.min(54 * u);
            let panel = Rect::new((full.w - w) / 2, y, w, full.h - y - 2 * u);
            glass(ui, panel, 150);
            let inner = panel.inset(2 * u);
            let rad = (inner.h / 9).clamp(3 * u, 7 * u);
            let cx = inner.x + inner.w / 2;
            avatar(ui, cx, inner.y + rad, rad, &name, pic.as_deref());
            let mut yy = inner.y + 2 * rad + u;
            let lh = ui.c.line_height(Font::Large);
            ui.label_color(
                Rect::new(inner.x, yy, inner.w, lh),
                &name,
                Font::Large,
                1,
                WHITE,
            );
            yy += lh + u;
            // PIN dots.
            let dot = u;
            let slots = p.field.chars().count().max(4) as i32;
            let dw = slots * dot * 3;
            let mut dx = cx - dw / 2 + dot * 3 / 2;
            for k in 0..slots {
                let filled = (k as usize) < p.field.chars().count();
                let c = if filled { WHITE } else { mix(WHITE, 0, 150) };
                ui.c.fill_circle(dx, yy + dot, if filled { dot } else { dot * 2 / 3 }, c);
                dx += dot * 3;
            }
            yy += 3 * dot + u;
            let padr = Rect::new(inner.x, yy, inner.w, inner.y + inner.h - yy);
            action = pad(ui, padr);
        }
    }
    message(ui, msg, full.h - ui.c.line_height(Font::Bold) - 5 * u);
    action
}

/// One app on the home screen: a glass card with an icon.
fn app_card(ui: &mut Ui<'_>, r: Rect, name: &str, sub: &str, icon: Icon, alert: bool) -> bool {
    let u = ui.u();
    let pressed = ui.pressed_in(r);
    glass(ui, r, if pressed { 210 } else { 140 });
    let side = (r.h * 45 / 100).min(r.w - 4 * u);
    let ir = Rect::new(r.x + (r.w - side) / 2, r.y + 3 * u, side, side);
    draw_icon(ui, ir, icon);
    if alert {
        let br = side / 7;
        ui.c.fill_circle(ir.x + ir.w - br / 2, ir.y + br / 2, br, ui.theme.danger);
    }
    let mut y = ir.y + ir.h + 2 * u;
    let lh = ui.c.line_height(Font::Large);
    ui.label_color(Rect::new(r.x, y, r.w, lh), name, Font::Large, 1, WHITE);
    y += lh + u / 2;
    let sub_h = (2 * ui.c.line_height(Font::Normal)).min(r.y + r.h - y - u);
    let sub_r = Rect::new(r.x + 2 * u, y, r.w - 4 * u, sub_h);
    ui.c.text_wrapped(sub_r, sub, Font::Normal, mix(WHITE, 0, 50));
    ui.target(r, name)
}

#[derive(Clone, Copy)]
enum Icon {
    Till,
    Stock,
    Settings,
    Reports,
}

/// Unit vectors (×100) for 8 compass directions, for the gear and confetti.
const DIRS: [(i32, i32); 8] = [
    (100, 0),
    (71, 71),
    (0, 100),
    (-71, 71),
    (-100, 0),
    (-71, -71),
    (0, -100),
    (71, -71),
];

/// A glass bar with a coloured fill for the shift goal; confetti at 100 %.
fn goal_bar(ui: &mut Ui<'_>, r: Rect, caption: &str, pct: u32) {
    let u = ui.u();
    glass(ui, r, 150);
    let track = r.inset(u / 2);
    let fill_w = track.w * pct.min(100) as i32 / 100;
    let rad = track.h / 2;
    let done = pct >= 100;
    if fill_w > 0 {
        let (top, bot) = if done {
            (0x00FB_BF24, 0x00F5_9E0B)
        } else {
            (0x0034_D399, 0x0005_9669)
        };
        ui.c.fill_gradient(
            Rect::new(track.x, track.y, fill_w.max(2 * rad), track.h),
            rad,
            top,
            bot,
        );
    }
    if done {
        confetti(ui, r);
    }
    ui.label_color(r, caption, Font::Bold, 1, WHITE);
}

/// Deterministic sprinkles around a rectangle (a small celebration).
fn confetti(ui: &mut Ui<'_>, r: Rect) {
    const COLORS: [Rgb888; 6] = [
        0x00F4_3F5E,
        0x00FB_BF24,
        0x0034_D399,
        0x0060_A5FA,
        0x00C0_84FC,
        0x00FF_FFFF,
    ];
    let u = ui.u();
    let mut seed: u32 = 0x9E37_79B9;
    for k in 0..36 {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        // Above the bar only, so the line below stays readable.
        let x = r.x + (seed % r.w.max(1) as u32) as i32;
        let dy = ((seed >> 8) % (3 * u.max(1) as u32)) as i32;
        let y = r.y - u - dy;
        let c = COLORS[(seed >> 4) as usize % COLORS.len()];
        let sz = u / 2 + ((seed >> 12) % 3) as i32 * u / 4;
        if k % 3 == 0 {
            ui.c.fill_circle(x, y, sz.max(1), c);
        } else {
            let (dx, dy) = DIRS[(seed >> 16) as usize % 8];
            let w = (sz * 2 + dx.abs() * sz / 100).max(2);
            let h = (sz + dy.abs() * sz / 100).max(2);
            ui.c.fill_round_rect(Rect::new(x, y, w, h), 1, c);
        }
    }
}

/// App icons drawn from shapes: a rounded gradient square with a
/// pictogram (cash register; stacked boxes).
fn draw_icon(ui: &mut Ui<'_>, r: Rect, icon: Icon) {
    let rad = r.w * 22 / 100;
    let (top, bot) = match icon {
        Icon::Till => (0x0034_D399, 0x0005_7A55),
        Icon::Stock => (0x00FB_BF24, 0x00EA_580C),
        Icon::Settings => (0x0094_A3B8, 0x0033_4155),
        Icon::Reports => (0x0060_A5FA, 0x0025_63EB),
    };
    ui.c.fill_gradient(r, rad, top, bot);
    let s = r.w;
    let px = |f: i32| f * s / 100;
    let at = |x: i32, y: i32, w: i32, h: i32| Rect::new(r.x + px(x), r.y + px(y), px(w), px(h));
    let soft = mix(WHITE, bot, 30);
    match icon {
        Icon::Till => {
            // Display on a stem, body, keys, drawer.
            ui.c.fill_round_rect(at(52, 18, 30, 18), px(3), WHITE);
            ui.c.fill_round_rect(at(56, 22, 22, 10), px(2), bot);
            ui.c.fill_rect(at(64, 36, 6, 8), WHITE);
            ui.c.fill_round_rect(at(18, 42, 64, 26), px(5), WHITE);
            for row in 0..2 {
                for col in 0..4 {
                    let k = at(24 + col * 14, 47 + row * 10, 9, 6);
                    ui.c.fill_round_rect(k, px(1), soft);
                }
            }
            ui.c.fill_round_rect(at(14, 70, 72, 14), px(4), WHITE);
            ui.c.fill_round_rect(at(42, 75, 16, 4), px(2), bot);
        }
        Icon::Stock => {
            // Three cartons, two below and one on top.
            for (x, y) in [(16, 50), (52, 50), (34, 18)] {
                let b = at(x, y, 32, 30);
                ui.c.fill_round_rect(b, px(3), WHITE);
                ui.c.fill_rect(at(x + 13, y, 6, 12), soft);
                ui.c.fill_rect(at(x + 5, y + 20, 10, 3), soft);
            }
        }
        Icon::Reports => {
            // A bar chart on a baseline, rising to the right.
            for (k, h) in [30, 48, 38, 62].into_iter().enumerate() {
                let x = 20 + k as i32 * 16;
                let b = at(x, 78 - h, 11, h);
                ui.c.fill_round_rect(b, px(2), WHITE);
                ui.c.fill_rect(at(x, 76, 11, 2), WHITE);
            }
            ui.c.fill_round_rect(at(14, 80, 72, 4), px(2), soft);
        }
        Icon::Settings => {
            // A gear: teeth around a wheel with a hole.
            let (cx, cy) = (r.x + px(50), r.y + px(50));
            let t = px(14);
            for (dx, dy) in DIRS {
                let tx = cx + px(29) * dx / 100;
                let ty = cy + px(29) * dy / 100;
                ui.c.fill_round_rect(Rect::new(tx - t / 2, ty - t / 2, t, t), px(2), WHITE);
            }
            ui.c.fill_circle(cx, cy, px(27), WHITE);
            ui.c.fill_circle(cx, cy, px(11), mix(top, bot, 128));
        }
    }
}

pub(crate) fn draw_home(
    ui: &mut Ui<'_>,
    v: &DeskView,
    msg: &Option<(String, bool)>,
) -> Option<UiAction> {
    let u = ui.u();
    let full = ui.c.bounds();
    ui.c.blit_full(&v.wall);
    let bar = top_bar(ui, v, true);
    let mut action = None;
    // Greeting with avatar.
    let mut y = bar.h + 4 * u;
    let lh = ui.c.line_height(Font::Large);
    if let Some((_, name, pic)) = &v.me {
        let rad = lh;
        avatar(ui, full.w / 2, y + rad, rad, name, pic.as_deref());
        y += 2 * rad + u;
    }
    ui.label_color(
        Rect::new(0, y, full.w, lh),
        &v.greeting,
        Font::Large,
        1,
        WHITE,
    );
    y += lh;
    let nh = ui.c.line_height(Font::Normal);
    ui.label_color(
        Rect::new(0, y, full.w, nh),
        &v.date,
        Font::Normal,
        1,
        mix(WHITE, 0, 50),
    );
    y += nh + 4 * u;
    // Dock.
    let dock_h = 8 * u;
    let dock_w = full.w.min(70 * u);
    let dock = Rect::new(
        (full.w - dock_w) / 2,
        full.h - dock_h - 2 * u,
        dock_w,
        dock_h,
    );
    // Goal progress.
    if let Some((caption, pct)) = &v.goal {
        let gh = ui.c.line_height(Font::Bold) + 3 * u;
        let gw = full.w.min(80 * u);
        let gr = Rect::new((full.w - gw) / 2, y, gw, gh);
        goal_bar(ui, gr, caption, *pct);
        y += gh + 2 * u;
    }
    if !v.today.is_empty() {
        let sh = ui.c.line_height(Font::Normal);
        ui.label_color(
            Rect::new(0, y, full.w, sh),
            &v.today,
            Font::Normal,
            1,
            mix(WHITE, 0, 40),
        );
        y += sh + 2 * u;
    }
    // App cards: only those this role may use, plus Settings.
    let mut cards: Vec<(&str, &str, Icon, bool, char)> = Vec::new();
    if v.can_till {
        cards.push(("Till", &v.till_sub, Icon::Till, false, '1'));
    }
    if v.can_stock {
        cards.push(("Inventory", &v.stock_sub, Icon::Stock, v.stock_alert, '2'));
    }
    if v.can_reports {
        cards.push(("Reports", &v.reports_sub, Icon::Reports, false, '4'));
    }
    cards.push(("Settings", &v.settings_sub, Icon::Settings, false, '3'));
    let n = cards.len() as i32;
    let gap = 4 * u;
    let avail = dock.y - y - 3 * u;
    let card_h = avail.min(44 * u);
    let card_w = (card_h * 9 / 10).min((full.w - 4 * u - (n + 1) * gap) / n);
    let x0 = (full.w - n * card_w - (n - 1) * gap) / 2;
    let y0 = y + (avail - card_h) / 2;
    for (k, (name, sub, icon, alert, key)) in cards.into_iter().enumerate() {
        let r = Rect::new(x0 + k as i32 * (card_w + gap), y0, card_w, card_h);
        if app_card(ui, r, name, sub, icon, alert) {
            action = Some(UiAction::Key(Key::Char(key)));
        }
    }
    glass(ui, dock, 150);
    let cells = dock.inset(u).grid(2, 1, u);
    let font = Font::Bold;
    if ui.button_font(cells[0], "Menu", pos_ui::ButtonStyle::Primary, font) {
        action = Some(UiAction::Key(Key::F(10)));
    }
    if ui.button_font(cells[1], "Sign out", pos_ui::ButtonStyle::Normal, font) {
        action = Some(UiAction::Key(Key::F(12)));
    }
    message(ui, msg, dock.y - ui.c.line_height(Font::Bold) - 3 * u);
    action
}

/// A quick key showing the product picture above its name.
pub(crate) fn picture_key(ui: &mut Ui<'_>, r: Rect, name: &str, pic: &Bitmap, font: Font) -> bool {
    let u = ui.u();
    let pressed = ui.pressed_in(r);
    let bg = if pressed {
        mix(ui.theme.button, WHITE, 90)
    } else {
        ui.theme.button
    };
    let rad = ui.c.px(6);
    ui.c.fill_round_rect(r, rad, bg);
    let lh = ui.c.line_height(font);
    let ph = (r.h - lh - u).max(u);
    let pr = Rect::new(r.x + u / 2, r.y + u / 2, r.w - u, ph - u / 2);
    ui.c.draw_bitmap(pr, pic, rad, 255);
    let tr = Rect::new(r.x + u / 2, r.y + ph, r.w - u, r.h - ph);
    ui.label_color(tr, name, font, 1, ui.theme.button_text);
    ui.target(r, name)
}

const DAYS: [&str; 7] = [
    "Thursday",
    "Friday",
    "Saturday",
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
];
const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

/// The clock screen saver: a dim clock on black that moves every minute
/// so nothing burns into the panel.
pub(crate) fn draw_saver(ui: &mut Ui<'_>, store: &str, now: i64) {
    let u = ui.u();
    let full = ui.c.bounds();
    ui.c.fill_rect(full, 0);
    let (time, date) = match pos_hal::DateTime::from_unix(now) {
        Some(d) => (
            format!("{:02}:{:02}", d.hour, d.minute),
            format!(
                "{}, {} {}",
                DAYS[(now.div_euclid(86_400).rem_euclid(7)) as usize],
                d.day,
                MONTHS[usize::from(d.month.clamp(1, 12)) - 1]
            ),
        ),
        None => ("--:--".into(), String::new()),
    };
    let huge = ui.c.line_height(Font::Huge);
    let nh = ui.c.line_height(Font::Normal);
    let bw = (ui.c.text_width(&time, Font::Huge) + 4 * u)
        .max(ui.c.text_width(&date, Font::Normal) + 4 * u)
        .min(full.w);
    let bh = huge + 2 * nh + 2 * u;
    // A new spot each minute (deterministic, spread over the screen).
    let m = now.div_euclid(60) as u64;
    let hx = (m.wrapping_mul(2_654_435_761) >> 7) as i32;
    let hy = (m.wrapping_mul(40_503) >> 3) as i32;
    let x = hx.rem_euclid((full.w - bw).max(1));
    let y = hy.rem_euclid((full.h - bh).max(1));
    let dim = 0x00B8_C0CC;
    ui.label_color(Rect::new(x, y, bw, huge), &time, Font::Huge, 1, dim);
    ui.label_color(
        Rect::new(x, y + huge, bw, nh),
        &date,
        Font::Normal,
        1,
        mix(dim, 0, 90),
    );
    ui.label_color(
        Rect::new(x, y + huge + nh + u, bw, nh),
        store,
        Font::Small,
        1,
        mix(dim, 0, 140),
    );
}

/// Pictures the desktop screens draw.
pub(crate) fn wanted<D: pos_hal::BlockDevice>(
    app: &crate::App<D>,
    out: &mut Vec<pos_core::media::PictureFor>,
) {
    use pos_core::media::PictureFor;
    out.push(PictureFor::Logo);
    out.extend(
        app.state
            .users
            .values()
            .filter(|u| u.active)
            .take(13)
            .map(|u| PictureFor::User(u.id)),
    );
}

pub(crate) fn view<D: pos_hal::BlockDevice>(app: &mut crate::App<D>, now: i64) -> DeskView {
    use pos_core::media::PictureFor;
    let (w, h) = (app.canvas.width(), app.canvas.height());
    let wall = app.gallery.wallpaper(&app.state.pictures, w, h);
    let dt = pos_hal::DateTime::from_unix(now);
    let (time, date, hour) = match dt {
        Some(d) => (
            format!("{:02}:{:02}", d.hour, d.minute),
            format!(
                "{}, {} {} {}",
                DAYS[(now.div_euclid(86_400).rem_euclid(7)) as usize],
                d.day,
                MONTHS[usize::from(d.month.clamp(1, 12)) - 1],
                d.year
            ),
            d.hour,
        ),
        None => ("--:--".into(), String::new(), 12),
    };
    let users: Vec<(u16, String, &'static str, Option<Arc<Bitmap>>)> = app
        .state
        .users
        .values()
        .filter(|u| u.active)
        .map(|u| {
            (
                u.id,
                u.name.clone(),
                u.role.label(),
                app.gallery.get(&PictureFor::User(u.id)),
            )
        })
        .collect();
    let me = app.user.map(|id| {
        (
            id,
            app.user_name(id),
            app.gallery.get(&PictureFor::User(id)),
        )
    });
    let part = match hour {
        0..=11 => "Good morning",
        12..=17 => "Good afternoon",
        _ => "Good evening",
    };
    let first = me
        .as_ref()
        .and_then(|m| m.1.split_whitespace().next().map(String::from))
        .unwrap_or_default();
    let till_sub = match &app.state.shift {
        Some(s) => format!("Shift {} open · {} sales", s.id, s.counters.sales),
        None => "No shift open - tap to start".into(),
    };
    let inv = &app.state.inventory;
    let alerts = inv.alerts().len();
    let stock_sub = if inv.tracked() == 0 {
        format!(
            "{} products · no stock tracked yet",
            app.state.catalog.len()
        )
    } else if alerts > 0 {
        format!(
            "{alerts} product{} low or out of stock",
            if alerts == 1 { "" } else { "s" }
        )
    } else {
        let n = inv.tracked();
        format!("{n} product{} in stock", if n == 1 { "" } else { "s" })
    };
    let me_id = app.user;
    let can_till = app.can_use(crate::Place::Till);
    let can_stock = app.can_use(crate::Place::Inventory);
    let can_reports = app.can_use(crate::Place::Reports);
    let reports_sub = match &app.state.shift {
        Some(s) => format!(
            "{} sales · {}",
            s.counters.sales,
            app.money(s.counters.sales_total)
        ),
        None => "Sales by hour · best sellers".into(),
    };
    let (goal, today) = shift_figures(app, me_id);
    let settings_sub = match me_id {
        Some(id)
            if app
                .state
                .allows(id, pos_core::auth::Permission::ManageUsers) =>
        {
            let n = app.state.users.values().filter(|u| u.active).count();
            format!("{n} user{} · roles · goal", if n == 1 { "" } else { "s" })
        }
        _ => "My PIN · about this till".into(),
    };
    DeskView {
        can_till,
        can_stock,
        can_reports,
        reports_sub,
        settings_sub,
        goal,
        today,
        wall,
        logo: app.gallery.get(&PictureFor::Logo),
        store: app.state.config.store_name.clone(),
        time,
        date,
        greeting: if first.is_empty() {
            part.into()
        } else {
            format!("{part}, {first}")
        },
        users,
        me,
        till_sub,
        stock_sub,
        stock_alert: alerts > 0,
        net: net_line(app, now),
    }
}

/// Shift goal bar and a line of shift figures (sales from memory: the last
/// receipts kept for reprints).
fn shift_figures<D: pos_hal::BlockDevice>(
    app: &crate::App<D>,
    me: Option<u16>,
) -> (Option<(String, u32)>, String) {
    let Some(shift) = &app.state.shift else {
        return (None, String::new());
    };
    let total = shift.counters.sales_total;
    let goal = app.state.policy.shift_goal;
    let mut mine = 0u32;
    let mut sold: alloc::collections::BTreeMap<&str, i64> = alloc::collections::BTreeMap::new();
    for j in &app.recent {
        if let crate::Journaled::Sale(s, _) = j
            && s.shift_id == shift.id
        {
            if Some(s.cashier) == me {
                mine += 1;
            }
            for l in &s.lines {
                *sold.entry(l.line.name.as_str()).or_default() += l.line.qty.0;
            }
        }
    }
    let mut parts = Vec::new();
    if me.is_some() && mine > 0 {
        parts.push(format!(
            "You: {mine} sale{}",
            if mine == 1 { "" } else { "s" }
        ));
    }
    parts.push(format!(
        "Shift {}: {} sales, {}",
        shift.id,
        shift.counters.sales,
        app.money(total)
    ));
    if let Some((name, q)) = sold.iter().max_by_key(|(_, q)| **q) {
        parts.push(format!("Top: {name} x{}", pos_core::Qty(*q)));
    }
    let today = parts.join("  ·  ");
    if goal.is_zero() || goal.is_negative() {
        return (None, today);
    }
    let pct = (total.0.max(0).saturating_mul(100) / goal.0.max(1)).min(999) as u32;
    let caption = if pct >= 100 {
        format!(
            "Goal reached! {} of {} - {pct}%",
            app.money(total),
            app.money(goal)
        )
    } else {
        format!(
            "Shift goal: {} of {} ({pct}%) - {} to go",
            app.money(total),
            app.money(goal),
            app.money(pos_core::Money(goal.0 - total.0))
        )
    };
    (Some((caption, pct)), today)
}

fn net_line<D: pos_hal::BlockDevice>(app: &crate::App<D>, now: i64) -> String {
    use pos_core::sync::NetRole;
    let n = &app.state.net;
    if !n.networked() {
        return String::new();
    }
    let s = app.net_status();
    let mut out = format!("Till {}", n.till);
    if !s.is_empty() {
        out.push_str(" · ");
        out.push_str(s);
    }
    if n.role == NetRole::Head {
        let (count, total, _) = app.state.sync.store_today(now);
        out.push_str(&format!(
            " · store today {count} sales {}",
            app.money(total)
        ));
    }
    out
}
