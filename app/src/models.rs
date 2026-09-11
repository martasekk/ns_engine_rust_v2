//! The local model service, as far as startup is concerned (M8 T0.3).
//!
//! `~/models` (nsmodels) serves embeddings, a cross-encoder reranker, OCR and
//! image-text ranking on loopback. This module does one thing: say at startup
//! whether it is answering, so a lane configured to use it does not discover
//! that fact one signal at a time, hours later, in a ledger full of
//! `Unavailable`.
//!
//! What it deliberately is not: a client. The scorer that Phase 2 builds
//! lives in `ns-evolution`, beside the evaluator it feeds, because that is
//! where the rule about what its output may mean lives — a signal, never a
//! decision, and a gate only on a measured true-positive rate.
//!
//! Written against `TcpStream` rather than the `reqwest` already in the tree
//! because a probe should fail the way a probe fails. A pooled, redirect-
//! following, TLS-capable client aimed at loopback answers a slightly
//! different question than "is there something on that port speaking HTTP
//! right now", and the answer this prints has to be the plain one.

use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// `host:port` and the path prefix from a base URL, or `None` if it is not
/// one this probe can dial.
///
/// Only `http://`: the service binds loopback and nsmodels serves no TLS, so
/// an `https://` base URL here is a config mistake worth being told about
/// rather than a scheme to support.
fn dial_target(base_url: &str) -> Option<String> {
    let rest = base_url.strip_prefix("http://")?;
    let authority = rest.split('/').next()?.trim();
    if authority.is_empty() {
        return None;
    }
    // A bare host means the HTTP default, the same as any other client.
    if authority.contains(':') {
        Some(authority.to_string())
    } else {
        Some(format!("{authority}:80"))
    }
}

/// Ask `/health` and return what it said, or why it could not be asked.
///
/// The response body is returned rather than a bool: nsmodels answers with
/// the list of models it actually loaded, and "reachable but loaded nothing
/// the lane needs" is a different problem from "not running", worth telling
/// them apart on the line that reports it.
async fn health(base_url: &str, timeout_ms: u64) -> Result<String, String> {
    let addr = dial_target(base_url)
        .ok_or_else(|| format!("{base_url:?} is not an http:// URL this probe can dial"))?;
    let deadline = std::time::Duration::from_millis(timeout_ms);

    let work = async {
        let mut stream = tokio::net::TcpStream::connect(&addr)
            .await
            .map_err(|e| e.to_string())?;
        let host = addr.clone();
        let req = format!(
            "GET /health HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\nAccept: application/json\r\n\r\n"
        );
        stream
            .write_all(req.as_bytes())
            .await
            .map_err(|e| e.to_string())?;
        let mut body = Vec::new();
        stream
            .read_to_end(&mut body)
            .await
            .map_err(|e| e.to_string())?;
        Ok::<String, String>(String::from_utf8_lossy(&body).into_owned())
    };

    let response = tokio::time::timeout(deadline, work)
        .await
        .map_err(|_| format!("no answer within {timeout_ms} ms"))??;

    let mut parts = response.splitn(2, "\r\n\r\n");
    let head = parts.next().unwrap_or_default();
    let body = parts.next().unwrap_or_default().trim().to_string();
    let status = head.lines().next().unwrap_or_default();
    if !status.contains(" 200") {
        return Err(format!("answered {:?}", status.trim()));
    }
    Ok(body)
}

/// One line at startup when `[models] enabled = true`, and silence otherwise.
///
/// Unreachable is a warning and never an exit, for the reason the pointer's
/// dial gives: the lane this feeds degrades to the signals it can compute
/// without a model, and a service that is not running must not take the
/// conversation down with it. It must, however, be said — the alternative is
/// a pass that quietly grades nothing and reports that it graded nothing in a
/// column no one reads.
pub async fn announce(cfg: &crate::config::ModelsSection) {
    if !cfg.enabled {
        return;
    }
    match health(&cfg.base_url, cfg.timeout_ms).await {
        Ok(body) => println!("models: {} answering — {body}", cfg.base_url),
        Err(e) => {
            eprintln!("models: no service on {} ({e});", cfg.base_url);
            eprintln!(
                "models: the evaluation lane will fall back to its no-model signals. \
                 Start it with `.venv/Scripts/python.exe -m nsmodels serve`."
            );
        }
    }
}

/// Whether the service is answering right now.
///
/// For the one caller that has to *decide* rather than degrade: the
/// paraphrase arm adds its hybrid arm only when there is a service behind it,
/// because an arm measured against a dead service is the bm25 arm wearing a
/// different label, and printing that number as "hybrid" would be the worst
/// outcome this whole phase could produce.
pub async fn reachable(cfg: &crate::config::ModelsSection) -> bool {
    health(&cfg.base_url, cfg.timeout_ms.max(2000))
        .await
        .is_ok()
}

/// The encoder the store runs its hybrid recall through (M8 T3.1/T3.2), or
/// `None` when `[models]` is off.
///
/// `LocalEvaluator` and not a new client: it already carries the retry table,
/// the refusal classification that survives a Czech Windows, and the rule
/// that two unreachable calls take the lane out. A second client beside it
/// would be a second copy of all three, and the one that drifted would be
/// the one on the hot path.
///
/// It is a *different instance* from the one the evolution pass grades with,
/// and deliberately: "disabled after two unreachable calls" is per instance,
/// and a grading pass that gave up on a dead service must not also silence
/// the recall path for the rest of the process — nor the other way round.
pub fn encoder(
    models: &crate::config::ModelsSection,
    recall: &crate::config::RecallSection,
) -> Option<std::sync::Arc<dyn nscore::TextEncoder>> {
    if !models.enabled {
        return None;
    }
    Some(std::sync::Arc::new(
        nsevolution::local::LocalEvaluator::new(
            nsevolution::local::LocalConfig {
                base_url: models.base_url.clone(),
                timeout_ms: models.timeout_ms,
                reask_cosine: models.reask_cosine,
                relevance_cut: models.relevance_cut,
                embed_model: recall.embed_model.clone(),
            },
            nsevolution::evaluate::SymbolicEvaluator::default(),
        ),
    ))
}

/// `[recall]` as the store reads it.
pub fn recall_tuning(recall: &crate::config::RecallSection) -> nsmemory_sqlite::Recall {
    nsmemory_sqlite::Recall {
        coarse_k: recall.coarse_k,
        rerank_budget_ms: recall.rerank_budget_ms,
    }
}

/// The store with everything `[memory]` and `[recall]` have to say about it.
///
/// One function because five call sites open this database and four of them
/// were one `.with_activation` chain that a fifth knob would have made a
/// fifth place to forget.
pub fn store(cfg: &crate::config::AppConfig) -> nsmemory_sqlite::SqliteStore {
    let mut store = nsmemory_sqlite::SqliteStore::open(std::path::Path::new(&cfg.store.path))
        .expect("open sqlite store")
        // M9 T3.1/T3.2. The composition root is where the `[memory]` knobs
        // meet the store; `EngineConfig` carries the same two numbers for
        // anything that reads the config as one object. Both default to
        // today's ranking.
        .with_activation(
            cfg.memory.activation_weight,
            cfg.memory.activation_half_life_days,
        )
        .with_recall(recall_tuning(&cfg.recall));
    if let Some(enc) = encoder(&cfg.models, &cfg.recall) {
        store = store.with_encoder(enc);
    }
    store
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Off by default, and off means no encoder at all — which is what makes
    /// `[recall] hybrid = true` inert on a box with no service configured.
    #[test]
    fn no_encoder_without_the_models_section() {
        let models = crate::config::ModelsSection::default();
        let recall = crate::config::RecallSection::default();
        assert!(encoder(&models, &recall).is_none());
        let on = crate::config::ModelsSection {
            enabled: true,
            ..Default::default()
        };
        let enc = encoder(&on, &recall).expect("an encoder when [models] is on");
        assert_eq!(enc.model(), "bge-m3", "`--model quality` is bge-m3");
    }

    /// The shapes a person actually writes, and the one they should be told
    /// about rather than silently dialled on the wrong port.
    #[test]
    fn a_base_url_becomes_a_dial_target() {
        assert_eq!(
            dial_target("http://127.0.0.1:7374").as_deref(),
            Some("127.0.0.1:7374")
        );
        // A trailing path is legal in a base URL and is not part of the address.
        assert_eq!(
            dial_target("http://127.0.0.1:7374/").as_deref(),
            Some("127.0.0.1:7374")
        );
        assert_eq!(
            dial_target("http://localhost").as_deref(),
            Some("localhost:80")
        );
        assert_eq!(dial_target("https://127.0.0.1:7374"), None);
        assert_eq!(dial_target("127.0.0.1:7374"), None);
        assert_eq!(dial_target("http://"), None);
    }

    /// The case that decides whether this is safe to leave on: a port with
    /// nothing behind it must come back as an error, promptly, rather than
    /// hold startup open for a service that is not there.
    #[tokio::test]
    async fn a_dead_port_is_an_error_and_not_a_wait() {
        // Port 1 on loopback: privileged, and nothing this box runs binds it.
        let started = std::time::Instant::now();
        let out = health("http://127.0.0.1:1", 500).await;
        assert!(out.is_err(), "expected a refusal, got {out:?}");
        assert!(
            started.elapsed() < std::time::Duration::from_millis(2000),
            "the probe outlived its own timeout"
        );
    }

    /// Against the real service, when there is one. Ignored by default: the
    /// suite must not depend on a Python process being up on this machine,
    /// and a test that passes only where someone remembered to start
    /// something is worse than no test. Run it with
    /// `cargo test -p ns-app -- --ignored the_real_service` after
    /// `.venv/Scripts/python.exe -m nsmodels serve`.
    #[tokio::test]
    #[ignore = "needs nsmodels serve on 127.0.0.1:7374"]
    async fn the_real_service_answers_health() {
        let body = health("http://127.0.0.1:7374", 2000)
            .await
            .expect("nsmodels /health");
        assert!(body.contains("\"ok\""), "unexpected body: {body}");
        assert!(body.contains("embed"), "no embedder loaded: {body}");
    }
}
