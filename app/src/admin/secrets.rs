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

/// One place in the configuration that names a variable.
///
/// The page needs the place and not just the name: a key is picked for a
/// company on one tab and its consequences land on another, and a row that
/// said only "used somewhere" would leave an operator opening files to find
/// out where.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) struct Reference {
    pub(crate) name: String,
    /// What it is for, in words: "acme's signing key", "the emitter's
    /// provider key".
    pub(crate) used_for: String,
    /// The company whose overlay names it, or none for the process config.
    pub(crate) company: Option<String>,
    /// The dotted path that names it, e.g. `llm.api_key_env`.
    pub(crate) path: String,
}

/// One variable, as the page is allowed to see it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) struct SecretStatus {
    pub(crate) name: String,
    /// What the config uses it for, in words.
    pub(crate) used_for: Vec<String>,
    /// Every place that names it, so a row can be read without opening a
    /// file.
    pub(crate) referenced_by: Vec<Reference>,
    /// The distinct companies among those references.
    pub(crate) companies: Vec<String>,
    /// Whether the process's own config names it too. The process is an
    /// owner like any company here: picking the house key for a company
    /// from the vault dropdown is a two-click mistake, and it puts that
    /// company on the deployment's own quota.
    pub(crate) process: bool,
    /// Two owners or more on one entry. They share the provider's quota
    /// and — by the throttle's key of (base URL, key hash) — one throttle,
    /// so one company's traffic paces the other's. Sometimes right; never
    /// something to find out later, which is why it is a field here and not
    /// an inference the page might forget to draw.
    pub(crate) shared: bool,
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
pub(crate) fn status(dir: &Path, wanted: &[Reference]) -> Vec<SecretStatus> {
    let in_file = read(&dir.join(SECRETS_FILE));
    gather(&in_file, wanted).into_values().collect()
}

/// The vault: everything [`status`] shows, plus every variable the file
/// holds that nothing names yet.
///
/// An unreferenced entry is listed rather than hidden because minting a key
/// before the company that will use it is the normal order of work, and a
/// key that vanished after being saved would read as a save that failed.
///
/// Only the *file* is enumerated, never the environment. A page that listed
/// every variable on the machine would be a way to learn what else runs on
/// it, which is the same reason [`status`] shows only what the config names.
pub(crate) fn vault(dir: &Path, wanted: &[Reference]) -> Vec<SecretStatus> {
    let in_file = read(&dir.join(SECRETS_FILE));
    let mut by_name = gather(&in_file, wanted);
    for name in in_file.keys() {
        by_name
            .entry(name.clone())
            .or_insert_with(|| blank(&in_file, name));
    }
    by_name.into_values().collect()
}

/// Everything naming this variable, named the way the page would say it:
/// every company, and "the process" when the deployment's own config names
/// it too. Two of them is what makes an entry shared, and what makes
/// removing it a refusal rather than a convenience.
///
/// The process counts because it spends the same quota out of the same
/// throttle. Leaving it out would let the commonest version of the mistake
/// — giving a company the house key — pass without a word.
pub(crate) fn owners_referencing(wanted: &[Reference], name: &str) -> Vec<String> {
    let mut owners: Vec<String> = Vec::new();
    for reference in wanted.iter().filter(|r| r.name == name) {
        let owner = reference
            .company
            .clone()
            .unwrap_or_else(|| "the process".into());
        if !owners.contains(&owner) {
            owners.push(owner);
        }
    }
    owners
}

fn gather(
    in_file: &BTreeMap<String, String>,
    wanted: &[Reference],
) -> BTreeMap<String, SecretStatus> {
    let mut by_name: BTreeMap<String, SecretStatus> = BTreeMap::new();
    for reference in wanted {
        let entry = by_name
            .entry(reference.name.clone())
            .or_insert_with(|| blank(in_file, &reference.name));
        if !entry.used_for.contains(&reference.used_for) {
            entry.used_for.push(reference.used_for.clone());
        }
        if !entry.referenced_by.contains(reference) {
            entry.referenced_by.push(reference.clone());
        }
        match &reference.company {
            Some(company) => {
                if !entry.companies.contains(company) {
                    entry.companies.push(company.clone());
                }
            }
            None => entry.process = true,
        }
        entry.shared = entry.companies.len() + usize::from(entry.process) > 1;
    }
    by_name
}

/// A variable nothing has claimed yet: known to exist, and nothing more.
fn blank(in_file: &BTreeMap<String, String>, name: &str) -> SecretStatus {
    let exported = std::env::var_os(name).is_some();
    let from_file = in_file.contains_key(name);
    SecretStatus {
        name: name.to_string(),
        used_for: Vec::new(),
        referenced_by: Vec::new(),
        companies: Vec::new(),
        process: false,
        shared: false,
        set: exported || from_file,
        // Exported wins, and is reported as such, because that is the value
        // in force and the page cannot change it.
        source: match (exported, from_file) {
            (true, _) => Source::Environment,
            (false, true) => Source::File,
            (false, false) => Source::None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wanted(pairs: &[(&str, &str)]) -> Vec<Reference> {
        pairs
            .iter()
            .map(|(name, used_for)| Reference {
                name: (*name).to_string(),
                used_for: (*used_for).to_string(),
                company: None,
                path: "llm.api_key_env".to_string(),
            })
            .collect()
    }

    /// The same variable, named by one company's overlay.
    fn by_company(name: &str, company: &str) -> Reference {
        Reference {
            name: name.to_string(),
            used_for: format!("{company}'s provider key"),
            company: Some(company.to_string()),
            path: "llm.api_key_env".to_string(),
        }
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

    /// A key minted before the company that will use it exists is the
    /// normal order of work. The vault says so; `status` still does not,
    /// because the form only draws what the config names.
    #[test]
    fn a_key_nothing_references_yet_is_in_the_vault_and_says_it_is_unused() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join(SECRETS_FILE),
            "NS_TEST_ADMIN_SPARE=\"minted-early\"\n",
        )
        .expect("the file");
        std::env::remove_var("NS_TEST_ADMIN_SPARE");

        let entries = vault(dir.path(), &[]);
        let spare = entries
            .iter()
            .find(|e| e.name == "NS_TEST_ADMIN_SPARE")
            .expect("the unreferenced entry is listed");
        assert!(spare.set);
        assert_eq!(spare.source, Source::File);
        assert!(spare.referenced_by.is_empty(), "nothing names it yet");
        assert!(status(dir.path(), &[]).is_empty(), "the form draws nothing");

        let json = serde_json::to_string(&entries).expect("serializes");
        assert!(!json.contains("minted-early"), "a value reached the page");
    }

    /// H1. Two companies on one entry share the provider's quota and one
    /// throttle; the row carries both names so the page can say so.
    #[test]
    fn a_key_two_companies_reference_is_marked_shared_and_names_both() {
        let dir = tempfile::tempdir().expect("tempdir");
        let wanted = vec![
            by_company("NS_TEST_ADMIN_SHARED", "acme"),
            by_company("NS_TEST_ADMIN_SHARED", "beta"),
        ];
        let entries = vault(dir.path(), &wanted);
        let shared = entries
            .iter()
            .find(|e| e.name == "NS_TEST_ADMIN_SHARED")
            .expect("the entry");
        assert!(shared.shared);
        assert_eq!(shared.companies, vec!["acme", "beta"]);
        assert_eq!(shared.referenced_by.len(), 2);
        assert_eq!(
            owners_referencing(&wanted, "NS_TEST_ADMIN_SHARED"),
            vec!["acme", "beta"]
        );
        assert!(!shared.process, "no process reference here");
    }

    /// The commonest version of the mistake: a company handed the key the
    /// deployment itself runs on. One quota, one throttle, and until now
    /// nothing said so because only companies were counted.
    #[test]
    fn a_key_the_process_and_a_company_share_is_shared_too() {
        let dir = tempfile::tempdir().expect("tempdir");
        let wanted = vec![
            Reference {
                name: "NS_TEST_ADMIN_HOUSE_KEY".into(),
                used_for: "the emitter's provider key".into(),
                company: None,
                path: "llm.api_key_env".into(),
            },
            Reference {
                path: "llm.api_key_env".into(),
                name: "NS_TEST_ADMIN_HOUSE_KEY".into(),
                ..by_company("NS_TEST_ADMIN_HOUSE_KEY", "acme")
            },
        ];
        let entry = vault(dir.path(), &wanted)
            .into_iter()
            .find(|e| e.name == "NS_TEST_ADMIN_HOUSE_KEY")
            .expect("the entry");
        assert!(entry.process, "the process names it");
        assert_eq!(entry.companies, vec!["acme"]);
        assert!(entry.shared, "one company plus the process is two owners");
        assert_eq!(
            owners_referencing(&wanted, "NS_TEST_ADMIN_HOUSE_KEY"),
            vec!["the process", "acme"]
        );
    }

    /// One company naming a key twice — its emitter and its summarizer, say
    /// — is not sharing it with anybody.
    #[test]
    fn one_company_naming_a_key_twice_is_not_shared() {
        let dir = tempfile::tempdir().expect("tempdir");
        let wanted = vec![
            Reference {
                path: "llm.api_key_env".into(),
                ..by_company("NS_TEST_ADMIN_ONCE", "acme")
            },
            Reference {
                path: "llm.summarizer.api_key_env".into(),
                ..by_company("NS_TEST_ADMIN_ONCE", "acme")
            },
        ];
        let entry = vault(dir.path(), &wanted)
            .into_iter()
            .find(|e| e.name == "NS_TEST_ADMIN_ONCE")
            .expect("the entry");
        assert!(!entry.shared, "{:?}", entry.companies);
        assert_eq!(entry.companies, vec!["acme"]);
        assert_eq!(entry.referenced_by.len(), 2, "both places are named");
    }
}
