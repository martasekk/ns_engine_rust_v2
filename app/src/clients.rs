//! What every provider call in this process shares: its pacing, and where
//! its wire log lands.
//!
//! Both are process-wide registries and both are keyed rather than global -
//! a throttle per endpoint and credential, a trace file per tenant - because
//! one process now serves many companies, and a single shared value is how
//! one company's traffic starts pacing or recording another's.

use crate::config::RoleTarget;
use std::sync::Arc;

/// One throttle per endpoint *and credential*: roles sharing a base URL and
/// an API key share the pacing, so the provider sees one paced stream per
/// account (seen live: Mistral 429s on bursts). Roles on different providers
/// are paced independently, and so are two tenants on one provider with keys
/// of their own — a quota each means pacing each (multi-tenant plan H8). Two
/// tenants sharing a key still share the throttle, because they share the
/// quota it exists to respect.
///
/// The key is hashed rather than stored: a `DefaultHasher` is enough here
/// because this is a partitioning key and not a security boundary — it only
/// has to separate two different keys, not resist anyone.
fn throttle_for(target: &RoleTarget, key: &str) -> Arc<nsllm::client::Throttle> {
    use std::hash::{Hash, Hasher};
    type Registry =
        std::sync::Mutex<std::collections::HashMap<(String, u64), Arc<nsllm::client::Throttle>>>;
    static THROTTLES: std::sync::OnceLock<Registry> = std::sync::OnceLock::new();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut hasher);
    let mut map = THROTTLES
        .get_or_init(Registry::default)
        .lock()
        .expect("throttle registry");
    map.entry((target.base_url_or_default().to_string(), hasher.finish()))
        .or_insert_with(|| Arc::new(nsllm::client::Throttle::new(target.min_interval_ms)))
        .clone()
}

/// `serve`: refuse an NS_TRACE that cannot hold one file per tenant. Called
/// at startup so a bad setting is a refusal rather than a surprise on the
/// first request.
///
/// It names no tenant of its own any more (plan B7): which company a record
/// belongs to travels with the client that writes it, so a process serving
/// two companies cannot put the second one's prompts in the first one's
/// file. All that is left here is the check.
pub(crate) fn check_trace_dir(tenant: &str) -> Result<(), String> {
    if let Some(raw) = env_override("NS_TRACE") {
        trace_path(&raw, Some(tenant))?;
    }
    Ok(())
}

/// Where NS_TRACE's value actually writes, for one tenant.
///
/// In the CLI (`tenant` is `None`) the value is the file, untouched. In
/// serve mode it must be a directory and the tenant gets its own file inside
/// it: one process serving twenty companies into one file would interleave
/// every company's prompts, a disclosure the first operator to open it would
/// cause by accident (multi-tenant plan H10).
pub(crate) fn trace_path(raw: &str, tenant: Option<&str>) -> Result<String, String> {
    let Some(tenant) = tenant else {
        return Ok(raw.to_string());
    };
    let dir = std::path::Path::new(raw);
    if dir.is_file() {
        return Err(format!(
            "NS_TRACE={raw} is a file; serving needs a directory to write one trace file per tenant into."
        ));
    }
    std::fs::create_dir_all(dir).map_err(|e| format!("NS_TRACE={raw}: {e}"))?;
    Ok(dir.join(format!("{tenant}.jsonl")).to_string_lossy().into())
}

/// The wire log for one tenant, opened once per path when NS_TRACE names
/// one. A path that cannot be opened is fatal: a trace the user asked for
/// and did not get would let them debug against a file that is silently
/// never written.
fn trace_sink(tenant: Option<&str>) -> Option<Arc<nsllm::trace::Trace>> {
    trace_sink_from(env_override("NS_TRACE"), tenant)
}

/// The same, with NS_TRACE's value handed in: the registry is keyed by the
/// resolved path, so two tenants under one directory are two sinks and the
/// same tenant asked twice is one (plan B7).
fn trace_sink_from(raw: Option<String>, tenant: Option<&str>) -> Option<Arc<nsllm::trace::Trace>> {
    type Sinks = std::sync::Mutex<std::collections::HashMap<String, Arc<nsllm::trace::Trace>>>;
    static SINKS: std::sync::OnceLock<Sinks> = std::sync::OnceLock::new();
    let raw = raw?;
    let path = trace_path(&raw, tenant).unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(1);
    });
    let mut map = SINKS
        .get_or_init(Sinks::default)
        .lock()
        .expect("trace registry");
    if let Some(sink) = map.get(&path) {
        return Some(sink.clone());
    }
    match nsllm::trace::Trace::open(&path) {
        Ok(t) => {
            eprintln!("tracing every provider request to {path}");
            Some(map.entry(path).or_insert(Arc::new(t)).clone())
        }
        Err(e) => {
            eprintln!("NS_TRACE={path}: {e}");
            std::process::exit(1);
        }
    }
}

pub(crate) fn client_for(
    target: &RoleTarget,
    transport: Arc<nsllm::transport::ReqwestTransport>,
    key: &str,
    // Whose trace file this client's requests land in. `None` is the
    // terminal: NS_TRACE is the file it names (plan B7).
    tenant: Option<&str>,
) -> nsllm::client::OpenRouterClient {
    let c = nsllm::client::OpenRouterClient::new(transport, key.to_string())
        .with_throttle(throttle_for(target, key));
    let c = match &target.base_url {
        Some(url) => c.with_base_url(url.clone()),
        None => c,
    };
    match trace_sink(tenant) {
        Some(trace) => c.with_trace(trace, target.role.as_str()),
        None => c,
    }
}

/// A non-empty env var, trimmed.
pub(crate) fn env_override(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Role;

    fn target(base_url: &str) -> RoleTarget {
        RoleTarget {
            role: Role::Emitter,
            model: "m".into(),
            base_url: Some(base_url.into()),
            api_key_env: "NS_TEST_KEY".into(),
            min_interval_ms: 100,
            prompt_cache: false,
            local: false,
        }
    }

    /// Multi-tenant plan H8: two companies on one provider, each with its
    /// own key, have their own quota, so one's traffic must not pace the
    /// other's.
    #[test]
    fn two_tenants_with_distinct_keys_get_distinct_throttles() {
        let t = target("http://h8-distinct.invalid");
        let acme = throttle_for(&t, "acme-key");
        let globex = throttle_for(&t, "globex-key");
        assert!(
            !Arc::ptr_eq(&acme, &globex),
            "one base URL, two keys, two quotas — the throttle must not be shared"
        );
    }

    /// The other half of H8: sharing a key means sharing a quota, so
    /// sharing the pacing is the correct answer, not a leak.
    #[test]
    fn two_tenants_sharing_a_key_share_a_throttle() {
        let t = target("http://h8-shared.invalid");
        let first = throttle_for(&t, "one-account");
        let second = throttle_for(&t, "one-account");
        assert!(
            Arc::ptr_eq(&first, &second),
            "one key is one quota — the two tenants must share the pacing"
        );
        // And a role on another endpoint under the same key is still its own.
        let elsewhere = throttle_for(&target("http://h8-elsewhere.invalid"), "one-account");
        assert!(
            !Arc::ptr_eq(&first, &elsewhere),
            "one throttle per endpoint"
        );
    }

    /// Multi-tenant plan H10: in serve mode NS_TRACE names a directory with
    /// one file per tenant. A regular file there would interleave every
    /// company's prompts, so it is refused before the first request.
    #[test]
    fn serve_mode_refuses_a_trace_path_that_is_a_regular_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("wire.jsonl");
        std::fs::write(&file, "").unwrap();
        let raw = file.to_string_lossy().to_string();
        let err = trace_path(&raw, Some("acme")).expect_err("a regular file is not a directory");
        assert!(err.contains("NS_TRACE"), "names the variable: {err}");
        assert!(err.contains("directory"), "says what it expects: {err}");
        assert!(err.contains(&raw), "names the path given: {err}");
        // A directory is accepted, and the file inside it is the tenant's.
        let ok = trace_path(&dir.path().to_string_lossy(), Some("acme")).unwrap();
        assert!(
            ok.ends_with("acme.jsonl"),
            "per tenant, not per process: {ok}"
        );
        let other = trace_path(&dir.path().to_string_lossy(), Some("globex")).unwrap();
        assert_ne!(ok, other, "two tenants, two files");
    }

    /// The CLI is one tenant on one box: NS_TRACE keeps naming the file it
    /// always named, byte for byte.
    #[test]
    fn cli_mode_still_accepts_a_trace_file_path() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("wire.jsonl");
        std::fs::write(&file, "").unwrap();
        let raw = file.to_string_lossy().to_string();
        assert_eq!(trace_path(&raw, None).unwrap(), raw);
        // Not even a path that does not exist yet is inspected: the CLI
        // hands NS_TRACE to `Trace::open` exactly as it was given.
        let fresh = dir
            .path()
            .join("not-yet.jsonl")
            .to_string_lossy()
            .to_string();
        assert_eq!(trace_path(&fresh, None).unwrap(), fresh);
    }

    /// Plan B7. The tenant is an argument, not a process-wide global set
    /// once: with a global, the second company in the process traced into
    /// the first one's file and nothing said so.
    #[test]
    fn two_tenants_traces_land_in_two_files() {
        let dir = tempfile::tempdir().unwrap();
        let raw = dir.path().to_string_lossy().to_string();
        let acme = trace_sink_from(Some(raw.clone()), Some("acme")).expect("a sink");
        let globex = trace_sink_from(Some(raw.clone()), Some("globex")).expect("a sink");
        assert!(
            !Arc::ptr_eq(&acme, &globex),
            "one company's prompts must not append to another's file"
        );
        assert!(dir.path().join("acme.jsonl").exists());
        assert!(dir.path().join("globex.jsonl").exists());
        // The same company asked twice is the one open file, as before.
        let again = trace_sink_from(Some(raw), Some("acme")).expect("a sink");
        assert!(Arc::ptr_eq(&acme, &again), "one sink per resolved path");
        // No tenant is the terminal: NS_TRACE is the file, untouched.
        let file = dir.path().join("wire.jsonl").to_string_lossy().to_string();
        let cli = trace_sink_from(Some(file.clone()), None).expect("a sink");
        assert_eq!(cli.path().to_string_lossy(), file);
    }
}
