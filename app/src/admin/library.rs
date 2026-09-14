//! The shared things, and who reaches them.
//!
//! A persona is prose in `personas/<name>.md`; a module is a file of
//! `[[http_component]]` entries in `modules/<name>.toml`. A company names
//! what it uses ([`crate::config::LibrarySection`]) rather than holding a
//! copy of it, which is the whole point: two companies meant to share a
//! persona that each held their own copy would drift apart with nothing
//! saying so.
//!
//! That property is also the hazard. Editing a shared thing edits it for
//! every company that names it, and the page has to say which ones *before*
//! the save rather than after — so every entry here carries its readers.
//!
//! Unlike [`super::secrets`], there is nothing here to keep from the
//! browser: a persona is text the operator wrote and a module is the same
//! TOML they would otherwise paste into a config. Both go to the page in
//! full, because editing them is the point.

use std::path::{Path, PathBuf};

use crate::config::AppConfig;
use crate::tenant::{MODULE_DIR, PERSONA_DIR};

/// Which of the two libraries. The page sends the word, so it is parsed
/// rather than trusted: a caller naming its own directory would be a way to
/// write files anywhere under the root.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Kind {
    Persona,
    Module,
}

impl Kind {
    pub(crate) fn parse(word: &str) -> Option<Self> {
        match word {
            "persona" => Some(Self::Persona),
            "module" => Some(Self::Module),
            _ => None,
        }
    }

    fn dir(self) -> &'static str {
        match self {
            Self::Persona => PERSONA_DIR,
            Self::Module => MODULE_DIR,
        }
    }

    fn extension(self) -> &'static str {
        match self {
            Self::Persona => "md",
            Self::Module => "toml",
        }
    }
}

/// One thing in the library, as the page sees it.
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct Entry {
    pub(crate) name: String,
    pub(crate) kind: Kind,
    /// The prose, or the TOML. Not a secret, and not editable without it.
    pub(crate) text: String,
    /// Every company whose overlay names it. The list the page has to show
    /// before an edit, because an edit reaches all of them.
    pub(crate) used_by: Vec<String>,
    /// Companies that name it *and* write their own `[persona] text`, so
    /// the reference sits there unused. Reported rather than silently
    /// ignored: a company that looks like it shares a persona and does not
    /// is exactly the thing somebody will edit expecting a change.
    pub(crate) overridden_by: Vec<String>,
    /// For a module, how many tools it registers. Nothing for a persona.
    pub(crate) components: Option<usize>,
    /// Why this file cannot be used, if it cannot. A module that does not
    /// parse is still listed — it is the one that needs opening.
    pub(crate) problem: Option<String>,
}

/// A library name, which becomes a file name. The loader's own rule, so the
/// page and startup cannot disagree about what is nameable — and so the
/// check that stops `../../secrets` lives in one place.
pub(crate) fn valid_name(name: &str) -> bool {
    crate::tenant::valid_library_name(name)
}

fn path_of(root: &Path, kind: Kind, name: &str) -> PathBuf {
    root.join(kind.dir())
        .join(format!("{name}.{}", kind.extension()))
}

/// Everything in both libraries, each with the companies that name it.
///
/// `companies` is the overlays as they were parsed, which is what says who
/// *names* a thing — the merged config would have resolved the reference
/// into the thing itself and lost the name.
pub(crate) fn list(root: &Path, base: &AppConfig, companies: &[(String, AppConfig)]) -> Vec<Entry> {
    // The process is a reader like any company: a `[library]` in the base
    // config reaches every company that does not say otherwise, so an edit
    // that left it out of the "used by" list would understate its blast
    // radius in exactly the case where the radius is largest.
    let mut readers: Vec<(String, &AppConfig)> = vec![("the process".to_string(), base)];
    readers.extend(companies.iter().map(|(id, c)| (id.clone(), c)));

    let mut entries = Vec::new();
    for kind in [Kind::Persona, Kind::Module] {
        for name in names_in(root, kind) {
            entries.push(entry(root, kind, &name, &readers));
        }
    }
    entries
}

/// The names in one library directory, sorted. A directory that is not
/// there is an empty library, not an error: a deployment that shares
/// nothing never creates one.
fn names_in(root: &Path, kind: Kind) -> Vec<String> {
    let wanted = kind.extension();
    let mut names: Vec<String> = std::fs::read_dir(root.join(kind.dir()))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            (path.extension()? == wanted).then(|| path.file_stem()?.to_str().map(str::to_string))?
        })
        .collect();
    names.sort();
    names
}

fn entry(root: &Path, kind: Kind, name: &str, readers: &[(String, &AppConfig)]) -> Entry {
    let path = path_of(root, kind, name);
    let (text, mut problem) = match std::fs::read_to_string(&path) {
        Ok(text) => (text, None),
        Err(e) => (String::new(), Some(format!("{}: {e}", path.display()))),
    };
    let mut components = None;
    if kind == Kind::Module && problem.is_none() {
        match crate::tenant::check_module(&text) {
            Ok(count) => components = Some(count),
            Err(detail) => problem = Some(detail),
        }
    }
    // A file whose name nothing could reference is listed rather than
    // hidden, with the reason: one that cannot be shown is the one nobody
    // can fix, and it is already on disk.
    if problem.is_none() && !valid_name(name) {
        problem = Some(format!(
            "{name:?} cannot be referenced: a library name is letters, digits, dashes \
             and underscores. Rename the file."
        ));
    }
    let mut used_by = Vec::new();
    let mut overridden_by = Vec::new();
    for (id, config) in readers {
        let names_it = match kind {
            Kind::Persona => config.library.persona.as_deref() == Some(name),
            Kind::Module => config.library.modules.iter().any(|m| m == name),
        };
        if !names_it {
            continue;
        }
        used_by.push(id.clone());
        // D5: an inline persona wins, and the reference is left unused.
        if kind == Kind::Persona && !config.persona.text.trim().is_empty() {
            overridden_by.push(id.clone());
        }
    }
    Entry {
        name: name.to_string(),
        kind,
        text,
        used_by,
        overridden_by,
        components,
        problem,
    }
}

/// Writes one library file, creating its directory the first time.
///
/// A module is parsed before it is written, for the reason every config
/// save is: a file this page wrote that the next start refuses is the worst
/// thing a settings page can do.
pub(crate) fn write(
    root: &Path,
    kind: Kind,
    name: &str,
    text: &str,
    fresh: bool,
) -> Result<(), String> {
    if !valid_name(name) {
        return Err(format!(
            "{name:?} is not usable as a library name: letters, digits, dashes and \
             underscores, up to 64 of them, because it becomes a file name"
        ));
    }
    // Creating is not a way to edit. "Create" sends empty text, and a name
    // that already belongs to something every company shares would be
    // blanked without any of the warning the editor gives — so a name that
    // is taken is refused rather than overwritten.
    if fresh && path_of(root, kind, name).exists() {
        return Err(format!(
            "there is already a {} called {name:?} — open it below to change it",
            match kind {
                Kind::Persona => "persona",
                Kind::Module => "module",
            }
        ));
    }
    if kind == Kind::Module {
        crate::tenant::check_module(text).map_err(|e| format!("this is not a module: {e}"))?;
    }
    let dir = root.join(kind.dir());
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let path = path_of(root, kind, name);
    std::fs::write(&path, text).map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(text: &str) -> AppConfig {
        AppConfig::parse(text).expect("parses")
    }

    #[test]
    fn a_name_that_would_leave_the_directory_is_refused() {
        for bad in ["", "../escape", "a/b", "with space", "dotted.name", "a:b"] {
            assert!(!valid_name(bad), "{bad:?} was accepted");
        }
        assert!(valid_name("support-brief"));
        assert!(valid_name("orders_v2"));
    }

    /// H4. The list carries the readers, so the page can name them before
    /// the save rather than after it.
    #[test]
    fn an_entry_names_every_company_that_uses_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            Kind::Persona,
            "support-brief",
            "Be brief.\n",
            true,
        )
        .expect("writes");
        let companies = vec![
            (
                "acme".to_string(),
                config("[library]\npersona = \"support-brief\"\n"),
            ),
            (
                "beta".to_string(),
                config("[library]\npersona = \"support-brief\"\n[persona]\ntext = \"ours\"\n"),
            ),
            (
                "gamma".to_string(),
                config("[store]\npath = \"g.sqlite\"\n"),
            ),
        ];
        let nothing = config("");
        let entries = list(dir.path(), &nothing, &companies);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].used_by, vec!["acme", "beta"]);
        // beta names it and writes its own, so the reference is inert.
        assert_eq!(entries[0].overridden_by, vec!["beta"]);
        assert_eq!(entries[0].text, "Be brief.\n");

        // The process names it too: it reaches every company that does not
        // say otherwise, which is the largest blast radius there is.
        let house = config("[library]\npersona = \"support-brief\"\n");
        let entries = list(dir.path(), &house, &companies);
        assert_eq!(entries[0].used_by, vec!["the process", "acme", "beta"]);
    }

    /// Creating is not a way to edit. "Create" sends empty text, and a name
    /// already belonging to something every company shares would otherwise
    /// be blanked with none of the editor's warning.
    #[test]
    fn creating_something_that_already_exists_is_refused_rather_than_blanking_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), Kind::Persona, "shared", "The words.\n", true).expect("writes");

        let refused = write(dir.path(), Kind::Persona, "shared", "", true).expect_err("refused");
        assert!(refused.contains("already"), "{refused}");
        let still =
            std::fs::read_to_string(dir.path().join("personas").join("shared.md")).expect("reads");
        assert_eq!(still, "The words.\n", "the text survived");

        // Editing the same name is exactly what the editor does, and works.
        write(dir.path(), Kind::Persona, "shared", "New words.\n", false).expect("edits");
    }

    /// The save gate: a module that would be refused at the next start is
    /// refused at the save, where the operator is still looking at it.
    #[test]
    fn a_module_that_is_not_a_module_is_refused_before_it_is_written() {
        let dir = tempfile::tempdir().expect("tempdir");
        let refused =
            write(dir.path(), Kind::Module, "orders", "not [[[ toml", true).expect_err("refused");
        assert!(refused.contains("not a module"), "{refused}");
        assert!(
            !dir.path().join("modules").join("orders.toml").exists(),
            "nothing was written"
        );

        // A file that parses and registers nothing is refused too: naming
        // it would leave a company without the tools it was sold, and the
        // only sign would be a "0 tools" pill nobody was looking at.
        for empty in ["", "[[http_components]]\nname = \"typo\"\n"] {
            assert!(
                write(dir.path(), Kind::Module, "orders", empty, true).is_err(),
                "{empty:?} was accepted as a module"
            );
        }

        let good = "[[http_component]]\nname = \"place_order\"\ndescription = \"d\"\n\
                    url = \"https://example.test/o\"\nside_effect = \"Pure\"\n\
                    args_schema = { type = \"object\" }\n";
        write(dir.path(), Kind::Module, "orders", good, true).expect("writes");
        let nothing = config("");
        let entries = list(dir.path(), &nothing, &[]);
        assert_eq!(entries[0].components, Some(1));
        assert_eq!(entries[0].problem, None);
    }

    /// A file already on disk whose name nothing could reference is listed
    /// with the reason rather than hidden: one that cannot be shown is the
    /// one nobody can fix.
    #[test]
    fn a_file_whose_name_cannot_be_referenced_is_listed_with_the_reason() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("personas")).expect("dir");
        std::fs::write(dir.path().join("personas").join("my.brief.md"), "x").expect("file");
        let nothing = config("");
        let entries = list(dir.path(), &nothing, &[]);
        assert_eq!(entries.len(), 1);
        assert!(
            entries[0]
                .problem
                .as_deref()
                .is_some_and(|p| p.contains("cannot be referenced")),
            "{:?}",
            entries[0]
        );
    }

    /// A module already on disk that does not parse is listed rather than
    /// hidden — it is the one somebody has to open.
    #[test]
    fn a_broken_module_on_disk_is_still_listed_with_its_problem() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("modules")).expect("dir");
        std::fs::write(dir.path().join("modules").join("orders.toml"), "[[[").expect("file");
        let nothing = config("");
        let entries = list(dir.path(), &nothing, &[]);
        assert_eq!(entries.len(), 1);
        assert!(entries[0].problem.is_some(), "{:?}", entries[0]);
    }

    #[test]
    fn a_library_that_does_not_exist_is_empty_rather_than_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let nothing = config("");
        assert!(list(dir.path(), &nothing, &[]).is_empty());
    }
}
