//! `postvec`: install, configure and diagnose the postvec PostgreSQL
//! extension against an existing cluster, and manage the models in an
//! engine root.
//!
//! The commands share one set of inspection primitives. `doctor` collects
//! facts and evaluates checks; `setup`/`uninstall` reuse the same
//! collectors for their preflight and smoke checks. Independent
//! implementations of "is this healthy?" would drift.
//!
//! This crate is primarily the `postvec` binary. The library target exists
//! so the registry publisher can consume the same index schema,
//! deterministic archive writer and strict archive reader the client uses.

pub mod checks;
pub mod cli;
pub mod cluster;
pub mod commands;
pub mod config;
pub mod db;
pub mod engine;
pub mod error;
pub mod facts;
pub mod output;
pub mod plan;
pub mod proc;
pub mod registry;
pub mod validate;

#[cfg(test)]
mod testing;

pub const CLI_VERSION: &str = env!("CARGO_PKG_VERSION");
