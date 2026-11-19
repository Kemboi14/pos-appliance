//! POS domain core: catalog, cart, pricing, tax, discounts, tenders,
//! refunds, shifts, reports, users and the event-sourced state machine.
//!
//! Pure logic, no I/O, no floating point, no `unsafe`.
#![no_std]
#![forbid(unsafe_code)]
#![deny(
    clippy::float_arithmetic,
    clippy::float_cmp,
    clippy::cast_precision_loss
)]

extern crate alloc;
#[cfg(test)]
extern crate std;

pub mod auth;
pub mod barcode;
pub mod cart;
pub mod catalog;
pub mod config;
pub mod csv;
pub mod fiscal;
pub mod money;
pub mod sale;
pub mod shift;
pub mod state;
pub mod tender;

pub use money::{Money, Qty, Rate, Rounding};
pub use state::{DomainError, Event, PosState};
