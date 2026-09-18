//! `ns-app serve`: how many companies this process runs, and the socket
//! they arrive on.
//!
//! One listener, no engine until a company speaks, and one refusal
//! ([`one_tenant`]) that the terminal shares: a set of more than one is a
//! shard, and a shard is this file.

use crate::clients::check_trace_dir;
use crate::{env_override, factory, registry, tenant};
use std::sync::Arc;

/// The one tenant this process runs, out of the set the working directory
/// resolved to, or the message to print and stop on.
///
/// Two refusals, and neither is a fallback. More than one tenant is a shard,
/// which is `ns-app serve` and the tenant registry; the terminal is one
/// person at one company. *None* is a `tenants/` directory
/// holding no overlay at all — a `README.md` in there, or the overlays moved
/// aside for a moment — and running it under the base config would put a
/// company's traffic on the shared persona and the shared store, which is
/// exactly what a missing overlay is already refused for
/// (`TenantError::Missing`). An operator who wants the base config deletes
/// the directory; one who left it empty by accident is told so.
pub(crate) fn one_tenant(
    mut set: Vec<tenant::TenantConfig>,
) -> Result<tenant::TenantConfig, String> {
    if set.is_empty() {
        return Err(format!(
            "{}/ holds no overlay — a tenant is its overlay, so an empty directory is refused \
             rather than run on the base config. Remove the directory to serve config.toml \
             alone.",
            tenant::TENANT_DIR
        ));
    }
    if set.len() > 1 {
        let ids: Vec<&str> = set.iter().map(|t| t.id.as_str()).collect();
        return Err(format!(
            "{} tenants are configured ({}) - the terminal is one person at one company, so it \
             serves one tenant; `ns-app serve` is what hosts a set.",
            set.len(),
            ids.join(", ")
        ));
    }
    Ok(set.remove(0))
}

/// `ns-app serve`: one listener, and an engine per company built on that
/// company's first message (plan B9).
///
/// The shape is the registry's, not a dispatcher's: nothing is built at
/// startup, because a shard of twenty companies would otherwise dial twenty
/// sets of providers and open twenty databases for the two that will speak
/// today. What startup does own is everything a mistake should be loud
/// about while the port is still free - the trace directory, the shard
/// ceiling, the listen address, the key table - and the socket itself, which
/// is one for the process however many companies arrive on it.
pub(crate) async fn serve_shard(mut set: Vec<tenant::TenantConfig>, max_requests: Option<u32>) {
    if set.is_empty() {
        // The same refusal the terminal gives, for the same reason: a
        // company is its overlay, and an empty directory is not one.
        match one_tenant(set) {
            Err(e) => eprintln!("{e}"),
            Ok(_) => unreachable!("an empty set holds no tenant"),
        }
        std::process::exit(1);
    }
    for tenant in &mut set {
        // Serving is the multi-tenant shape even at one tenant, so the wire
        // trace is per tenant (plan H10): NS_TRACE names a directory and
        // each company writes its own file in it. Refused here, at startup,
        // rather than on the first request.
        if let Err(e) = check_trace_dir(&tenant.id) {
            eprintln!("{e}");
            std::process::exit(1);
        }
        // The same swap the base config gets: NS_PROVIDER=ollama ns-app.
        tenant
            .app
            .llm
            .apply_overrides(env_override("NS_PROVIDER"), env_override("NS_MODEL"));
    }
    // `[serve]` and the shard ceiling are process-owned
    // (`tenant::PROCESS_OWNED`), so any member's copy is the process's.
    let process = &set[0];
    // Plan B6: the ceiling on turns running at once across every tenant this
    // process hosts - the shard's, not a company's. Read before the bind, so
    // a typo is refused while the port is still free.
    let shard = nsengine::dispatch::ShardSlots::new(
        process
            .app
            .engine
            .shard_worker_slots()
            .unwrap_or_else(|e| factory::StartupError::Config(e).exit()),
    );
    let serve_cfg = &process.app.serve;
    // Shared auth off loopback is refused here rather than at the bind
    // (plan A6): the port is still free, and the message names the config
    // key instead of the socket.
    if let Err(e) = factory::check_serve_address(serve_cfg).await {
        e.exit()
    }
    // Which resolver the listener decides identity with is the whole of
    // `[serve] auth`, now over the whole set: a token minted by one company
    // reaches that company's engine and no other. A missing token, an
    // unnamed signing key, or one shared token asked to stand for several
    // companies is refused before the socket.
    let auth = match factory::shard_resolver(&set) {
        Ok(auth) => auth,
        Err(e) => e.exit(),
    };
    // Every way in meets at one hub: a company's queue is filled by its
    // sockets, its browser windows and its platform webhooks alike, and the
    // registry watches the hub rather than one listener per transport. It is
    // also the shutdown now — the last way in closed, not the first.
    let hub = nschannel_hub::Hub::new();
    // The platforms this shard answers webhooks for, read while the ports
    // are still free: a company whose WhatsApp secrets are missing is a
    // named refusal here rather than one that fails every delivery Meta
    // sends it.
    let platforms = match factory::shard_platforms(&set, &process.app.http) {
        Ok(platforms) => platforms,
        Err(e) => e.exit(),
    };
    if !platforms.is_empty() && !process.app.http.enabled() {
        factory::StartupError::Config(
            "a [whatsapp] account is configured but [http] listen is empty: a platform has \
             nowhere to deliver to"
                .into(),
        )
        .exit()
    }
    // One socket serves the whole process, so the bind is the caller's and
    // not the factory's (plan A1): a shard building one engine per company
    // would otherwise reach for the same address once per tenant.
    let listener = match nschannel_tcp::TcpChannel::bind_on(
        hub.clone(),
        &serve_cfg.listen,
        auth.clone(),
        serve_cfg.max_connections,
        serve_cfg.allow_remote,
        std::time::Duration::from_millis(serve_cfg.hello_timeout_ms),
    )
    .await
    {
        Ok(listener) => listener,
        Err(e) => factory::StartupError::Serve(e.to_string()).exit(),
    };
    let addr = listener.local_addr();
    // The second way in, and only if it was asked for: a config written
    // before `[http]` existed opens the socket alone, exactly as it did.
    let http = if process.app.http.enabled() {
        let cfg = http_config(&process.app.http);
        match nschannel_http::HttpChannel::bind_on(hub.clone(), cfg, auth, platforms).await {
            Ok(http) => {
                eprintln!(
                    "http: chat windows, requests and platform webhooks on {}",
                    http.local_addr()
                );
                Some(http)
            }
            Err(e) => factory::StartupError::Serve(e.to_string()).exit(),
        }
    } else {
        None
    };
    let registry = Arc::new(registry::TenantRegistry::new(
        tenant_builder(set, addr, max_requests),
        hub,
        shard,
        registry::RegistryLimits::default(),
    ));
    // The wake loop replaces the single dispatcher: a company that speaks
    // and has nobody draining its queue gets an engine, and a company's own
    // failure stops that company alone. `Err` is the one thing no company
    // can decide for itself (plan B5), and it ends the process the way a
    // dead engine always has - the reason on stderr, a non-zero status.
    let outcome = registry.clone().serve().await;
    // Held until the wake loop is over: each is a way in, and dropping the
    // last of them is what shuts the hub down.
    drop(listener);
    drop(http);
    // What each company cost while this shard was up (plan B8), printed
    // where it can still be read: after the wake loop, whichever way it
    // ended. Nothing else in the process counts per company.
    let report = registry.report();
    if !report.is_empty() {
        eprint!("{report}");
    }
    if let Err(fatal) = outcome {
        eprintln!("engine stopped: {fatal}");
        std::process::exit(1);
    }
}

/// `[http]` as the channel crate wants it. A translation and nothing more,
/// except for one decision: `origins = ["*"]` is `Any`, because a widget a
/// customer embeds on their own site arrives from a domain this shard has
/// never been told about, and listing them all would be a deployment per
/// customer.
fn http_config(cfg: &crate::config::HttpSection) -> nschannel_http::HttpConfig {
    nschannel_http::HttpConfig {
        listen: cfg.listen.clone(),
        allow_remote: cfg.allow_remote,
        max_connections: cfg.max_connections,
        hello_timeout: std::time::Duration::from_millis(cfg.hello_timeout_ms),
        reply_timeout: std::time::Duration::from_millis(cfg.reply_timeout_ms),
        max_body: cfg.max_body_bytes,
        origins: if cfg.origins.iter().any(|o| o == "*") {
            nschannel_http::Origins::Any
        } else {
            nschannel_http::Origins::These(cfg.origins.clone())
        },
    }
}

/// How the registry builds one company: its overlay out of the set that was
/// loaded, on the channel the registry hands it (plan B9).
///
/// A name the set does not hold is refused rather than built. The identity
/// layer already refuses an unknown company before it can have a queue, so
/// this is the same answer given twice on purpose: the alternative to a
/// refusal here is not an error, it is a company quietly assembled from the
/// base config, on the shared persona and the shared store.
fn tenant_builder(
    set: Vec<tenant::TenantConfig>,
    addr: std::net::SocketAddr,
    max_requests: Option<u32>,
) -> registry::BuildTenant<factory::BuiltTenant> {
    let by_id: Arc<std::collections::HashMap<String, tenant::TenantConfig>> =
        Arc::new(set.into_iter().map(|t| (t.id.clone(), t)).collect());
    Arc::new(move |id, channel| {
        let by_id = by_id.clone();
        Box::pin(async move {
            let Some(tenant) = by_id.get(&id) else {
                return Err(factory::StartupError::UnknownTenant { tenant: id });
            };
            factory::build_engine(tenant, factory::Mode::Serve { channel, addr }, max_requests)
                .await
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clients::trace_path;
    use crate::DEFAULT_TENANT;

    /// Write a tenant set under a scratch root and hand back the root.
    fn tenant_root(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(dir.path().join(tenant::TENANT_DIR)).expect("tenants dir");
        for (id, text) in files {
            std::fs::write(
                dir.path()
                    .join(tenant::TENANT_DIR)
                    .join(format!("{id}.toml")),
                text,
            )
            .expect("overlay");
        }
        dir
    }

    /// R1. A `tenants/` directory holding no overlay — a `README.md` in
    /// there, or the files moved aside for a moment — used to take the
    /// vector's first element and abort the process on an index panic. It is
    /// a refusal now, with the directory named, because running it on the
    /// base config would put a company on the shared persona and the shared
    /// store.
    #[test]
    fn an_empty_tenants_directory_does_not_panic() {
        let root = tenant_root(&[]);
        let set = tenant::load_set("", root.path(), DEFAULT_TENANT).expect("an empty set loads");
        assert!(set.is_empty(), "the directory holds no overlay");
        let err = match one_tenant(set) {
            Err(e) => e,
            Ok(t) => panic!("an empty tenants/ must be refused, not run as {:?}", t.id),
        };
        assert!(err.contains(tenant::TENANT_DIR), "{err}");

        // And the two cases either side of it are unchanged: one tenant is
        // that tenant, more than one is still the registry's refusal.
        let root = tenant_root(&[("acme", "")]);
        let set = tenant::load_set("", root.path(), DEFAULT_TENANT).expect("one overlay");
        assert_eq!(one_tenant(set).expect("one tenant").id, "acme");

        // Each names its own store, learned file and ledger, because a set
        // that shares any of the three is refused before it gets here.
        let root = tenant_root(&[
            (
                "acme",
                "[store]\npath = \"ns-acme.sqlite\"\n\
                 [evolution]\nlearned_path = \"l-acme.toml\"\nledger_path = \"g-acme.json\"\n",
            ),
            (
                "beta",
                "[store]\npath = \"ns-beta.sqlite\"\n\
                 [evolution]\nlearned_path = \"l-beta.toml\"\nledger_path = \"g-beta.json\"\n",
            ),
        ]);
        let set = tenant::load_set("", root.path(), DEFAULT_TENANT).expect("two overlays");
        let err = match one_tenant(set) {
            Err(e) => e,
            Ok(_) => panic!("two tenants must be refused"),
        };
        assert!(err.contains("acme") && err.contains("beta"), "{err}");
    }

    /// Plan B9. The shard builds a company out of the set that was loaded,
    /// and a name that set does not hold is refused. The danger it guards
    /// is not an error going unreported: it is a company quietly assembled
    /// from the base config - shared persona, shared store - because the
    /// builder took whatever it could find rather than what was asked for.
    #[tokio::test]
    async fn a_message_for_an_unknown_tenant_is_refused_not_built() {
        struct NullChannel;
        #[async_trait::async_trait]
        impl nscore::Channel for NullChannel {
            async fn recv(&self) -> Result<nscore::Incoming, nscore::ChannelError> {
                Err(nscore::ChannelError::Closed)
            }
            async fn send(
                &self,
                _s: &nscore::SessionId,
                _t: &str,
            ) -> Result<(), nscore::ChannelError> {
                Ok(())
            }
        }

        let root = tenant_root(&[("acme", "[store]\npath = \"ns-acme.sqlite\"\n")]);
        let set = tenant::load_set("", root.path(), DEFAULT_TENANT).expect("one overlay");
        assert_eq!(set.len(), 1, "one company is configured");
        let build = tenant_builder(set, "127.0.0.1:0".parse().expect("a literal address"), None);
        let channel: Arc<dyn nscore::Channel> = Arc::new(NullChannel);
        let err = match build("globex".to_string(), channel).await {
            Err(e) => e.to_string(),
            Ok(_) => panic!("a company nobody configured must not be built"),
        };
        assert!(err.contains("globex"), "{err}");
        assert!(err.contains(tenant::TENANT_DIR), "{err}");
    }

    /// R2. The trace file is named for the tenant that was resolved, not for
    /// the id the process would have used had there been no overlay: a
    /// single `tenants/acme.toml` writes `acme.jsonl`. Labelling it before
    /// the set was loaded put acme's prompts in `local.jsonl`.
    #[test]
    fn a_tenants_trace_is_named_for_the_tenant_not_the_default() {
        let root = tenant_root(&[("acme", "")]);
        let set = tenant::load_set("", root.path(), DEFAULT_TENANT).expect("one overlay");
        let tenant = one_tenant(set).expect("one tenant");
        assert_ne!(tenant.id, DEFAULT_TENANT, "the overlay names it");

        let dir = root.path().join("trace");
        let named = trace_path(&dir.to_string_lossy(), Some(&tenant.id)).expect("a trace path");
        assert!(named.ends_with("acme.jsonl"), "{named}");
        let defaulted =
            trace_path(&dir.to_string_lossy(), Some(DEFAULT_TENANT)).expect("a trace path");
        assert_ne!(named, defaulted, "the default id is a different file");
    }
}
