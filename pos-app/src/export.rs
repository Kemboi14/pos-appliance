//! Export (journal, Z reports, catalog → CSV on a second USB stick) and
//! import (catalog CSV, settings) through `ExternalMedia`.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use pos_core::auth::UserId;
use pos_core::csv;
use pos_core::shift::ReportKind;
use pos_core::state::Event;
use pos_hal::BlockDevice;

use crate::receipt::hex;
use crate::{App, Hw, Journaled};

/// Journal files are split so no single write needs a huge buffer.
const CHUNK: usize = 8 * 1024 * 1024;

pub fn export<D: BlockDevice>(
    app: &mut App<D>,
    hw: &mut Hw<'_>,
    by: UserId,
    now: i64,
) -> Result<String, String> {
    if !hw.media.is_present() {
        return Err("Insert a FAT-formatted USB stick for export".into());
    }
    let cfg = app.state.config.clone();
    let store = app.store.as_mut().ok_or("store unavailable")?;
    let mut files: Vec<(String, Vec<u8>)> = Vec::new();
    let mut chunk = String::from(csv::JOURNAL_HEADER);
    chunk.push_str("\r\n");
    let mut first: Option<u64> = None;
    let mut last = 0u64;
    let mut rows = 0u64;
    let mut decode_err = None;
    let end = store
        .scan_retained(|r| {
            let e: Event = match postcard::from_bytes(&r.payload) {
                Ok(e) => e,
                Err(_) => {
                    decode_err = Some(r.seq);
                    return false;
                }
            };
            if let Event::Report(rep) = &e
                && rep.kind == ReportKind::Z
            {
                let name = format!("Z{:06}.CSV", rep.z_no.unwrap_or(0));
                files.push((name, csv::report_to_csv(rep, &cfg).into_bytes()));
            }
            chunk.push_str(&csv::journal_row(r.seq, &hex(&r.hash), &e, &cfg));
            first.get_or_insert(r.seq);
            last = r.seq;
            rows += 1;
            if chunk.len() >= CHUNK {
                let s = core::mem::replace(&mut chunk, String::from(csv::JOURNAL_HEADER));
                chunk.push_str("\r\n");
                files.push((format!("J{:08}.CSV", r.seq), s.into_bytes()));
            }
            true
        })
        .map_err(|e| format!("journal read failed: {e:?}"))?;
    if let Some(seq) = decode_err {
        return Err(format!("record {seq} could not be decoded; export aborted"));
    }
    if rows > 0 {
        files.push((format!("J{last:08}.CSV"), chunk.into_bytes()));
    }
    let items: Vec<&pos_core::catalog::Item> = app.state.catalog.items().collect();
    files.push((
        "CATALOG.CSV".into(),
        csv::catalog_to_csv(&items, &cfg).into_bytes(),
    ));
    let mut bytes = 0u64;
    for (name, data) in &files {
        hw.media
            .write_file(name, data)
            .map_err(|e| format!("writing {name}: {e}"))?;
        bytes += data.len() as u64;
    }
    // Files are on the stick (write_file flushes): journal it, then allow
    // the exported records to be recycled.
    let what = match first {
        Some(f) => format!("journal {f}-{last}, {} files", files.len()),
        None => format!("{} files (no new journal records)", files.len()),
    };
    app.commit(alloc::vec![Event::Exported {
        by,
        at: now,
        what: what.clone(),
        bytes
    }])
    .map_err(|e| crate::describe(&e))?;
    if let Some(store) = app.store.as_mut() {
        store
            .set_retain(end)
            .map_err(|e| format!("retain: {e:?}"))?;
    }
    Ok(format!("Exported {what}"))
}

/// Find a sale/refund by receipt number among retained journal records.
pub fn find_receipt<D: BlockDevice>(app: &mut App<D>, n: u64) -> Option<Journaled> {
    let store = app.store.as_mut()?;
    let mut found = None;
    let _ = store.scan_retained(|r| {
        if let Ok(e) = postcard::from_bytes::<Event>(&r.payload) {
            match e {
                Event::SaleCompleted(s) if s.receipt_no == n => {
                    found = Some(Journaled::Sale(s, r.hash))
                }
                Event::RefundCompleted(x) if x.receipt_no == n => {
                    found = Some(Journaled::Refund(x, r.hash))
                }
                _ => {}
            }
        }
        found.is_none()
    });
    found
}

pub fn import_catalog<D: BlockDevice>(
    app: &mut App<D>,
    hw: &mut Hw<'_>,
    by: UserId,
    now: i64,
    mut events: Vec<Event>,
) -> Result<usize, String> {
    if !hw.media.is_present() {
        return Err("Insert the USB stick with CATALOG.CSV".into());
    }
    let data = hw
        .media
        .read_file("CATALOG.CSV", 32 * 1024 * 1024)
        .map_err(|e| format!("CATALOG.CSV: {e}"))?;
    let text = core::str::from_utf8(&data).map_err(|_| String::from("CATALOG.CSV is not UTF-8"))?;
    let items = csv::catalog_from_csv(text, &app.state.config)
        .map_err(|e| format!("CATALOG.CSV line {}: {}", e.line, e.msg))?;
    let n = items.len();
    // One record: the import is atomic across power loss.
    events.push(Event::CatalogReplace {
        by,
        at: now,
        items,
        source: "CATALOG.CSV".into(),
    });
    app.commit(events).map_err(|e| crate::describe(&e))?;
    Ok(n)
}

pub fn import_settings<D: BlockDevice>(
    app: &mut App<D>,
    hw: &mut Hw<'_>,
    by: UserId,
    now: i64,
    mut events: Vec<Event>,
) -> Result<(), String> {
    if !hw.media.is_present() {
        return Err("Insert the USB stick with SETTINGS.TXT".into());
    }
    let data = hw
        .media
        .read_file("SETTINGS.TXT", 64 * 1024)
        .map_err(|e| format!("SETTINGS.TXT: {e}"))?;
    let text =
        core::str::from_utf8(&data).map_err(|_| String::from("SETTINGS.TXT is not UTF-8"))?;
    let cfg = crate::settings::parse(text, &app.state.config)?;
    app.policy
        .validate_config(&cfg)
        .map_err(|f| format!("rejected by fiscal policy: {}", f.0))?;
    events.push(Event::ConfigSet {
        by,
        at: now,
        config: cfg,
    });
    app.commit(events).map_err(|e| crate::describe(&e))?;
    let c = &app.state.config;
    app.scan.set_config(pos_ui::scan::ScanConfig {
        max_gap_us: u64::from(c.scanner_max_gap_ms) * 1000,
        min_len: usize::from(c.scanner_min_len),
    });
    Ok(())
}
