//! IQ Tables — a database portal for IQ Labs on-chain tables, written in
//! dependency-free Rust and compiled to WebAssembly.

pub mod account;
pub mod accounts_flow;
pub mod app;
pub mod attach;
pub mod chain;
pub mod codec;
pub mod crypto;
pub mod host;
pub mod inscribe;
pub mod iq;
pub mod json;
pub mod net;
pub mod pack;
pub mod qr;
pub mod solana;
pub mod state;
pub mod ui;
pub mod views;
pub mod views_account;

#[cfg(test)]
mod tests;
