//! Opt-in wire log: one JSON line per provider request/response.
//!
//! The event log records what the *engine* decided; it does not record what
//! the model was actually shown or what it literally said back. When a model
//! answers oddly that gap is the whole debugging problem, so `NS_TRACE=<path>`
//! appends the raw exchange here instead of requiring a proxy in front of the
//! provider.
//!
//! Headers are never written: the API key lives in one. Bodies are, so a
//! trace file holds the full conversation — treat it like the session log.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub struct Trace {
    path: PathBuf,
    file: Mutex<std::fs::File>,
}

impl Trace {
    /// Appends to `path`, creating it. Err when the file cannot be opened —
    /// a trace the user asked for and did not get must not pass silently.
    pub fn open(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        Ok(Self {
            path,
            file: Mutex::new(file),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// One line per attempt. A write failure is reported once to stderr and
    /// otherwise ignored: tracing must never take a turn down.
    pub fn record(&self, entry: &serde_json::Value) {
        let mut line = match serde_json::to_string(entry) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("trace: {e}");
                return;
            }
        };
        line.push('\n');
        let mut file = match self.file.lock() {
            Ok(f) => f,
            Err(p) => p.into_inner(),
        };
        if let Err(e) = file.write_all(line.as_bytes()) {
            eprintln!("trace: {e}");
        }
    }
}

/// Milliseconds since the epoch, for the `at` field.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appends_one_line_per_record_and_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trace.jsonl");
        {
            let t = Trace::open(&path).unwrap();
            t.record(&serde_json::json!({"n": 1}));
            t.record(&serde_json::json!({"n": 2}));
        }
        // Reopening appends rather than truncating.
        Trace::open(&path)
            .unwrap()
            .record(&serde_json::json!({"n": 3}));
        let text = std::fs::read_to_string(&path).unwrap();
        let ns: Vec<i64> = text
            .lines()
            .map(|l| {
                serde_json::from_str::<serde_json::Value>(l).unwrap()["n"]
                    .as_i64()
                    .unwrap()
            })
            .collect();
        assert_eq!(ns, vec![1, 2, 3]);
    }

    #[test]
    fn a_bad_path_is_an_error_not_a_silent_no_op() {
        assert!(Trace::open("/definitely/not/a/directory/trace.jsonl").is_err());
    }
}
