//! Everything `ns-app` can be asked to do once and be finished with.
//!
//! Seven subcommands that read a log, price a session or print a table, the
//! flags they parse, and the renderers they print. None of them builds an
//! engine and none of them outlives the call: [`run_oneshot`] either handles
//! what was asked and says so, or hands the process back to `main` for the
//! terminal.

use crate::config::{AppConfig, Role};
use crate::{budget, env_override, eval, factory, grade, models};
use nscore::{SessionId, Tool};

/// Every spec `ns-app budget` can price a recorded tool array with (M10
/// T0.1): the engine's seven synthetic actions, the desktop set, and the one
/// built-in tool. Deliberately *not* `build_tools` — that dials the pointer
/// daemon and reads the http component config, and `budget` reads a log on a
/// box where neither has to be up. A name the snapshot does not hold prints
/// `n/a` rather than a guess.
pub(crate) fn budget_specs(profile: nscore::SchemaProfile) -> Vec<nscore::ActionSpec> {
    let mut specs = nsengine::turn::synthetic_specs(profile);
    specs.extend(nscomponents_std::pointer_tool::specs(profile));
    specs.push(
        nscomponents_std::time_tool::GetTimeTool::new()
            .spec()
            .clone(),
    );
    specs
}

/// `ns-app evolve [--dry-run] [--spend]` → Ok((dry_run, spend))
///
/// M12 T0.2: `--dry-run` alone now spends nothing — no judge, no note
/// proposer, no probes — and `--spend` is the only way to buy those lanes
/// back out of one. Without `--dry-run` the pass spends anyway, so
/// `--spend` there is accepted and says nothing new.
fn parse_evolve_args(args: &[String]) -> Result<(bool, bool), String> {
    let usage = || format!("usage: ns-app evolve [--dry-run] [--spend] (got {args:?})");
    let (mut dry_run, mut spend) = (false, false);
    for arg in args {
        let flag = match arg.as_str() {
            "--dry-run" => &mut dry_run,
            "--spend" => &mut spend,
            _ => return Err(usage()),
        };
        if *flag {
            return Err(usage());
        }
        *flag = true;
    }
    Ok((dry_run, spend))
}

/// `ns-app [--max-requests N] [--session ID]` → Ok((max_requests, session))
///
/// M12 T6.1: the two things a metered live session needs that the config
/// file cannot give it — a ceiling on what this run may spend, and an id of
/// its own so the reading is separable from every other CLI session in the
/// store. `NS_MAX_REQUESTS` and `NS_SESSION` are the fallback for each; the
/// flag wins when both are given.
pub(crate) fn parse_repl_args(
    args: &[String],
    env_max_requests: Option<String>,
    env_session: Option<String>,
) -> Result<(Option<u32>, String), String> {
    let usage = || format!("usage: ns-app [--max-requests N] [--session ID] (got {args:?})");
    let mut max_requests = env_max_requests;
    let mut session = env_session;
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        let slot = match arg.as_str() {
            "--max-requests" => &mut max_requests,
            "--session" => &mut session,
            _ => return Err(usage()),
        };
        let Some(value) = rest.next() else {
            return Err(format!("{arg} needs a value — {}", usage()));
        };
        *slot = Some(value.clone());
    }
    let max_requests = match max_requests {
        Some(n) => Some(
            n.parse::<u32>()
                .map_err(|_| format!("--max-requests wants a number, got {n:?}"))?,
        ),
        None => None,
    };
    Ok((max_requests, session.unwrap_or_else(|| "cli".into())))
}

/// The subcommand this process was asked for, if it was asked for one of
/// these: `true` when it ran and the process is done.
///
/// `main` consults this before anything a live session needs, because every
/// one of these answers is local - a log, a config, a table - and none of
/// them wants a key, a socket or a store handle it did not open itself.
pub(crate) async fn run_oneshot(
    args: &[String],
    cfg: &AppConfig,
    schema_profile: nscore::SchemaProfile,
) -> bool {
    // `ns-app providers`: which backends exist, which keys are present, and
    // what this config resolves to. Needs no key and no network.
    if args.get(1).map(String::as_str) == Some("providers") {
        print!("{}", render_providers(cfg));
        return true;
    }

    // `ns-app dump <session_id>`: print the session log as JSONL and exit.
    // Needs no API key — the log is local.
    if args.get(1).map(String::as_str) == Some("dump") {
        let Some(session) = args.get(2) else {
            eprintln!("usage: ns-app dump <session_id>");
            std::process::exit(2);
        };
        let store = nsmemory_sqlite::SqliteStore::open(std::path::Path::new(&cfg.store.path))
            .expect("open sqlite store");
        let events = nscore::MemoryStore::load(&store, &SessionId(session.clone()))
            .await
            .expect("load session");
        println!("{}", render_dump(&events));
        return true;
    }

    // `ns-app echo <session_id>`: per-turn copy ratio for the session log.
    // Needs no API key — the metric is lexical and the log is local.
    if args.get(1).map(String::as_str) == Some("echo") {
        let Some(session) = args.get(2) else {
            eprintln!("usage: ns-app echo <session_id>");
            std::process::exit(2);
        };
        let store = nsmemory_sqlite::SqliteStore::open(std::path::Path::new(&cfg.store.path))
            .expect("open sqlite store");
        let events = nscore::MemoryStore::load(&store, &SessionId(session.clone()))
            .await
            .expect("load session");
        print!(
            "{}",
            render_echo(&events, cfg.memory.window_turns, cfg.memory.caps())
        );
        return true;
    }

    // `ns-app budget <session_id>`: requests, tokens and characters per turn
    // (M7 T0.2). Needs no API key — the cost was recorded when the calls
    // were made, and a session recorded before `ModelCall` existed is
    // reconstructed from the log rather than re-run.
    if args.get(1).map(String::as_str) == Some("budget") {
        let Some(session) = args.get(2) else {
            eprintln!("usage: ns-app budget <session_id>");
            std::process::exit(2);
        };
        let store = nsmemory_sqlite::SqliteStore::open(std::path::Path::new(&cfg.store.path))
            .expect("open sqlite store");
        let events = nscore::MemoryStore::load(&store, &SessionId(session.clone()))
            .await
            .expect("load session");
        print!(
            "{}",
            budget::render_budget(
                &events,
                cfg.memory.window_turns,
                cfg.memory.caps(),
                cfg.memory.trace_verbatim_lines,
                cfg.memory.tool_result_max_chars,
                cfg.persona.text.len(),
                &budget_specs(schema_profile),
            )
        );
        return true;
    }

    // `ns-app eval [<ledger-path>]`: the six memory abilities against a fixed
    // model (M7 T5.2). Needs no API key and no network — every model in the
    // set is a scripted double, which is the point: a number that moves
    // between two runs moved because the harness changed. Exits non-zero when
    // an ability failed, so a release script can gate on it.
    if args.get(1).map(String::as_str) == Some("eval") {
        let parsed = match eval::parse_args(&args[2..]) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(2);
            }
        };
        if parsed.paraphrase {
            std::process::exit(eval::run_paraphrase(parsed.activation, parsed.facts).await);
        }
        // M10 T5.4. The same shape: a report, not a gate.
        if parsed.obligations {
            std::process::exit(eval::run_knob(eval::Knob::Obligations).await);
        }
        if parsed.guidelines {
            std::process::exit(eval::run_knob(eval::Knob::Guidelines).await);
        }
        // M9 T0.4. A report, not a gate: exits 0 whatever the delta.
        if let Some(block) = parsed.ablate {
            std::process::exit(eval::run_ablate(block, parsed.activation).await);
        }
        std::process::exit(
            eval::run_at(
                &parsed.ledger,
                parsed.activation,
                parsed.depth,
                parsed.profile,
            )
            .await,
        );
    }

    // `ns-app grade [--local] [--split dev|held|all]`: what an evaluator is
    // worth against a hundred labelled turns (M8 T2.7). No key. The symbolic
    // arm needs no network either; `--local` dials nsmodels and degrades if it
    // is not there.
    if args.get(1).map(String::as_str) == Some("grade") {
        let parsed = match grade::parse_args(&args[2..]) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(2);
            }
        };
        let code = grade::run_cmd(
            &parsed,
            &cfg.models.base_url,
            cfg.models.timeout_ms,
            cfg.models.reask_cosine,
            cfg.models.relevance_cut,
        )
        .await;
        std::process::exit(code);
    }

    // `ns-app evolve [--dry-run]`: driver A (spec M5 §5). The symbolic lane
    // needs no key; without one the notes lane is skipped with a warning.
    if args.get(1).map(String::as_str) == Some("evolve") {
        let (dry_run, spend) = match parse_evolve_args(&args[2..]) {
            Ok(flags) => flags,
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(2);
            }
        };
        if dry_run && !spend {
            eprintln!(
                "dry run: the judge, the note proposer and the probes are skipped \
                 (add --spend to buy them)."
            );
        }
        let rules = factory::load_rules(cfg).unwrap_or_else(|e| e.exit());
        let tools = factory::build_tools(cfg, schema_profile)
            .await
            .unwrap_or_else(|e| e.exit());
        let emitter = factory::role(cfg, Role::Emitter).unwrap_or_else(|e| e.exit());
        // `ns-app evolve` is the idle pass run by hand, in the terminal: one
        // tenant on one box, so NS_TRACE is the file it names (plan B7).
        let pass = factory::build_pass(cfg, rules, &tools, &emitter, dry_run, spend, None);
        // M8 T3.1: `evolve` is the idle pass run by hand, and the embeddings
        // backfill is one of its steps — so this store needs the encoder the
        // running harness's does, or `ns-app evolve` would be the one place
        // the backfill never happens.
        let store = models::store(cfg);
        match pass.run_report(&store).await {
            Ok(report) => println!("{report}"),
            Err(e) => {
                eprintln!("evolve: {e}");
                std::process::exit(1);
            }
        }
        return true;
    }
    false
}

/// `ns-app providers`: the presets, which keys are present, which one this
/// config is on, and where the full guide lives.
fn render_providers(cfg: &AppConfig) -> String {
    let roles = cfg.llm.roles();
    // A preset is "active" when a role actually resolves onto its endpoint.
    let active: std::collections::HashSet<&str> = match &roles {
        Ok(targets) => targets
            .iter()
            .filter_map(|t| nsllm::provider::for_base_url(t.base_url_or_default()))
            .map(|p| p.name)
            .collect(),
        Err(_) => std::collections::HashSet::new(),
    };

    let mut s = String::from(
        "Providers — one word swaps the agent.\n\n\
         \x20 in config.toml   [llm] provider = \"<name>\"\n\
         \x20 for one run      NS_PROVIDER=<name> [NS_MODEL=<id>] cargo run -p ns-app\n\
         \x20 for one role     [llm.replier] model = \"<name>:<model-id>\"\n\n",
    );
    let row = |mark: &str, name: &str, url: &str, env: &str, key: &str, model: &str| {
        format!("  {mark:<2}{name:<12}{url:<32}{env:<21}{key:<9}{model}\n")
    };
    s.push_str(&row(
        "",
        "NAME",
        "ENDPOINT",
        "KEY (env var)",
        "KEY?",
        "DEFAULT MODEL",
    ));
    for p in nsllm::provider::PROVIDERS {
        let key = if p.local {
            "local"
        } else if env_override(p.api_key_env).is_some() {
            "set"
        } else {
            "MISSING"
        };
        s.push_str(&row(
            if active.contains(p.name) { "→" } else { "" },
            p.name,
            p.base_url,
            p.api_key_env,
            key,
            p.default_model.unwrap_or("(name one)"),
        ));
    }
    s.push_str("\n  → = what this config uses.  local = key optional, any value works.\n\n");
    match &roles {
        Ok(targets) => {
            s.push_str("Active roles:\n");
            for t in targets {
                s.push_str(&format!(
                    "  {:<12}{} @ {}\n",
                    t.role.as_str(),
                    t.model,
                    t.base_url_or_default()
                ));
            }
        }
        Err(e) => s.push_str(&format!("Active roles: config.toml: {e}\n")),
    }
    s.push_str("\nSetup, per-provider notes and troubleshooting: docs/providers.md\n");
    s
}

/// `echo_ratio` per replied turn, reconstructed from a stored log: the
/// acceptance number for the reply-entrainment work (plan §3, phase 0).
///
/// The material is what the log preserves — the window as of that turn, the
/// summary in force, and the turn's own trace. Standing facts are not in it:
/// the log records what the engine decided, not which facts were selected,
/// and the facts table holds only current values. So this **under**-reports
/// against the runtime check, which sees the rendered facts too. It is a
/// floor on the copying in a session, not a ceiling.
fn render_echo(events: &[nscore::Event], window_turns: usize, caps: nscore::Caps) -> String {
    let mut out = String::new();
    let mut ratios: Vec<f32> = Vec::new();
    for (i, ev) in events.iter().enumerate() {
        let nscore::EventKind::Replied { text } = &ev.kind else {
            continue;
        };
        // Fold everything before this turn: records exist only for completed
        // turns, so this is exactly the window the reply was shown.
        let before: Vec<nscore::Event> = events[..i]
            .iter()
            .filter(|e| e.turn < ev.turn)
            .cloned()
            .collect();
        let state = nsengine::state::fold(&before);
        let window = state.window(window_turns);
        let mut material = nsengine::trace::turn_trace(events, ev.turn);
        if let Some(s) = &state.summary {
            material.push('\n');
            material.push_str(&nscore::render_summary(s));
        }
        material.push('\n');
        material.push_str(&nscore::render_window(&window, window.len(), &caps));
        let ratio = nsengine::echo::echo_ratio(text, &material);
        ratios.push(ratio);
        let head: String = text.replace('\n', " ").chars().take(70).collect();
        out.push_str(&format!("t{:<5} {ratio:.2}  {head}\n", ev.turn));
    }
    let n = ratios.len();
    let over = ratios.iter().filter(|r| **r >= 0.6).count();
    let mean = if n == 0 {
        0.0
    } else {
        ratios.iter().sum::<f32>() / n as f32
    };
    out.push_str(&format!(
        "\n{n} replied turns · {over} at or over 0.60 · mean {mean:.3}\n"
    ));
    out
}

/// JSONL: one serialized event per line (spec §7 — eyeball any session).
fn render_dump(events: &[nscore::Event]) -> String {
    events
        .iter()
        .map(|e| serde_json::to_string(e).expect("event serialization is infallible"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// `ns-app token <tenant> [--subject ID] [--minutes N]` → the token, on
/// stdout, and nothing else on it.
///
/// In production a company's own back end mints these, at the moment it
/// knows which of *its* users is on the page, and this subcommand does not
/// exist in that path. What it is for is the step before that exists: a test
/// company, a staging environment, a demo, someone holding the chat window
/// open to see whether any of this works.
///
/// Nothing but the token is printed, so `TOKEN=$(ns-app token acme)` is the
/// whole of using it. Everything else goes to stderr.
pub(crate) fn mint_token(
    set: &[crate::tenant::TenantConfig],
    args: &[String],
) -> Result<String, String> {
    let usage = "usage: ns-app token <tenant> [--subject ID] [--minutes N]";
    let Some(id) = args.first() else {
        return Err(usage.to_string());
    };
    let (mut subject, mut minutes) = ("u1".to_string(), 60u64);
    let mut rest = args[1..].iter();
    while let Some(flag) = rest.next() {
        let Some(value) = rest.next() else {
            return Err(format!("{flag} needs a value — {usage}"));
        };
        match flag.as_str() {
            "--subject" => subject = value.clone(),
            "--minutes" => {
                minutes = value
                    .parse()
                    .map_err(|_| format!("--minutes takes a number, not {value:?}"))?
            }
            _ => return Err(usage.to_string()),
        }
    }
    let tenant = set.iter().find(|t| &t.id == id).ok_or_else(|| {
        // The configured ids, because the likeliest mistake here is a typo
        // or a company whose overlay was never written.
        let known: Vec<&str> = set.iter().map(|t| t.id.as_str()).collect();
        format!(
            "no tenant {id:?} is configured — this working directory serves: {}",
            known.join(", ")
        )
    })?;
    if tenant.app.serve.auth != crate::config::AuthMode::Jwt {
        // Minting against a server that will not verify tokens is a token
        // that cannot be used, and the refusal it would meet says only that
        // something was denied.
        return Err(format!(
            "tenant {id:?} runs [serve] auth = \"shared\", which verifies no token — set \
             auth = \"jwt\" and give [auth] signing_key_envs a variable to mint against"
        ));
    }
    // The *current* key, which is the one tokens are signed with; a second
    // name in `signing_key_envs` is the key being rotated out, still
    // accepted but no longer minted with.
    let auth = factory::tenant_auth(tenant).map_err(|e| e.to_string())?;
    nsidentity::mint_hs256(
        &auth.current,
        &tenant.id,
        &subject,
        nsidentity::now_secs(),
        minutes.saturating_mul(60),
    )
    .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_dump_is_one_json_line_per_event() {
        let mut log = nscore::EventLog::new(nscore::SessionId("d".into()));
        log.append(
            1,
            nscore::Timestamp(1),
            nscore::EventKind::UserSaid { text: "hi".into() },
        );
        log.append(
            1,
            nscore::Timestamp(2),
            nscore::EventKind::Replied { text: "ho".into() },
        );
        let out = render_dump(log.events());
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2);
        let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(first["kind"]["type"], "UserSaid");
    }

    #[test]
    fn providers_listing_names_every_preset_and_the_active_roles() {
        let cfg = AppConfig::parse("[llm]\nprovider = \"ollama\"").unwrap();
        let out = render_providers(&cfg);
        for p in nsllm::provider::PROVIDERS {
            assert!(out.contains(p.name), "{} missing from:\n{out}", p.name);
            assert!(out.contains(p.base_url), "{} url missing", p.name);
        }
        assert!(
            out.contains("emitter     qwen2.5:3b @ http://localhost:11434"),
            "{out}"
        );
        assert!(
            out.contains("replier     qwen2.5:3b @ http://localhost:11434"),
            "{out}"
        );
        assert!(
            out.contains("OLLAMA_API_KEY       local"),
            "local key is optional: {out}"
        );
        assert!(
            out.contains("docs/providers.md"),
            "points at the guide: {out}"
        );
        // The preset in use is marked; the others are not.
        let ollama_row = out.lines().find(|l| l.contains("ollama ")).unwrap();
        assert!(ollama_row.trim_start().starts_with('→'), "{ollama_row:?}");
        let openai_row = out.lines().find(|l| l.contains("openai ")).unwrap();
        assert!(!openai_row.contains('→'), "{openai_row:?}");
    }

    /// A bad provider name must surface in the listing, not panic it.
    #[test]
    fn providers_listing_reports_a_config_error_instead_of_roles() {
        let cfg = AppConfig::parse("[llm]\nprovider = \"nope\"").unwrap();
        let out = render_providers(&cfg);
        assert!(out.contains("is unknown"), "{out}");
    }

    /// M12 T0.2: `--dry-run` is free by default, and `--spend` is the only
    /// way to buy requests out of one.
    #[test]
    fn evolve_args_accept_dry_run_and_spend() {
        let arg = |flags: &[&str]| {
            parse_evolve_args(&flags.iter().map(|s| s.to_string()).collect::<Vec<_>>())
        };
        assert_eq!(arg(&[]), Ok((false, false)));
        assert_eq!(arg(&["--dry-run"]), Ok((true, false)));
        assert_eq!(arg(&["--spend"]), Ok((false, true)));
        assert_eq!(arg(&["--dry-run", "--spend"]), Ok((true, true)));
        assert_eq!(arg(&["--spend", "--dry-run"]), Ok((true, true)));
        assert!(arg(&["--wat"]).is_err());
        assert!(arg(&["--dry-run", "--dry-run"]).is_err());
    }

    /// M12 T6.1: the two flags a metered live session is run with.
    #[test]
    fn repl_args_accept_max_requests_and_session() {
        let arg = |flags: &[&str]| {
            parse_repl_args(
                &flags.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
                None,
                None,
            )
        };
        assert_eq!(arg(&[]), Ok((None, "cli".to_string())));
        assert_eq!(
            arg(&["--max-requests", "60", "--session", "m12-live"]),
            Ok((Some(60), "m12-live".to_string()))
        );
        assert_eq!(
            arg(&["--session", "m12-live"]),
            Ok((None, "m12-live".to_string()))
        );
        assert!(arg(&["--max-requests", "sixty"]).is_err());
        assert!(arg(&["--max-requests"]).is_err());
        assert!(arg(&["--wat"]).is_err());
        // The environment is the fallback; a flag beside it wins.
        assert_eq!(
            parse_repl_args(&[], Some("40".into()), Some("env-session".into())),
            Ok((Some(40), "env-session".to_string()))
        );
        assert_eq!(
            parse_repl_args(
                &["--max-requests".to_string(), "60".to_string()],
                Some("40".into()),
                None
            ),
            Ok((Some(60), "cli".to_string()))
        );
        assert!(parse_repl_args(&[], Some("lots".into()), None).is_err());
    }
}
