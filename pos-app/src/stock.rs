//! The Inventory app: stock list with filters and search, an item page
//! with receive / count / write-off / reorder level, and STOCK.CSV export.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;

use pos_core::Money;
use pos_core::inventory::{Adjust, StockStatus};
use pos_core::media::PictureFor;
use pos_core::state::Event;
use pos_hal::Rgb888;
use pos_hal::{BlockDevice, Key};
use pos_ui::image::Bitmap;
use pos_ui::{ButtonStyle, Font, Rect, Ui};

use crate::screens::{Next, Pending, Screen, UiAction, authorize, prompt};
use crate::{App, Hw, Place};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Filter {
    #[default]
    All,
    Low,
    Out,
    Untracked,
}

impl Filter {
    const ALL: [Filter; 4] = [Filter::All, Filter::Low, Filter::Out, Filter::Untracked];
    fn label(self) -> &'static str {
        match self {
            Filter::All => "All",
            Filter::Low => "Low",
            Filter::Out => "Out",
            Filter::Untracked => "Not tracked",
        }
    }
}

/// Where the stock list is (kept while the item page or a prompt is open).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StockList {
    pub filter: Filter,
    pub query: String,
    pub sel: usize,
}

/// A stock change waiting for permission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StockOp {
    Receive {
        sku: String,
        qty: pos_core::Qty,
        cost: Option<Money>,
    },
    Count {
        sku: String,
        counted: pos_core::Qty,
    },
    WriteOff {
        sku: String,
        qty: pos_core::Qty,
        reason: Adjust,
    },
    Reorder {
        sku: String,
        level: pos_core::Qty,
    },
}

impl StockOp {
    pub fn sku(&self) -> &str {
        match self {
            StockOp::Receive { sku, .. }
            | StockOp::Count { sku, .. }
            | StockOp::WriteOff { sku, .. }
            | StockOp::Reorder { sku, .. } => sku,
        }
    }
}

/// SKUs shown for the current filter and search, in name order.
pub(crate) fn rows<D: BlockDevice>(app: &App<D>) -> Vec<String> {
    let l = &app.stock;
    let inv = &app.state.inventory;
    let q = l.query.to_lowercase();
    let mut v: Vec<(&str, &str)> = app
        .state
        .catalog
        .items()
        .filter(|i| {
            q.is_empty()
                || i.name.to_lowercase().contains(&q)
                || i.sku.to_lowercase().starts_with(&q)
                || i.barcode.as_deref() == Some(l.query.trim())
        })
        .filter(|i| {
            let s = inv.status(&i.sku);
            match l.filter {
                Filter::All => true,
                Filter::Low => matches!(s, StockStatus::Low | StockStatus::Out),
                Filter::Out => s == StockStatus::Out,
                Filter::Untracked => s == StockStatus::Untracked,
            }
        })
        .map(|i| (i.name.as_str(), i.sku.as_str()))
        .collect();
    v.sort_by(|a, b| {
        a.0.to_lowercase()
            .cmp(&b.0.to_lowercase())
            .then(a.1.cmp(b.1))
    });
    v.into_iter().map(|(_, s)| s.to_string()).collect()
}

pub(crate) fn open<D: BlockDevice>(app: &mut App<D>) -> Screen {
    app.place = Place::Inventory;
    app.stock.query.clear();
    app.stock.sel = 0;
    Screen::Stock
}

pub(crate) fn list_key<D: BlockDevice>(
    app: &mut App<D>,
    key: Key,
    at: u64,
    hw: &mut Hw<'_>,
) -> Screen {
    let n = rows(app).len();
    if let Key::Char(c) = key {
        app.scan.char(c, at);
    } else if key != Key::Enter {
        app.scan.other_key();
    }
    let l = &mut app.stock;
    match key {
        Key::Up => l.sel = l.sel.saturating_sub(1),
        Key::Down => l.sel = (l.sel + 1).min(n.saturating_sub(1)),
        Key::PageUp => l.sel = l.sel.saturating_sub(8),
        Key::PageDown => l.sel = (l.sel + 8).min(n.saturating_sub(1)),
        Key::F(k @ 1..=4) => {
            l.filter = Filter::ALL[usize::from(k - 1)];
            l.sel = 0;
        }
        Key::F(5) => {
            return prompt(
                "Find product",
                "Name, SKU or barcode",
                false,
                Next::StockSearch,
            );
        }
        Key::F(6) => return authorize(app, Pending::ExportStock, hw),
        Key::F(10) => return Screen::Menu { sel: 0 },
        Key::Char(c) if l.query.chars().count() < 30 => {
            l.query.push(c);
            l.sel = 0;
        }
        Key::Backspace => {
            l.query.pop();
            l.sel = 0;
        }
        Key::Escape if !l.query.is_empty() => {
            l.query.clear();
            l.sel = 0;
        }
        Key::Escape | Key::Home => return crate::screens::go_home(app),
        Key::Enter => {
            // A scanned or typed barcode opens that item directly.
            if let pos_ui::scan::Enter::Scan(code) = app.scan.enter(at) {
                let keep = app
                    .stock
                    .query
                    .chars()
                    .count()
                    .saturating_sub(code.chars().count());
                app.stock.query = app.stock.query.chars().take(keep).collect();
                let norm =
                    pos_core::barcode::normalize_code(&code).unwrap_or_else(|_| code.clone());
                if let Some(i) = app.state.catalog.by_barcode(&norm) {
                    return Screen::StockItem(i.sku.clone());
                }
                app.set_msg(format!("Unknown barcode {norm}"), true);
                return Screen::Stock;
            }
            let code = app.stock.query.trim().to_string();
            if let Some(i) = app.state.catalog.by_barcode(&code) {
                let sku = i.sku.clone();
                app.stock.query.clear();
                return Screen::StockItem(sku);
            }
            if let Some(sku) = rows(app).get(app.stock.sel) {
                return Screen::StockItem(sku.clone());
            }
        }
        _ => {}
    }
    Screen::Stock
}

pub(crate) fn item_key<D: BlockDevice>(app: &mut App<D>, sku: String, key: Key) -> Screen {
    let name = app
        .state
        .catalog
        .get(&sku)
        .map(|i| i.name.clone())
        .unwrap_or_default();
    match key {
        Key::F(1) => prompt(
            &format!("Receive: {name}"),
            "Quantity delivered",
            false,
            Next::StockReceiveQty(sku),
        ),
        Key::F(2) => prompt(
            &format!("Count: {name}"),
            "Quantity counted on the shelf (replaces the book stock)",
            false,
            Next::StockCount(sku),
        ),
        Key::F(3) => prompt(
            &format!("Write off: {name}"),
            "Quantity to remove",
            false,
            Next::StockWriteOffQty(sku),
        ),
        Key::F(4) => prompt(
            &format!("Reorder level: {name}"),
            "Warn when stock falls to this quantity (0 = never)",
            false,
            Next::StockReorder(sku),
        ),
        Key::Escape | Key::Enter => Screen::Stock,
        Key::Home => crate::screens::go_home(app),
        _ => Screen::StockItem(sku),
    }
}

/// Build the event for an authorised stock change.
pub(crate) fn event<D: BlockDevice>(
    app: &App<D>,
    op: StockOp,
    by: pos_core::auth::UserId,
    at: i64,
) -> Event {
    match op {
        StockOp::Receive { sku, qty, cost } => Event::StockReceived {
            by,
            at,
            supplier: String::new(),
            lines: alloc::vec![pos_core::inventory::Received {
                sku,
                qty,
                unit_cost: cost,
            }],
        },
        StockOp::Count { sku, counted } => {
            let book = app
                .state
                .inventory
                .get(&sku)
                .map_or(pos_core::Qty(0), |l| l.on_hand);
            Event::StockCounted {
                by,
                at,
                variance: pos_core::Qty(counted.0 - book.0),
                sku,
                counted,
            }
        }
        StockOp::WriteOff { sku, qty, reason } => Event::StockWrittenOff {
            by,
            at,
            sku,
            qty,
            reason,
        },
        StockOp::Reorder { sku, level } => Event::ReorderLevelSet { by, at, sku, level },
    }
}

pub(crate) fn done_message(op: &StockOp) -> String {
    match op {
        StockOp::Receive { qty, .. } => format!("Received {qty}"),
        StockOp::Count { counted, .. } => format!("Stock set to {counted}"),
        StockOp::WriteOff { qty, reason, .. } => format!("Wrote off {qty} ({})", reason.label()),
        StockOp::Reorder { level, .. } => format!("Reorder level set to {level}"),
    }
}

/// STOCK.CSV: one row per catalog item.
pub fn stock_csv<D: BlockDevice>(app: &App<D>) -> String {
    let d = app.state.config.decimals;
    let mut s = String::from("sku,name,status,on_hand,reorder_at,last_cost,retail_value\r\n");
    for i in app.state.catalog.items() {
        let inv = &app.state.inventory;
        let l = inv.get(&i.sku);
        let on = l.map(|l| l.on_hand.to_string()).unwrap_or_default();
        let re = l.map(|l| l.reorder_at.to_string()).unwrap_or_default();
        let cost = l
            .and_then(|l| l.last_cost)
            .map(|c| c.display(d).to_string())
            .unwrap_or_default();
        let value = l
            .and_then(|l| retail_value(i.price, l.on_hand))
            .map(|m| m.display(d).to_string())
            .unwrap_or_default();
        let name = i.name.replace('"', "\"\"");
        s.push_str(&format!(
            "{},\"{}\",{},{},{},{},{}\r\n",
            i.sku,
            name,
            inv.status(&i.sku).label(),
            on,
            re,
            cost,
            value
        ));
    }
    s
}

fn retail_value(price: Money, q: pos_core::Qty) -> Option<Money> {
    if q.0 <= 0 {
        return Some(Money::ZERO);
    }
    let v = i128::from(price.0) * i128::from(q.0) / 1000;
    i64::try_from(v).ok().map(Money)
}

// ------------------------------------------------------------------ drawing

/// What the inventory screens show, copied out before drawing.
#[derive(Debug, Clone)]
pub(crate) struct StockView {
    pub list: StockList,
    pub rows: Vec<Row>,
    pub products: usize,
    pub low: usize,
    pub out: usize,
    pub value: String,
    pub item: Option<ItemView>,
}

#[derive(Debug, Clone)]
pub(crate) struct Row {
    pub sku: String,
    pub name: String,
    pub price: String,
    pub on_hand: String,
    pub status: StockStatus,
    pub pic: Option<Arc<Bitmap>>,
}

#[derive(Debug, Clone)]
pub(crate) struct ItemView {
    pub row: Row,
    pub barcode: String,
    pub facts: Vec<(String, String)>,
}

/// The rows near the selection whose pictures should be decoded.
pub(crate) fn wanted<D: BlockDevice>(app: &App<D>, out: &mut Vec<PictureFor>) {
    match &app.screen {
        Screen::Stock => {
            let r = rows(app);
            let from = app.stock.sel.saturating_sub(16);
            out.extend(r.into_iter().skip(from).take(32).map(PictureFor::Item));
        }
        Screen::StockItem(s) => out.push(PictureFor::Item(s.clone())),
        _ => {}
    }
}

pub(crate) fn view<D: BlockDevice>(app: &App<D>) -> Option<StockView> {
    if !matches!(app.screen, Screen::Stock | Screen::StockItem(_)) {
        return None;
    }
    let d = app.state.config.decimals;
    let inv = &app.state.inventory;
    let make_row = |sku: &str| -> Option<Row> {
        let i = app.state.catalog.get(sku)?;
        Some(Row {
            sku: sku.into(),
            name: i.name.clone(),
            price: format!("{}", i.price.display(d)),
            on_hand: inv
                .get(sku)
                .map(|l| {
                    if i.weighable {
                        format!("{} kg", l.on_hand)
                    } else {
                        l.on_hand.to_string()
                    }
                })
                .unwrap_or_else(|| "-".into()),
            status: inv.status(sku),
            pic: app.gallery.get(&PictureFor::Item(sku.into())),
        })
    };
    let all = rows(app);
    let from = app.stock.sel.saturating_sub(16);
    let rows_v: Vec<Row> = all
        .iter()
        .skip(from)
        .take(32)
        .filter_map(|s| make_row(s))
        .collect();
    let mut value = Money::ZERO;
    let (mut low, mut out) = (0, 0);
    for (sku, l) in inv.levels() {
        match inv.status(sku) {
            StockStatus::Low => low += 1,
            StockStatus::Out => {
                out += 1;
            }
            _ => {}
        }
        if let Some(i) = app.state.catalog.get(sku)
            && let Some(v) = retail_value(i.price, l.on_hand)
        {
            value = value.checked_add(v).unwrap_or(value);
        }
    }
    let item = match &app.screen {
        Screen::StockItem(sku) => make_row(sku).map(|row| {
            let i = app.state.catalog.get(sku);
            let l = inv.get(sku);
            let when = |t: i64| {
                if t == 0 {
                    "never".to_string()
                } else {
                    crate::receipt::fmt_time(t)
                }
            };
            let mut facts = alloc::vec![
                ("On hand".to_string(), row.on_hand.clone()),
                ("Status".into(), row.status.label().into()),
                (
                    "Price".into(),
                    format!("{} {}", app.state.config.currency, row.price)
                ),
            ];
            if let Some(l) = l {
                facts.push((
                    "Reorder at".into(),
                    if l.reorder_at.0 > 0 {
                        l.reorder_at.to_string()
                    } else {
                        "not set".into()
                    },
                ));
                if let Some(c) = l.last_cost {
                    facts.push(("Last unit cost".into(), format!("{}", c.display(d))));
                }
                facts.push(("Sold since last count".into(), l.sold_since.to_string()));
                facts.push(("Last delivery".into(), when(l.last_received)));
                facts.push(("Last count".into(), when(l.last_counted)));
            }
            ItemView {
                barcode: i.and_then(|i| i.barcode.clone()).unwrap_or_default(),
                row,
                facts,
            }
        }),
        _ => None,
    };
    Some(StockView {
        list: app.stock.clone(),
        rows: rows_v,
        products: app.state.catalog.len(),
        low,
        out,
        value: format!("{} {}", app.state.config.currency, value.display(d)),
        item,
    })
    .map(|mut v| {
        // Rows are a window starting at `from`; keep the selection relative.
        v.list.sel = app.stock.sel - from;
        v
    })
}

fn status_color(s: StockStatus) -> Rgb888 {
    match s {
        StockStatus::Ok => 0x0028_A745,
        StockStatus::Low => 0x00E3_B341,
        StockStatus::Out => 0x00D7_3A49,
        StockStatus::Untracked => 0x0060_6A78,
    }
}

/// Colour for a placeholder tile, stable per name.
pub(crate) fn tile_color(name: &str) -> Rgb888 {
    const P: [Rgb888; 8] = [
        0x002F_81F7,
        0x008B_5CF6,
        0x0010_B981,
        0x00F5_9E0B,
        0x00EF_4444,
        0x0006_B6D4,
        0x00EC_4899,
        0x0084_CC16,
    ];
    let h = name
        .bytes()
        .fold(7u32, |h, b| h.wrapping_mul(31).wrapping_add(u32::from(b)));
    P[(h % 8) as usize]
}

/// A product picture, or a coloured tile with the name's first letter.
pub(crate) fn thumb(ui: &mut Ui<'_>, r: Rect, name: &str, pic: Option<&Bitmap>) {
    let rad = r.w.min(r.h) / 6;
    match pic {
        Some(b) => {
            ui.c.fill_round_rect(r, rad, 0x00FF_FFFF);
            ui.c.draw_bitmap(r, b, rad, 255);
        }
        None => {
            let c = tile_color(name);
            ui.c.fill_gradient(
                r,
                rad,
                pos_ui::mix(c, 0x00FF_FFFF, 40),
                pos_ui::mix(c, 0, 50),
            );
            let initial: String = name.chars().take(1).collect::<String>().to_uppercase();
            let f = if r.h >= ui.c.line_height(Font::Huge) {
                Font::Huge
            } else if r.h >= ui.c.line_height(Font::Large) {
                Font::Large
            } else {
                Font::Bold
            };
            ui.label_color(r, &initial, f, 1, 0x00FF_FFFF);
        }
    }
}

fn pill(ui: &mut Ui<'_>, r: Rect, text: &str, c: Rgb888) {
    let f = Font::Small;
    let w = ui.c.text_width(text, f) + ui.u() * 2;
    let h = ui.c.line_height(f) + ui.u() / 2;
    let p = Rect::new(r.x + r.w - w, r.y + (r.h - h) / 2, w, h);
    ui.c.fill_round_rect(p, h / 2, c);
    ui.label_color(p, text, f, 1, 0x00FF_FFFF);
}

pub(crate) fn draw_list(ui: &mut Ui<'_>, body: Rect, v: &StockView) -> Option<UiAction> {
    let u = ui.u();
    let mut action = None;
    // Summary cards.
    let (cards, rest) = body.split_top(11 * u);
    let cells = cards.grid(4, 1, u);
    let stats = [
        ("Products", v.products.to_string(), ui.theme.accent),
        ("Low stock", v.low.to_string(), 0x00E3_B341),
        ("Out of stock", v.out.to_string(), ui.theme.danger),
        ("Stock value", v.value.clone(), ui.theme.ok),
    ];
    for ((label, val, col), cell) in stats.iter().zip(cells) {
        ui.panel(cell);
        let bar = Rect::new(cell.x, cell.y + u, ui.c.px(4).max(2), cell.h - 2 * u);
        ui.c.fill_rect(bar, *col);
        let inner = cell.inset(u + u / 2);
        let (a, b) = inner.split_top(ui.c.line_height(Font::Small));
        let muted = ui.theme.muted;
        ui.label_color(a, label, Font::Small, 0, muted);
        ui.label(b, val, Font::Large, 0);
    }
    let rest = Rect::new(rest.x, rest.y + u, rest.w, rest.h - u);
    // Filter tabs and tools.
    let (tabs, rest) = rest.split_top(6 * u);
    let tc = tabs.grid(7, 1, u / 2);
    let tf = ui.grid_font(
        tc[0],
        Filter::ALL
            .iter()
            .map(|f| f.label())
            .chain(["Search", "Export", "Home"]),
    );
    for (k, f) in Filter::ALL.iter().enumerate() {
        let style = if *f == v.list.filter {
            ButtonStyle::Primary
        } else {
            ButtonStyle::Normal
        };
        if ui.button_font(tc[k], f.label(), style, tf) {
            action = Some(UiAction::Key(Key::F(k as u8 + 1)));
        }
    }
    if ui.button_font(tc[4], "Search", ButtonStyle::Normal, tf) {
        action = Some(UiAction::Key(Key::F(5)));
    }
    if ui.button_font(tc[5], "Export", ButtonStyle::Normal, tf) {
        action = Some(UiAction::Key(Key::F(6)));
    }
    if ui.button_font(tc[6], "Home", ButtonStyle::Normal, tf) {
        action = Some(UiAction::Key(Key::Home));
    }
    let rest = Rect::new(rest.x, rest.y + u, rest.w, rest.h - u);
    let (search, list) = rest.split_top(ui.c.line_height(Font::Normal) + u);
    let muted = ui.theme.muted;
    if v.list.query.is_empty() {
        ui.label_color(
            search,
            "Type or scan to search · tap a product for stock actions",
            Font::Small,
            0,
            muted,
        );
    } else {
        ui.label(
            search,
            &format!("Search: {}_   ({} found)", v.list.query, v.rows.len()),
            Font::Normal,
            0,
        );
    }
    // Rows.
    ui.panel(list);
    let list = list.inset(u / 2);
    let row_h = (ui.c.line_height(Font::Bold) + ui.c.line_height(Font::Small) + u).max(7 * u);
    let n = (list.h / row_h).max(1) as usize;
    if v.rows.is_empty() {
        ui.label_color(list, "No products", Font::Normal, 1, muted);
    }
    let first = v.list.sel.saturating_sub(n - 1);
    for (k, row) in v.rows.iter().enumerate().skip(first).take(n) {
        let r = Rect::new(list.x, list.y + (k - first) as i32 * row_h, list.w, row_h);
        if k == v.list.sel {
            ui.c.fill_round_rect(r, ui.c.px(4), ui.theme.select);
        } else if ui.pressed_in(r) {
            ui.c.fill_round_rect(r, ui.c.px(4), ui.theme.button);
        }
        let th = r.h - u;
        thumb(
            ui,
            Rect::new(r.x + u / 2, r.y + u / 2, th, th),
            &row.name,
            row.pic.as_deref(),
        );
        let tx = r.x + th + 2 * u;
        let pill_w = 16 * u;
        let qty_w = 14 * u;
        let name_w = r.w - (tx - r.x) - pill_w - qty_w - u;
        let (l1, l2) =
            Rect::new(tx, r.y + u / 2, name_w, r.h - u).split_top(ui.c.line_height(Font::Bold));
        ui.label(l1, &row.name, Font::Bold, 0);
        ui.label_color(
            l2,
            &format!("{} · {}", row.sku, row.price),
            Font::Small,
            0,
            muted,
        );
        let q = Rect::new(tx + name_w, r.y, qty_w, r.h);
        ui.label(q, &row.on_hand, Font::Large, 2);
        let p = Rect::new(r.x + r.w - pill_w - u, r.y, pill_w, r.h);
        pill(ui, p, row.status.label(), status_color(row.status));
        if ui.target(r, &row.name) {
            action = Some(UiAction::StockRow(k));
        }
    }
    action
}

pub(crate) fn draw_item(ui: &mut Ui<'_>, body: Rect, v: &StockView) -> Option<UiAction> {
    let u = ui.u();
    let mut action = None;
    let Some(it) = &v.item else {
        return None;
    };
    let (main, btns) = body.split_bottom(9 * u);
    let side = (main.h - 2 * u).min(main.w * 2 / 5);
    let (left, right) = main.split_left(side + 2 * u);
    let pic = Rect::new(left.x + u, left.y + u, side, side);
    thumb(ui, pic, &it.row.name, it.row.pic.as_deref());
    let right = right.inset(u);
    let (title, rest) = right.split_top(ui.c.line_height(Font::Large) + u);
    ui.label(title, &it.row.name, Font::Large, 0);
    let (sub, rest) = rest.split_top(ui.c.line_height(Font::Normal) + u);
    let muted = ui.theme.muted;
    let code = if it.barcode.is_empty() {
        format!("SKU {}", it.row.sku)
    } else {
        format!("SKU {} · barcode {}", it.row.sku, it.barcode)
    };
    ui.label_color(sub, &code, Font::Normal, 0, muted);
    // Big stock figure with a status colour band.
    let (big, rest) = rest.split_top(ui.c.line_height(Font::Huge) + 2 * u);
    ui.panel(big);
    let band = Rect::new(big.x, big.y, ui.c.px(6).max(3), big.h);
    ui.c.fill_rect(band, status_color(it.row.status));
    let bi = big.inset(u + u / 2);
    ui.label_color(bi, "On hand", Font::Normal, 0, muted);
    ui.label(bi, &it.row.on_hand, Font::Huge, 2);
    let rest = Rect::new(rest.x, rest.y + u, rest.w, rest.h - u);
    ui.panel(rest);
    let inner = rest.inset(u);
    let lh = ui.c.line_height(Font::Normal) + u / 2;
    for (k, (a, b)) in it.facts.iter().skip(1).enumerate() {
        let y = inner.y + k as i32 * lh;
        if y + lh > inner.y + inner.h {
            break;
        }
        let r = Rect::new(inner.x, y, inner.w, lh);
        ui.label_color(r, a, Font::Normal, 0, muted);
        ui.label(r, b, Font::Bold, 2);
    }
    let cells = btns.grid(5, 1, u);
    let b = [
        ("Receive", Key::F(1), ButtonStyle::Ok),
        ("Count", Key::F(2), ButtonStyle::Primary),
        ("Write off", Key::F(3), ButtonStyle::Danger),
        ("Reorder level", Key::F(4), ButtonStyle::Normal),
        ("Back", Key::Escape, ButtonStyle::Normal),
    ];
    let font = ui.grid_font(cells[0], b.iter().map(|x| x.0));
    for ((label, k, style), cell) in b.into_iter().zip(cells) {
        if ui.button_font(cell, label, style, font) {
            action = Some(UiAction::Key(k));
        }
    }
    action
}
