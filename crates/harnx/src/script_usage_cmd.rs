//! `harnx info script-usage`: validates the scope and window, collects the
//! typed report from stored session logs, and prints it as compact text or the
//! raw typed JSON. Collection itself is read-only and lives in
//! `harnx_runtime::script_usage`; nothing here connects before the arguments
//! have been validated.

use crate::cli::InfoScriptUsageArgs;
use crate::config::WorkingMode;
use crate::init_frontend_config;
use crate::script_usage_figures::num;
use crate::script_usage_render::{render_report_text, render_session_text};
use anyhow::{bail, Context, Result};
use chrono::{DateTime, NaiveDate, Utc};
use harnx_runtime::script_usage::{
    agent_matcher, collect_script_usage, collect_session_script_usage, ScriptUsageScope, TimeWindow,
};

/// What the arguments ask for, once validated.
#[derive(Debug)]
enum Request {
    Report(ScriptUsageScope),
    Session { agent: String, session_id: String },
}

const WHEN_FORMS: &str = "an RFC 3339 timestamp (2026-11-20T10:00:00Z), \
a date (2026-11-20, UTC midnight) or a duration ago (7d, 36h)";

fn parse_when(value: &str, now: DateTime<Utc>) -> Result<DateTime<Utc>> {
    let value = value.trim();
    if let Ok(at) = DateTime::parse_from_rfc3339(value) {
        return Ok(at.with_timezone(&Utc));
    }
    if let Ok(date) = NaiveDate::parse_from_str(value, "%Y-%m-%d") {
        if let Some(midnight) = date.and_hms_opt(0, 0, 0) {
            return Ok(midnight.and_utc());
        }
    }
    if let Ok(ago) = humantime::parse_duration(value) {
        let ago = chrono::Duration::from_std(ago).context("duration is too large")?;
        return now
            .checked_sub_signed(ago)
            .context("duration reaches before the supported time range");
    }
    bail!("'{value}' is not {WHEN_FORMS}")
}

fn parse_bound(
    name: &str,
    value: Option<&str>,
    now: DateTime<Utc>,
) -> Result<Option<DateTime<Utc>>> {
    value
        .map(|value| parse_when(value, now).with_context(|| format!("invalid {name}")))
        .transpose()
}

fn parse_window(args: &InfoScriptUsageArgs, now: DateTime<Utc>) -> Result<TimeWindow> {
    let since = parse_bound("--since", args.since.as_deref(), now)?;
    let until = parse_bound("--until", args.until.as_deref(), now)?;
    if let (Some(since), Some(until)) = (since, until) {
        if since >= until {
            bail!("--since ({since}) must be earlier than --until ({until})");
        }
    }
    Ok(TimeWindow { since, until })
}

/// The collector builds its matcher with the same function, so a pattern
/// accepted here is accepted there, and a bad one fails even on an empty
/// cluster.
fn check_agent_pattern(pattern: &str) -> Result<()> {
    agent_matcher(pattern).map_err(|error| anyhow::anyhow!("invalid --agent: {error:#}"))?;
    Ok(())
}

fn check_exact_agent(agent: &str) -> Result<()> {
    if agent.contains(['*', '?', '[']) {
        bail!("--agent must be an exact agent name when a session id is given, not '{agent}'");
    }
    Ok(())
}

fn validate(args: &InfoScriptUsageArgs, now: DateTime<Utc>) -> Result<Request> {
    match (&args.agent, &args.session_id) {
        (Some(agent), Some(session_id)) => {
            check_exact_agent(agent)?;
            Ok(Request::Session {
                agent: agent.clone(),
                session_id: session_id.clone(),
            })
        }
        (None, Some(_)) => bail!("a session id requires --agent"),
        (agent, None) => {
            if let Some(pattern) = agent {
                check_agent_pattern(pattern)?;
            }
            Ok(Request::Report(ScriptUsageScope {
                agent_pattern: agent.clone(),
                window: parse_window(args, now)?,
            }))
        }
    }
}

/// Reading a large cluster takes minutes, so say how much is coming. stdout
/// carries the report alone.
fn announce_reads(count: u64) {
    eprintln!("{}", reading_message(count));
}

fn reading_message(count: u64) -> String {
    let noun = if count == 1 { "log" } else { "logs" };
    format!("reading {} session {noun}...", num(count))
}

pub async fn run(args: &InfoScriptUsageArgs) -> Result<u8> {
    let request = validate(args, Utc::now())?;
    let config = init_frontend_config(WorkingMode::Cmd, true).await?;
    match request {
        Request::Report(scope) => {
            let report =
                collect_script_usage(&config, &args.cluster, &scope, &announce_reads).await?;
            if args.json {
                print_json(&report)?;
            } else {
                print!("{}", render_report_text(&report));
            }
        }
        Request::Session { agent, session_id } => {
            let detail =
                collect_session_script_usage(&config, &args.cluster, &agent, &session_id).await?;
            if args.json {
                print_json(&detail)?;
            } else {
                print!("{}", render_session_text(&detail));
            }
        }
    }
    Ok(0)
}

// The typed report is the artifact: print only JSON, no header or trailing
// chatter, so stdout pipes straight into jq.
fn print_json<T: serde::Serialize>(value: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 26, 12, 0, 0)
            .single()
            .unwrap()
    }

    fn args(agent: Option<&str>, session: Option<&str>) -> InfoScriptUsageArgs {
        InfoScriptUsageArgs {
            cluster: "prod".to_string(),
            agent: agent.map(str::to_string),
            session_id: session.map(str::to_string),
            since: None,
            until: None,
            json: false,
        }
    }

    #[test]
    fn the_reading_message_groups_digits_and_singularizes() {
        assert_eq!(reading_message(0), "reading 0 session logs...");
        assert_eq!(reading_message(1), "reading 1 session log...");
        assert_eq!(reading_message(999), "reading 999 session logs...");
        assert_eq!(reading_message(1234), "reading 1,234 session logs...");
        assert_eq!(
            reading_message(1_234_567),
            "reading 1,234,567 session logs..."
        );
    }

    #[test]
    fn parse_when_reads_rfc3339() {
        let at = parse_when("2026-11-20T10:00:00Z", now()).unwrap();
        assert_eq!(at, Utc.with_ymd_and_hms(2026, 11, 20, 10, 0, 0).unwrap());
        let offset = parse_when("2026-11-20T10:00:00+02:00", now()).unwrap();
        assert_eq!(offset, Utc.with_ymd_and_hms(2026, 11, 20, 8, 0, 0).unwrap());
    }

    #[test]
    fn parse_when_reads_a_date_as_utc_midnight() {
        let at = parse_when("2026-11-20", now()).unwrap();
        assert_eq!(at, Utc.with_ymd_and_hms(2026, 11, 20, 0, 0, 0).unwrap());
    }

    #[test]
    fn parse_when_subtracts_a_duration_from_now() {
        let week = parse_when("7d", now()).unwrap();
        assert_eq!(week, Utc.with_ymd_and_hms(2026, 11, 19, 12, 0, 0).unwrap());
        let day_and_half = parse_when("36h", now()).unwrap();
        assert_eq!(
            day_and_half,
            Utc.with_ymd_and_hms(2026, 11, 25, 0, 0, 0).unwrap()
        );
    }

    #[test]
    fn parse_when_rejects_other_text_and_names_the_forms() {
        let message = parse_when("yesterday-ish", now()).unwrap_err().to_string();
        assert!(message.contains("yesterday-ish"), "{message}");
        assert!(message.contains("RFC 3339"), "{message}");
        assert!(message.contains("date"), "{message}");
        assert!(message.contains("duration"), "{message}");
    }

    #[test]
    fn validate_names_the_bad_window_argument() {
        let mut bad = args(None, None);
        bad.until = Some("soon".to_string());
        let message = format!("{:#}", validate(&bad, now()).unwrap_err());
        assert!(message.contains("--until"), "{message}");
    }

    #[test]
    fn validate_rejects_an_empty_or_reversed_window() {
        let mut reversed = args(None, None);
        reversed.since = Some("2026-11-20".to_string());
        reversed.until = Some("2026-11-20".to_string());
        let message = validate(&reversed, now()).unwrap_err().to_string();
        assert!(message.contains("--since"), "{message}");
    }

    #[test]
    fn validate_builds_a_window_from_both_bounds() {
        let mut windowed = args(Some("pantheon*"), None);
        windowed.since = Some("2026-11-01".to_string());
        windowed.until = Some("1d".to_string());
        match validate(&windowed, now()).unwrap() {
            Request::Report(scope) => {
                assert_eq!(scope.agent_pattern.as_deref(), Some("pantheon*"));
                assert_eq!(
                    scope.window.since,
                    Some(Utc.with_ymd_and_hms(2026, 11, 1, 0, 0, 0).unwrap())
                );
                assert_eq!(
                    scope.window.until,
                    Some(Utc.with_ymd_and_hms(2026, 11, 25, 12, 0, 0).unwrap())
                );
            }
            other => panic!("unexpected request: {other:?}"),
        }
    }

    #[test]
    fn validate_rejects_an_invalid_glob() {
        let message = format!(
            "{:#}",
            validate(&args(Some("pantheon/[a"), None), now()).unwrap_err()
        );
        assert!(message.contains("--agent"), "{message}");
    }

    #[test]
    fn validate_accepts_a_star_that_crosses_slashes() {
        assert!(validate(&args(Some("pantheon*"), None), now()).is_ok());
        assert!(validate(&args(Some("*/atlas"), None), now()).is_ok());
    }

    #[test]
    fn validate_requires_an_exact_agent_with_a_session_id() {
        for pattern in ["pantheon/*", "a?b", "a[bc]"] {
            let message = validate(&args(Some(pattern), Some("sess-1")), now())
                .unwrap_err()
                .to_string();
            assert!(message.contains("--agent"), "{message}");
        }
        match validate(&args(Some("pantheon/atlas"), Some("sess-1")), now()).unwrap() {
            Request::Session { agent, session_id } => {
                assert_eq!(agent, "pantheon/atlas");
                assert_eq!(session_id, "sess-1");
            }
            other => panic!("unexpected request: {other:?}"),
        }
    }
}
