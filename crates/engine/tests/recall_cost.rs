//! What one turn asks of the encoder, counted call by call.
//!
//! The hybrid recall path makes two kinds of model call through
//! `nscore::TextEncoder`: an embedding and a rerank. These tests run whole
//! turns against the SQLite store with an encoder that records every call it
//! is given, so a number here is a number of requests a deployment would send.

use nscore::*;
use nsengine::script::{EchoTool, ScriptedEmitter};
use nsengine::store::NoopConsolidator;
use nsengine::turn::{Engine, EngineConfig};
use nsmemory_sqlite::{Recall, SqliteStore};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// One request the store sent to the encoder.
#[derive(Debug, Clone, PartialEq)]
enum Call {
    /// `embed(.., "query")`: the question of a search.
    Query(String),
    /// `embed(.., "passage")`: the texts of stored rows.
    Passages(Vec<String>),
    Rerank {
        docs: Vec<String>,
    },
}

/// Records every call, and answers each after the delay it was given.
#[derive(Default)]
struct RecordingEncoder {
    calls: Arc<Mutex<Vec<Call>>>,
    embed_delay: Duration,
    rerank_delay: Duration,
}

impl RecordingEncoder {
    /// Eight buckets of token lengths and one constant, normalised: every
    /// text gets a direction, and two texts sharing words share some of it.
    fn vector(text: &str) -> Vec<f32> {
        let mut v = vec![0.0f32; 9];
        for t in nscore::query_tokens(text) {
            let bucket = t.bytes().map(usize::from).sum::<usize>() % 8;
            v[bucket] += 1.0;
        }
        v[8] = 0.25;
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        for x in v.iter_mut() {
            *x /= norm;
        }
        v
    }
}

#[async_trait::async_trait]
impl TextEncoder for RecordingEncoder {
    fn model(&self) -> &str {
        "recording"
    }
    async fn embed(&self, texts: &[String], kind: &str) -> Result<Vec<Vec<f32>>, StoreError> {
        self.calls.lock().unwrap().push(if kind == "query" {
            Call::Query(texts.join(" | "))
        } else {
            Call::Passages(texts.to_vec())
        });
        if !self.embed_delay.is_zero() {
            tokio::time::sleep(self.embed_delay).await;
        }
        Ok(texts.iter().map(|t| Self::vector(t)).collect())
    }
    async fn rerank(
        &self,
        query: &str,
        docs: &[String],
        k: usize,
    ) -> Result<Vec<(usize, f32)>, StoreError> {
        self.calls.lock().unwrap().push(Call::Rerank {
            docs: docs.to_vec(),
        });
        if !self.rerank_delay.is_zero() {
            tokio::time::sleep(self.rerank_delay).await;
        }
        let q = Self::vector(query);
        let mut scored: Vec<(usize, f32)> = docs
            .iter()
            .enumerate()
            .map(|(i, d)| (i, nscore::cosine(&q, &Self::vector(d)).unwrap_or(0.0)))
            .collect();
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(k);
        Ok(scored)
    }
}

/// The calls of one kind, counted.
#[derive(Debug, PartialEq)]
struct Counts {
    queries: usize,
    reranks: usize,
    /// Texts sent as passages, over all passage requests.
    passages: usize,
}

fn counts(calls: &Mutex<Vec<Call>>) -> Counts {
    let calls = calls.lock().unwrap();
    Counts {
        queries: calls.iter().filter(|c| matches!(c, Call::Query(_))).count(),
        reranks: calls
            .iter()
            .filter(|c| matches!(c, Call::Rerank { .. }))
            .count(),
        passages: calls
            .iter()
            .map(|c| match c {
                Call::Passages(texts) => texts.len(),
                _ => 0,
            })
            .sum(),
    }
}

struct NullChannel;
#[async_trait::async_trait]
impl Channel for NullChannel {
    async fn recv(&self) -> Result<Incoming, ChannelError> {
        Err(ChannelError::Closed)
    }
    async fn send(&self, _s: &SessionId, _t: &str) -> Result<(), ChannelError> {
        Ok(())
    }
}

struct FixedRouter(Tier);
impl nsengine::router::Router for FixedRouter {
    fn route(&self, _input: &nsengine::router::RouteInput<'_>) -> nsengine::router::Route {
        nsengine::router::Route {
            tier: self.0,
            cues: vec!["fixed".into()],
            tools: None,
        }
    }
}

/// An engine on `tier` whose emitter acts out `script` and then answers.
fn engine(store: Arc<SqliteStore>, tier: Tier, hybrid: bool, script: Vec<Proposal>) -> Engine {
    let mut b = HarnessBuilder::new();
    b.set_emitter(Box::new(ScriptedEmitter::new(script)));
    b.set_memory(store);
    b.set_channel(Box::new(NullChannel));
    b.set_consolidator(Box::new(NoopConsolidator));
    b.add_tool(Arc::new(EchoTool::new()));
    Engine::with_clock(
        b.build().unwrap(),
        EngineConfig {
            max_echo_ratio: 1.1,
            router: Some(Arc::new(FixedRouter(tier))),
            recall_hybrid: hybrid,
            ..EngineConfig::default()
        },
        Box::new(|| Timestamp(42)),
    )
}

fn echo(text: &str) -> Proposal {
    Proposal {
        rationale: "echo it".into(),
        action: "echo".into(),
        args: serde_json::json!({ "text": text }),
    }
}

const QUESTION: &str =
    "when is the report due, and what is the campaign budget for the office printer paper";

/// Six facts outside the pinned prefix, so every one a turn shows was chosen
/// by the search. The question shares a word with each of them.
const FACTS: &[(&str, &str)] = &[
    ("project.deadline", "the report is due on friday"),
    ("printer.model", "the office printer is a Brother laser"),
    ("campaign.budget", "2000 crowns for the whole campaign"),
    ("office.address", "the office is on Lidicka street in Brno"),
    (
        "meeting.day",
        "the team meeting is every tuesday in the office",
    ),
    ("supplier.paper", "paper comes from the Papyrus supplier"),
];

fn fact(key: &str, value: &str, at: u64) -> Fact {
    Fact {
        key: key.into(),
        value: serde_json::json!(value),
        confidence: 1.0,
        last_validated: Timestamp(at),
        prov: Provenance::Constant,
        valid_from: Timestamp(at),
        ..Default::default()
    }
}

/// A store holding [`FACTS`], each with its vector, and the encoder's call
/// log emptied afterwards: what is counted is what the turn sent.
async fn seeded(
    dir: &tempfile::TempDir,
    encoder: RecordingEncoder,
    recall: Recall,
) -> (Arc<SqliteStore>, Arc<Mutex<Vec<Call>>>) {
    let calls = encoder.calls.clone();
    let store = SqliteStore::open(&dir.path().join("recall.sqlite"))
        .unwrap()
        .with_encoder(Arc::new(encoder))
        .with_recall(recall);
    for (i, (key, value)) in FACTS.iter().enumerate() {
        store
            .put_fact(fact(key, value, i as u64 + 1))
            .await
            .unwrap();
    }
    assert_eq!(
        store.backfill_embeddings(100).await.unwrap(),
        0,
        "every fact has its vector before the turn starts"
    );
    calls.lock().unwrap().clear();
    (Arc::new(store), calls)
}

/// How many facts the turn showed: the reply path counts a use on each.
async fn used(store: &SqliteStore) -> usize {
    store
        .facts("global", "")
        .await
        .unwrap()
        .iter()
        .filter(|f| f.uses == 1)
        .count()
}

async fn ask(e: &Engine, session: &str) {
    e.run_turn(Incoming {
        session: SessionId(session.into()),
        text: QUESTION.into(),
    })
    .await
    .unwrap();
}

// ---------------------------------------------------------------------------
// What one turn sends. The probes of 2026-11-26 counted, on `cbdc0ae`:
// Task answering at once 2 questions, 2 reranks, 5 facts; Task with one tool
// call 3, 3, 5; Deep with history 3, 3, 5; Chat 0, 0, 5. RC-1 to RC-3 turned
// them into the tests below.
// ---------------------------------------------------------------------------

/// RC-2: a Task-tier turn searches once for every emitter call and once for
/// the reply, all with the same question over the same facts. The question
/// is embedded once and the list reranked once, answering at once (one
/// prompt, one reply) or after one tool call (two prompts, one reply).
#[tokio::test]
async fn a_task_turn_embeds_the_question_once_and_reranks_once() {
    for script in [vec![], vec![echo("hi")]] {
        let steps = script.len();
        let dir = tempfile::tempdir().unwrap();
        let (store, calls) = seeded(&dir, RecordingEncoder::default(), Recall::default()).await;
        ask(&engine(store.clone(), Tier::Task, true, script), "s").await;

        assert_eq!(used(&store).await, 5, "relevant_max facts were shown");
        assert_eq!(
            counts(&calls),
            Counts {
                queries: 1,
                reranks: 1,
                passages: 0
            },
            "{steps} tool calls: {:?}",
            calls.lock().unwrap()
        );
    }
}

/// RC-1: the reply counts a use on every fact it showed, and a write that
/// keeps the text sends no request, on any tier. On the Chat tier, with
/// `[recall] hybrid` on or off, the turn reaches the encoder not at all.
#[tokio::test]
async fn a_turn_embeds_no_fact_whose_text_did_not_change() {
    for (tier, hybrid) in [(Tier::Task, true), (Tier::Chat, true), (Tier::Chat, false)] {
        let dir = tempfile::tempdir().unwrap();
        let (store, calls) = seeded(&dir, RecordingEncoder::default(), Recall::default()).await;
        ask(&engine(store.clone(), tier, hybrid, vec![]), "s").await;

        assert_eq!(used(&store).await, 5, "{tier:?}, hybrid = {hybrid}");
        let c = counts(&calls);
        assert_eq!(
            c.passages,
            0,
            "{tier:?}, hybrid = {hybrid}: {:?}",
            calls.lock().unwrap()
        );
        if tier == Tier::Chat {
            assert_eq!(
                c,
                Counts {
                    queries: 0,
                    reranks: 0,
                    passages: 0
                },
                "hybrid = {hybrid}"
            );
        }
    }
}

/// RC-2: a Deep-tier turn with older turns to recall. The turn search asks
/// the facts' question once more; it is embedded once, and the two lists,
/// facts and turns, are reranked once each.
#[tokio::test]
async fn a_deep_turn_embeds_the_question_once() {
    let dir = tempfile::tempdir().unwrap();
    let (store, calls) = seeded(&dir, RecordingEncoder::default(), Recall::default()).await;
    let sid = SessionId("s".into());
    let mut log = EventLog::new(sid.clone());
    for t in 1..=8u32 {
        log.append(
            t,
            Timestamp(t as u64),
            EventKind::UserSaid {
                text: format!("turn {t}: the report for the campaign is due on friday"),
            },
        );
    }
    store.append(&sid, log.events()).await.unwrap();
    store.backfill_embeddings(100).await.unwrap();
    calls.lock().unwrap().clear();

    ask(&engine(store.clone(), Tier::Deep, true, vec![]), "s").await;
    assert_eq!(
        counts(&calls),
        Counts {
            queries: 1,
            reranks: 2,
            passages: 0
        },
        "{:?}",
        calls.lock().unwrap()
    );
}

/// RC-1, the store alone: a write that only moves a counter sends nothing.
#[tokio::test]
async fn a_write_that_only_moves_a_counter_sends_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (store, calls) = seeded(&dir, RecordingEncoder::default(), Recall::default()).await;
    let mut f = store.facts("global", "campaign.budget").await.unwrap()[0].clone();
    f.uses += 1;
    f.last_used = Timestamp(99);
    store.put_fact(f).await.unwrap();
    assert_eq!(*calls.lock().unwrap(), vec![]);
}

/// Not one of the three costs, found while reading: a live fact with no
/// vector is dropped from a hybrid result even when word matching ranks it
/// first, because the result is built from the rows that have a vector.
#[tokio::test]
async fn probe_a_fact_with_no_vector_is_missing_from_a_hybrid_result() {
    let dir = tempfile::tempdir().unwrap();
    let (seed, _) = seeded(&dir, RecordingEncoder::default(), Recall::default()).await;
    drop(seed);
    // Written with no encoder, as a fact is when the service is down.
    let bare = SqliteStore::open(&dir.path().join("recall.sqlite")).unwrap();
    bare.put_fact(fact(
        "report.reviewer",
        "the report is reviewed by Jana",
        50,
    ))
    .await
    .unwrap();
    drop(bare);
    let store = SqliteStore::open(&dir.path().join("recall.sqlite"))
        .unwrap()
        .with_encoder(Arc::new(RecordingEncoder::default()));

    let q = "who is the reviewer of the report";
    let lexical = store.search_facts("global", q, 5).await.unwrap();
    assert_eq!(lexical[0].key, "report.reviewer");
    let hybrid = store.search_facts_hybrid("global", q, 5).await.unwrap();
    assert!(
        !hybrid.iter().any(|f| f.key == "report.reviewer"),
        "{hybrid:?}"
    );
}

/// The store alone: the budget is read after the call returns, so a slow
/// call holds the search for as long as the call takes.
#[tokio::test]
async fn probe_a_slow_call_holds_the_search_past_its_budget() {
    let slow = Duration::from_millis(400);
    let recall = Recall {
        coarse_k: 10,
        rerank_budget_ms: 50,
    };
    for (name, encoder) in [
        (
            "embed",
            RecordingEncoder {
                embed_delay: slow,
                ..Default::default()
            },
        ),
        (
            "rerank",
            RecordingEncoder {
                rerank_delay: slow,
                ..Default::default()
            },
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        // Seeded with a fast encoder, then reopened with the slow one, so
        // the only slow calls are the search's own.
        let (seed, _) = seeded(&dir, RecordingEncoder::default(), recall).await;
        drop(seed);
        let calls = encoder.calls.clone();
        let store = SqliteStore::open(&dir.path().join("recall.sqlite"))
            .unwrap()
            .with_encoder(Arc::new(encoder))
            .with_recall(recall);

        let lexical = store.search_facts("global", QUESTION, 5).await.unwrap();
        let started = std::time::Instant::now();
        let hybrid = store
            .search_facts_hybrid("global", QUESTION, 5)
            .await
            .unwrap();
        let took = started.elapsed();

        assert_eq!(
            hybrid, lexical,
            "slow {name}: over budget is the word-match list"
        );
        assert!(
            took >= slow,
            "slow {name}: the search waited {took:?} on a 50 ms budget"
        );
        let c = counts(&calls);
        match name {
            "embed" => assert_eq!(
                (c.queries, c.reranks),
                (1, 0),
                "no rerank after a late embed"
            ),
            _ => assert_eq!((c.queries, c.reranks), (1, 1)),
        }
    }
}
