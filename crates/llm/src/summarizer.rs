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

pub struct CloudSummarizer {
    client: OpenRouterClient,
    model: String,
    max_tokens: u32,
}

impl CloudSummarizer {
    pub fn new(client: OpenRouterClient, model: String) -> Self {
        Self {
            client,
            model,
            max_tokens: 400,
        }
    }
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
        let request = serde_json::json!({
            "model": self.model,
            "max_tokens": self.max_tokens,
            "temperature": 0,
            "messages": [
                {"role": "system", "content": SYSTEM},
                {"role": "user", "content": render_input(&input)},
            ],
        });
        let body = self.client.chat(request).await.map_err(|e| match e {
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
