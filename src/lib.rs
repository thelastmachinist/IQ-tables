//! IQ Tables — a database portal for IQ Labs on-chain tables, written in
//! dependency-free Rust and compiled to WebAssembly.
//!
//! The modules without `#[cfg(feature = "app")]` are pure (no browser, no
//! clock, no network): the embeddable decoder in `decoder/` is built from
//! them alone.

// The hashing, cipher and compression code indexes arrays the way their
// specifications are written; iterator rewrites would read worse there.
#![allow(clippy::needless_range_loop, clippy::explicit_counter_loop)]

#[cfg(feature = "app")]
pub mod account;
#[cfg(feature = "app")]
pub mod accounts_flow;
#[cfg(feature = "app")]
pub mod app;
#[cfg(feature = "app")]
pub mod attach;
#[cfg(feature = "app")]
pub mod chain;
#[cfg(feature = "app")]
pub mod cli;
pub mod codec;
#[cfg(feature = "app")]
pub mod constraints;
pub mod crypto;
pub mod dates;
#[cfg(feature = "app")]
pub mod ddl;
#[cfg(feature = "app")]
pub mod editor;
#[cfg(feature = "app")]
pub mod embed;
#[cfg(feature = "app")]
pub mod git;
#[cfg(feature = "app")]
pub mod host;
#[cfg(feature = "app")]
pub mod inscribe;
pub mod iq;
pub mod json;
pub mod net;
pub mod pack;
#[cfg(feature = "app")]
pub mod qr;
pub mod records;
pub mod schema;
#[cfg(feature = "app")]
pub mod sheet;
pub mod solana;
#[cfg(feature = "app")]
pub mod sql;
#[cfg(feature = "app")]
pub mod sql_exec;
#[cfg(feature = "app")]
pub mod state;
pub mod ui;
#[cfg(feature = "app")]
pub mod upload;
#[cfg(feature = "app")]
pub mod views;
#[cfg(feature = "app")]
pub mod views_account;
#[cfg(feature = "app")]
pub mod views_ws;
#[cfg(feature = "app")]
pub mod ws_actions;

#[cfg(all(test, feature = "app"))]
mod tests;
#[cfg(all(test, feature = "app"))]
mod tests_sql;
