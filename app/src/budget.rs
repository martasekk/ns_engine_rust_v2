//! `ns-app budget <session_id>` — what a session spent (M7 plan T0.2).
//!
//! Two resources, and the one this deployment runs out of first is not
//! tokens. `openrouter/free` allows fifty requests a day, and a desktop turn
//! can spend up to twelve emitter iterations plus a replier and a summarizer
//! — three or four such turns and the day is over. So `reqs` sums
//! [`nscore::Usage::attempts`] rather than counting `ModelCall` events: a call
//! that succeeded on its third attempt was one decision and three requests,
//! and it is the requests that run out.
//!
//! Sessions recorded before `ModelCall` existed (T0.1) hold none of this. The
//! fallback reconstructs the material from the log the way `render_echo`
//! does and labels every number an estimate, which is what gives the plan its
//! baseline row (T0.3) for the `cli` session without spending a request.

use nscore::{Caps, ContextManifest, Event, EventKind, Usage};

/// Per-turn arithmetic over one turn's `ModelCall` events.
#[derive(Default)]
struct Measured {
    turn: u32,
    /// Provider calls — decisions. `requests` is the resource.
    calls: usize,
    requests: u32,
    emitter_requests: u32,
    replier_requests: u32,
    summarizer_requests: u32,
    prompt: u64,
    completion: u64,
    /// Prompt tokens the provider served from its cache, and the prompt
    /// tokens it was measured over. Only calls with the provider's own
    /// numbers count on either side: an estimated call has no cache split
    /// to report, and folding its chars/4 prompt into the denominator would
    /// report a real cache hit as a smaller one.
    cached: u64,
    cached_prompt: u64,
    peak_prompt: u32,
    /// At least one call's counts came from chars/4, not the provider.
    estimated: bool,
    schema_tokens: u64,
    /// Calls that were *sent* schemas, and the prompt tokens they spent. The
    /// only denominator that makes `schema_tokens` a fraction of anything:
    /// the replier and the summarizer are sent no tools, so folding their
    /// prompts in would report the emitter's schema cost as smaller than it
    /// is.
    schema_calls: usize,
    schema_prompt: u64,
    /// Summed over the turn's calls on purpose. The emitter rebuilds the
    /// trace on every iteration and sends it again, so a 14k-character
    /// `pointer_ui_read` at iteration 2 is charged once more per remaining
    /// iteration — that repetition is the cost phase 1 exists to remove, and
    /// a per-turn maximum would hide it.
    trace_chars: u64,
    clipped_chars: u64,
    tool_calls: usize,
    /// Estimated tokens of the candidate cache prefix, one entry per call
    /// that carried block sizes (M9 T0.5). The prefix is the run of blocks
    /// before the breakpoint a role could set: facts + summary + window for
    /// the emitter, whose trace is what changes between iterations, and
    /// persona + facts + summary for the replier, whose window is the turn
    /// it is answering. Kept per call rather than summed because the number
    /// T2.3 reads is a *level* against the provider's 1,024-token floor, and
    /// a sum over four calls clears a floor no single call does.
    emitter_prefix: Vec<u32>,
    replier_prefix: Vec<u32>,
}

impl Measured {
    fn add_call(&mut self, usage: &Usage, manifest: &ContextManifest, persona_chars: usize) {
        self.calls += 1;
        self.requests += usage.attempts;
        match usage.role.as_str() {
            "emitter" => self.emitter_requests += usage.attempts,
            "replier" => self.replier_requests += usage.attempts,
            "summarizer" => self.summarizer_requests += usage.attempts,
            // A role this command has not heard of still spent requests. It
            // stays in `requests`, where the total keeps adding up, and is
            // simply absent from the by-role split.
            _ => {}
        }
        self.prompt += u64::from(usage.prompt_tokens);
        self.completion += u64::from(usage.completion_tokens);
        if !usage.estimated {
            self.cached += u64::from(usage.cached_tokens);
            self.cached_prompt += u64::from(usage.prompt_tokens);
        }
        self.peak_prompt = self.peak_prompt.max(usage.prompt_tokens);
        self.estimated |= usage.estimated;
        self.schema_tokens += u64::from(usage.tools_tokens);
        if manifest.tools > 0 {
            self.schema_calls += 1;
            self.schema_prompt += u64::from(usage.prompt_tokens);
        }
        self.trace_chars += manifest.trace_chars as u64;
        self.clipped_chars += manifest.clipped_chars as u64;
        // All three zero means the call never recorded them: a summarizer,
        // which is sent no stable blocks, or a log written before M9 added
        // the sizes. Neither has a prefix to estimate, and folding a zero in
        // would drag the median under the floor for free.
        if manifest.facts_chars != 0 || manifest.summary_chars != 0 || manifest.window_chars != 0 {
            let stable = manifest.facts_chars + manifest.summary_chars;
            match usage.role.as_str() {
                "emitter" => self
                    .emitter_prefix
                    .push(nscore::estimate_tokens(stable + manifest.window_chars)),
                "replier" => self
                    .replier_prefix
                    .push(nscore::estimate_tokens(persona_chars + stable)),
                _ => {}
            }
        }
    }

    fn merge(&mut self, other: &Measured) {
        self.calls += other.calls;
        self.requests += other.requests;
        self.emitter_requests += other.emitter_requests;
        self.replier_requests += other.replier_requests;
        self.summarizer_requests += other.summarizer_requests;
        self.prompt += other.prompt;
        self.completion += other.completion;
        self.cached += other.cached;
        self.cached_prompt += other.cached_prompt;
        self.peak_prompt = self.peak_prompt.max(other.peak_prompt);
        self.estimated |= other.estimated;
        self.schema_tokens += other.schema_tokens;
        self.schema_calls += other.schema_calls;
        self.schema_prompt += other.schema_prompt;
        self.trace_chars += other.trace_chars;
        self.clipped_chars += other.clipped_chars;
        self.tool_calls += other.tool_calls;
        self.emitter_prefix.extend_from_slice(&other.emitter_prefix);
        self.replier_prefix.extend_from_slice(&other.replier_prefix);
    }

    fn cells(&self, label: String) -> Vec<String> {
        vec![
            label,
            self.requests.to_string(),
            self.calls.to_string(),
            format!(
                "{}/{}/{}",
                self.emitter_requests, self.replier_requests, self.summarizer_requests
            ),
            self.prompt.to_string(),
            self.cached.to_string(),
            self.completion.to_string(),
            self.peak_prompt.to_string(),
            self.schema_tokens.to_string(),
            percent(self.schema_tokens, self.schema_prompt),
            self.trace_chars.to_string(),
            self.clipped_chars.to_string(),
            self.tool_calls.to_string(),
        ]
    }
}

/// What the log still shows of a turn recorded before `ModelCall` existed.
#[derive(Default)]
struct Reconstructed {
    turn: u32,
    window_chars: usize,
    summary_chars: usize,
    trace_chars: usize,
    /// The same trace after the phase-1 cap and fold, from the engine's own
    /// renderer rather than a copy of it. On a log recorded before phase 1
    /// this is the two numbers side by side: what the turn did cost, and
    /// what it would cost now.
    sent_trace_chars: usize,
    tool_calls: usize,
}

impl Reconstructed {
    fn chars(&self) -> usize {
        self.window_chars + self.summary_chars + self.trace_chars
    }

    fn cells(&self, label: String) -> Vec<String> {
        vec![
            label,
            self.window_chars.to_string(),
            self.summary_chars.to_string(),
            self.trace_chars.to_string(),
            self.sent_trace_chars.to_string(),
            self.chars().to_string(),
            nscore::estimate_tokens(self.chars()).to_string(),
            self.tool_calls.to_string(),
        ]
    }
}

const MEASURED_COLUMNS: &[(&str, usize)] = &[
    ("turn", 7),
    ("reqs", 6),
    ("calls", 7),
    ("e/r/s", 9),
    ("prompt", 9),
    ("cached", 8),
    ("compl", 8),
    ("peak", 8),
    ("schema", 8),
    ("schema%", 9),
    ("trace", 8),
    ("clip", 8),
    ("tools", 7),
];

/// One line of the per-tool table (M10 T0.1).
#[derive(Default)]
struct ToolRow {
    name: String,
    /// Calls this tool rode on. Not tool *calls*: a schema is paid for on
    /// every request that carried it, whether or not the model chose it, and
    /// the tools nobody chooses are the ones P1 is looking for.
    calls: usize,
    /// `calls × schema_tokens`, or `None` when no spec for this name is in
    /// the snapshot — a tool a deployment has since dropped, or one this
    /// binary was not built with.
    tokens: Option<u64>,
}

const TOOL_COLUMNS: &[(&str, usize)] = &[
    ("tool", 26),
    ("calls", 8),
    ("tokens", 9),
    ("each", 8),
    ("share", 9),
];

const RECONSTRUCTED_COLUMNS: &[(&str, usize)] = &[
    ("turn", 7),
    ("window", 9),
    ("summary", 9),
    ("trace", 9),
    ("sent", 9),
    ("chars", 9),
    ("~tokens", 9),
    ("tools", 7),
];

/// Requests, tokens and characters per turn for one session's log.
///
/// Pure over the events so it can be checked from a hand-built log: these
/// numbers are what the plan's later phases are graded against, and a
/// measurement that needed a store and a network to test would be graded by
/// nobody.
pub fn render_budget(
    events: &[Event],
    window_turns: usize,
    caps: Caps,
    verbatim_lines: usize,
    tool_result_max_chars: usize,
    persona_chars: usize,
    specs: &[nscore::ActionSpec],
) -> String {
    if events.is_empty() {
        return "budget: no events for this session — `ns-app dump <session_id>` shows the log.\n"
            .to_string();
    }
    let mut out = if events
        .iter()
        .any(|e| matches!(e.kind, EventKind::ModelCall { .. }))
    {
        render_measured(events, persona_chars, specs)
    } else {
        render_reconstructed(
            events,
            window_turns,
            caps,
            verbatim_lines,
            tool_result_max_chars,
        )
    };
    // Printed on both paths (M10 T0.2). A rejection is a request already
    // spent — the emitter was called and the answer thrown away — and the
    // `Proposed`/`Rejected` pair is in every log, including the ones written
    // before `ModelCall` existed, so the rate is readable where nothing else
    // about cost is.
    out.push_str(&format!(
        "rejections by reason: {}\n",
        nscore::tally_rejections(events).line()
    ));
    out
}

fn render_measured(events: &[Event], persona_chars: usize, specs: &[nscore::ActionSpec]) -> String {
    let mut rows: Vec<Measured> = Vec::new();
    for e in events {
        if !rows.iter().any(|r| r.turn == e.turn) {
            rows.push(Measured {
                turn: e.turn,
                ..Default::default()
            });
        }
        let row = rows
            .iter_mut()
            .find(|r| r.turn == e.turn)
            .expect("just inserted");
        match &e.kind {
            EventKind::ModelCall { usage, manifest } => {
                row.add_call(usage, manifest, persona_chars)
            }
            EventKind::ToolCalled { .. } => row.tool_calls += 1,
            _ => {}
        }
    }
    let mut total = Measured::default();
    for r in &rows {
        total.merge(r);
    }

    let mut out = String::from("budget · measured from ModelCall (M7 T0.1)\n\n");
    out.push_str(&line(MEASURED_COLUMNS, &headings(MEASURED_COLUMNS)));
    for r in &rows {
        let label = turn_label(r.turn, r.estimated);
        out.push_str(&line(MEASURED_COLUMNS, &r.cells(label)));
    }
    let total_label = if total.estimated { "total~" } else { "total" };
    out.push_str(&line(
        MEASURED_COLUMNS,
        &total.cells(total_label.to_string()),
    ));

    let per_turn = f64::from(total.requests) / rows.len().max(1) as f64;
    out.push_str(&format!(
        "\n{} · {} ({per_turn:.1} per turn) · {} · {}\n",
        plural(rows.len(), "turn"),
        plural(total.requests as usize, "request"),
        plural(total.calls, "model call"),
        plural(total.tool_calls, "tool call")
    ));
    out.push_str(&format!(
        "requests by role: emitter {} · replier {} · summarizer {}\n",
        total.emitter_requests, total.replier_requests, total.summarizer_requests
    ));
    out.push_str(&format!(
        "peak prompt {} tokens · trace {} chars sent · {} chars clipped before sending\n",
        total.peak_prompt, total.trace_chars, total.clipped_chars
    ));
    // The number T2.4 exists to decide, spelled out rather than left to be
    // read off a column: under a quarter, shortening tool descriptions is
    // housekeeping, and only near 60% is restructuring the legal set
    // warranted — a restructuring that would be paid for in the portability
    // the flat schemas buy across all seven provider presets.
    out.push_str(&format!(
        "tool schemas: {} of the {} prompt tokens on the {} that carried them — {} (T2.4)\n",
        total.schema_tokens,
        total.schema_prompt,
        plural(total.schema_calls, "call"),
        percent(total.schema_tokens, total.schema_prompt)
    ));
    // What the provider says it did not have to re-read. Only the calls it
    // reported numbers for are in either half, so this is a measurement and
    // not a mixture of one with chars/4.
    out.push_str(&format!(
        "cached: {} of {} prompt tokens on measured calls ({})\n",
        total.cached,
        total.cached_prompt,
        if total.cached_prompt == 0 {
            "0.0%".to_string()
        } else {
            percent(total.cached, total.cached_prompt)
        }
    ));
    // The level T2.3 is gated on. A breakpoint below the provider's
    // 1,024-token minimum is not a cheaper prompt, it is a no-op, so what is
    // printed is per call and per role rather than a total: the median says
    // what a typical call would cache, the max whether any call clears the
    // floor at all.
    out.push_str(&format!(
        "stable prefix (est.): emitter {} (facts+summary+window) \u{b7}          replier {} (persona+facts+summary) \u{b7} breakpoint floor 1,024\n",
        prefix_summary(&total.emitter_prefix),
        prefix_summary(&total.replier_prefix),
    ));
    out.push_str(&render_tool_table(events, specs));
    out.push_str(&format!(
        "the free tier meters requests, not tokens: 50 a day on openrouter/free, {} spent here\n",
        total.requests
    ));
    if total.estimated {
        out.push_str(
            "\n~ the provider returned no usage block for at least one call in that turn; \
             those tokens are chars/4.\n",
        );
    }
    out
}

/// Which tool carried how much of the session's schema bill (M10 T0.1).
///
/// `tools_tokens` says what the array cost and nothing about which tool cost
/// it, and every cut M10 P1 proposes is a decision about *which text*. The
/// names come from the manifest — what was legal on that call — and the
/// price from `nsllm::schema`, recompiled here at report time from the same
/// function the request was built with, so a row is the request's own bytes
/// rather than a model of them. `respond_directly` is added once per call
/// because `build_tools` appends it to every array; without it the table
/// could not sum to what the call was charged.
///
/// The total lands within `estimate_tokens` rounding of `Σ tools_tokens` on
/// the calls that carried names: each tool's chars/4 is floored separately,
/// and the array's own two brackets and `n - 1` commas are in the call's
/// number and in no row.
fn render_tool_table(events: &[Event], specs: &[nscore::ActionSpec]) -> String {
    let by_name: std::collections::HashMap<&str, &nscore::ActionSpec> =
        specs.iter().map(|s| (s.name.as_str(), s)).collect();
    let mut rows: Vec<ToolRow> = Vec::new();
    let mut calls_with_names = 0usize;
    let mut calls_without = 0usize;
    let mut measured: u64 = 0;
    for e in events {
        let EventKind::ModelCall { usage, manifest } = &e.kind else {
            continue;
        };
        if manifest.tool_names.is_empty() {
            // A call that was sent tools but recorded no names: a log
            // written before M10. A call that was sent none (replier,
            // summarizer, a `Chat` turn's floor) is simply not a row.
            if manifest.tools > 0 {
                calls_without += 1;
            }
            continue;
        }
        calls_with_names += 1;
        measured += u64::from(usage.tools_tokens);
        for name in manifest
            .tool_names
            .iter()
            .map(String::as_str)
            .chain(std::iter::once(nsllm::schema::RESPOND_DIRECTLY))
        {
            let tokens = if name == nsllm::schema::RESPOND_DIRECTLY {
                Some(u64::from(nsllm::schema::respond_directly_tokens()))
            } else {
                by_name
                    .get(name)
                    .map(|s| u64::from(nsllm::schema::schema_tokens(s)))
            };
            match rows.iter_mut().find(|r| r.name == name) {
                Some(row) => {
                    row.calls += 1;
                    row.tokens = match (row.tokens, tokens) {
                        (Some(a), Some(b)) => Some(a + b),
                        _ => None,
                    };
                }
                None => rows.push(ToolRow {
                    name: name.to_string(),
                    calls: 1,
                    tokens,
                }),
            }
        }
    }

    if calls_with_names == 0 {
        return format!(
            "per-tool schemas: n/a — {} (M10 T0.1 records the names; this log predates it)\n",
            if calls_without == 0 {
                "no call in this session was sent a tool array".to_string()
            } else {
                plural(calls_without, "call") + " carried tools but recorded no names"
            }
        );
    }

    // Largest bill first: the table is read to decide what to shorten.
    rows.sort_by(|a, b| b.tokens.cmp(&a.tokens).then(a.name.cmp(&b.name)));
    let table_total: u64 = rows.iter().filter_map(|r| r.tokens).sum();
    let unpriced = rows.iter().filter(|r| r.tokens.is_none()).count();

    let mut out = String::from("\nper-tool schemas (M10 T0.1)\n\n");
    out.push_str(&line(TOOL_COLUMNS, &headings(TOOL_COLUMNS)));
    for r in &rows {
        out.push_str(&line(
            TOOL_COLUMNS,
            &[
                r.name.clone(),
                r.calls.to_string(),
                r.tokens.map(|t| t.to_string()).unwrap_or("n/a".into()),
                r.tokens
                    .map(|t| (t / r.calls.max(1) as u64).to_string())
                    .unwrap_or("n/a".into()),
                r.tokens
                    .map(|t| percent(t, table_total))
                    .unwrap_or("n/a".into()),
            ],
        ));
    }
    out.push_str(&line(
        TOOL_COLUMNS,
        &[
            "total".to_string(),
            calls_with_names.to_string(),
            table_total.to_string(),
            String::new(),
            percent(table_total, table_total),
        ],
    ));
    // The invariant, printed rather than assumed: if these two ever drift by
    // more than the separators, the table is pricing a different array from
    // the one that was sent.
    out.push_str(&format!(
        "per-tool total {table_total} tok vs {measured} measured on {} \u{2014} {} within estimate_tokens rounding\n",
        plural(calls_with_names, "call"),
        if table_total > measured {
            format!("+{}", table_total - measured)
        } else {
            format!("-{}", measured - table_total)
        }
    ));
    if unpriced > 0 {
        out.push_str(&format!(
            "  {} of those names has no spec in this build \u{2014} priced n/a, and out of the total\n",
            plural(unpriced, "tool")
        ));
    }
    if calls_without > 0 {
        out.push_str(&format!(
            "  {} carried tools before M10 recorded names and are out of this table\n",
            plural(calls_without, "call")
        ));
    }
    out.push('\n');
    out
}

/// The pre-`ModelCall` path: rebuild what each turn was shown, the way
/// `render_echo` does, and say plainly what the rebuild cannot see.
fn render_reconstructed(
    events: &[Event],
    window_turns: usize,
    caps: Caps,
    verbatim_lines: usize,
    tool_result_max_chars: usize,
) -> String {
    let mut rows: Vec<Reconstructed> = Vec::new();
    for turn in turns(events) {
        // Records exist only for completed turns, so folding everything
        // strictly before this one gives exactly the window this turn was
        // shown.
        let before: Vec<Event> = events.iter().filter(|e| e.turn < turn).cloned().collect();
        let state = nsengine::state::fold(&before);
        let window = state.window(window_turns);
        rows.push(Reconstructed {
            turn,
            window_chars: nscore::render_window(&window, window.len(), &caps)
                .chars()
                .count(),
            summary_chars: state
                .summary
                .as_ref()
                .map(|s| nscore::render_summary(s).chars().count())
                .unwrap_or_default(),
            trace_chars: nsengine::trace::turn_trace(events, turn).chars().count(),
            sent_trace_chars: nsengine::trace::trace_for_prompt(
                events,
                turn,
                verbatim_lines,
                tool_result_max_chars,
            )
            .0
            .join("\n")
            .chars()
            .count(),
            tool_calls: events
                .iter()
                .filter(|e| e.turn == turn && matches!(e.kind, EventKind::ToolCalled { .. }))
                .count(),
        });
    }
    let total = Reconstructed {
        turn: 0,
        window_chars: rows.iter().map(|r| r.window_chars).sum(),
        summary_chars: rows.iter().map(|r| r.summary_chars).sum(),
        trace_chars: rows.iter().map(|r| r.trace_chars).sum(),
        sent_trace_chars: rows.iter().map(|r| r.sent_trace_chars).sum(),
        tool_calls: rows.iter().map(|r| r.tool_calls).sum(),
    };

    let mut out = String::from("budget · no ModelCall events — estimated (reconstructed)\n\n");
    out.push_str(&line(
        RECONSTRUCTED_COLUMNS,
        &headings(RECONSTRUCTED_COLUMNS),
    ));
    for r in &rows {
        let label = turn_label(r.turn, false);
        out.push_str(&line(RECONSTRUCTED_COLUMNS, &r.cells(label)));
    }
    out.push_str(&line(
        RECONSTRUCTED_COLUMNS,
        &total.cells("total".to_string()),
    ));

    out.push_str(&format!(
        "\n{} · {} chars · ~{} tokens · {}\n",
        plural(rows.len(), "turn"),
        total.chars(),
        nscore::estimate_tokens(total.chars()),
        plural(total.tool_calls, "tool call")
    ));
    out.push_str("requests: not recorded — this session predates ModelCall (M7 T0.1).\n");
    // The acceptance number for phase 1, on the session the plan's §2
    // evidence came from: `trace` is what those turns really sent, `sent` is
    // what the same trace costs now, through the engine's own renderer. Per
    // turn, and once — the emitter re-sends it on every iteration, so the
    // difference is multiplied by however many iterations followed.
    out.push_str(&format!(
        "trace after the phase-1 cap and fold: {} chars, was {} — {} saved per send\n",
        total.sent_trace_chars,
        total.trace_chars,
        total.trace_chars.saturating_sub(total.sent_trace_chars)
    ));
    out.push_str(
        "\nA floor, not a measurement:\n\
         · standing facts are NOT counted. The log records what the engine decided, not which\n\
         \x20 facts were selected, and the facts table holds only current values — the same\n\
         \x20 limit `render_echo` carries, for the same reason.\n\
         · the system prompt, the tool schemas and the user's own text are not counted either.\n\
         · the emitter side is under-reported. The trace is rebuilt and re-sent on every\n\
         \x20 iteration; this counts one send per turn, because the log does not say how many\n\
         \x20 iterations there were.\n\
         · ~tokens is chars/4 (nscore::estimate_tokens); Czech text runs nearer chars/3.\n",
    );
    out
}

/// Turn numbers in the order they first appear, which for a log is ascending.
fn turns(events: &[Event]) -> Vec<u32> {
    let mut out: Vec<u32> = Vec::new();
    for e in events {
        if !out.contains(&e.turn) {
            out.push(e.turn);
        }
    }
    out
}

fn turn_label(turn: u32, estimated: bool) -> String {
    format!("t{turn}{}", if estimated { "~" } else { "" })
}

fn headings(columns: &[(&str, usize)]) -> Vec<String> {
    columns
        .iter()
        .map(|(name, _)| (*name).to_string())
        .collect()
}

/// Header and rows go through one function so the two cannot drift apart.
fn line(columns: &[(&str, usize)], cells: &[String]) -> String {
    let mut out = String::new();
    for (i, (column, cell)) in columns.iter().zip(cells).enumerate() {
        let width = column.1;
        if i == 0 {
            out.push_str(&format!("{cell:<width$}"));
        } else {
            out.push_str(&format!("{cell:>width$}"));
        }
    }
    out.push('\n');
    out
}

/// A person reads the summary line, and "1 calls" reads as a bug in the
/// counter rather than as one call.
fn plural(n: usize, word: &str) -> String {
    if n == 1 {
        format!("{n} {word}")
    } else {
        format!("{n} {word}s")
    }
}

/// `median N tok, max M`, or `n/a` when no call of that role carried block
/// sizes — a session with no such call, or any log written before M9
/// recorded them. `n/a` rather than `0` because nothing was measured, and a
/// printed zero would read as a prefix that exists and is empty.
fn prefix_summary(estimates: &[u32]) -> String {
    if estimates.is_empty() {
        return "n/a".to_string();
    }
    let mut sorted = estimates.to_vec();
    sorted.sort_unstable();
    let median = sorted[sorted.len() / 2];
    let max = *sorted.last().expect("non-empty");
    format!("median {median} tok, max {max}")
}

/// `-` rather than `0.0%` when nothing carried a schema: a turn with no
/// schema-bearing call has no fraction, and a printed zero would read as one.
fn percent(part: u64, whole: u64) -> String {
    if whole == 0 {
        return "-".to_string();
    }
    format!("{:.1}%", 100.0 * part as f64 / whole as f64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nscore::{EventLog, SessionId, Timestamp};

    /// The engine's own default, so these numbers stay the ones a default
    /// deployment would see.
    const DEFAULT_CAP: usize = nsengine::trace::DEFAULT_TOOL_RESULT_MAX_CHARS;

    fn usage(role: &str, attempts: u32, prompt: u32, tools_tokens: u32) -> Usage {
        Usage {
            role: role.into(),
            model: "m".into(),
            prompt_tokens: prompt,
            completion_tokens: 10,
            estimated: false,
            attempts,
            latency_ms: 5,
            tools_tokens,
            cached_tokens: 0,
        }
    }

    fn cached(role: &str, prompt: u32, cached_tokens: u32) -> Usage {
        Usage {
            cached_tokens,
            ..usage(role, 1, prompt, 0)
        }
    }

    fn manifest(tools: usize, trace_chars: usize, clipped_chars: usize) -> ContextManifest {
        ContextManifest {
            tools,
            trace_chars,
            clipped_chars,
            ..Default::default()
        }
    }

    /// A manifest carrying only the three stable block sizes, as an emitter
    /// or replier call records them after the fit.
    fn blocks(facts: usize, summary: usize, window: usize) -> ContextManifest {
        ContextManifest {
            facts_chars: facts,
            summary_chars: summary,
            window_chars: window,
            ..Default::default()
        }
    }

    fn log() -> EventLog {
        EventLog::new(SessionId("b".into()))
    }

    fn call(log: &mut EventLog, turn: u32, usage: Usage, manifest: ContextManifest) {
        log.append(
            turn,
            Timestamp(turn as u64),
            EventKind::ModelCall { usage, manifest },
        );
    }

    fn said(log: &mut EventLog, turn: u32, text: &str) {
        log.append(
            turn,
            Timestamp(turn as u64),
            EventKind::UserSaid { text: text.into() },
        );
    }

    fn replied(log: &mut EventLog, turn: u32, text: &str) {
        log.append(
            turn,
            Timestamp(turn as u64),
            EventKind::Replied { text: text.into() },
        );
    }

    /// The distinction the whole command turns on: three attempts inside one
    /// call are three requests off a fifty-a-day tier and one decision.
    #[test]
    fn requests_sum_attempts_rather_than_counting_calls() {
        let mut log = log();
        said(&mut log, 1, "click the thing");
        call(
            &mut log,
            1,
            usage("emitter", 3, 1_000, 200),
            manifest(17, 40, 0),
        );
        call(
            &mut log,
            1,
            usage("emitter", 1, 1_400, 200),
            manifest(17, 900, 13_000),
        );
        call(
            &mut log,
            1,
            usage("replier", 1, 800, 0),
            manifest(0, 900, 0),
        );
        let out = render_budget(log.events(), 6, Caps::default(), 5, DEFAULT_CAP, 0, &[]);

        let row = out.lines().find(|l| l.starts_with("t1")).expect("a t1 row");
        let cells: Vec<&str> = row.split_whitespace().collect();
        assert_eq!(cells[1], "5", "5 requests from 3 calls: {row}");
        assert_eq!(cells[2], "3", "3 calls: {row}");
        assert_eq!(cells[3], "4/1/0", "by role: {row}");
        assert_eq!(
            cells[7], "1400",
            "peak prompt is the max, not the sum: {row}"
        );
        // Summed, not maxed: the same trace re-sent on the next iteration is
        // paid for again, and that is what phase 1 has to move.
        assert_eq!(cells[10], "1840", "trace chars summed over calls: {row}");
        assert_eq!(cells[11], "13000", "clipped chars summed: {row}");
        assert!(
            out.contains("5 requests (5.0 per turn)"),
            "requests per turn is the free-tier number: {out}"
        );
    }

    /// The T2.4 fraction. Its denominator is the prompts that carried
    /// schemas: adding the replier's 2,000 schema-free tokens would report
    /// 10% for a legal set that is really costing 20%.
    #[test]
    fn schema_fraction_counts_only_the_calls_that_carried_tools() {
        let mut log = log();
        call(
            &mut log,
            1,
            usage("emitter", 1, 1_000, 200),
            manifest(17, 0, 0),
        );
        call(
            &mut log,
            1,
            usage("replier", 1, 2_000, 0),
            manifest(0, 0, 0),
        );
        let out = render_budget(log.events(), 6, Caps::default(), 5, DEFAULT_CAP, 0, &[]);

        let row = out.lines().find(|l| l.starts_with("t1")).expect("a t1 row");
        assert!(row.contains("20.0%"), "200 of 1000 emitter tokens: {row}");
        assert!(
            out.contains("200 of the 1000 prompt tokens on the 1 call that carried them — 20.0%"),
            "{out}"
        );
        assert!(
            !out.contains("6.7%"),
            "the replier's prompt is not in it: {out}"
        );
    }

    /// The cache share is a measurement or it is nothing: an estimated call
    /// has no provider number to split, so neither its cached tokens nor its
    /// chars/4 prompt may stand in the fraction.
    #[test]
    fn cached_column_sums_only_measured_calls() {
        let mut log = log();
        call(
            &mut log,
            1,
            cached("emitter", 1_000, 100),
            manifest(0, 0, 0),
        );
        call(
            &mut log,
            1,
            cached("emitter", 1_000, 300),
            manifest(0, 0, 0),
        );
        let mut guessed = cached("replier", 5_000, 999);
        guessed.estimated = true;
        call(&mut log, 1, guessed, manifest(0, 0, 0));
        let out = render_budget(log.events(), 6, Caps::default(), 5, DEFAULT_CAP, 0, &[]);

        let row = out.lines().find(|l| l.starts_with("t1")).expect("a t1 row");
        let cells: Vec<&str> = row.split_whitespace().collect();
        assert_eq!(cells[5], "400", "the estimated call's 999 is out: {row}");
        assert!(
            out.contains("cached: 400 of 2000 prompt tokens on measured calls (20.0%)"),
            "the denominator is the two measured calls only: {out}"
        );
    }

    /// The prefix estimate reads the manifest's three block sizes and nothing
    /// else, and a call that recorded none of them is not a prefix of zero —
    /// it is a call with no prefix to report (M9 T0.5).
    #[test]
    fn prefix_estimate_uses_the_manifests_block_sizes() {
        let mut log = log();
        for chars in [4_000usize, 12_000, 8_000] {
            call(
                &mut log,
                1,
                usage("emitter", 1, 100, 0),
                blocks(chars / 4, chars / 4, chars / 2),
            );
        }
        // Sent no stable blocks, so it records none: it must not become a
        // fourth sample at zero and pull the median down.
        call(
            &mut log,
            1,
            usage("summarizer", 1, 100, 0),
            manifest(0, 0, 0),
        );
        let out = render_budget(log.events(), 6, Caps::default(), 5, DEFAULT_CAP, 0, &[]);

        let line = out
            .lines()
            .find(|l| l.starts_with("stable prefix"))
            .expect("a stable prefix line");
        assert!(
            line.contains(&format!(
                "emitter median {} tok, max {} (facts+summary+window)",
                nscore::estimate_tokens(8_000),
                nscore::estimate_tokens(12_000)
            )),
            "median is the middle of 4k/8k/12k and the summarizer is out: {line}"
        );
        assert!(
            line.contains("replier n/a (persona+facts+summary)"),
            "no replier called, so no replier prefix: {line}"
        );
        assert!(line.contains("breakpoint floor 1,024"), "{line}");
    }

    /// A call whose tokens the provider never reported is marked, so nobody
    /// averages a guess with a measurement.
    #[test]
    fn an_estimated_call_marks_its_turn_and_the_total() {
        let mut log = log();
        call(&mut log, 1, usage("emitter", 1, 100, 0), manifest(17, 0, 0));
        let mut guessed = usage("replier", 1, 100, 0);
        guessed.estimated = true;
        call(&mut log, 2, guessed, manifest(0, 0, 0));
        let out = render_budget(log.events(), 6, Caps::default(), 5, DEFAULT_CAP, 0, &[]);

        assert!(
            out.lines().any(|l| l.starts_with("t1 ")),
            "t1 unmarked: {out}"
        );
        assert!(
            out.lines().any(|l| l.starts_with("t2~")),
            "t2 marked: {out}"
        );
        assert!(out.lines().any(|l| l.starts_with("total~")), "{out}");
        assert!(out.contains("chars/4"), "the marker is explained: {out}");
    }

    /// The `cli` session and every other one recorded before T0.1: no usage
    /// to report, so the table says what it is and what it leaves out.
    #[test]
    fn a_session_without_model_calls_is_labelled_a_reconstruction() {
        let mut log = log();
        said(&mut log, 1, "ahoj");
        replied(&mut log, 1, "zdravím");
        said(&mut log, 2, "co je nového");
        log.append(
            2,
            Timestamp(2),
            EventKind::ToolCalled {
                action: "get_time".into(),
                args: vec![],
            },
        );
        replied(&mut log, 2, "je poledne");
        let out = render_budget(log.events(), 6, Caps::default(), 5, DEFAULT_CAP, 0, &[]);

        assert!(out.contains("estimated (reconstructed)"), "{out}");
        assert!(out.contains("A floor, not a measurement"), "{out}");
        assert!(
            out.contains("standing facts are NOT counted"),
            "the known omission is stated: {out}"
        );
        assert!(
            out.contains("re-sent on every"),
            "the iteration under-report is stated: {out}"
        );
        assert!(
            out.contains("requests: not recorded"),
            "no request count is invented: {out}"
        );
        // Turn 2 is shown the record of turn 1; turn 1 has no window at all.
        let t1 = out.lines().find(|l| l.starts_with("t1")).expect("a t1 row");
        let t2 = out.lines().find(|l| l.starts_with("t2")).expect("a t2 row");
        assert_eq!(t1.split_whitespace().nth(1), Some("0"), "{t1}");
        assert!(
            t2.split_whitespace()
                .nth(1)
                .unwrap()
                .parse::<usize>()
                .unwrap()
                > 0,
            "{t2}"
        );
        // Last column, so adding one earlier does not silently retarget this.
        assert_eq!(
            t2.split_whitespace().last(),
            Some("1"),
            "one tool call: {t2}"
        );
    }

    /// The acceptance number for phase 1, on a log recorded before it: the
    /// raw trace beside the same trace as it would be sent now. Computed by
    /// the engine's own renderer, so it cannot drift from what the engine
    /// actually sends.
    #[test]
    fn the_reconstruction_reports_the_trace_before_and_after_the_phase_one_cap() {
        let mut log = EventLog::new(SessionId("wide".into()));
        log.append(
            1,
            Timestamp(1),
            EventKind::UserSaid {
                text: "what is on the screen".into(),
            },
        );
        let call = log
            .append(
                1,
                Timestamp(2),
                EventKind::ToolCalled {
                    action: "pointer_ui_read".into(),
                    args: vec![],
                },
            )
            .id;
        log.append(
            1,
            Timestamp(3),
            EventKind::ToolReturned {
                call,
                outcome: nscore::ToolOutcome::Ok {
                    output: nscore::ToolOutput {
                        summary: "node ".repeat(3000),
                        artifact: None,
                        trust: nscore::Trust::External,
                    },
                },
            },
        );
        replied(&mut log, 1, "a browser window");
        let out = render_budget(log.events(), 6, Caps::default(), 5, DEFAULT_CAP, 0, &[]);

        let t1 = out.lines().find(|l| l.starts_with("t1")).expect("a t1 row");
        let cells: Vec<&str> = t1.split_whitespace().collect();
        let raw: usize = cells[3].parse().expect("trace");
        let sent: usize = cells[4].parse().expect("sent");
        assert!(raw > 14_000, "a screen read of the recorded size: {raw}");
        assert!(
            sent < 2_000,
            "the cap is what phase 1 is graded on: {sent} of {raw}"
        );
        assert!(
            out.contains("trace after the phase-1 cap and fold:"),
            "{out}"
        );
    }

    /// A spec whose schema is big enough that two of them are visibly
    /// different sizes, so a row that mixed them up would show.
    fn spec(name: &str, description: &str, props: serde_json::Value) -> nscore::ActionSpec {
        nscore::ActionSpec {
            name: name.into(),
            description: description.into(),
            args_schema: serde_json::json!({"type": "object", "properties": props}),
            side_effect: nscore::SideEffect::Pure,
            residual_policy: Default::default(),
            dedupe_tag: None,
        }
    }

    /// The exit criterion for M10 T0.1: a table built from the manifest's
    /// names and the compiler's own bytes adds up to what the calls were
    /// charged, within the floor in `estimate_tokens` and the array's
    /// brackets and commas. Built the only honest way — the `tools_tokens`
    /// on each call is measured from the real `build_tools` output, exactly
    /// as `nsllm::client` measures it from the real request.
    #[test]
    fn the_tool_table_sums_to_the_sessions_tools_tokens() {
        let specs = vec![
            spec(
                "pointer_click",
                "Click on the remote machine. Irreversible: whatever is under the pointer \
                 will be activated.",
                serde_json::json!({
                    "x": {"type": "number", "description": "Absolute pixel from the left."},
                    "y": {"type": "number", "description": "Absolute pixel from the top."},
                    "button": {"type": "string", "enum": ["left", "right", "middle"]},
                }),
            ),
            spec("ask_clarification", "Ask one short question.", serde_json::json!({
                "question": {"type": "string"}
            })),
            spec("recall", "Search earlier turns.", serde_json::json!({})),
        ];
        // Turn 1 was legal for all three, turn 2 for one — the narrowing the
        // engine does between iterations, and the reason a per-call list is
        // recorded rather than a per-session one.
        let sets: [&[usize]; 3] = [&[0, 1, 2], &[0, 1, 2], &[1]];
        let mut log = log();
        let mut expected_calls: Vec<Vec<String>> = Vec::new();
        for (i, set) in sets.iter().enumerate() {
            let legal = nscore::LegalActionSet {
                actions: set.iter().map(|&k| specs[k].clone()).collect(),
            };
            // What the client would have recorded for this request.
            let tools_tokens =
                nscore::estimate_tokens(nsllm::schema::build_tools(&legal).to_string().len());
            let names: Vec<String> = legal.actions.iter().map(|s| s.name.clone()).collect();
            expected_calls.push(names.clone());
            call(
                &mut log,
                1 + i as u32,
                usage("emitter", 1, 1_000, tools_tokens),
                ContextManifest {
                    tools: names.len(),
                    tool_names: names,
                    ..Default::default()
                },
            );
        }
        // A replier: no tools, so it is not a row and not in the comparison.
        call(
            &mut log,
            3,
            usage("replier", 1, 500, 0),
            ContextManifest::default(),
        );
        let out = render_budget(log.events(), 6, Caps::default(), 5, DEFAULT_CAP, 0, &specs);

        let table: Vec<&str> = out
            .lines()
            .skip_while(|l| !l.starts_with("per-tool schemas"))
            .collect();
        assert!(!table.is_empty(), "no per-tool table: {out}");
        // Calls it rode on, per tool, plus respond_directly on every call.
        let row_of = |name: &str| -> Vec<String> {
            table
                .iter()
                .find(|l| l.starts_with(name))
                .unwrap_or_else(|| panic!("no {name} row in:\n{out}"))
                .split_whitespace()
                .map(str::to_string)
                .collect()
        };
        assert_eq!(row_of("pointer_click")[1], "2", "legal on two of three");
        assert_eq!(row_of("ask_clarification")[1], "3", "legal on all three");
        assert_eq!(row_of("recall")[1], "2");
        assert_eq!(
            row_of("respond_directly")[1],
            "3",
            "build_tools appends it to every array"
        );
        // Each tool's own bytes, not an apportionment of the total.
        assert_eq!(
            row_of("pointer_click")[3],
            nsllm::schema::schema_tokens(&specs[0]).to_string(),
            "per-call price is the compiler's"
        );

        // The invariant. The gap is bounded by what the array adds and the
        // floor takes: per call, `n + 1` tools each losing under a token to
        // the floor, against `n + 1` separator characters worth under a
        // token in total.
        let total_line = out
            .lines()
            .find(|l| l.starts_with("per-tool total"))
            .expect("the comparison line");
        let nums: Vec<u64> = total_line
            .split_whitespace()
            .filter_map(|w| w.parse::<u64>().ok())
            .collect();
        let (table_total, measured) = (nums[0], nums[1]);
        let slack: u64 = expected_calls
            .iter()
            .map(|names| names.len() as u64 + 2)
            .sum();
        assert!(
            table_total.abs_diff(measured) <= slack,
            "table {table_total} vs measured {measured}, slack {slack}: {out}"
        );
        assert!(table_total > 0 && measured > 0, "{out}");
        assert!(
            total_line.contains("within estimate_tokens rounding"),
            "{total_line}"
        );
    }

    /// The other half of T0.1: the recorded 21-turn log was written before
    /// `tool_names` and can never gain one, so the table says `n/a` and says
    /// why, rather than printing a table of nothing.
    #[test]
    fn a_log_without_tool_names_prints_n_a_for_the_tool_table() {
        let mut log = log();
        call(
            &mut log,
            1,
            usage("emitter", 1, 1_000, 731),
            manifest(17, 0, 0),
        );
        let out = render_budget(log.events(), 6, Caps::default(), 5, DEFAULT_CAP, 0, &[]);
        assert!(out.contains("per-tool schemas: n/a"), "{out}");
        assert!(
            out.contains("1 call carried tools but recorded no names"),
            "{out}"
        );
        // And the session-level schema number is untouched: T0.1 added a
        // breakdown, it did not change what was measured.
        assert!(out.contains("tool schemas: 731 of the 1000"), "{out}");
    }

    /// The exit criterion for M10 T0.2. Nine rejections against 81
    /// proposals is 11.1 per 100 — the recorded desktop log's own shape,
    /// with its own split: six repeat-gate loops, two illegal actions, one
    /// malformed. The buckets are different fixes, which is why the line is
    /// a split and not a total.
    #[test]
    fn rejections_are_bucketed_by_reason_and_rated_per_hundred_proposals() {
        let mut log = log();
        let mut proposal_ids = Vec::new();
        for i in 0..81u32 {
            let e = log.append(
                1,
                Timestamp(i as u64),
                EventKind::Proposed {
                    proposal: nscore::Proposal {
                        action: "pointer_click".into(),
                        args: serde_json::json!({}),
                        rationale: String::new(),
                    },
                },
            );
            proposal_ids.push(e.id);
        }
        let reasons = std::iter::repeat_n(
            nscore::RejectReason::GuardDenied {
                guard: "repeat_gate".into(),
                reason: "identical call already executed this turn".into(),
            },
            6,
        )
        .chain(std::iter::repeat_n(
            nscore::RejectReason::IllegalAction {
                action: "pointer_drag".into(),
            },
            2,
        ))
        .chain(std::iter::once(nscore::RejectReason::Malformed {
            detail: "x is not a number".into(),
        }));
        for (i, reason) in reasons.enumerate() {
            log.append(
                1,
                Timestamp(100 + i as u64),
                EventKind::Rejected {
                    proposal_of: proposal_ids[i],
                    reason,
                },
            );
        }
        let out = render_budget(log.events(), 6, Caps::default(), 5, DEFAULT_CAP, 0, &[]);

        let line = out
            .lines()
            .find(|l| l.starts_with("rejections by reason"))
            .expect("a rejections line");
        assert_eq!(
            line,
            "rejections by reason: repeat_gate 6, IllegalAction 2, Malformed 1 \u{2014} 11.1 per 100 proposals (81 proposed)",
            "the M10 T0.2 exit line: {out}"
        );

        // A guard is named by the guard, not by the variant: "GuardDenied 6"
        // names no fix, and the whole point of the bucket is that a
        // repeat-gate loop is prompt-side while a malformed argument is the
        // schema.
        let tally = nscore::tally_rejections(log.events());
        assert_eq!(tally.by_reason.get("repeat_gate"), Some(&6));
        assert!(!tally.by_reason.contains_key("GuardDenied"));
        assert_eq!(tally.rejections(), 9);
        assert!((tally.per_hundred().unwrap() - 11.111).abs() < 0.01);
    }

    /// A log with no proposals has no rate, and a printed `0.0 per 100`
    /// would read as a session that proposed cleanly.
    #[test]
    fn a_session_that_proposed_nothing_has_no_rejection_rate() {
        let mut log = log();
        said(&mut log, 1, "ahoj");
        replied(&mut log, 1, "zdravím");
        let out = render_budget(log.events(), 6, Caps::default(), 5, DEFAULT_CAP, 0, &[]);
        assert!(
            out.contains("rejections by reason: no proposals in this log"),
            "{out}"
        );
        assert!(!out.contains("0.0 per 100"), "{out}");
    }

    #[test]
    fn an_empty_session_says_so_instead_of_printing_a_table() {
        let out = render_budget(&[], 6, Caps::default(), 5, DEFAULT_CAP, 0, &[]);
        assert!(out.contains("no events for this session"), "{out}");
        assert!(!out.contains("turn"), "no header for nothing: {out}");
    }
}
