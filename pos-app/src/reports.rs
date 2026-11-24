//! The Reports app: how the shift is going, at a glance. Stat tiles, sales
//! by hour (tap a bar for its figures), best sellers, cashiers and payment
//! types. Reads `PosState::stats` and the shift counters; changes nothing.
//!
//! Charts follow one rule set: one series per chart in the accent hue,
//! thin bars with a rounded end on the value side and a gap between bars,
//! a quiet grid, values in text colours (never the bar colour), and a
//! direct label only on the peak and the tapped bar.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use pos_core::Money;
use pos_core::shift::ShiftStats;
use pos_hal::{BlockDevice, Key, Rgb888};
use pos_ui::{ButtonStyle, Font, Rect, Ui, mix};

use crate::screens::{self, Pending, Screen, UiAction};
use crate::{App, Hw, Place};

pub(crate) fn open<D: BlockDevice>(app: &mut App<D>) -> Screen {
    app.place = Place::Reports;
    Screen::Reports { hour: None }
}

pub(crate) fn key<D: BlockDevice>(
    app: &mut App<D>,
    hour: Option<u8>,
    key: Key,
    hw: &mut Hw<'_>,
) -> Screen {
    let span = hour_span(app);
    let step = |d: i32| -> Option<u8> {
        let (a, b) = span;
        let cur = i32::from(hour.unwrap_or(b));
        Some((cur + d).clamp(i32::from(a), i32::from(b)) as u8)
    };
    match key {
        Key::Left => Screen::Reports { hour: step(-1) },
        Key::Right => Screen::Reports { hour: step(1) },
        Key::F(1) | Key::Char('p' | 'P') => {
            if app.state.shift.is_none() {
                app.set_msg("No shift open", true);
                return Screen::Reports { hour };
            }
            screens::authorize(app, Pending::XReport, hw)
        }
        Key::Escape | Key::Home => screens::go_home(app),
        Key::F(12) => screens::sign_out(app, hw),
        _ => Screen::Reports { hour },
    }
}

/// First and last hour shown: the shift's opening hour to now (or the
/// last sale), at least eight hours wide.
fn hour_span<D: BlockDevice>(app: &App<D>) -> (u8, u8) {
    let st = &app.state;
    let open = st.shift.as_ref().map(|s| ShiftStats::hour_of(s.opened_at));
    let first = st.stats.hours.keys().next().copied();
    let last = st.stats.hours.keys().next_back().copied();
    let now = ShiftStats::hour_of(app.now_unix());
    let a = open.or(first).unwrap_or(now).min(first.unwrap_or(23));
    let mut b = last
        .unwrap_or(a)
        .max(if st.shift.is_some() { now } else { 0 });
    if b < a {
        // Shift ran past midnight: show the whole day.
        return (0, 23);
    }
    if b - a < 7 {
        b = (a + 7).min(23);
    }
    let a = a.min(b.saturating_sub(7));
    (a, b)
}

pub(crate) struct ReportsView {
    title: String,
    tiles: Vec<(String, String, String)>,
    /// (hour, sales, total)
    hours: Vec<(u8, u32, Money)>,
    hour: Option<u8>,
    items: Vec<(String, String, Money)>,
    cashiers: Vec<(String, String, Money)>,
    tenders: Vec<(String, Money)>,
    decimals: u8,
    partial: bool,
}

pub(crate) fn view<D: BlockDevice>(app: &App<D>) -> Option<ReportsView> {
    let Screen::Reports { hour } = app.screen else {
        return None;
    };
    let st = &app.state;
    let m = |x: Money| app.money(x);
    let (title, counters, partial) = match &st.shift {
        Some(s) => {
            let t = pos_hal::DateTime::from_unix(s.opened_at)
                .map(|d| format!("{:02}:{:02}", d.hour, d.minute))
                .unwrap_or_default();
            (
                format!(
                    "Shift {} - opened {t} by {}",
                    s.id,
                    app.user_name(s.opened_by)
                ),
                Some(s.counters.clone()),
                st.stats.shift_id != s.id,
            )
        }
        None if st.stats.shift_id > 0 => {
            (format!("Shift {} (closed)", st.stats.shift_id), None, false)
        }
        None => ("No shift yet".into(), None, false),
    };
    let sales: u32 = st.stats.hours.values().map(|h| h.0).sum();
    let total = Money(st.stats.hours.values().map(|h| h.1.0).sum());
    let avg = if sales > 0 {
        Money(total.0 / i64::from(sales))
    } else {
        Money::ZERO
    };
    let (ref_n, ref_t) = counters
        .as_ref()
        .map_or((0, Money::ZERO), |c| (c.refunds, c.refunds_total));
    let tiles = alloc::vec![
        ("Sales".into(), m(total), st.config.currency.clone()),
        (
            "Receipts".into(),
            format!("{sales}"),
            "completed sales".into()
        ),
        ("Average sale".into(), m(avg), "per receipt".into()),
        (
            "Refunds".into(),
            m(ref_t),
            format!("{ref_n} refund{}", if ref_n == 1 { "" } else { "s" }),
        ),
    ];
    let (a, b) = hour_span(app);
    let hours = (a..=b)
        .map(|h| {
            let (n, t) = st.stats.hours.get(&h).copied().unwrap_or_default();
            (h, n, t)
        })
        .collect();
    let items = st
        .stats
        .top_items(5)
        .into_iter()
        .map(|(_, i)| (i.name.clone(), format!("{}", pos_core::Qty(i.qty)), i.total))
        .collect();
    let mut cashiers: Vec<(String, String, Money)> = st
        .stats
        .cashiers
        .iter()
        .map(|(id, (n, t))| {
            (
                app.user_name(*id),
                format!("{n} sale{}", if *n == 1 { "" } else { "s" }),
                *t,
            )
        })
        .collect();
    cashiers.sort_by_key(|c| core::cmp::Reverse(c.2));
    cashiers.truncate(4);
    // Takings per payment type: cash net of change given.
    let tenders = counters
        .map(|c| {
            c.tenders
                .iter()
                .map(|(k, v)| {
                    let mut l = String::from(k.label());
                    l[1..].make_ascii_lowercase();
                    let net = if *k == pos_core::tender::TenderKind::Cash {
                        Money(v.0 - c.change_given.0)
                    } else {
                        *v
                    };
                    (l, net)
                })
                .collect()
        })
        .unwrap_or_default();
    Some(ReportsView {
        title,
        tiles,
        hours,
        hour,
        items,
        cashiers,
        tenders,
        decimals: st.config.decimals,
        partial,
    })
}

fn card(ui: &mut Ui<'_>, r: Rect) {
    let rad = ui.c.px(8);
    ui.c.fill_round_rect(r, rad, ui.theme.panel);
}

/// A bar with a rounded end on the value side and a square base.
fn bar_up(ui: &mut Ui<'_>, r: Rect, c: Rgb888) {
    if r.h <= 0 || r.w <= 0 {
        return;
    }
    let rad = ui.c.px(4).min(r.w / 2).min(r.h);
    ui.c.fill_round_rect(r, rad, c);
    let sq = (r.h / 2).min(rad);
    ui.c.fill_rect(Rect::new(r.x, r.y + r.h - sq, r.w, sq), c);
}

fn bar_right(ui: &mut Ui<'_>, r: Rect, c: Rgb888) {
    if r.h <= 0 || r.w <= 0 {
        return;
    }
    let rad = ui.c.px(4).min(r.h / 2).min(r.w);
    ui.c.fill_round_rect(r, rad, c);
    let sq = (r.w / 2).min(rad);
    ui.c.fill_rect(Rect::new(r.x, r.y, sq, r.h), c);
}

/// A tidy axis maximum: 1, 2 or 5 × 10^n at or above `v`.
fn nice_max(v: i64) -> i64 {
    let mut p = 1i64;
    while p.saturating_mul(10) <= v.max(1) {
        p = p.saturating_mul(10);
    }
    for f in [1, 2, 5, 10] {
        if p.saturating_mul(f) >= v {
            return p.saturating_mul(f);
        }
    }
    p.saturating_mul(10)
}

pub(crate) fn draw(ui: &mut Ui<'_>, body: Rect, v: &ReportsView) -> Option<UiAction> {
    let u = ui.u();
    let t = ui.theme;
    let money = |m: Money| format!("{}", m.display(v.decimals));
    let mut action = None;

    let (head, rest) = body.split_top(ui.c.line_height(Font::Large) + 2 * u);
    let (head, btns) = head.split_right(head.w.min(56 * u));
    ui.label(head, &v.title, Font::Large, 0);
    let cells = btns.inset(u / 2).grid(2, 1, u);
    if ui.button_font(cells[0], "Print X report", ButtonStyle::Primary, Font::Bold) {
        action = Some(UiAction::Key(Key::F(1)));
    }
    if ui.button_font(cells[1], "Home", ButtonStyle::Normal, Font::Bold) {
        action = Some(UiAction::Key(Key::Home));
    }

    // Stat tiles: label, hero number, note.
    let tile_h = ui.c.line_height(Font::Small) * 2 + ui.c.line_height(Font::Large) + 3 * u;
    let (tiles, rest) = rest.split_top(tile_h + u);
    for ((label, value, note), r) in v.tiles.iter().zip(tiles.grid(4, 1, u)) {
        let r = Rect::new(r.x, r.y + u, r.w, r.h - u);
        card(ui, r);
        let inner = r.inset(u + u / 2);
        let sh = ui.c.line_height(Font::Small);
        let lh = ui.c.line_height(Font::Large);
        ui.label_color(
            Rect::new(inner.x, inner.y, inner.w, sh),
            label,
            Font::Small,
            0,
            t.muted,
        );
        ui.label_color(
            Rect::new(inner.x, inner.y + sh, inner.w, lh),
            value,
            Font::Large,
            0,
            t.text,
        );
        ui.label_color(
            Rect::new(inner.x, inner.y + sh + lh, inner.w, sh),
            note,
            Font::Small,
            0,
            t.muted,
        );
    }

    let rest = Rect::new(rest.x, rest.y + u, rest.w, rest.h - u);
    // Bottom cards just tall enough for their rows; the chart gets the rest.
    let rows = v.cashiers.len().max(v.tenders.len()).max(1) as i32;
    let need =
        ui.c.line_height(Font::Bold) + 5 * u + rows * (ui.c.line_height(Font::Normal) + 2 * u);
    let bottom_h = need.clamp(rest.h * 25 / 100, rest.h * 42 / 100);
    let (top, bottom) = rest.split_bottom(bottom_h);
    let (chart, items) = top.split_left(top.w * 60 / 100);
    let chart = Rect::new(chart.x, chart.y, chart.w - u / 2, chart.h - u / 2);
    let items = Rect::new(items.x + u / 2, items.y, items.w - u / 2, items.h - u / 2);
    if let Some(a) = draw_hours(ui, chart, v, &money) {
        action = Some(a);
    }
    draw_hbars(
        ui,
        items,
        "Best sellers",
        "No sales yet",
        &v.items
            .iter()
            .map(|(n, q, m)| (format!("{n}  x{q}"), *m))
            .collect::<Vec<_>>(),
        &money,
    );
    let (cash, tend) = bottom.split_left(bottom.w * 60 / 100);
    let cash = Rect::new(cash.x, cash.y + u / 2, cash.w - u / 2, cash.h - u / 2);
    let tend = Rect::new(
        tend.x + u / 2,
        tend.y + u / 2,
        tend.w - u / 2,
        tend.h - u / 2,
    );
    draw_hbars(
        ui,
        cash,
        "Cashiers",
        "No sales yet",
        &v.cashiers
            .iter()
            .map(|(n, s, m)| (format!("{n}  ({s})"), *m))
            .collect::<Vec<_>>(),
        &money,
    );
    draw_hbars(ui, tend, "Payments", "No payments yet", &v.tenders, &money);
    if v.partial {
        let note = "Figures from before the last software update are not included";
        let r = Rect::new(
            body.x,
            body.y + body.h - ui.c.line_height(Font::Small),
            body.w,
            ui.c.line_height(Font::Small),
        );
        ui.label_color(r, note, Font::Small, 2, t.muted);
    }
    action
}

/// Sales by hour: vertical bars, quiet grid, peak and tapped bar labelled.
fn draw_hours(
    ui: &mut Ui<'_>,
    r: Rect,
    v: &ReportsView,
    money: &dyn Fn(Money) -> String,
) -> Option<UiAction> {
    let u = ui.u();
    let t = ui.theme;
    card(ui, r);
    let inner = r.inset(2 * u);
    let bh = ui.c.line_height(Font::Bold);
    let sh = ui.c.line_height(Font::Small);
    // Title, and the tapped hour's figures on the right.
    ui.label_color(
        Rect::new(inner.x, inner.y, inner.w, bh),
        "Sales by hour",
        Font::Bold,
        0,
        t.text,
    );
    let sel = v.hour.and_then(|h| v.hours.iter().find(|x| x.0 == h));
    let detail = match sel {
        Some((h, n, m)) => format!(
            "{h:02}:00-{:02}:00  ·  {n} sale{}  ·  {}",
            (h + 1) % 24,
            if *n == 1 { "" } else { "s" },
            money(*m)
        ),
        None => String::from("Tap a bar for its hour"),
    };
    ui.label_color(
        Rect::new(inner.x, inner.y, inner.w, bh),
        &detail,
        Font::Normal,
        2,
        if sel.is_some() { t.text } else { t.muted },
    );
    let axis_w = ui.c.text_width("000,000", Font::Small).min(inner.w / 5);
    let plot = Rect::new(
        inner.x + axis_w + u,
        inner.y + bh + 2 * u,
        inner.w - axis_w - u,
        inner.h - bh - 2 * u - sh - u,
    );
    let max = v.hours.iter().map(|h| h.2.0).max().unwrap_or(0);
    let top = nice_max(max);
    // Grid at 0, ½ and the top, with values in the muted ink.
    let grid = mix(t.panel, t.text, 28);
    for k in 0..=2 {
        let y = plot.y + plot.h - plot.h * k / 2;
        ui.c.fill_rect(Rect::new(plot.x, y, plot.w, 1.max(u / 8)), grid);
        let val = Money(top * i64::from(k) / 2);
        let label = if max == 0 {
            String::from("0")
        } else {
            money(val)
        };
        ui.label_color(
            Rect::new(inner.x, y - sh / 2, axis_w, sh),
            &label,
            Font::Small,
            2,
            t.muted,
        );
    }
    let n = v.hours.len().max(1) as i32;
    let slot = plot.w / n;
    let gap = (slot / 5).clamp(2, 2 * u);
    let peak = v
        .hours
        .iter()
        .filter(|h| h.2.0 > 0)
        .max_by_key(|h| h.2.0)
        .map(|h| h.0);
    let every = if ui.c.text_width("00", Font::Small) + u > slot {
        2
    } else {
        1
    };
    let mut action = None;
    for (k, (h, _, m)) in v.hours.iter().enumerate() {
        let x = plot.x + k as i32 * slot;
        let col = Rect::new(x, plot.y, slot, plot.h);
        let hgt = if top > 0 {
            (plot.h as i64 * m.0.max(0) / top) as i32
        } else {
            0
        };
        let chosen = v.hour == Some(*h);
        let c = if v.hour.is_some() && !chosen {
            mix(t.accent, t.panel, 110)
        } else {
            t.accent
        };
        if hgt > 0 {
            bar_up(
                ui,
                Rect::new(x + gap / 2, plot.y + plot.h - hgt, slot - gap, hgt.max(2)),
                c,
            );
        }
        if (chosen || (v.hour.is_none() && peak == Some(*h))) && m.0 > 0 {
            let lab = money(*m);
            let lw = ui.c.text_width(&lab, Font::Small) + u;
            let ly = (plot.y + plot.h - hgt - sh - u / 2).max(plot.y - sh);
            ui.label_color(
                Rect::new(x + slot / 2 - lw / 2, ly, lw, sh),
                &lab,
                Font::Small,
                1,
                t.text,
            );
        }
        if k as i32 % every == 0 || chosen {
            ui.label_color(
                Rect::new(x, plot.y + plot.h + u / 2, slot, sh),
                &format!("{h:02}"),
                Font::Small,
                1,
                if chosen { t.text } else { t.muted },
            );
        }
        // The whole column is the hit target, larger than the bar.
        if ui.target(col, &format!("hour {h:02}")) {
            action = Some(UiAction::Bar(*h));
        }
    }
    action
}

/// A ranked list with horizontal bars (one series, so no legend: the title
/// names it).
fn draw_hbars(
    ui: &mut Ui<'_>,
    r: Rect,
    title: &str,
    empty: &str,
    rows: &[(String, Money)],
    money: &dyn Fn(Money) -> String,
) {
    let u = ui.u();
    let t = ui.theme;
    card(ui, r);
    let inner = r.inset(2 * u);
    let bh = ui.c.line_height(Font::Bold);
    ui.label_color(
        Rect::new(inner.x, inner.y, inner.w, bh),
        title,
        Font::Bold,
        0,
        t.text,
    );
    let list = Rect::new(inner.x, inner.y + bh + u, inner.w, inner.h - bh - u);
    if rows.is_empty() {
        ui.label_color(list, empty, Font::Normal, 1, t.muted);
        return;
    }
    let max = rows.iter().map(|x| x.1.0).max().unwrap_or(1).max(1);
    let nh = ui.c.line_height(Font::Normal);
    let row_h = (list.h / rows.len() as i32).min(nh + 2 * u);
    let bar_h = (row_h - nh - u / 2).clamp(u / 2, u);
    for (k, (label, m)) in rows.iter().enumerate() {
        let y = list.y + k as i32 * row_h;
        let val = money(*m);
        let vw = ui.c.text_width(&val, Font::Normal) + u;
        ui.label_color(
            Rect::new(list.x, y, list.w - vw, nh),
            label,
            Font::Normal,
            0,
            t.text,
        );
        ui.label_color(
            Rect::new(list.x, y, list.w, nh),
            &val,
            Font::Normal,
            2,
            t.muted,
        );
        let track = Rect::new(list.x, y + nh + u / 4, list.w, bar_h);
        ui.c.fill_round_rect(track, bar_h / 2, mix(t.panel, t.text, 18));
        let w = (list.w as i64 * m.0.max(0) / max) as i32;
        bar_right(
            ui,
            Rect::new(list.x, track.y, w.max(bar_h), bar_h),
            t.accent,
        );
    }
}
