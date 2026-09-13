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

/// The keys the process owns and an overlay may therefore not set, as dotted
/// paths into the config. `[models]` is the local model service this box runs;
/// `[serve] listen` is the shard's own socket. The wire trace has no config
/// key at all — it is `NS_TRACE`, read by the process — so there is nothing
/// here to refuse for it.
const PROCESS_OWNED: [&str; 2] = ["models", "serve.listen"];

/// One tenant, resolved: the id it is addressed by and the config a factory
/// builds its engine from.
pub(crate) struct TenantConfig {
    pub(crate) id: String,
    pub(crate) app: AppConfig,
}

impl TenantConfig {
    /// The store file this tenant's data lives in, lexically normalised so
    /// `./ns.sqlite` and `ns.sqlite` are recognised as the one file. Lexical
    /// and not `canonicalize`, because the file does not exist yet at load.
    /// `Components` drops every `.` but a leading one, so that one goes here.
    fn store_path(&self) -> PathBuf {
        Path::new(&self.app.store.path)
            .components()
            .filter(|c| !matches!(c, std::path::Component::CurDir))
            .collect()
    }
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
    /// Plan H9: two overlays resolve to one store file. Both are named,
    /// because the copy-pasted one is not knowable from here and the
    /// operator has to see the pair to find it.
    StoreClash { a: String, b: String, path: String },
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
            Self::StoreClash { a, b, path } => write!(
                f,
                "tenants {a:?} and {b:?} both resolve to the store path {path:?} — \
                 two companies would share one database."
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
        return Ok(vec![TenantConfig {
            id: default_id.to_string(),
            app: AppConfig::parse(base_text).map_err(TenantError::Base)?,
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
        set.push(load_one(base_text, &dir, id)?);
    }
    check_store_paths(&set)?;
    Ok(set)
}

/// One tenant: the base config with `<dir>/<id>.toml` laid over it.
pub(crate) fn load_one(base_text: &str, dir: &Path, id: &str) -> Result<TenantConfig, TenantError> {
    let path = dir.join(format!("{id}.toml"));
    let shown = path.display().to_string();
    let overlay_text = std::fs::read_to_string(&path).map_err(|_| TenantError::Missing {
        tenant: id.to_string(),
        path: shown.clone(),
    })?;
    let app = overlay(base_text, &overlay_text, id, &shown)?;
    Ok(TenantConfig {
        id: id.to_string(),
        app,
    })
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

/// Plan H9. Two tenants on one store file is two companies in one database,
/// and nothing downstream would ever notice.
fn check_store_paths(set: &[TenantConfig]) -> Result<(), TenantError> {
    let mut seen: std::collections::HashMap<PathBuf, &str> = std::collections::HashMap::new();
    for t in set {
        let path = t.store_path();
        if let Some(first) = seen.get(&path) {
            return Err(TenantError::StoreClash {
                a: first.to_string(),
                b: t.id.clone(),
                path: t.app.store.path.clone(),
            });
        }
        seen.insert(path, &t.id);
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
        let err = match load_one(BASE, &root.path().join(TENANT_DIR), "acme") {
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

    /// What the process owns, an overlay may not move: a tenant that could
    /// name the listen address could take the shard's other tenants off the
    /// air, and `[models]` is this box's service, not this company's.
    #[test]
    fn an_overlay_naming_a_process_owned_key_is_refused() {
        for (key, text) in [
            ("serve.listen", "[serve]\nlisten = \"0.0.0.0:9999\"\n"),
            ("models", "[models]\nenabled = true\n"),
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
}
