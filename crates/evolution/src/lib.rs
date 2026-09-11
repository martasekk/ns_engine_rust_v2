//! Evolution pass (spec docs/superpowers/specs/2026-09-02-evolution-pass-design.md):
//! mine → propose → gate → apply → ledger. Pure library; ns-app drives it.
pub mod consolidate;
pub mod evaluate;
pub mod files;
pub mod fitness;
pub mod kappa;
pub mod ledger;
pub mod local;
pub mod mine;
pub mod notes;
pub mod pass;
pub mod symbolic;
