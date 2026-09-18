//! Which platform messages this shard has already run a turn for.
//!
//! A platform retries anything it did not get a `200` for — Meta for up to
//! seven days — and may batch several messages into one delivery. The
//! engine's own append is idempotent by event id, so the *log* is safe; what
//! is not safe is the turn, which would run again, answer the customer a
//! second time and bill a second time (plan H4).
//!
//! So the set is persistent, not in memory: a retry outlives a restart, and
//! a seen-set that did not would let the first delivery after every deploy
//! run twice. It is a file of one `tenant\tmessage-id` per line, appended on
//! each accepted message and read back at startup — a table would be a
//! better answer at a hundred shards, and this is the answer that is
//! reviewable with `cat` and deletable with `rm`.
//!
//! It is bounded. Past [`CAP`] entries the oldest are forgotten and the file
//! is rewritten with what is left, so the cost of a busy month is a rewrite
//! and not an unbounded file. Forgetting an id means a retry older than the
//! last [`CAP`] messages would run twice — which is a worse failure than
//! forgetting, so the cap is high enough that reaching it takes a flood
//! rather than a week.

use std::collections::{HashSet, VecDeque};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

/// How many ids are remembered. At a message a second, about fourteen
/// hours; at a realistic rate, months.
const CAP: usize = 50_000;

pub struct SeenIds {
    /// `None` for the in-memory set the tests use. A shard always has a
    /// path: see the module note on why.
    path: Option<PathBuf>,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    ids: HashSet<String>,
    /// Insertion order, so the cap forgets the oldest.
    order: VecDeque<String>,
}

impl SeenIds {
    /// Opens the set at `path`, reading back what is already there. A file
    /// that cannot be read is reported and treated as empty: refusing to
    /// start because a dedupe file is corrupt would take a shard down over
    /// something whose worst case is one duplicated reply.
    pub fn open(path: impl Into<PathBuf>) -> SeenIds {
        let path = path.into();
        let mut state = State::default();
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                for line in text.lines().filter(|l| !l.trim().is_empty()) {
                    state.remember(line.to_string());
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => eprintln!(
                "http: cannot read the seen-message file {}: {e} — starting empty, so a \
                 redelivery from before now may be answered twice",
                path.display()
            ),
        }
        SeenIds {
            path: Some(path),
            state: Mutex::new(state),
        }
    }

    /// A set that forgets everything when the process ends. For tests, and
    /// for a shard that has deliberately said it does not want the file.
    pub fn in_memory() -> SeenIds {
        SeenIds {
            path: None,
            state: Mutex::new(State::default()),
        }
    }

    /// True if this message has not been handled before, and records it.
    ///
    /// One call decides and records, because two — a check and a later mark
    /// — is a window in which a retry arriving beside the original runs the
    /// turn twice, which is the whole thing this exists to prevent.
    pub fn first_time(&self, tenant: &str, message_id: &str) -> bool {
        let key = format!("{tenant}\t{message_id}");
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if state.ids.contains(&key) {
            return false;
        }
        let evicted = state.remember(key.clone());
        if let Some(path) = &self.path {
            if evicted {
                // Rewritten rather than appended, so the file cannot grow
                // past the cap however long the shard runs.
                let kept: Vec<&str> = state.order.iter().map(String::as_str).collect();
                if let Err(e) = std::fs::write(path, kept.join("\n") + "\n") {
                    eprintln!("http: cannot rewrite {}: {e}", path.display());
                }
            } else if let Err(e) = append(path, &key) {
                // The turn still runs: a message this shard cannot record
                // is better answered once now and possibly twice after a
                // restart than not answered at all.
                eprintln!(
                    "http: cannot record message id in {}: {e} — a redelivery of it after a \
                     restart would be answered twice",
                    path.display()
                );
            }
        }
        true
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.state.lock().expect("a test's own lock").ids.len()
    }
}

impl State {
    /// Records one key, forgetting the oldest if the cap is reached. True
    /// when something was forgotten, which is what makes the file need a
    /// rewrite rather than an append.
    fn remember(&mut self, key: String) -> bool {
        if !self.ids.insert(key.clone()) {
            return false;
        }
        self.order.push_back(key);
        let mut evicted = false;
        while self.order.len() > CAP {
            if let Some(oldest) = self.order.pop_front() {
                self.ids.remove(&oldest);
                evicted = true;
            }
        }
        evicted
    }
}

fn append(path: &PathBuf, key: &str) -> std::io::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(file, "{key}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_repeated_message_id_is_seen_once_and_the_second_time_is_not_the_first() {
        let seen = SeenIds::in_memory();
        assert!(seen.first_time("acme", "wamid.1"));
        assert!(!seen.first_time("acme", "wamid.1"), "the retry");
        assert!(seen.first_time("acme", "wamid.2"));
    }

    /// Two companies' ids live in one set, and a platform that numbers its
    /// messages per account must not let one company's id silence another's
    /// message.
    #[test]
    fn the_same_id_from_two_companies_is_two_messages() {
        let seen = SeenIds::in_memory();
        assert!(seen.first_time("acme", "1"));
        assert!(seen.first_time("globex", "1"));
    }

    /// The point of the file: a retry that arrives after a restart is still
    /// a retry.
    #[test]
    fn a_restart_does_not_forget_what_was_already_answered() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("seen.tsv");
        let seen = SeenIds::open(&path);
        assert!(seen.first_time("acme", "wamid.1"));
        drop(seen);

        let reopened = SeenIds::open(&path);
        assert!(
            !reopened.first_time("acme", "wamid.1"),
            "a redelivery after a restart is not a new message"
        );
        assert!(reopened.first_time("acme", "wamid.2"));
    }

    /// A file that is not there is an empty set, not a refusal to start.
    #[test]
    fn a_missing_file_is_an_empty_set() {
        let dir = tempfile::tempdir().expect("tempdir");
        let seen = SeenIds::open(dir.path().join("nothing-here.tsv"));
        assert_eq!(seen.len(), 0);
        assert!(seen.first_time("acme", "1"));
    }
}
