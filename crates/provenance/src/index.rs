use nscore::{Event, EventId, EventKind, ToolOutcome, Trust};

/// One JSON leaf of one tool output, addressable for CopiedOutput claims.
#[derive(Debug, Clone, PartialEq)]
pub struct OutputLeaf {
    pub call: EventId,
    pub path: String,
    pub value: serde_json::Value,
    pub trust: Trust,
}

/// Everything this session has legitimately seen, rebuilt from the log.
pub struct ValueIndex {
    pub user_texts: Vec<(u32, String)>,
    pub output_leaves: Vec<OutputLeaf>,
}

fn walk_leaves(
    call: EventId,
    trust: Trust,
    path: &str,
    v: &serde_json::Value,
    out: &mut Vec<OutputLeaf>,
) {
    match v {
        serde_json::Value::Object(map) => {
            for (k, child) in map {
                walk_leaves(call, trust, &format!("{path}.{k}"), child, out);
            }
        }
        serde_json::Value::Array(items) => {
            for (i, child) in items.iter().enumerate() {
                walk_leaves(call, trust, &format!("{path}[{i}]"), child, out);
            }
        }
        serde_json::Value::Null => {}
        leaf => out.push(OutputLeaf { call, path: path.to_string(), value: leaf.clone(), trust }),
    }
}

impl ValueIndex {
    pub fn from_events(events: &[Event]) -> ValueIndex {
        let mut user_texts = Vec::new();
        let mut output_leaves = Vec::new();
        for e in events {
            match &e.kind {
                EventKind::UserSaid { text } => user_texts.push((e.turn, text.clone())),
                EventKind::ToolReturned { call, outcome: ToolOutcome::Ok { output } } => {
                    output_leaves.push(OutputLeaf {
                        call: *call,
                        path: "$".into(),
                        value: serde_json::Value::String(output.summary.clone()),
                        trust: output.trust,
                    });
                    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&output.summary)
                    {
                        if parsed.is_object() || parsed.is_array() {
                            walk_leaves(*call, output.trust, "$", &parsed, &mut output_leaves);
                        }
                    }
                }
                _ => {}
            }
        }
        ValueIndex { user_texts, output_leaves }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nscore::*;

    fn log_with_tool_output(summary: &str, trust: Trust) -> EventLog {
        let mut log = EventLog::new(SessionId("s".into()));
        log.append(1, Timestamp(1), EventKind::UserSaid { text: "check stock for widget".into() });
        let call = log
            .append(
                1,
                Timestamp(2),
                EventKind::ToolCalled { action: "check_stock".into(), args: vec![] },
            )
            .id;
        log.append(
            1,
            Timestamp(3),
            EventKind::ToolReturned {
                call,
                outcome: ToolOutcome::Ok {
                    output: ToolOutput { summary: summary.into(), artifact: None, trust },
                },
            },
        );
        log
    }

    #[test]
    fn indexes_user_turns() {
        let log = log_with_tool_output("ok", Trust::System);
        let idx = ValueIndex::from_events(log.events());
        assert_eq!(idx.user_texts, vec![(1, "check stock for widget".to_string())]);
    }

    #[test]
    fn json_summary_yields_addressable_leaves() {
        let log = log_with_tool_output(r#"{"in_stock":true,"items":[{"id":42}]}"#, Trust::External);
        let idx = ValueIndex::from_events(log.events());
        let paths: Vec<(&str, &serde_json::Value)> =
            idx.output_leaves.iter().map(|l| (l.path.as_str(), &l.value)).collect();
        assert!(paths.iter().any(|(p, v)| *p == "$.in_stock" && **v == serde_json::json!(true)));
        assert!(paths.iter().any(|(p, v)| *p == "$.items[0].id" && **v == serde_json::json!(42)));
        assert!(idx.output_leaves.iter().all(|l| l.trust == Trust::External));
    }

    #[test]
    fn non_json_summary_yields_only_root_leaf() {
        let log = log_with_tool_output("current unix time (ms): 99", Trust::System);
        let idx = ValueIndex::from_events(log.events());
        assert_eq!(idx.output_leaves.len(), 1);
        assert_eq!(idx.output_leaves[0].path, "$");
        assert_eq!(idx.output_leaves[0].value, serde_json::json!("current unix time (ms): 99"));
    }

    #[test]
    fn err_outcomes_contribute_nothing() {
        let mut log = EventLog::new(SessionId("s".into()));
        let call = log
            .append(1, Timestamp(1), EventKind::ToolCalled { action: "x".into(), args: vec![] })
            .id;
        log.append(
            1,
            Timestamp(2),
            EventKind::ToolReturned {
                call,
                outcome: ToolOutcome::Err { kind: "network".into(), detail: "down".into() },
            },
        );
        let idx = ValueIndex::from_events(log.events());
        assert!(idx.output_leaves.is_empty());
    }
}
