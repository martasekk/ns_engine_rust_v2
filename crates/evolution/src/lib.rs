//! Evolution pass (spec docs/superpowers/specs/2026-09-02-evolution-pass-design.md):
//! mine → propose → gate → apply → ledger. Pure library; ns-app drives it.
pub mod files;
pub mod ledger;
pub mod mine;
pub mod symbolic;
pub mod notes;
pub mod pass;
