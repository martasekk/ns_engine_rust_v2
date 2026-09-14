//! Changing one value in a TOML file without rewriting the file.
//!
//! The config is not a serialization of a struct — it is a document someone
//! wrote, with the reasons for each choice in the comments beside it, and
//! `config.example.toml` is mostly comment by volume. Round-tripping it
//! through `toml::to_string` would give back something semantically equal
//! and throw all of that away, which is why this uses `toml_edit`: it
//! parses to a document that remembers its own formatting, and setting one
//! key touches one key.
//!
//! **Nothing is written that would not load.** Every save parses the edited
//! document, folds it through `AppConfig` exactly as startup does, and
//! writes only if that succeeded — so the failure mode of this UI is a
//! refusal on screen rather than a process that will not start next time.
//! Then it writes through a temporary file and renames, because a crash
//! halfway through writing a config is the one way a settings page can cost
//! somebody their engine.

use std::path::Path;

use toml_edit::{Array, DocumentMut, Item, Table, Value};

/// A value as it arrives from the page: JSON, because that is what a form
/// sends, and the schema says which of these each field may be.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Setting {
    Text(String),
    Number(i64),
    Flag(bool),
    List(Vec<String>),
    /// The field was cleared. The key is removed rather than set empty, so
    /// the config falls back to its default instead of carrying `""` — and
    /// an empty `listen` is not the same thing as no `listen` at all.
    Unset,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum EditError {
    #[error("{path}: {detail}")]
    Malformed { path: String, detail: String },
    #[error("{0} is not a key this page may set")]
    UnknownField(String),
    #[error("the result would not load: {0}")]
    WouldNotLoad(String),
    #[error("{path}: {detail}")]
    Io { path: String, detail: String },
}

/// Reads a TOML file into an editable document. A file that is not there is
/// an empty document: a company whose overlay has never been written is
/// configured entirely by its defaults, and saving one field is how the
/// file comes to exist.
pub(crate) fn read(path: &Path) -> Result<DocumentMut, EditError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            return Err(EditError::Io {
                path: path.display().to_string(),
                detail: e.to_string(),
            })
        }
    };
    text.parse::<DocumentMut>()
        .map_err(|e| EditError::Malformed {
            path: path.display().to_string(),
            detail: e.to_string(),
        })
}

/// Sets (or removes) one dotted path, creating the tables on the way down.
///
/// `llm.emitter.model` means `[llm.emitter] model`. Tables this creates are
/// marked implicit, so writing `llm.emitter.model` into a file with no
/// `[llm]` section produces `[llm.emitter]` alone rather than an empty
/// `[llm]` header above it.
pub(crate) fn set(doc: &mut DocumentMut, path: &str, value: Setting) -> Result<(), EditError> {
    let mut segments: Vec<&str> = path.split('.').collect();
    let key = segments
        .pop()
        .ok_or_else(|| EditError::UnknownField(path.into()))?;
    let mut table = doc.as_table_mut();
    for segment in segments {
        let entry = table
            .entry(segment)
            .or_insert_with(|| Item::Table(implicit_table()));
        table = entry.as_table_mut().ok_or_else(|| EditError::Malformed {
            path: path.to_string(),
            detail: format!("{segment} is already a value, not a section"),
        })?;
    }
    match value {
        Setting::Unset => {
            table.remove(key);
        }
        other => {
            let mut replacement = into_value(other);
            // The decoration is the whitespace and comments around a value,
            // and it belongs to the value rather than to the key — so
            // assigning a fresh one would drop the `# why` written after it
            // on the same line. Carried across, the line keeps its note and
            // only the value changes.
            if let Some(existing) = table.get_mut(key).and_then(Item::as_value_mut) {
                *replacement.decor_mut() = existing.decor().clone();
            }
            table[key] = Item::Value(replacement);
        }
    }
    Ok(())
}

/// Reads one dotted path back, for the page's own rendering.
pub(crate) fn get(doc: &DocumentMut, path: &str) -> Option<Setting> {
    let mut item: &Item = doc.as_item();
    for segment in path.split('.') {
        item = item.as_table_like()?.get(segment)?;
    }
    let value = item.as_value()?;
    Some(match value {
        Value::String(s) => Setting::Text(s.value().clone()),
        Value::Integer(n) => Setting::Number(*n.value()),
        Value::Boolean(b) => Setting::Flag(*b.value()),
        Value::Array(a) => Setting::List(
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect(),
        ),
        // A float or a datetime is not something this page offers, and
        // showing it as text would invite saving it back as a string.
        _ => return None,
    })
}

fn implicit_table() -> Table {
    let mut table = Table::new();
    table.set_implicit(true);
    table
}

fn into_value(setting: Setting) -> Value {
    match setting {
        Setting::Text(s) => Value::from(s),
        Setting::Number(n) => Value::from(n),
        Setting::Flag(b) => Value::from(b),
        Setting::List(items) => {
            let mut array = Array::new();
            for item in items {
                array.push(item);
            }
            Value::Array(array)
        }
        // Handled by the caller, which removes the key instead.
        Setting::Unset => Value::from(""),
    }
}

/// Writes a document that has already been checked, through a temporary
/// file in the same directory and a rename. The rename is what makes a
/// reader see either the old file or the new one and never half of either.
pub(crate) fn write(path: &Path, doc: &DocumentMut) -> Result<(), EditError> {
    let failed = |e: std::io::Error| EditError::Io {
        path: path.display().to_string(),
        detail: e.to_string(),
    };
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(failed)?;
    }
    let temporary = path.with_extension("toml.saving");
    std::fs::write(&temporary, doc.to_string()).map_err(failed)?;
    // Windows will not rename onto an existing file, so the old one goes
    // first. The window between is why the temporary is written first: the
    // content is already safely on disk before anything is removed.
    if path.exists() {
        std::fs::remove_file(path).map_err(failed)?;
    }
    std::fs::rename(&temporary, path).map_err(failed)
}

/// The gate every save passes: does this document still load as a config?
///
/// The same parse startup does, so a save that would make `ns-app` refuse to
/// start is refused here instead, while the running process still has the
/// config it started with.
pub(crate) fn loads_as_config(doc: &DocumentMut) -> Result<(), EditError> {
    crate::config::AppConfig::parse(&doc.to_string())
        .map(|_| ())
        .map_err(|e| EditError::WouldNotLoad(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMMENTED: &str = "\
# The provider this deployment drives. One word swaps the agent.
[llm]
provider = \"ollama\"   # a local server

# Where the socket listens.
[serve]
listen = \"127.0.0.1:7375\"
max_connections = 8
";

    /// The whole reason this module exists rather than a serde round trip.
    #[test]
    fn setting_one_key_keeps_every_comment_and_every_other_key() {
        let mut doc = COMMENTED.parse::<DocumentMut>().expect("parses");
        set(&mut doc, "llm.provider", Setting::Text("openrouter".into())).expect("sets");
        let out = doc.to_string();

        assert!(out.contains("provider = \"openrouter\""));
        assert!(
            out.contains("# The provider this deployment drives. One word swaps the agent."),
            "the comment above it survived:\n{out}"
        );
        assert!(
            out.contains("# a local server"),
            "the comment beside it survived:\n{out}"
        );
        assert!(out.contains("# Where the socket listens."), "{out}");
        assert!(out.contains("max_connections = 8"), "{out}");
    }

    #[test]
    fn a_nested_path_creates_only_the_section_it_needs() {
        let mut doc = "".parse::<DocumentMut>().expect("parses");
        set(
            &mut doc,
            "llm.emitter.model",
            Setting::Text("qwen2.5:3b".into()),
        )
        .expect("sets");
        let out = doc.to_string();
        assert!(out.contains("[llm.emitter]"), "{out}");
        // An empty `[llm]` header above it would be noise in a file people
        // read.
        assert!(!out.contains("[llm]\n"), "{out}");
    }

    /// Clearing a field removes the key, so the config falls back to its
    /// default. Setting it to `""` would mean something different — and for
    /// `listen`, an empty string is "do not open the endpoint" rather than
    /// "use the default".
    #[test]
    fn clearing_a_field_removes_the_key_rather_than_emptying_it() {
        let mut doc = COMMENTED.parse::<DocumentMut>().expect("parses");
        set(&mut doc, "serve.max_connections", Setting::Unset).expect("unsets");
        let out = doc.to_string();
        assert!(!out.contains("max_connections"), "{out}");
        assert!(out.contains("listen = \"127.0.0.1:7375\""), "{out}");
    }

    #[test]
    fn every_kind_survives_being_written_and_read_back() {
        let mut doc = "".parse::<DocumentMut>().expect("parses");
        let cases = [
            ("http.listen", Setting::Text("127.0.0.1:8787".into())),
            ("http.max_connections", Setting::Number(256)),
            ("http.allow_remote", Setting::Flag(false)),
            (
                "http.origins",
                Setting::List(vec!["https://a.test".into(), "https://b.test".into()]),
            ),
        ];
        for (path, value) in &cases {
            set(&mut doc, path, value.clone()).expect("sets");
        }
        let reparsed = doc.to_string().parse::<DocumentMut>().expect("reparses");
        for (path, value) in &cases {
            assert_eq!(get(&reparsed, path).as_ref(), Some(value), "{path}");
        }
        assert_eq!(get(&reparsed, "http.nothing_here"), None);
    }

    /// The gate. A save that would stop the process from starting is
    /// refused while the running one still has a config that works.
    #[test]
    fn a_document_that_would_not_load_is_refused_before_it_is_written() {
        let mut doc = COMMENTED.parse::<DocumentMut>().expect("parses");
        set(
            &mut doc,
            "serve.max_connections",
            Setting::Text("lots".into()),
        )
        .expect("sets");
        let err = loads_as_config(&doc).expect_err("refused");
        assert!(matches!(err, EditError::WouldNotLoad(_)), "{err:?}");

        // And the same document with a number in it passes.
        set(&mut doc, "serve.max_connections", Setting::Number(4)).expect("sets");
        loads_as_config(&doc).expect("loads");
    }

    #[test]
    fn writing_replaces_the_file_and_leaves_no_temporary_behind() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, COMMENTED).expect("the file");

        let mut doc = read(&path).expect("reads");
        set(&mut doc, "llm.provider", Setting::Text("groq".into())).expect("sets");
        write(&path, &doc).expect("writes");

        let out = std::fs::read_to_string(&path).expect("reads back");
        assert!(out.contains("provider = \"groq\""), "{out}");
        assert!(out.contains("# a local server"), "{out}");
        assert!(
            !path.with_extension("toml.saving").exists(),
            "the temporary was renamed, not left"
        );
    }

    /// A company whose overlay has never been written is configured by its
    /// defaults; saving one field is how the file comes to exist.
    #[test]
    fn a_file_that_does_not_exist_is_an_empty_document_and_saving_creates_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("tenants").join("acme.toml");
        let mut doc = read(&path).expect("an absent file is empty, not an error");
        assert_eq!(doc.to_string(), "");
        set(
            &mut doc,
            "store.path",
            Setting::Text("ns-acme.sqlite".into()),
        )
        .expect("sets");
        write(&path, &doc).expect("writes, creating the directory");
        assert!(std::fs::read_to_string(&path)
            .expect("reads back")
            .contains("ns-acme.sqlite"));
    }
}
