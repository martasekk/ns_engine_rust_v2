//! Atomic file I/O for `learned.toml` and the ledger: write to a temp file in
//! the same directory, fsync, rename. A crash mid-write leaves the old file.
use nscore::LearnedRules;
use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum FileError {
    #[error("io: {0}")]
    Io(String),
    #[error("parse: {0}")]
    Parse(String),
}

pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), FileError> {
    use std::io::Write;
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut tmp = tempfile::NamedTempFile::new_in(dir).map_err(|e| FileError::Io(e.to_string()))?;
    tmp.write_all(bytes)
        .map_err(|e| FileError::Io(e.to_string()))?;
    tmp.as_file()
        .sync_all()
        .map_err(|e| FileError::Io(e.to_string()))?;
    tmp.persist(path)
        .map_err(|e| FileError::Io(e.error.to_string()))?;
    Ok(())
}

/// Missing file → defaults. Unparsable file → `Parse` (the caller decides
/// whether that is fatal; ns-app treats it so at startup).
pub fn load_rules(path: &Path) -> Result<LearnedRules, FileError> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            toml::from_str(&text).map_err(|e| FileError::Parse(format!("{}: {e}", path.display())))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(LearnedRules::default()),
        Err(e) => Err(FileError::Io(e.to_string())),
    }
}

pub fn save_rules_atomic(path: &Path, rules: &LearnedRules) -> Result<(), FileError> {
    let text = toml::to_string_pretty(rules).map_err(|e| FileError::Parse(e.to_string()))?;
    write_atomic(path, text.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use nscore::{AliasAction, Note};

    #[test]
    fn missing_file_loads_as_defaults_and_round_trips_after_save() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("learned.toml");
        assert_eq!(load_rules(&p).unwrap(), LearnedRules::default());
        let rules = LearnedRules {
            alias_action: vec![AliasAction {
                from: "a".into(),
                to: "b".into(),
            }],
            notes: vec![Note::new("global", "Be brief.", 0.25)],
            ..Default::default()
        };
        save_rules_atomic(&p, &rules).unwrap();
        assert_eq!(load_rules(&p).unwrap(), rules);
        // No temp file left behind.
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(leftovers, vec![std::ffi::OsString::from("learned.toml")]);
    }

    #[test]
    fn unparsable_file_is_a_parse_error() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("learned.toml");
        std::fs::write(&p, "version = \"one\"\n[[alias_action]]\nfrom = 1").unwrap();
        assert!(matches!(load_rules(&p), Err(FileError::Parse(_))));
    }
}
