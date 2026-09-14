//! One company's config: the base `config.toml` with that company's overlay
//! laid over it.
//!
//! Multi-tenant plan T1.2. The shared file holds everything a hundred
//! companies agree on; `tenants/<id>.toml` holds the handful of keys one of
//! them does not. What may differ is what belongs to the company — its
//! persona, its templates, its `[llm]` targets and credentials, its
//! `[memory]`, its `[router]`, its HTTP tools, its worker slots and its store
//! file. What may not is what belongs to the *process* hosting it: the listen
//! address, the trace configuration and `[models]`. A tenant that could move
//! the listener could take the shard's other nineteen tenants off the air.
//!
//! Refusal is by name, at load. Assembly is already a gate — `HarnessBuilder::
//! build` refuses a missing slot, a duplicate slot and two tools claiming one
//! name — and this keeps it one gate rather than two: a missing overlay, an
//! unparseable one, a process-owned key and two tenants resolving to one
//! store file are all refused here, each with the tenant and the file in the
//! message, rather than silently defaulted.

use crate::config::AppConfig;
use std::path::{Path, PathBuf};

/// Where a shard looks for overlays, relative to the working directory. No
/// such directory means no tenant set, which is the CLI: one tenant, the base
/// config alone.
pub(crate) const TENANT_DIR: &str = "tenants";

/// The shared library, beside `tenants/`. A persona is prose; a module is a
/// file of `[[http_component]]` entries — literally the TOML you would
/// otherwise paste into each company's overlay.
///
/// Files, and not a database, for the reason the overlays are files: a file
/// is reviewable and diffable, and the answer that made the tenant set
/// legible should not be given up the first time something else needs
/// storing.
pub(crate) const PERSONA_DIR: &str = "personas";
pub(crate) const MODULE_DIR: &str = "modules";

/// One module file: the same shape `[[http_component]]` has in the config,
/// so a module is moved into the library by cutting the entries out of a
/// config and pasting them into a file.
///
/// `deny_unknown_fields` because the failure it prevents is silent: a file
/// spelling the table `[[http_components]]`, or holding something else
/// entirely, would otherwise parse as a module with nothing in it, and the
/// company that named it would simply not have the tools it was sold.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ModuleFile {
    #[serde(default, rename = "http_component")]
    http_components: Vec<nscomponents_std::http_tool::HttpToolConfig>,
}

/// A name in the library, which is also a file name.
///
/// The charset is the whole of the check, and it is the only thing standing
/// between `[library] persona = "…"` and an arbitrary file: the name is
/// joined onto `personas/` and read, so a value carrying `..` or a separator
/// would put a file from anywhere on the box into every prompt this company
/// sends. Letters, digits, dashes and underscores leave no way to say it.
pub(crate) fn valid_library_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// The keys the process owns and an overlay may therefore not set, as dotted
/// paths into the config. `[models]` is the local model service this box runs;
/// everything under `[serve]` named here belongs to the one socket the
/// process listens on — its address, the token it accepts, how it decides
/// who a caller is, and how long a silent client has. A company that could
/// set any of them could take the shard's other tenants off the air, or
/// downgrade the whole listener to a shared token it knows (plan A6).
///
/// `[engine] shard_worker_slots` is the ceiling on turns running at once
/// across every tenant (plan B6): a company sets how many turns *it* runs at
/// once, but raising the shard's would spend the others' concurrency.
///
/// `[auth]` is deliberately absent: the keys that say who a caller is are
/// the company's, and each signs with its own. The wire trace has no config
/// key at all — it is `NS_TRACE`, read by the process — so there is nothing
/// here to refuse for it.
const PROCESS_OWNED: [&str; 7] = [
    "http",
    "models",
    "serve.listen",
    "serve.token_env",
    "serve.auth",
    "serve.hello_timeout_ms",
    "engine.shard_worker_slots",
];

/// One tenant, resolved: the id it is addressed by and the config a factory
/// builds its engine from.
pub(crate) struct TenantConfig {
    pub(crate) id: String,
    pub(crate) app: AppConfig,
}

impl TenantConfig {
    /// The files this tenant must not share with another one, each as
    /// `(the key that names it, what sharing it would mean, its value)`.
    ///
    /// The store is the company's conversations; the learned rules are what
    /// its traffic taught the engine to say; the ledger is the record of
    /// that learning. Two companies on one of them is one company's traffic
    /// shaping another's replies, and nothing downstream would ever notice.
    fn private_paths(&self) -> [(&'static str, &'static str, &str); 3] {
        [
            ("store path", "database", self.app.store.path.as_str()),
            (
                "evolution.learned_path",
                "set of learned rules",
                self.app.evolution.learned_path.as_str(),
            ),
            (
                "evolution.ledger_path",
                "evolution ledger",
                self.app.evolution.ledger_path.as_str(),
            ),
        ]
    }
}

/// A configured path, lexically normalised so `./ns.sqlite` and `ns.sqlite`
/// are recognised as the one file. Lexical and not `canonicalize`, because
/// the file does not exist yet at load. `Components` drops every `.` but a
/// leading one, so that one goes here.
fn normalise(path: &str) -> PathBuf {
    Path::new(path)
        .components()
        .filter(|c| !matches!(c, std::path::Component::CurDir))
        .collect()
}

/// A tenant set that refused itself, with the tenant and the file named.
#[derive(Debug)]
pub(crate) enum TenantError {
    /// The shared `config.toml` does not parse. No tenant is loadable.
    Base(String),
    /// `tenants/` exists but cannot be read.
    Dir { path: String, detail: String },
    /// A tenant was named and its overlay is not there. Never defaulted: a
    /// company whose file is missing would otherwise run on another
    /// company's persona and another company's store.
    Missing { tenant: String, path: String },
    /// The overlay is not parseable TOML, or does not fit the config shape.
    Malformed {
        tenant: String,
        path: String,
        detail: String,
    },
    /// The overlay set a key the process owns.
    ProcessOwned {
        tenant: String,
        path: String,
        key: String,
    },
    /// H5. The overlay names a persona or a module the library does not
    /// hold. Refused here rather than started without it: a company whose
    /// persona silently failed to load would answer in the base's voice,
    /// and a company missing a module would say it cannot do the thing it
    /// was bought for — both of which look like the model having a bad day
    /// rather than a file being absent.
    MissingFromLibrary {
        tenant: String,
        /// "persona" or "module", for the message.
        kind: &'static str,
        name: String,
        /// The file it should have been.
        path: String,
    },
    /// A module file is there and is not a set of components.
    MalformedModule {
        tenant: String,
        name: String,
        path: String,
        detail: String,
    },
    /// A library name that is not usable as a file name. Refused before it
    /// is joined onto a directory, because the join is what would turn
    /// `../../secrets` into a file read.
    BadLibraryName {
        tenant: String,
        kind: &'static str,
        name: String,
    },
    /// Two modules this company named define the same tool. Dropping one
    /// silently would leave an operator who had just added a module looking
    /// at a tool that still calls the old endpoint.
    ModuleToolClash {
        tenant: String,
        tool: String,
        a: String,
        b: String,
    },
    /// Plan H9, widened by guard G1: two overlays resolve to one of the
    /// files a tenant must have to itself — its store, its learned rules or
    /// its ledger. Both tenants are named, because the copy-pasted one is
    /// not knowable from here and the operator has to see the pair to find
    /// it.
    PathClash {
        a: String,
        b: String,
        /// The key that names the file, so the operator knows which line.
        key: &'static str,
        /// What the two would be sharing, in words.
        what: &'static str,
        path: String,
    },
}

impl std::fmt::Display for TenantError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Base(e) => write!(f, "config.toml: {e}"),
            Self::Dir { path, detail } => write!(f, "{path}: {detail}"),
            Self::Missing { tenant, path } => write!(
                f,
                "tenant {tenant:?}: {path} is missing — a tenant is its overlay, \
                 and a missing one is refused rather than defaulted to the base config."
            ),
            Self::Malformed {
                tenant,
                path,
                detail,
            } => write!(f, "tenant {tenant:?}: {path}: {detail}"),
            Self::ProcessOwned { tenant, path, key } => write!(
                f,
                "tenant {tenant:?}: {path}: {key} is owned by the process and cannot be \
                 set per tenant."
            ),
            Self::MissingFromLibrary {
                tenant,
                kind,
                name,
                path,
            } => write!(
                f,
                "tenant {tenant:?}: {kind} {name:?} is not in the library — it should be \
                 {path}. A named {kind} that is not there is refused rather than skipped."
            ),
            Self::MalformedModule {
                tenant,
                name,
                path,
                detail,
            } => write!(f, "tenant {tenant:?}: module {name:?}: {path}: {detail}"),
            Self::BadLibraryName { tenant, kind, name } => write!(
                f,
                "tenant {tenant:?}: {name:?} is not usable as a {kind} name — letters, \
                 digits, dashes and underscores, because it names a file."
            ),
            Self::ModuleToolClash { tenant, tool, a, b } => write!(
                f,
                "tenant {tenant:?}: modules {a:?} and {b:?} both define the tool {tool:?} — \
                 one would silently win, so neither is loaded."
            ),
            Self::PathClash {
                a,
                b,
                key,
                what,
                path,
            } => write!(
                f,
                "tenants {a:?} and {b:?} both resolve to the {key} {path:?} — \
                 two companies would share one {what}."
            ),
        }
    }
}

impl std::error::Error for TenantError {}

/// Every tenant this working directory serves.
///
/// No `tenants/` directory is the CLI and every one-shot subcommand: one
/// tenant, named by `default_id`, built from the base config alone, with
/// every default today's default. One `tenants/<id>.toml` per company
/// otherwise, in id order, each refused by name if it will not load.
pub(crate) fn load_set(
    base_text: &str,
    root: &Path,
    default_id: &str,
) -> Result<Vec<TenantConfig>, TenantError> {
    let dir = root.join(TENANT_DIR);
    if !dir.is_dir() {
        // The library is resolved here too. A single-tenant deployment that
        // names a shared persona is a deployment that means it, and skipping
        // the reference because there happened to be no `tenants/` would be
        // the silent failure H5 exists to stop — just without a company id
        // to blame it on.
        let mut app = AppConfig::parse(base_text).map_err(TenantError::Base)?;
        resolve_library(&mut app, base_text, base_text, root, default_id)?;
        return Ok(vec![TenantConfig {
            id: default_id.to_string(),
            app,
        }]);
    }
    let mut ids = Vec::new();
    let entries = std::fs::read_dir(&dir).map_err(|e| TenantError::Dir {
        path: dir.display().to_string(),
        detail: e.to_string(),
    })?;
    for entry in entries {
        let entry = entry.map_err(|e| TenantError::Dir {
            path: dir.display().to_string(),
            detail: e.to_string(),
        })?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }
        if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
            ids.push(stem.to_string());
        }
    }
    // Directory order is the filesystem's; the tenant set is the operator's,
    // so it is read in one order on every box.
    ids.sort();
    let mut set = Vec::new();
    for id in &ids {
        set.push(load_one(base_text, root, id)?);
    }
    check_private_paths(&set)?;
    Ok(set)
}

/// One tenant: the base config with `<dir>/<id>.toml` laid over it.
pub(crate) fn load_one(
    base_text: &str,
    root: &Path,
    id: &str,
) -> Result<TenantConfig, TenantError> {
    let path = root.join(TENANT_DIR).join(format!("{id}.toml"));
    let shown = path.display().to_string();
    let overlay_text = std::fs::read_to_string(&path).map_err(|_| TenantError::Missing {
        tenant: id.to_string(),
        path: shown.clone(),
    })?;
    let mut app = overlay(base_text, &overlay_text, id, &shown)?;
    resolve_library(&mut app, base_text, &overlay_text, root, id)?;
    Ok(TenantConfig {
        id: id.to_string(),
        app,
    })
}

/// Turns the names in `[library]` into the things they name.
///
/// **The persona.** An inline value beats a reference, at the level that
/// asked for the reference: a company that wrote its own `[persona] text`
/// gets it, and a base that wrote both gets its own too. The test is never
/// the *merged* text, because the merged text is usually the base's and
/// treating that as an override would mean a library persona could never
/// reach anybody at all.
///
/// **The modules.** A union, not a replacement. The components a company
/// writes inline stay, and each named module adds its own; a module naming a
/// tool the company already defines is left out rather than registered
/// twice, because assembly refuses two tools claiming one name and the
/// message would be about the harness rather than about the module.
fn resolve_library(
    app: &mut AppConfig,
    base_text: &str,
    overlay_text: &str,
    root: &Path,
    id: &str,
) -> Result<(), TenantError> {
    if let Some(name) = app.library.persona.clone() {
        if !valid_library_name(&name) {
            return Err(TenantError::BadLibraryName {
                tenant: id.to_string(),
                kind: "persona",
                name,
            });
        }
        let path = root.join(PERSONA_DIR).join(format!("{name}.md"));
        let prose =
            std::fs::read_to_string(&path).map_err(|_| TenantError::MissingFromLibrary {
                tenant: id.to_string(),
                kind: "persona",
                name: name.clone(),
                path: path.display().to_string(),
            })?;
        // Whichever level asked for the persona is the level whose own
        // `[persona] text` gets to override it.
        let asked = if names_a_library_persona(overlay_text) {
            overlay_text
        } else {
            base_text
        };
        if !writes_its_own_persona(asked) {
            app.persona.text = prose.trim().to_string();
        }
    }
    // Which module brought each tool, so a collision between two of them
    // can name both. Tools the company wrote inline are not in here: losing
    // to those is the documented rule, not a mistake.
    let mut brought_by: Vec<(String, String)> = Vec::new();
    for name in app.library.modules.clone() {
        if !valid_library_name(&name) {
            return Err(TenantError::BadLibraryName {
                tenant: id.to_string(),
                kind: "module",
                name,
            });
        }
        let path = root.join(MODULE_DIR).join(format!("{name}.toml"));
        let text = std::fs::read_to_string(&path).map_err(|_| TenantError::MissingFromLibrary {
            tenant: id.to_string(),
            kind: "module",
            name: name.clone(),
            path: path.display().to_string(),
        })?;
        let module: ModuleFile =
            toml::from_str(&text).map_err(|e| TenantError::MalformedModule {
                tenant: id.to_string(),
                name: name.clone(),
                path: path.display().to_string(),
                detail: e.to_string(),
            })?;
        if module.http_components.is_empty() {
            return Err(TenantError::MalformedModule {
                tenant: id.to_string(),
                name: name.clone(),
                path: path.display().to_string(),
                detail: "there are no [[http_component]] entries in it, so naming it \
                         registers nothing"
                    .to_string(),
            });
        }
        for component in module.http_components {
            if let Some((_, first)) = brought_by.iter().find(|(tool, _)| tool == &component.name) {
                return Err(TenantError::ModuleToolClash {
                    tenant: id.to_string(),
                    tool: component.name.clone(),
                    a: first.clone(),
                    b: name.clone(),
                });
            }
            brought_by.push((component.name.clone(), name.clone()));
            // A tool the company defines for itself stays its own: the
            // module adds what is missing rather than replacing what is
            // there, and assembly would refuse the pair anyway.
            if !app.http_components.iter().any(|c| c.name == component.name) {
                app.http_components.push(component);
            }
        }
    }
    Ok(())
}

/// Whether this text is a module file, and how many components it holds.
///
/// The settings page writes library files, and a module that does not parse
/// would be refused at the next start rather than at the save — which is the
/// failure mode the whole "every save must load" rule exists to prevent.
pub(crate) fn check_module(text: &str) -> Result<usize, String> {
    let module: ModuleFile = toml::from_str(text).map_err(|e| e.to_string())?;
    if module.http_components.is_empty() {
        return Err(
            "there are no [[http_component]] entries in it, so naming it would register \
             nothing"
                .to_string(),
        );
    }
    Ok(module.http_components.len())
}

/// Whether this overlay writes a persona of its own, as opposed to
/// inheriting the base's. Read off the overlay's own text for the reason in
/// [`resolve_library`].
fn writes_its_own_persona(text: &str) -> bool {
    reads(text, "persona", "text")
        .and_then(|v| v.as_str().map(|t| !t.trim().is_empty()))
        .unwrap_or(false)
}

/// Whether this level is the one that named a library persona.
fn names_a_library_persona(text: &str) -> bool {
    reads(text, "library", "persona").is_some()
}

fn reads(text: &str, table: &str, key: &str) -> Option<toml::Value> {
    toml::from_str::<toml::Value>(text)
        .ok()?
        .get(table)?
        .get(key)
        .cloned()
}

/// The merge itself.
///
/// **Rule.** A key absent from the overlay keeps the base's value; a key
/// present in the overlay replaces it. Tables merge key by key, recursively,
/// so `[llm.emitter] model = ...` leaves the base's `[llm] base_url` alone.
/// Every other value, arrays included, replaces wholesale: a tenant's
/// `[[http_component]]` list is the tools that company has, not the shared
/// list with more appended, and there is no way to remove an inherited
/// element from a list you can only add to.
fn overlay(
    base_text: &str,
    overlay_text: &str,
    id: &str,
    shown: &str,
) -> Result<AppConfig, TenantError> {
    let mut merged: toml::Value =
        toml::from_str(base_text).map_err(|e| TenantError::Base(e.to_string()))?;
    let over: toml::Value = toml::from_str(overlay_text).map_err(|e| TenantError::Malformed {
        tenant: id.to_string(),
        path: shown.to_string(),
        detail: e.to_string(),
    })?;
    for key in PROCESS_OWNED {
        if holds(&over, key) {
            return Err(TenantError::ProcessOwned {
                tenant: id.to_string(),
                path: shown.to_string(),
                key: key.to_string(),
            });
        }
    }
    merge(&mut merged, over);
    merged
        .try_into()
        .map_err(|e: toml::de::Error| TenantError::Malformed {
            tenant: id.to_string(),
            path: shown.to_string(),
            detail: e.to_string(),
        })
}

/// Whether a dotted path is set in this document at all.
fn holds(value: &toml::Value, dotted: &str) -> bool {
    let mut cur = value;
    for part in dotted.split('.') {
        match cur.get(part) {
            Some(next) => cur = next,
            None => return false,
        }
    }
    true
}

/// Tables key by key; everything else replaces.
fn merge(base: &mut toml::Value, over: toml::Value) {
    match (base, over) {
        (toml::Value::Table(b), toml::Value::Table(o)) => {
            for (k, v) in o {
                match b.get_mut(&k) {
                    Some(slot) => merge(slot, v),
                    None => {
                        b.insert(k, v);
                    }
                }
            }
        }
        (slot, v) => *slot = v,
    }
}

/// Plan H9, widened by guard G1. Two tenants on one store file is two
/// companies in one database; two on one `learned.toml` or one ledger is one
/// company's traffic shaping the other's replies. Neither is anything
/// downstream would ever notice, so all three are one check here.
///
/// Each kind gets its own table: a tenant naming its ledger after another
/// tenant's store would be strange, but it is not the sharing this guards
/// against and inventing a refusal for it would be a rule nobody asked for.
fn check_private_paths(set: &[TenantConfig]) -> Result<(), TenantError> {
    for slot in 0..3 {
        let mut seen: std::collections::HashMap<PathBuf, &str> = std::collections::HashMap::new();
        for t in set {
            let (key, what, raw) = t.private_paths()[slot];
            if let Some(first) = seen.insert(normalise(raw), &t.id) {
                return Err(TenantError::PathClash {
                    a: first.to_string(),
                    b: t.id.clone(),
                    key,
                    what,
                    path: raw.to_string(),
                });
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = r#"
        [persona]
        text = "the shared persona"
        [store]
        path = "ns.sqlite"
        [memory]
        window_turns = 7
        [llm]
        base_url = "https://shared.example"
    "#;

    /// Write a tenant set under a scratch root and hand back the root.
    fn tenants(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(dir.path().join(TENANT_DIR)).expect("tenants dir");
        for (id, text) in files {
            std::fs::write(dir.path().join(TENANT_DIR).join(format!("{id}.toml")), text)
                .expect("overlay");
        }
        dir
    }

    /// Put a file in the shared library beside `tenants/`.
    fn library(root: &Path, kind: &str, name: &str, text: &str) {
        let dir = root.join(kind);
        std::fs::create_dir_all(&dir).expect("library dir");
        let extension = if kind == PERSONA_DIR { "md" } else { "toml" };
        std::fs::write(dir.join(format!("{name}.{extension}")), text).expect("library file");
    }

    fn a_component(tool: &str, description: &str) -> String {
        format!(
            "[[http_component]]\nname = \"{tool}\"\n\
             description = \"{description}\"\n\
             url = \"https://example.test/{tool}\"\n\
             side_effect = \"Pure\"\n\
             args_schema = {{ type = \"object\" }}\n"
        )
    }

    fn a_module(tool: &str) -> String {
        a_component(tool, "from the library")
    }

    /// T3.2 and D5. Two companies name one persona and get the same words;
    /// the one that wrote its own gets its own, and the reference is simply
    /// not used.
    #[test]
    fn a_company_can_use_a_shared_persona_and_override_it() {
        let root = tenants(&[
            (
                "acme",
                "[store]\npath = \"ns-acme.sqlite\"\n\
                 [evolution]\nlearned_path = \"acme-learned.toml\"\n\
                 ledger_path = \"acme-ledger.jsonl\"\n\
                 [library]\npersona = \"support-brief\"\n",
            ),
            (
                "beta",
                "[store]\npath = \"ns-beta.sqlite\"\n\
                 [evolution]\nlearned_path = \"beta-learned.toml\"\n\
                 ledger_path = \"beta-ledger.jsonl\"\n\
                 [library]\npersona = \"support-brief\"\n\
                 [persona]\ntext = \"beta says it its own way\"\n",
            ),
        ]);
        library(
            root.path(),
            PERSONA_DIR,
            "support-brief",
            "Answer in two sentences.\n",
        );
        let set = load_set(BASE, root.path(), "local").expect("the set loads");

        let acme = set.iter().find(|t| t.id == "acme").expect("acme");
        assert_eq!(acme.app.persona.text, "Answer in two sentences.");
        let beta = set.iter().find(|t| t.id == "beta").expect("beta");
        assert_eq!(
            beta.app.persona.text, "beta says it its own way",
            "an inline persona beats the reference"
        );
    }

    /// D5. Modules add; they do not replace. A company keeps the components
    /// it wrote and gains the ones it named.
    #[test]
    fn modules_from_the_library_and_the_overlay_are_one_set() {
        let root = tenants(&[(
            "acme",
            &format!(
                "[library]\nmodules = [\"orders\", \"stock\"]\n{}",
                a_component("its_own", "written into the overlay")
            ),
        )]);
        library(root.path(), MODULE_DIR, "orders", &a_module("place_order"));
        library(root.path(), MODULE_DIR, "stock", &a_module("check_stock"));
        let set = load_set(BASE, root.path(), "local").expect("the set loads");

        let mut names: Vec<&str> = set[0]
            .app
            .http_components
            .iter()
            .map(|c| c.name.as_str())
            .collect();
        names.sort();
        assert_eq!(names, ["check_stock", "its_own", "place_order"]);
    }

    /// A module that names a tool the company already defines does not
    /// register it twice — assembly refuses two tools on one name, and the
    /// message would be about the harness rather than about the module.
    #[test]
    fn a_module_does_not_shadow_a_tool_the_company_already_has() {
        let root = tenants(&[(
            "acme",
            &format!(
                "[library]\nmodules = [\"orders\"]\n{}",
                a_component("place_order", "the company's own")
            ),
        )]);
        library(root.path(), MODULE_DIR, "orders", &a_module("place_order"));
        let set = load_set(BASE, root.path(), "local").expect("the set loads");

        assert_eq!(set[0].app.http_components.len(), 1);
        assert_eq!(
            set[0].app.http_components[0].description,
            "the company's own"
        );
    }

    /// H5. A named persona that is not there is refused at load, with the
    /// file it should have been — not started in the base's voice.
    #[test]
    fn a_missing_persona_is_refused_naming_the_file() {
        let root = tenants(&[("acme", "[library]\npersona = \"not-written-yet\"\n")]);
        let said = match load_set(BASE, root.path(), "local") {
            Err(e) => e.to_string(),
            Ok(_) => panic!("a persona that is not there must be refused"),
        };
        assert!(said.contains("acme"), "{said}");
        assert!(said.contains("not-written-yet"), "{said}");
        assert!(said.contains("not-written-yet.md"), "{said}");
        assert!(said.contains("personas"), "{said}");
    }

    /// The same for a module, and for one that is there and is not a set of
    /// components.
    #[test]
    fn a_missing_or_broken_module_is_refused_naming_the_file() {
        let root = tenants(&[("acme", "[library]\nmodules = [\"orders\"]\n")]);
        let absent = match load_set(BASE, root.path(), "local") {
            Err(e) => e.to_string(),
            Ok(_) => panic!("a module that is not there must be refused"),
        };
        assert!(absent.contains("orders.toml"), "{absent}");

        library(root.path(), MODULE_DIR, "orders", "this is not toml [[[");
        let said = match load_set(BASE, root.path(), "local") {
            Err(e) => e.to_string(),
            Ok(_) => panic!("a module that does not parse must be refused"),
        };
        assert!(said.contains("orders"), "{said}");
        assert!(said.contains("acme"), "{said}");
    }

    /// The `[library]` value is a file name, which makes it the one field
    /// where an accepted key can still carry a path. A name that walked out
    /// of `personas/` would put a file from anywhere on the box into every
    /// prompt the company sends — and hand it back to whoever chats with it.
    #[test]
    fn a_library_name_that_would_leave_the_directory_is_refused_at_load() {
        let secret = tempfile::tempdir().expect("tempdir");
        std::fs::write(secret.path().join("private.md"), "the private notes").expect("file");

        for name in ["../../private", "..\\..\\private", "a/b", "a:b", ""] {
            let root = tenants(&[(
                "acme",
                &format!("[library]\npersona = {}\n", toml_string(name)),
            )]);
            let said = match load_set(BASE, root.path(), "local") {
                Err(e) => e.to_string(),
                Ok(set) => panic!(
                    "{name:?} was accepted; persona became {:?}",
                    set[0].app.persona.text
                ),
            };
            assert!(said.contains("not usable"), "{name:?}: {said}");
        }
    }

    fn toml_string(s: &str) -> String {
        format!("\"{}\"", s.replace('\\', "\\\\"))
    }

    /// Two modules defining one tool: one would silently win, and the
    /// operator who just added the second would see a tool still calling
    /// the first one's endpoint.
    #[test]
    fn two_modules_defining_one_tool_are_refused_naming_both() {
        let root = tenants(&[("acme", "[library]\nmodules = [\"orders\", \"orders_v2\"]\n")]);
        library(root.path(), MODULE_DIR, "orders", &a_module("place_order"));
        library(
            root.path(),
            MODULE_DIR,
            "orders_v2",
            &a_module("place_order"),
        );

        let said = match load_set(BASE, root.path(), "local") {
            Err(e) => e.to_string(),
            Ok(_) => panic!("two modules on one tool name must be refused"),
        };
        assert!(said.contains("orders"), "{said}");
        assert!(said.contains("orders_v2"), "{said}");
        assert!(said.contains("place_order"), "{said}");
    }

    /// A module that parses and registers nothing is the silent version of
    /// a missing module: the company simply does not have the tools it was
    /// sold, and nothing says so.
    #[test]
    fn a_module_that_registers_nothing_is_refused() {
        let root = tenants(&[("acme", "[library]\nmodules = [\"orders\"]\n")]);
        // The plural is a real typo and would otherwise parse as nothing.
        library(
            root.path(),
            MODULE_DIR,
            "orders",
            "[[http_components]]\nname = \"place_order\"\n",
        );
        let said = match load_set(BASE, root.path(), "local") {
            Err(e) => e.to_string(),
            Ok(_) => panic!("a module registering nothing must be refused"),
        };
        assert!(said.contains("orders"), "{said}");
    }

    /// The CLI and every one-shot subcommand take the no-`tenants/` path.
    /// A deployment that names a shared persona there means it, and the
    /// reference is resolved and refused exactly as it would be for a
    /// company — skipping it because there was no tenant directory would be
    /// the silent failure with nobody to blame it on.
    #[test]
    fn a_deployment_with_no_tenants_directory_still_gets_its_library() {
        let root = tempfile::tempdir().expect("tempdir");
        library(root.path(), PERSONA_DIR, "house", "The house voice.\n");
        let set = load_set("[library]\npersona = \"house\"\n", root.path(), "local")
            .expect("the set loads");
        assert_eq!(set[0].app.persona.text, "The house voice.");

        let said = match load_set("[library]\npersona = \"absent\"\n", root.path(), "local") {
            Err(e) => e.to_string(),
            Ok(_) => panic!("a persona that is not there must be refused"),
        };
        assert!(said.contains("absent.md"), "{said}");
    }

    /// The override rule applies at the level that asked for the
    /// reference. A base that names a persona *and* writes its own text
    /// gets its own; a company that inherits that reference and writes
    /// nothing gets the library's.
    #[test]
    fn an_inline_persona_beats_the_reference_at_the_level_that_made_it() {
        let root = tenants(&[("acme", "[store]\npath = \"ns-acme.sqlite\"\n")]);
        library(root.path(), PERSONA_DIR, "house", "The library voice.\n");

        let both = "[library]\npersona = \"house\"\n[persona]\ntext = \"the base's own\"\n";
        let set = load_set(both, root.path(), "local").expect("loads");
        assert_eq!(
            set[0].app.persona.text, "the base's own",
            "the base wrote both, so its own text stands"
        );

        let reference_only = "[library]\npersona = \"house\"\n";
        let set = load_set(reference_only, root.path(), "local").expect("loads");
        assert_eq!(set[0].app.persona.text, "The library voice.");
    }

    /// T4.3. Groups are expressed and inert. The loader carries them and
    /// nothing downstream narrows anything by them — a company with a group
    /// naming one module still has every tool it had, because the
    /// enforcement seam is a filter over the registered tool set inside the
    /// turn loop, and it does not exist yet.
    #[test]
    fn groups_are_stored_and_change_nothing_about_the_tools() {
        let root = tenants(&[(
            "acme",
            &format!(
                "[library]\nmodules = [\"orders\"]\n\
                 [groups]\nagents = [\"orders\"]\nreadonly = []\n{}",
                a_component("its_own", "written into the overlay")
            ),
        )]);
        library(root.path(), MODULE_DIR, "orders", &a_module("place_order"));
        let set = load_set(BASE, root.path(), "local").expect("the set loads");

        assert_eq!(set[0].app.groups["agents"], vec!["orders"]);
        assert!(set[0].app.groups["readonly"].is_empty());
        // Both tools are still registered: the group narrowed nothing.
        let mut names: Vec<&str> = set[0]
            .app
            .http_components
            .iter()
            .map(|c| c.name.as_str())
            .collect();
        names.sort();
        assert_eq!(names, ["its_own", "place_order"]);
    }

    /// The CLI invariant: no `tenants/` directory is one tenant called
    /// `local`, the base config alone, every default today's.
    #[test]
    fn no_tenants_directory_means_one_tenant_with_todays_defaults() {
        let root = tempfile::tempdir().expect("tempdir");
        let set = load_set(BASE, root.path(), "local").expect("the base config alone");
        assert_eq!(set.len(), 1);
        assert_eq!(set[0].id, "local");
        assert_eq!(set[0].app.persona.text, "the shared persona");
        assert_eq!(set[0].app.store.path, "ns.sqlite");
        // And the keys nobody set are the defaults the CLI has always had.
        let default = AppConfig::parse("").expect("the empty config");
        assert_eq!(
            set[0].app.engine.max_iterations,
            default.engine.max_iterations
        );
        assert_eq!(
            set[0].app.memory.facts_in_context,
            default.memory.facts_in_context
        );
        assert_eq!(set[0].app.serve.listen, default.serve.listen);
    }

    /// The merge rule, in the direction that matters: what the overlay does
    /// not mention it inherits, and a table it does mention keeps the base's
    /// other keys.
    #[test]
    fn an_absent_overlay_key_keeps_the_base_value() {
        let root = tenants(&[(
            "acme",
            "[persona]\ntext = \"acme's own persona\"\n[store]\npath = \"ns-acme.sqlite\"\n",
        )]);
        let set = load_set(BASE, root.path(), "local").expect("one overlay");
        assert_eq!(set.len(), 1);
        let t = &set[0];
        assert_eq!(t.id, "acme");
        // Present: replaced.
        assert_eq!(t.app.persona.text, "acme's own persona");
        assert_eq!(t.app.store.path, "ns-acme.sqlite");
        // Absent: the base's, not the serde default.
        assert_eq!(t.app.memory.window_turns, 7);
        assert_eq!(
            t.app.llm.base_url.as_deref(),
            Some("https://shared.example")
        );
    }

    /// A tenant named with no overlay is refused, with the tenant and the
    /// file in the message. Silently falling back to the base config would
    /// run that company on another company's persona and store.
    #[test]
    fn a_missing_tenant_overlay_is_refused_by_name() {
        let root = tenants(&[]);
        let err = match load_one(BASE, root.path(), "acme") {
            Err(e) => e.to_string(),
            Ok(_) => panic!("a missing overlay must be refused"),
        };
        assert!(err.contains("acme"), "{err}");
        assert!(err.contains("acme.toml"), "{err}");
    }

    /// The same for an overlay that is there and will not load, in both
    /// senses: not TOML at all, and TOML of the wrong shape.
    #[test]
    fn a_malformed_tenant_overlay_is_refused_by_name() {
        let root = tenants(&[("acme", "[persona\ntext = \"unclosed table header\"")]);
        let err = match load_set(BASE, root.path(), "local") {
            Err(e) => e.to_string(),
            Ok(_) => panic!("unparseable TOML must be refused"),
        };
        assert!(err.contains("acme"), "{err}");
        assert!(err.contains("acme.toml"), "{err}");

        let root = tenants(&[("beta", "[memory]\nwindow_turns = \"seven\"\n")]);
        let err = match load_set(BASE, root.path(), "local") {
            Err(e) => e.to_string(),
            Ok(_) => panic!("a wrongly typed key must be refused"),
        };
        assert!(err.contains("beta"), "{err}");
        assert!(err.contains("beta.toml"), "{err}");
        assert!(err.contains("window_turns"), "{err}");
    }

    /// Plan H9. The copy-pasted overlay: two companies, one database, and
    /// nothing downstream that would ever notice.
    #[test]
    fn two_overlays_sharing_a_store_path_are_refused_naming_both() {
        let root = tenants(&[
            ("acme", "[store]\npath = \"ns-acme.sqlite\"\n"),
            ("beta", "[store]\npath = \"./ns-acme.sqlite\"\n"),
        ]);
        let err = match load_set(BASE, root.path(), "local") {
            Err(e) => e.to_string(),
            Ok(_) => panic!("two tenants on one store must be refused"),
        };
        // Both, because which one is the copy is not knowable from here.
        assert!(err.contains("acme"), "{err}");
        assert!(err.contains("beta"), "{err}");
        assert!(err.contains("ns-acme.sqlite"), "{err}");

        // And the pair that inherits the base path without saying so is the
        // same clash, which is why the check is on the resolved value.
        let root = tenants(&[
            ("acme", "[persona]\ntext = \"a\"\n"),
            ("beta", "[persona]\ntext = \"b\"\n"),
        ]);
        let err = match load_set(BASE, root.path(), "local") {
            Err(e) => e.to_string(),
            Ok(_) => panic!("two tenants inheriting one store must be refused"),
        };
        assert!(err.contains("acme") && err.contains("beta"), "{err}");
    }

    /// Guard G1. The learned rules are what one company's traffic taught the
    /// engine to say; two companies on one file is one company shaping the
    /// other's replies, and it is the same copy-paste that produces a shared
    /// store.
    #[test]
    fn two_tenants_sharing_a_learned_path_are_refused_naming_both() {
        let root = tenants(&[
            (
                "acme",
                "[store]\npath = \"ns-acme.sqlite\"\n\
                 [evolution]\nlearned_path = \"learned-acme.toml\"\nledger_path = \"l-acme.json\"\n",
            ),
            (
                "beta",
                "[store]\npath = \"ns-beta.sqlite\"\n\
                 [evolution]\nlearned_path = \"./learned-acme.toml\"\nledger_path = \"l-beta.json\"\n",
            ),
        ]);
        let err = match load_set(BASE, root.path(), "local") {
            Err(e) => e.to_string(),
            Ok(_) => panic!("two tenants on one learned.toml must be refused"),
        };
        assert!(err.contains("acme") && err.contains("beta"), "{err}");
        assert!(err.contains("learned-acme.toml"), "{err}");
        assert!(err.contains("learned_path"), "{err}");
    }

    /// Guard G1, the ledger: the record of what was learned and what it cost.
    /// Shared, one company's regressions are charged to the other's budget.
    #[test]
    fn two_tenants_sharing_a_ledger_path_are_refused_naming_both() {
        let root = tenants(&[
            (
                "acme",
                "[store]\npath = \"ns-acme.sqlite\"\n\
                 [evolution]\nlearned_path = \"learned-acme.toml\"\nledger_path = \"ledger.json\"\n",
            ),
            (
                "beta",
                "[store]\npath = \"ns-beta.sqlite\"\n\
                 [evolution]\nlearned_path = \"learned-beta.toml\"\nledger_path = \"./ledger.json\"\n",
            ),
        ]);
        let err = match load_set(BASE, root.path(), "local") {
            Err(e) => e.to_string(),
            Ok(_) => panic!("two tenants on one ledger must be refused"),
        };
        assert!(err.contains("acme") && err.contains("beta"), "{err}");
        assert!(err.contains("ledger.json"), "{err}");
        assert!(err.contains("ledger_path"), "{err}");
    }

    /// What the process owns, an overlay may not move: a tenant that could
    /// name the listen address could take the shard's other tenants off the
    /// air, and `[models]` is this box's service, not this company's.
    #[test]
    fn an_overlay_naming_a_process_owned_key_is_refused() {
        for (key, text) in [
            ("serve.listen", "[serve]\nlisten = \"0.0.0.0:9999\"\n"),
            ("models", "[models]\nenabled = true\n"),
            ("serve.token_env", "[serve]\ntoken_env = \"ACME_TOKEN\"\n"),
            ("serve.auth", "[serve]\nauth = \"shared\"\n"),
            (
                "serve.hello_timeout_ms",
                "[serve]\nhello_timeout_ms = 60000\n",
            ),
        ] {
            let root = tenants(&[("acme", text)]);
            let err = match load_set(BASE, root.path(), "local") {
                Err(e) => e.to_string(),
                Ok(_) => panic!("{key} is not overlayable"),
            };
            assert!(err.contains("acme"), "{err}");
            assert!(err.contains(key), "{err}");
        }
    }

    /// Plan A6, the one worth naming on its own. `serve.token_env` is the
    /// variable the *listener* reads: an overlay that could point it
    /// somewhere else would choose the token every other company's clients
    /// must present. `[auth]` is the other side of the same line — the keys
    /// that say who a caller is are the company's, and are overlayable.
    #[test]
    fn an_overlay_naming_serve_token_env_is_refused() {
        let root = tenants(&[("acme", "[serve]\ntoken_env = \"ACME_TOKEN\"\n")]);
        let err = match load_set(BASE, root.path(), "local") {
            Err(e) => e.to_string(),
            Ok(_) => panic!("a tenant may not name the listener's token"),
        };
        assert!(err.contains("acme"), "{err}");
        assert!(err.contains("serve.token_env"), "{err}");

        let root = tenants(&[(
            "acme",
            "[store]\npath = \"ns-acme.sqlite\"\n\
             [auth]\nsigning_key_envs = [\"ACME_SIGNING_KEY\"]\niat_floor = 1700000000\n",
        )]);
        let set = load_set(BASE, root.path(), "local").expect("[auth] is the company's");
        assert_eq!(set[0].app.auth.signing_key_envs, ["ACME_SIGNING_KEY"]);
        assert_eq!(set[0].app.auth.iat_floor, 1_700_000_000);
    }

    /// Plan B6. A company sets how many turns *it* runs at once; the ceiling
    /// for the whole shard is the process's, and an overlay that could raise
    /// it would spend every other company's concurrency.
    #[test]
    fn an_overlay_naming_the_shard_cap_is_refused() {
        let root = tenants(&[(
            "acme",
            "[engine]\nmax_iterations = 5\nmax_emit_retries = 3\nshard_worker_slots = 512\n",
        )]);
        let err = match load_set(BASE, root.path(), "local") {
            Err(e) => e.to_string(),
            Ok(_) => panic!("a tenant may not set the shard's ceiling"),
        };
        assert!(err.contains("acme"), "{err}");
        assert!(err.contains("engine.shard_worker_slots"), "{err}");

        // Its own slots, both knobs, stay the company's to set.
        let root = tenants(&[(
            "acme",
            "[store]\npath = \"ns-acme.sqlite\"\n\
             [engine]\nmax_iterations = 5\nmax_emit_retries = 3\n\
             worker_slots = 2\nserve_worker_slots = 7\n",
        )]);
        let set = load_set(BASE, root.path(), "local").expect("its own slots are overlayable");
        assert_eq!(set[0].app.engine.slots_for(false).unwrap(), 2);
        assert_eq!(set[0].app.engine.slots_for(true).unwrap(), 7);
    }
}
