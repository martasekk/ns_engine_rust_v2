mod budget;
mod cli;
mod clients;
mod config;
mod eval;
mod factory;
mod grade;
mod models;
mod registry;
mod serve;
mod tenant;

// The names `factory.rs` knows the client plumbing under: it asks the crate
// root for both, and a split is not a reason to rewrite its imports.
pub(crate) use clients::{client_for, env_override};

use config::AppConfig;

/// The one tenant a process with no tenant set is: `ns-app` on its own is
/// the company called `local` (multi-tenant plan §2).
pub(crate) const DEFAULT_TENANT: &str = "local";

/// What was asked for, in the order it has to be asked in: a subcommand
/// that answers and stops, `serve`, or the terminal.
#[tokio::main]
async fn main() {
    let cfg_text = std::fs::read_to_string("config.toml").unwrap_or_default();
    let mut cfg = match AppConfig::parse(&cfg_text) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("config.toml: {e}");
            std::process::exit(1);
        }
    };
    // A swap without editing config: NS_PROVIDER=ollama ns-app.
    cfg.llm
        .apply_overrides(env_override("NS_PROVIDER"), env_override("NS_MODEL"));
    let cfg = cfg;
    // M10 T1.3: resolved once, here, because every subcommand that prices or
    // sends a tool array has to price or send the same one.
    let schema_profile = match cfg.llm.schema_profile() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };
    // M12 T1.1, checked beside it: the emitter's preamble, the engine and
    // the reply path all read the same one fact about the model in play.
    // The factory resolves it again for the tenant it builds; refusing it
    // here keeps a bad `[llm]` a startup error for every subcommand.
    if let Err(e) = cfg.llm.capability() {
        eprintln!("{e}");
        std::process::exit(1);
    }
    // M10 T2.3, resolved here for the same reason: an unreadable depth is a
    // startup error, not a turn that silently runs at the default.
    if let Err(e) = cfg.router.depth() {
        eprintln!("{e}");
        std::process::exit(1);
    }
    let args: Vec<String> = std::env::args().collect();

    // Every subcommand that answers out of a local log or a local config
    // and then stops. The live session below is what is left when none of
    // them was asked for.
    if cli::run_oneshot(&args, &cfg, schema_profile).await {
        return;
    }

    // `ns-app serve`: the chat assembly below, on the TCP channel instead of
    // stdin (multi-conversation plan Phase 3). Not a separate path — the
    // harness is built exactly as for the chat, and the flag is consulted
    // at the points that differ: the channel, the fact scope, the banner.
    let serve = args.get(1).map(String::as_str) == Some("serve");

    // The tenant set this working directory serves (plan T1.2). With no
    // `tenants/` directory that is one tenant called `local` built from
    // `config.toml` alone, which is this process exactly as it was; the
    // overlays are refused by name here rather than at the first turn.
    //
    // Read before the flags because `token` is a subcommand of the *set*
    // rather than of a session, and flag parsing would otherwise refuse its
    // arguments before the set had been looked at.
    let set = tenant::load_set(&cfg_text, std::path::Path::new("."), DEFAULT_TENANT)
        .unwrap_or_else(|e| {
            eprintln!("{e}");
            std::process::exit(1);
        });

    // `ns-app token <tenant>`: a credential for one of this working
    // directory's companies, printed and nothing else. Here rather than in
    // `run_oneshot` because it is the first subcommand that needs the tenant
    // set — a company's signing key is its own, and the set is what holds it.
    if args.get(1).map(String::as_str) == Some("token") {
        match cli::mint_token(&set, &args[2..]) {
            // Alone on stdout, so `TOKEN=$(ns-app token acme)` is the whole
            // of using it.
            Ok(token) => println!("{token}"),
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(2);
            }
        }
        return;
    }

    // M12 T6.1: the metered-session flags. `serve` takes its session ids
    // from its clients, so only the ceiling means anything there.
    let flags_from = if serve { 2 } else { 1 };
    let (max_requests, cli_session) = match cli::parse_repl_args(
        &args[flags_from.min(args.len())..],
        env_override("NS_MAX_REQUESTS"),
        env_override("NS_SESSION"),
    ) {
        Ok(parsed) => parsed,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };

    // `serve` is the whole set: one socket, and an engine per company built
    // on its first message (plan B9). The terminal below is unchanged - one
    // tenant, one dispatcher, and more than one of them still refused.
    if serve {
        serve::serve_shard(set, max_requests).await;
        return;
    }
    let mut tenant = serve::one_tenant(set).unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(1);
    });
    // The same swap the base config gets: NS_PROVIDER=ollama ns-app.
    tenant
        .app
        .llm
        .apply_overrides(env_override("NS_PROVIDER"), env_override("NS_MODEL"));
    let tenant = tenant;
    let mode = factory::Mode::Cli {
        session: cli_session,
    };
    let built = factory::build_engine(&tenant, mode, max_requests)
        .await
        .unwrap_or_else(|e| e.exit());
    // Exactly the dispatcher `Engine::run` built for itself (`crates/engine`,
    // `turn/run.rs`): the engine, its channel, its slot count, and the
    // default `Fatal` failure policy. Built here instead so the engine stays
    // in the `Arc` the tenant registry will hold it in (plan B2) - taking it
    // back out would panic the moment a second handle existed.
    let dispatcher =
        nsengine::dispatch::Dispatcher::new(built.engine, built.channel, built.worker_slots);
    match dispatcher.run().await {
        Ok(()) => {}
        // M12 T6.1: the ceiling this run was given, reached. A stop by
        // arrangement rather than a failure, but non-zero all the same, so a
        // script driving the session can tell it ended early.
        Err(nsengine::turn::EngineError::RequestCap { spent, cap }) => {
            eprintln!("request cap {cap} reached after {spent} requests; stopping");
            std::process::exit(3);
        }
        Err(e) => eprintln!("engine stopped: {e}"),
    }
}
