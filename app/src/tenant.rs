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
    check_private_paths(&set)?;
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
