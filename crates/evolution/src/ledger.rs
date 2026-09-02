//! Verdict ledger (spec M5 §4.3): candidate content-hash → verdict, so a
//! rejected candidate is never re-verified and an accepted one never re-proposed.
use crate::files::{write_atomic, FileError};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Evidence {
    pub session: String,
    pub turn: u32,
    pub event_id: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    Accepted,
    Rejected,
    Unverified,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LedgerEntry {
    pub verdict: Verdict,
    pub numbers: serde_json::Value,
    pub evidence: Vec<Evidence>,
    /// Unix milliseconds.
    pub at: u64,
}

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct Ledger {
    #[serde(default)]
    pub entries: BTreeMap<String, LedgerEntry>,
}

impl Ledger {
    pub fn load(path: &Path) -> Result<Ledger, FileError> {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text)
                .map_err(|e| FileError::Parse(format!("{}: {e}", path.display()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Ledger::default()),
            Err(e) => Err(FileError::Io(e.to_string())),
        }
    }
    pub fn save_atomic(&self, path: &Path) -> Result<(), FileError> {
        let text =
            serde_json::to_string_pretty(self).map_err(|e| FileError::Parse(e.to_string()))?;
        write_atomic(path, text.as_bytes())
    }
    /// Accepted or rejected candidates are never re-proposed / re-verified.
    pub fn settled(&self, hash: &str) -> bool {
        matches!(
            self.entries.get(hash).map(|e| e.verdict),
            Some(Verdict::Accepted | Verdict::Rejected)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(v: Verdict) -> LedgerEntry {
        LedgerEntry {
            verdict: v,
            numbers: serde_json::json!({"flipped": 1}),
            evidence: vec![Evidence {
                session: "s".into(),
                turn: 1,
                event_id: 4,
            }],
            at: 7,
        }
    }

    #[test]
    fn settled_only_for_accepted_or_rejected() {
        let mut l = Ledger::default();
        l.entries.insert("a".into(), entry(Verdict::Accepted));
        l.entries.insert("r".into(), entry(Verdict::Rejected));
        l.entries.insert("u".into(), entry(Verdict::Unverified));
        assert!(l.settled("a") && l.settled("r"));
        assert!(!l.settled("u") && !l.settled("missing"));
    }

    #[test]
    fn round_trips_through_json_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("ledger.json");
        assert_eq!(Ledger::load(&p).unwrap(), Ledger::default());
        let mut l = Ledger::default();
        l.entries
            .insert("sha256:x".into(), entry(Verdict::Accepted));
        l.save_atomic(&p).unwrap();
        assert_eq!(Ledger::load(&p).unwrap(), l);
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.contains("\"verdict\": \"accepted\""), "{text}");
    }
}
