//! Evaluation harnesses that drive the engine but are never driven by it.
//!
//! `eval` is the ability set (`ns-app eval`); `paraphrase` is the recall
//! measurement arm (`ns-app eval --paraphrase`). Both were library modules of
//! `ns-engine` because they began as integration tests and had to become
//! callable from the binary. That made them library code, but it also compiled
//! them into every build of the engine, with nothing on the turn path calling
//! either.
//!
//! They live here instead. `ns-engine` keeps `replay`, `script` and
//! `store::InMemoryStore`, which are *not* test-only despite reading like it:
//! `replay_with` builds a scripted emitter, an in-memory store and a noop
//! consolidator on its ordinary path, and it is the hard dependency of
//! `ns-evolution`'s `verify_patch` gate.
pub mod eval;
pub mod grading;
pub mod paraphrase;
