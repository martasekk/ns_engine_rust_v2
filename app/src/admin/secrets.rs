//! The values the config only ever names.
//!
//! Every credential in this workspace is named rather than held: `[llm]`
//! names `api_key_env`, `[auth]` names `signing_key_envs`, `[whatsapp]`
//! names three more. That rule is what keeps a key out of a file somebody
//! commits, and a settings page is exactly the thing that would break it by
//! offering a box to type the key into the config.
//!
//! So the page writes values *here* instead: a `.env` beside the config,
//! read into the environment at startup, gitignored, and never part of what
//! the config says. The config keeps naming variables; this is where the
//! variables get their values. What the page can do is set one, replace one,
//! and remove one.
//!
//! **A value never goes back to the browser.** The page is told a name, what
//! the config uses it for, and whether it is set — never the secret itself.
//! A settings page that renders keys puts them in the scrollback, the
//! screenshot and the browser cache of anyone who opens it.
//!
//! **The process environment wins.** A variable already exported is left
//! alone: an operator who ran `export NS_SIGNING_KEY_ACME=…` has said what
//! they meant, and a file quietly overriding that would make the two
//! disagree with no way to see which was in force.

use std::collections::BTreeMap;
use std::path::Path;

/// The file, beside `config.toml`. Named like the convention it is, so that
/// the first thing anybody does with it — add it to `.gitignore` — is the
/// thing they already know to do.
pub(crate) const SECRETS_FILE: &str = ".env";

/// One variable, as the page is allowed to see it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) struct SecretStatus {
    pub(crate) name: String,
    /// What the config uses it for, in words: "acme's signing key", "the
    /// emitter's provider key".
    pub(crate) used_for: Vec<String>,
    pub(crate) set: bool,
    /// Where the value came from. Never *what* it is.
    pub(crate) source: Source,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Source {
    /// Exported into the process before it started. The page cannot change
    /// it, and says so rather than writing a file that would be ignored.
    Environment,
    /// This file. The page owns it.
    File,
    None,
}

/// Reads the file into name → value. Not public, and deliberately never
/// returned to a caller that renders: [`status`] is the shape the page sees.
fn read(path: &Path) -> BTreeMap<String, String> {
    let mut values = BTreeMap::new();
    let Ok(text) = std::fs::read_to_string(path) else {
        return values;
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((name, value)) = line.split_once('=') else {
            continue;
        };
        // Quotes are stripped, because a value pasted from a shell line
        // often arrives wearing them and a key with a quote in it fails at
        // the provider with a message about authentication.
        let value = value.trim();
        let value = value
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .unwrap_or(value);
        values.insert(name.trim().to_string(), value.to_string());
    }
    values
}

/// Loads the file into the process environment, leaving anything already
/// exported alone. Called once at startup, before any config is read.
///
/// Silent when the file is not there, which is the normal case for a
/// deployment that exports its own variables.
pub(crate) fn load_into_environment(dir: &Path) {
    for (name, value) in read(&dir.join(SECRETS_FILE)) {
        if std::env::var_os(&name).is_none() {
            std::env::set_var(name, value);
        }
    }
}

/// Sets one variable in the file, and in this process, so a page that just
/// added a provider key does not need a restart to report it as set.
pub(crate) fn set(dir: &Path, name: &str, value: &str) -> Result<(), String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("a variable needs a name".into());
    }
    // The charset an environment variable may have. Refused rather than
    // written, because a name with an `=` or a newline in it would come back
    // as a different variable, or as two.
    if !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        || name.as_bytes()[0].is_ascii_digit()
    {
        return Err(format!(
            "{name:?} is not usable as an environment variable: letters, digits and \
             underscores, and not starting with a digit"
        ));
    }
    if value.contains('\n') {
        return Err("a value cannot contain a newline".into());
    }
    let path = dir.join(SECRETS_FILE);
    let mut values = read(&path);
    values.insert(name.to_string(), value.to_string());
    write(&path, &values)?;
    std::env::set_var(name, value);
    Ok(())
}

/// Removes one variable from the file. It stays in *this* process, which
/// already read it — the page says so rather than pretending otherwise.
pub(crate) fn remove(dir: &Path, name: &str) -> Result<(), String> {
    let path = dir.join(SECRETS_FILE);
    let mut values = read(&path);
    values.remove(name);
    write(&path, &values)
}

fn write(path: &Path, values: &BTreeMap<String, String>) -> Result<(), String> {
    let mut text = String::from(
        "# Values for the variables config.toml and tenants/*.toml name.\n\
         # Written by `ns-app admin`. Keep it out of version control.\n",
    );
    for (name, value) in values {
        // Quoted, so a value with a space in it survives, and because a
        // reader pasting a line into a shell gets what they expect.
        text.push_str(&format!("{name}=\"{value}\"\n"));
    }
    let temporary = path.with_extension("saving");
    std::fs::write(&temporary, &text).map_err(|e| format!("{}: {e}", temporary.display()))?;
    if path.exists() {
        std::fs::remove_file(path).map_err(|e| format!("{}: {e}", path.display()))?;
    }
    std::fs::rename(&temporary, path).map_err(|e| format!("{}: {e}", path.display()))
}

/// What the page is told about every variable the config names.
///
/// `wanted` is the list the schema builds by reading the config: a variable
/// nothing names is not shown, because a settings page listing the whole
/// environment is a way to learn what else is on the machine.
pub(crate) fn status(dir: &Path, wanted: &[(String, String)]) -> Vec<SecretStatus> {
    let in_file = read(&dir.join(SECRETS_FILE));
    let mut by_name: BTreeMap<String, SecretStatus> = BTreeMap::new();
    for (name, used_for) in wanted {
        let entry = by_name.entry(name.clone()).or_insert_with(|| {
            let exported = std::env::var_os(name).is_some();
            let from_file = in_file.contains_key(name);
            SecretStatus {
                name: name.clone(),
                used_for: Vec::new(),
                set: exported || from_file,
                // Exported wins, and is reported as such, because that is
                // the value in force and the page cannot change it.
                source: match (exported, from_file) {
                    (true, _) => Source::Environment,
                    (false, true) => Source::File,
                    (false, false) => Source::None,
                },
            }
        });
        if !entry.used_for.contains(used_for) {
            entry.used_for.push(used_for.clone());
        }
    }
    by_name.into_values().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wanted(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect()
    }

    #[test]
    fn a_value_set_here_comes_back_as_set_and_never_as_itself() {
        let dir = tempfile::tempdir().expect("tempdir");
        set(dir.path(), "NS_TEST_ADMIN_KEY_A", "sk-the-actual-secret").expect("sets");

        let shown = status(
            dir.path(),
            &wanted(&[("NS_TEST_ADMIN_KEY_A", "the emitter's key")]),
        );
        assert_eq!(shown.len(), 1);
        assert!(shown[0].set);
        assert_eq!(shown[0].used_for, vec!["the emitter's key"]);
        // The whole point: what the page receives can be rendered anywhere
        // without leaking anything.
        let json = serde_json::to_string(&shown).expect("serializes");
        assert!(
            !json.contains("sk-the-actual-secret"),
            "a secret reached the page: {json}"
        );
        std::env::remove_var("NS_TEST_ADMIN_KEY_A");
    }

    #[test]
    fn a_variable_nothing_names_is_not_listed() {
        let dir = tempfile::tempdir().expect("tempdir");
        set(dir.path(), "NS_TEST_ADMIN_UNRELATED", "value").expect("sets");
        // A page that listed the whole environment would be a way to read
        // what else is on the machine.
        let shown = status(
            dir.path(),
            &wanted(&[("NS_TEST_ADMIN_ABSENT", "something")]),
        );
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].name, "NS_TEST_ADMIN_ABSENT");
        assert!(!shown[0].set);
        assert_eq!(shown[0].source, Source::None);
        std::env::remove_var("NS_TEST_ADMIN_UNRELATED");
    }

    #[test]
    fn what_was_exported_wins_over_the_file_and_is_reported_as_exported() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join(SECRETS_FILE),
            "NS_TEST_ADMIN_KEY_B=\"from-the-file\"\n",
        )
        .expect("the file");
        std::env::set_var("NS_TEST_ADMIN_KEY_B", "from-the-environment");

        load_into_environment(dir.path());
        assert_eq!(
            std::env::var("NS_TEST_ADMIN_KEY_B").as_deref(),
            Ok("from-the-environment"),
            "an exported value is not quietly replaced by a file"
        );
        let shown = status(dir.path(), &wanted(&[("NS_TEST_ADMIN_KEY_B", "a key")]));
        assert_eq!(shown[0].source, Source::Environment);
        std::env::remove_var("NS_TEST_ADMIN_KEY_B");
    }

    #[test]
    fn the_file_fills_in_what_was_not_exported() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::env::remove_var("NS_TEST_ADMIN_KEY_C");
        std::fs::write(
            dir.path().join(SECRETS_FILE),
            "# a comment\n\nNS_TEST_ADMIN_KEY_C = \"from-the-file\"\n",
        )
        .expect("the file");
        load_into_environment(dir.path());
        assert_eq!(
            std::env::var("NS_TEST_ADMIN_KEY_C").as_deref(),
            Ok("from-the-file"),
            "comments, blank lines and spacing are skipped"
        );
        std::env::remove_var("NS_TEST_ADMIN_KEY_C");
    }

    #[test]
    fn a_name_that_is_not_a_variable_is_refused_rather_than_written() {
        let dir = tempfile::tempdir().expect("tempdir");
        for bad in [
            "",
            "has space",
            "HAS=EQUALS",
            "1LEADING_DIGIT",
            "has\nnewline",
        ] {
            assert!(set(dir.path(), bad, "v").is_err(), "{bad:?} was accepted");
        }
        assert!(
            set(dir.path(), "NS_TEST_ADMIN_KEY_D", "line\nbreak").is_err(),
            "a value with a newline would come back as two variables"
        );
        std::env::remove_var("NS_TEST_ADMIN_KEY_D");
    }

    #[test]
    fn removing_one_leaves_the_others_alone() {
        let dir = tempfile::tempdir().expect("tempdir");
        set(dir.path(), "NS_TEST_ADMIN_KEY_E", "one").expect("sets");
        set(dir.path(), "NS_TEST_ADMIN_KEY_F", "two").expect("sets");
        remove(dir.path(), "NS_TEST_ADMIN_KEY_E").expect("removes");

        let text = std::fs::read_to_string(dir.path().join(SECRETS_FILE)).expect("reads");
        assert!(!text.contains("NS_TEST_ADMIN_KEY_E"), "{text}");
        assert!(text.contains("NS_TEST_ADMIN_KEY_F"), "{text}");
        std::env::remove_var("NS_TEST_ADMIN_KEY_E");
        std::env::remove_var("NS_TEST_ADMIN_KEY_F");
    }
}
