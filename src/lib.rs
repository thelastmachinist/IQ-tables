//! IQ Tables — a database portal for IQ Labs on-chain tables, written in
//! dependency-free Rust and compiled to WebAssembly.

pub mod account;
pub mod accounts_flow;
pub mod app;
pub mod attach;
pub mod chain;
pub mod codec;
pub mod constraints;
pub mod crypto;
pub mod dates;
pub mod ddl;
pub mod editor;
pub mod host;
pub mod inscribe;
pub mod iq;
pub mod json;
pub mod net;
pub mod pack;
pub mod qr;
pub mod schema;
pub mod sheet;
pub mod solana;
pub mod sql;
pub mod sql_exec;
pub mod state;
pub mod ui;
pub mod views;
pub mod views_account;
pub mod views_ws;
pub mod ws_actions;

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_sql;
