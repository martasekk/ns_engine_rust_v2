//! Rolling-summary model (M6 spec §5.1): a cheap model turns the verbatim
//! records that fell out of the window into a fixed-field summary. Plain
//! text, JSON out, temperature 0; output size is clamped by the engine.
use crate::client::{ApiError, OpenRouterClient};
use async_trait::async_trait;
use nscore::{SummarizeError, Summarizer, SummaryDraft, SummaryInput};

const SYSTEM: &str = "You maintain the running summary of a conversation for an assistant. \
Reply with JSON only: {\"topic\": \"<one sentence: what the user is trying to do>\", \
\"established\": [\"<short decisions or answers already given>\"], \"open\": [\"<pending \
questions or unconfirmed requests>\"]}. Keep every string short and plain. Do not repeat the \
known facts. Do not invent anything absent from the input.";

/// The fixed fields, as a JSON schema (M11 T0.5). The same three the prompt
/// asks for and [`nscore::SummaryDraft`] deserializes — written twice on
/// purpose: the prompt is what a model without structured output reads, this
/// is what a provider that has it enforces, and both must describe the same
/// object or the fence parser and the schema path would disagree.
pub fn summary_schema() -> serde_json::Value {
    serde_json::json!({
        "name": "summary",
        "strict": true,
        "schema": {
            "type": "object",
            "properties": {
                "topic": {"type": "string"},
                "established": {"type": "array", "items": {"type": "string"}},
                "open": {"type": "array", "items": {"type": "string"}},
            },
            "required": ["topic", "established", "open"],
            "additionalProperties": false,
        },
    })
}

/// The summarizer's output is three short lists; the engine clamps it anyway.
pub const MAX_TOKENS: u32 = 400;

/// The request shape this summarizer has always sent (M11 T0.2).
pub fn default_shape() -> crate::provider::RequestShape {
    crate::provider::RequestShape::pinned(MAX_TOKENS)
}

pub struct CloudSummarizer {
    client: OpenRouterClient,
    model: String,
    shape: crate::provider::RequestShape,
    structured_output: bool,
    guidelines: Vec<String>,
}

impl CloudSummarizer {
    pub fn new(client: OpenRouterClient, model: String) -> Self {
        Self {
            client,
            model,
            shape: default_shape(),
            structured_output: false,
            guidelines: Vec::new(),
        }
    }

    /// `[llm.summarizer]`'s shaping fields, already resolved against the
    /// model id (M11 T0.2). Unset everywhere is [`default_shape`].
    pub fn with_shape(mut self, shape: crate::provider::RequestShape) -> Self {
        self.shape = shape;
        self
    }

    /// Send `response_format: json_schema` (M11 T0.5). On only where the
    /// preset advertises it ([`crate::provider::Provider::structured_output`]),
    /// because a provider that does not know the field may 400 on it. Off is
    /// the request this summarizer has always sent.
    pub fn with_structured_output(mut self, on: bool) -> Self {
        self.structured_output = on;
        self
    }

    /// `[memory] summary_guidelines` (M9 T5.2) — hand-written lines appended
    /// to the system prompt after the fixed-field instructions.
    ///
    /// Ships empty, and empty means the prompt is byte-identical to what it
    /// was before M9. The knob exists so a graded summary failure has
    /// somewhere to go; `ns-app eval --ablate summary` cannot read it yet
    /// (no scripted fixture carries a summary), so nothing here is tuned on
    /// a number and the default is the conservative one.
    pub fn with_guidelines(mut self, guidelines: Vec<String>) -> Self {
        self.guidelines = guidelines;
        self
    }
}

/// The system prompt: the fixed-field instructions, then the guidelines.
///
/// Order is load-bearing. The JSON contract has to be the last thing a cheap
/// model cannot misread, so guidance about *what to write* follows the
/// instruction about *what shape to write it in*, never interleaves with it.
pub fn system_prompt(guidelines: &[String]) -> String {
    if guidelines.is_empty() {
        return SYSTEM.to_string();
    }
    let mut s = String::from(SYSTEM);
    s.push_str("\nGuidelines:\n");
    for g in guidelines {
        s.push_str(&format!("- {g}\n"));
    }
    s
}

fn strip_fence(s: &str) -> &str {
    let t = s.trim();
    let t = t
        .strip_prefix("```json")
        .or_else(|| t.strip_prefix("```"))
        .unwrap_or(t);
    let t = t.strip_suffix("```").unwrap_or(t);
    t.trim()
}

/// Known facts → previous summary → the verbatim records to fold in.
pub fn render_input(input: &SummaryInput<'_>) -> String {
    let mut s = String::new();
    if !input.facts.is_empty() {
        s.push_str("Known facts (do not repeat):\n");
        for f in input.facts {
            s.push_str(&format!("- {}\n", nscore::render_fact(f)));
        }
    }
    if let Some(prev) = input.previous {
        s.push_str("Previous summary:\n");
        s.push_str(&nscore::render_summary(prev));
        s.push('\n');
    }
    s.push_str("Turns to fold in:\n");
    s.push_str(&nscore::render_window(
        input.records,
        input.records.len(),
        input.caps,
    ));
    s.push_str("\nProduce the updated summary as JSON.");
    s
}

#[async_trait]
impl Summarizer for CloudSummarizer {
    async fn summarize(
        &self,
        input: SummaryInput<'_>,
    ) -> Result<Option<SummaryDraft>, SummarizeError> {
        let mut request = serde_json::json!({
            "model": self.model,
            "messages": [
                {"role": "system", "content": system_prompt(&self.guidelines)},
                {"role": "user", "content": render_input(&input)},
            ],
        });
        self.shape.apply(&mut request);
        if self.structured_output {
            // The prompt's JSON instruction stays: `response_format` is the
            // belt, the instruction the braces, and `strip_fence` below
            // still parses whatever comes back.
            request["response_format"] = serde_json::json!({
                "type": "json_schema",
                "json_schema": summary_schema(),
            });
        }
        let body = self
            .client
            .chat_into(request, input.usage.as_deref())
            .await
            .map_err(|e| match e {
                ApiError::Transport(d) => SummarizeError::Transport(d),
                ApiError::Status { status, detail } => {
                    SummarizeError::Transport(format!("status {status}: {detail}"))
                }
            })?;
        let content = body["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or("")
            .to_string();
        let draft: SummaryDraft = serde_json::from_str(strip_fence(&content))
            .map_err(|e| SummarizeError::Malformed(format!("{e}: {content}")))?;
        if draft.topic.trim().is_empty() {
            return Err(SummarizeError::Malformed(format!("empty topic: {content}")));
        }
        Ok(Some(draft))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::{HttpResponse, MockTransport};
    use nscore::{Caps, SessionSummary, Trust, TurnRecord};

    /// M9 T5.2. Guidelines follow the fixed-field instructions; the JSON
    /// contract stays the last thing a cheap model reads about *shape*.
    #[test]
    fn guidelines_render_after_the_fixed_field_instructions() {
        let p = system_prompt(&[
            "Name the person who asked for something in `open`.".to_string(),
            "Keep `topic` to one clause.".to_string(),
        ]);
        assert!(p.starts_with(SYSTEM), "the fixed fields come first");
        assert_eq!(
            &p[SYSTEM.len()..],
            "\nGuidelines:\n\
             - Name the person who asked for something in `open`.\n\
             - Keep `topic` to one clause.\n"
        );
        // Order is the config's, not sorted.
        assert!(p.find("Name the person").unwrap() < p.find("Keep `topic`").unwrap());
    }

    /// The knob ships empty, and empty has to cost nothing — not a newline,
    /// not a header. Anything else would be an unmeasured prompt change on
    /// every summary this box has ever written.
    #[test]
    fn no_guidelines_leave_the_prompt_byte_identical() {
        assert_eq!(system_prompt(&[]), SYSTEM);
        assert_eq!(
            system_prompt(&Vec::<String>::new()).as_bytes(),
            SYSTEM.as_bytes()
        );
    }

    fn records() -> Vec<TurnRecord> {
        vec![
            TurnRecord {
                turn: 1,
                user: "hi".into(),
                did: vec![],
                reply: "Hello!".into(),
                trust: Trust::User,
            },
            TurnRecord {
                turn: 2,
                user: "what time is it".into(),
                did: vec!["get_time -> ok: 10:41 UTC".into()],
                reply: "10:41 UTC.".into(),
                trust: Trust::System,
            },
        ]
    }

    fn reply(content: &str) -> HttpResponse {
        HttpResponse {
            status: 200,
            body: serde_json::json!({"choices": [{"message": {"content": content}}]}),
        }
    }

    #[tokio::test]
    async fn renders_blocks_and_parses_fenced_json() {
        let mock = MockTransport::new(vec![Ok(reply(
            "```json\n{\"topic\": \"Asking the time.\", \"established\": [\"time given\"], \"open\": []}\n```",
        ))]);
        let s = CloudSummarizer::new(
            OpenRouterClient::new(mock.clone(), "k".into()).with_retry(1, 1),
            "m".into(),
        );
        let prev = SessionSummary {
            through_turn: 0,
            topic: "greeting".into(),
            established: vec![],
            open: vec![],
            trust: Trust::User,
            rebuilt_from: 1,
        };
        let facts = vec![nscore::Fact {
            key: "user.name".into(),
            value: serde_json::json!("Martin"),
            ..Default::default()
        }
        .into()];
        let recs = records();
        let draft = s
            .summarize(nscore::SummaryInput {
                previous: Some(&prev),
                records: &recs,
                caps: &Caps::default(),
                facts: &facts,
                usage: None,
            })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(draft.topic, "Asking the time.");
        assert_eq!(draft.established, vec!["time given".to_string()]);
        let req = mock.requests.lock().unwrap()[0].clone();
        assert_eq!(req["temperature"], 0);
        let user = req["messages"][1]["content"].as_str().unwrap();
        let at = |n: &str| user.find(n).unwrap_or_else(|| panic!("{n}: {user}"));
        assert!(at("Known facts (do not repeat):\n- user.name") < at("Previous summary:\n"));
        assert!(at("Previous summary:") < at("Turns to fold in:\n[t1] user: hi"));
        assert!(user.contains("[t2] user: what time is it"));
    }

    fn input_of<'a>(recs: &'a [TurnRecord], caps: &'a Caps) -> nscore::SummaryInput<'a> {
        nscore::SummaryInput {
            previous: None,
            records: recs,
            caps,
            facts: &[],
            usage: None,
        }
    }

    /// M11 T0.2. The summarizer takes the same shape as the other two roles:
    /// `sampling = "none"` removes the key, effort rides as a block, and the
    /// role's `max_tokens` replaces its constant.
    #[tokio::test]
    async fn the_summarizer_takes_the_same_shape() {
        let mock = MockTransport::new(vec![Ok(reply(
            "{\"topic\": \"t\", \"established\": [], \"open\": []}",
        ))]);
        let (shape, coercion) = crate::provider::RoleShaping {
            reasoning: Some("low".into()),
            max_tokens: Some(1024),
            ..Default::default()
        }
        .resolve(
            "summarizer",
            "anthropic/claude-sonnet-5",
            crate::summarizer::default_shape(),
        );
        assert!(coercion.is_some());
        let s = CloudSummarizer::new(
            OpenRouterClient::new(mock.clone(), "k".into()).with_retry(1, 1),
            "anthropic/claude-sonnet-5".into(),
        )
        .with_shape(shape);
        let recs = records();
        let caps = Caps::default();
        s.summarize(input_of(&recs, &caps)).await.unwrap().unwrap();
        let req = mock.requests.lock().unwrap()[0].clone();
        assert!(req.get("temperature").is_none(), "{req}");
        assert_eq!(req["reasoning"], serde_json::json!({"effort": "low"}));
        assert_eq!(req["max_tokens"], 1024);
    }

    /// M11 T0.5. The field goes out where the preset says the endpoint knows
    /// it, and nowhere else — an unknown top-level field is a 400 on several
    /// of the presets this workspace ships.
    #[tokio::test]
    async fn structured_output_is_sent_only_where_the_preset_advertises_it() {
        async fn request_with(on: bool) -> serde_json::Value {
            let mock = MockTransport::new(vec![Ok(reply(
                "{\"topic\": \"Asking the time.\", \"established\": [\"time given\"], \"open\": []}",
            ))]);
            let s = CloudSummarizer::new(
                OpenRouterClient::new(mock.clone(), "k".into()).with_retry(1, 1),
                "m".into(),
            )
            .with_structured_output(on);
            let recs = records();
            let caps = Caps::default();
            s.summarize(input_of(&recs, &caps)).await.unwrap().unwrap();
            let req = mock.requests.lock().unwrap()[0].clone();
            req
        }
        // Default off: the request is the one that has always gone out.
        let off = request_with(false).await;
        assert!(off.get("response_format").is_none(), "{off}");
        assert_eq!(off["temperature"], 0);
        assert_eq!(off["max_tokens"], 400);

        let on = request_with(true).await;
        assert_eq!(on["response_format"]["type"], "json_schema");
        let js = &on["response_format"]["json_schema"];
        assert_eq!(js["name"], "summary");
        assert_eq!(js["strict"], true);
        assert_eq!(js["schema"]["additionalProperties"], false);
        assert_eq!(
            js["schema"]["required"],
            serde_json::json!(["topic", "established", "open"])
        );
        assert_eq!(js["schema"]["properties"]["topic"]["type"], "string");
        assert_eq!(
            js["schema"]["properties"]["open"]["items"]["type"],
            "string"
        );
        // The prompt still carries the instruction: the schema is the belt,
        // not a replacement for the braces.
        let system = on["messages"][0]["content"].as_str().unwrap();
        assert!(system.contains("Reply with JSON only"), "{system}");
        // Only the two presets that advertise it get it.
        for p in crate::provider::PROVIDERS {
            assert_eq!(
                p.structured_output,
                p.name == "openrouter" || p.name == "openai"
            );
        }
    }

    /// M11 T0.5's exit criterion: identical `SummaryDraft` from both paths —
    /// a bare object (what the schema path returns) and a fenced one (what a
    /// prompt-forced model returns).
    #[tokio::test]
    async fn a_fenced_reply_still_parses() {
        async fn draft_of(content: &str, structured: bool) -> SummaryDraft {
            let mock = MockTransport::new(vec![Ok(reply(content))]);
            let s = CloudSummarizer::new(
                OpenRouterClient::new(mock, "k".into()).with_retry(1, 1),
                "m".into(),
            )
            .with_structured_output(structured);
            let recs = records();
            let caps = Caps::default();
            s.summarize(input_of(&recs, &caps)).await.unwrap().unwrap()
        }
        let bare = draft_of(
            "{\"topic\": \"Asking the time.\", \"established\": [\"time given\"], \"open\": []}",
            true,
        )
        .await;
        let fenced = draft_of(
            "```json\n{\"topic\": \"Asking the time.\", \"established\": [\"time given\"], \
             \"open\": []}\n```",
            true,
        )
        .await;
        assert_eq!(bare.topic, fenced.topic);
        assert_eq!(bare.established, fenced.established);
        assert_eq!(bare.open, fenced.open);
        assert_eq!(fenced.topic, "Asking the time.");
    }

    /// See the emitter's twin: the input's sink takes the call's cost, the
    /// client's own stays empty.
    #[tokio::test]
    async fn the_calls_cost_lands_in_the_inputs_sink_not_the_clients() {
        let mock = MockTransport::new(vec![Ok(reply(
            "{\"topic\": \"Asking the time.\", \"established\": [], \"open\": []}",
        ))]);
        let own = std::sync::Arc::new(nscore::UsageSink::new());
        let s = CloudSummarizer::new(
            OpenRouterClient::new(mock, "k".into()).with_usage_sink(own.clone(), "summarizer"),
            "m".into(),
        );
        let recs = records();
        let call = std::sync::Arc::new(nscore::UsageSink::new());
        s.summarize(nscore::SummaryInput {
            previous: None,
            records: &recs,
            caps: &Caps::default(),
            facts: &[],
            usage: Some(call.clone()),
        })
        .await
        .unwrap();
        let recorded = call.drain();
        assert_eq!(recorded.len(), 1, "the call's sink took the cost");
        assert_eq!(recorded[0].role, "summarizer");
        assert!(own.drain().is_empty(), "the client's own sink was not used");
    }

    #[tokio::test]
    async fn bad_json_and_empty_topic_are_malformed_and_status_is_transport() {
        let mock = MockTransport::new(vec![
            Ok(reply("not json")),
            Ok(reply("{\"topic\": \"  \"}")),
            Ok(HttpResponse {
                status: 429,
                body: serde_json::json!({"message": "Rate limit exceeded"}),
            }),
        ]);
        let s = CloudSummarizer::new(
            OpenRouterClient::new(mock, "k".into()).with_retry(1, 1),
            "m".into(),
        );
        let recs = records();
        let caps = Caps::default();
        let facts: Vec<nscore::FactView> = vec![];
        let input = || nscore::SummaryInput {
            previous: None,
            records: &recs,
            caps: &caps,
            facts: &facts,
            usage: None,
        };
        assert!(matches!(
            s.summarize(input()).await,
            Err(SummarizeError::Malformed(_))
        ));
        assert!(matches!(
            s.summarize(input()).await,
            Err(SummarizeError::Malformed(_))
        ));
        assert!(matches!(
            s.summarize(input()).await,
            Err(SummarizeError::Transport(_))
        ));
    }
}
