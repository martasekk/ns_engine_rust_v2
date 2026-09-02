use async_trait::async_trait;
use nscore::{ActionSpec, SideEffect, Tool, ToolCtx, ToolError, ToolOutput, Trust};

pub struct GetTimeTool {
    spec: ActionSpec,
    clock: Box<dyn Fn() -> u64 + Send + Sync>,
}

impl GetTimeTool {
    pub fn new() -> Self {
        Self::with_clock(Box::new(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0)
        }))
    }

    pub fn with_clock(clock: Box<dyn Fn() -> u64 + Send + Sync>) -> Self {
        Self {
            spec: ActionSpec {
                name: "get_time".into(),
                description: "Get the current date and time".into(),
                args_schema: serde_json::json!({
                    "type": "object", "properties": {}, "required": []
                }),
                side_effect: SideEffect::Pure,
                residual_policy: Default::default(),
                dedupe_tag: None,
            },
            clock,
        }
    }
}

impl Default for GetTimeTool {
    fn default() -> Self {
        Self::new()
    }
}

/// Proleptic Gregorian civil date from days since 1970-01-01 (Howard
/// Hinnant's `civil_from_days`). No time zone database: the harness reports
/// UTC and says so.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

const WEEKDAYS: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];

/// `2026-09-02 10:41:28 UTC (Wednesday)` — what a person (and a small reply
/// model) can read. The raw millisecond count is not shown: seen live, the
/// replier echoed "1788345688203 milliseconds" back to the user.
pub fn format_utc(unix_ms: u64) -> String {
    let secs = (unix_ms / 1000) as i64;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let weekday = WEEKDAYS[(days + 4).rem_euclid(7) as usize];
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02} UTC ({weekday})",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

#[async_trait]
impl Tool for GetTimeTool {
    fn spec(&self) -> &ActionSpec {
        &self.spec
    }

    async fn call(
        &self,
        _args: &serde_json::Value,
        _ctx: &ToolCtx,
    ) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput {
            summary: format_utc((self.clock)()),
            artifact: None,
            trust: Trust::System,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nscore::SessionId;

    #[test]
    fn formats_known_instants_as_readable_utc() {
        // Recorded live: turn 74 of the cli session.
        assert_eq!(
            format_utc(1_788_345_688_203),
            "2026-09-02 10:41:28 UTC (Wednesday)"
        );
        assert_eq!(
            format_utc(1_756_700_000_000),
            "2025-09-01 04:13:20 UTC (Monday)"
        );
        assert_eq!(format_utc(0), "1970-01-01 00:00:00 UTC (Thursday)");
        // Leap day and year boundary.
        assert_eq!(
            format_utc(1_709_164_800_000),
            "2024-02-29 00:00:00 UTC (Thursday)"
        );
        assert_eq!(
            format_utc(1_735_689_599_000),
            "2024-12-31 23:59:59 UTC (Tuesday)"
        );
    }

    #[tokio::test]
    async fn reports_injected_clock_as_system_trust() {
        let t = GetTimeTool::with_clock(Box::new(|| 1_756_700_000_000));
        assert_eq!(t.spec().name, "get_time");
        assert_eq!(t.spec().side_effect, SideEffect::Pure);
        let out = t
            .call(
                &serde_json::json!({}),
                &ToolCtx {
                    session: SessionId("s".into()),
                    artifacts: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(out.summary, "2025-09-01 04:13:20 UTC (Monday)");
        assert_eq!(out.trust, Trust::System);
    }
}
