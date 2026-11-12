mod tool_specs;
mod tool_templates;

use async_trait::async_trait;
use chrono::{Datelike, Offset, TimeZone, Utc};
use chrono_tz::Tz;
use harnx_toolset::{ToolInvocation, ToolInvokeError, ToolSpec, Toolset};
use jiff::{civil, Span, Timestamp};
use serde::Deserialize;
use serde_json::{json, Value};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// RFC 3339 at second precision with the offset, matching `wait_until`'s target.
const RFC3339_SECONDS: &str = "%Y-%m-%dT%H:%M:%S%:z";

#[derive(Clone)]
pub struct TimeToolset {
    local_tz: String,
}

impl TimeToolset {
    pub fn new() -> Self {
        let local_tz = iana_time_zone::get_timezone().unwrap_or_else(|_| "UTC".to_string());
        Self { local_tz }
    }

    fn current_time(&self, args: Value) -> Result<Value, ToolInvokeError> {
        let args: GetCurrentTimeParams = parse_args(args)?;
        let timezone = if args.timezone.is_empty() {
            self.local_tz.as_str()
        } else {
            args.timezone.as_str()
        };
        let tz: Tz = timezone
            .parse()
            .map_err(|_| ToolInvokeError::Recoverable(format!("Invalid timezone: {timezone}")))?;
        let now = Utc::now().with_timezone(&tz);
        let current_offset = now.offset().fix().local_minus_utc();
        let jan1 = tz
            .with_ymd_and_hms(now.year(), 1, 1, 12, 0, 0)
            .single()
            .map(|datetime| datetime.offset().fix().local_minus_utc());
        let jul1 = tz
            .with_ymd_and_hms(now.year(), 7, 1, 12, 0, 0)
            .single()
            .map(|datetime| datetime.offset().fix().local_minus_utc());
        let standard_offset = match (jan1, jul1) {
            (Some(january), Some(july)) => january.min(july),
            _ => current_offset,
        };

        Ok(json!({
            "timezone": timezone,
            "datetime": now.format("%Y-%m-%dT%H:%M:%S%:z").to_string(),
            "day_of_week": now.format("%A").to_string(),
            "is_dst": current_offset != standard_offset,
        }))
    }

    fn convert_time(&self, args: Value) -> Result<Value, ToolInvokeError> {
        let args: ConvertTimeParams = parse_args(args)?;
        let timestamp = conversion_timestamp(&args)?;
        let timestamp = timestamp
            .checked_add(conversion_offset(&args)?)
            .map_err(recoverable)?;
        let formatted = format_timestamp(timestamp, args.timezone.as_deref())?;

        Ok(json!({
            "timestamp": formatted,
            "unixTimestamp": timestamp.as_second(),
            "epochMillis": timestamp.as_millisecond(),
        }))
    }

    async fn wait(&self, args: Value, signals: WaitSignals) -> Result<Value, ToolInvokeError> {
        let args: WaitParams = parse_args(args)?;
        if !(0.0..=3600.0).contains(&args.seconds) || args.seconds == 0.0 {
            return Err(ToolInvokeError::Recoverable(
                "seconds must be positive and no greater than 3600".to_string(),
            ));
        }
        let duration = Duration::from_secs_f64(args.seconds);
        let tz: Tz = self.local_tz.parse().unwrap_or(chrono_tz::UTC);
        let started_at = Utc::now().with_timezone(&tz);
        let stopped_after = signals.sleep(duration).await?;
        let message = match stopped_after {
            None => format!("Waited {:.1} seconds", args.seconds),
            Some(waited) => format!(
                "Waited {:.1} of {:.1} seconds; a new message arrived",
                waited.as_secs_f64(),
                args.seconds
            ),
        };
        let mut result = json!({
            "message": message,
            "started_at": started_at.format(RFC3339_SECONDS).to_string(),
            "ended_at": Utc::now().with_timezone(&tz).format(RFC3339_SECONDS).to_string(),
        });
        mark_stopped(&mut result, stopped_after);
        Ok(result)
    }

    async fn wait_until(
        &self,
        args: Value,
        signals: WaitSignals,
    ) -> Result<Value, ToolInvokeError> {
        let args: WaitUntilParams = parse_args(args)?;
        let timezone = args.timezone.as_deref().unwrap_or(&self.local_tz);
        let tz: Tz = timezone
            .parse()
            .map_err(|_| ToolInvokeError::Recoverable(format!("Invalid timezone: {timezone}")))?;
        let now = Utc::now().with_timezone(&tz);
        let target_naive = chrono::NaiveDateTime::parse_from_str(&args.time, "%Y-%m-%dT%H:%M:%S")
            .or_else(|_| chrono::NaiveDateTime::parse_from_str(&args.time, "%Y-%m-%dT%H:%M"))
            .or_else(|_| {
                args.time
                    .parse::<chrono::NaiveTime>()
                    .map(|time| now.date_naive().and_time(time))
            })
            .map_err(|_| {
                ToolInvokeError::Recoverable(
                    "invalid time format; expected HH:MM, YYYY-MM-DDTHH:MM, or YYYY-MM-DDTHH:MM:SS"
                        .to_string(),
                )
            })?;
        let mut target = tz
            .from_local_datetime(&target_naive)
            .single()
            .ok_or_else(|| {
                ToolInvokeError::Recoverable("ambiguous or invalid local time".to_string())
            })?;
        let mut duration = target.signed_duration_since(now);
        if duration < chrono::Duration::zero() && !args.time.contains('-') {
            target += chrono::Duration::days(1);
            duration = target.signed_duration_since(now);
        }
        if duration < chrono::Duration::zero() || duration > chrono::Duration::hours(24) {
            return Err(ToolInvokeError::Recoverable(
                "target must be within the next 24 hours".to_string(),
            ));
        }
        let duration = duration.to_std().map_err(recoverable)?;
        let stopped_after = signals.sleep(duration).await?;
        let mut result = json!({
            "target": target.format(RFC3339_SECONDS).to_string(),
            "waited_seconds": stopped_after.unwrap_or(duration).as_secs_f64(),
            "started_at": now.format(RFC3339_SECONDS).to_string(),
            "ended_at": Utc::now().with_timezone(&tz).format(RFC3339_SECONDS).to_string(),
        });
        mark_stopped(&mut result, stopped_after);
        Ok(result)
    }

    async fn dispatch(
        &self,
        tool: &str,
        args: Value,
        signals: WaitSignals,
    ) -> Result<Value, ToolInvokeError> {
        match tool {
            "get_current_time" => self.current_time(args),
            "convert_time" => self.convert_time(args),
            "wait" => self.wait(args, signals).await,
            "wait_until" => self.wait_until(args, signals).await,
            _ => Err(ToolInvokeError::Recoverable(format!(
                "unknown time tool: {tool}"
            ))),
        }
    }
}

/// What can end a wait before its time. Cancellation fails the call; a new
/// message for the caller's session ends it early with its normal result.
struct WaitSignals {
    cancel: CancellationToken,
    new_message: CancellationToken,
}

impl WaitSignals {
    /// Sleep for `duration`. Returns how long the wait lasted when a new
    /// message ended it early, and `None` when it ran its full course.
    async fn sleep(&self, duration: Duration) -> Result<Option<Duration>, ToolInvokeError> {
        let started = tokio::time::Instant::now();
        tokio::select! {
            biased;
            _ = self.cancel.cancelled() => {
                Err(ToolInvokeError::Fatal("tool call cancelled".to_string()))
            }
            _ = self.new_message.cancelled() => Ok(Some(started.elapsed())),
            _ = tokio::time::sleep(duration) => Ok(None),
        }
    }
}

/// Name the new message in a wait's result when one ended it early.
fn mark_stopped(result: &mut Value, stopped_after: Option<Duration>) {
    if stopped_after.is_some() {
        result["stopped_by"] = json!("new_message");
    }
}

impl Default for TimeToolset {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Toolset for TimeToolset {
    fn name(&self) -> &str {
        "time"
    }

    fn default_mcp_http_port(&self) -> u16 {
        3001
    }

    fn tools(&self) -> Vec<ToolSpec> {
        tool_specs::all()
    }

    async fn invoke(
        &self,
        tool: &str,
        args: Value,
        cancel: CancellationToken,
    ) -> Result<Value, ToolInvokeError> {
        let signals = WaitSignals {
            cancel,
            new_message: CancellationToken::new(),
        };
        self.dispatch(tool, args, signals).await
    }

    async fn invoke_with_context(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Value, ToolInvokeError> {
        let signals = WaitSignals {
            cancel: invocation.cancel,
            new_message: invocation.context.new_message,
        };
        self.dispatch(&invocation.tool, invocation.args, signals)
            .await
    }
}

#[derive(Deserialize)]
struct GetCurrentTimeParams {
    #[serde(default)]
    timezone: String,
}

#[derive(Deserialize)]
struct ConvertTimeParams {
    #[serde(rename = "isoTimestamp", default)]
    iso_timestamp: Option<String>,
    #[serde(rename = "unixTimestamp", default)]
    unix_timestamp: Option<f64>,
    #[serde(rename = "epochMillis", default)]
    epoch_millis: Option<i64>,
    #[serde(rename = "offsetSeconds", default)]
    offset_seconds: Option<i64>,
    #[serde(rename = "offsetMinutes", default)]
    offset_minutes: Option<i64>,
    #[serde(rename = "offsetHours", default)]
    offset_hours: Option<i64>,
    #[serde(rename = "offsetDays", default)]
    offset_days: Option<i64>,
    #[serde(default)]
    timezone: Option<String>,
    #[serde(rename = "sourceTimezone", default)]
    source_timezone: Option<String>,
}

fn conversion_timestamp(args: &ConvertTimeParams) -> Result<Timestamp, ToolInvokeError> {
    let base_inputs = [
        args.iso_timestamp.is_some(),
        args.unix_timestamp.is_some(),
        args.epoch_millis.is_some(),
    ];
    if base_inputs.into_iter().filter(|present| *present).count() > 1 {
        return Err(ToolInvokeError::Recoverable(
            "provide only one of isoTimestamp, unixTimestamp, or epochMillis".to_string(),
        ));
    }
    if let Some(value) = args.iso_timestamp.as_deref() {
        return parse_iso_timestamp(value, args.source_timezone.as_deref());
    }
    if let Some(value) = args.unix_timestamp {
        return parse_unix_timestamp(value);
    }
    if let Some(value) = args.epoch_millis {
        return Timestamp::from_millisecond(value).map_err(recoverable);
    }
    Ok(Timestamp::now())
}

fn conversion_offset(args: &ConvertTimeParams) -> Result<Span, ToolInvokeError> {
    let mut span = Span::new();
    if let Some(value) = args.offset_days {
        span = span.try_days(value).map_err(recoverable)?;
    }
    if let Some(value) = args.offset_hours {
        span = span.try_hours(value).map_err(recoverable)?;
    }
    if let Some(value) = args.offset_minutes {
        span = span.try_minutes(value).map_err(recoverable)?;
    }
    if let Some(value) = args.offset_seconds {
        span = span.try_seconds(value).map_err(recoverable)?;
    }
    Ok(span)
}

fn format_timestamp(
    timestamp: Timestamp,
    timezone: Option<&str>,
) -> Result<String, ToolInvokeError> {
    match timezone {
        Some(timezone) => timestamp
            .in_tz(timezone)
            .map(|timestamp| timestamp.to_string())
            .map_err(recoverable),
        None => Ok(timestamp.to_string()),
    }
}
#[derive(Deserialize)]
struct WaitParams {
    seconds: f64,
}

#[derive(Deserialize)]
struct WaitUntilParams {
    time: String,
    #[serde(default)]
    timezone: Option<String>,
}

fn parse_args<T>(args: Value) -> Result<T, ToolInvokeError>
where
    T: for<'de> Deserialize<'de>,
{
    serde_json::from_value(args)
        .map_err(|error| ToolInvokeError::Recoverable(format!("invalid tool arguments: {error}")))
}

fn parse_iso_timestamp(
    value: &str,
    source_timezone: Option<&str>,
) -> Result<Timestamp, ToolInvokeError> {
    if let Ok(timestamp) = value.parse::<Timestamp>() {
        return Ok(timestamp);
    }
    let source_timezone = source_timezone.ok_or_else(|| {
        ToolInvokeError::Recoverable(
            "isoTimestamp is missing timezone information; provide sourceTimezone".to_string(),
        )
    })?;
    let datetime = value
        .parse::<civil::DateTime>()
        .map_err(|_| ToolInvokeError::Recoverable(format!("Invalid isoTimestamp: {value}")))?;
    datetime
        .in_tz(source_timezone)
        .map(|zoned| zoned.timestamp())
        .map_err(recoverable)
}

fn parse_unix_timestamp(value: f64) -> Result<Timestamp, ToolInvokeError> {
    if !value.is_finite() {
        return Err(ToolInvokeError::Recoverable(
            "unixTimestamp must be finite".to_string(),
        ));
    }
    let nanos = checked_unix_nanos(value)?;
    Timestamp::from_nanosecond(nanos).map_err(recoverable)
}

fn checked_unix_nanos(value: f64) -> Result<i128, ToolInvokeError> {
    let nanos = value * 1_000_000_000.0;
    if !nanos.is_finite() {
        return Err(unix_timestamp_out_of_range());
    }
    if !((i128::MIN as f64)..=(i128::MAX as f64)).contains(&nanos) {
        return Err(unix_timestamp_out_of_range());
    }
    Ok(nanos.round() as i128)
}

fn unix_timestamp_out_of_range() -> ToolInvokeError {
    ToolInvokeError::Recoverable("unixTimestamp is out of range".to_string())
}

fn recoverable(error: impl std::fmt::Display) -> ToolInvokeError {
    ToolInvokeError::Recoverable(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_tool_advertises_a_call_template() {
        for spec in TimeToolset::new().tools() {
            let call_template = spec
                .meta
                .as_ref()
                .and_then(|meta| meta.get("call_template"))
                .and_then(Value::as_str)
                .unwrap_or_else(|| panic!("tool '{}' has no call_template", spec.name));
            assert!(
                !call_template.is_empty(),
                "tool '{}' has an empty call_template",
                spec.name
            );
        }
    }

    #[tokio::test]
    async fn current_time_returns_expected_shape() {
        let result = TimeToolset::new()
            .invoke(
                "get_current_time",
                json!({ "timezone": "UTC" }),
                CancellationToken::new(),
            )
            .await
            .expect("get current time");
        assert_eq!(result["timezone"], "UTC");
        assert!(result["datetime"].as_str().is_some());
    }

    #[test]
    fn tool_specs_advertise_defaults_and_execution_bounds() {
        let tools = TimeToolset::new().tools();
        let current = tools
            .iter()
            .find(|tool| tool.name == "get_current_time")
            .expect("current-time spec");
        assert!(current.input_schema.get("required").is_none());
        let wait = tools
            .iter()
            .find(|tool| tool.name == "wait")
            .expect("wait spec");
        assert!(wait.timeout_secs.expect("wait timeout") > 3_600);
        let wait_until = tools
            .iter()
            .find(|tool| tool.name == "wait_until")
            .expect("wait-until spec");
        assert!(wait_until.timeout_secs.expect("wait-until timeout") > 86_400);
    }

    #[tokio::test]
    async fn wait_honors_cancellation() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let result = TimeToolset::new()
            .invoke("wait", json!({ "seconds": 30.0 }), cancel)
            .await;
        assert!(matches!(result, Err(ToolInvokeError::Fatal(_))));
    }

    #[tokio::test]
    async fn wait_reports_start_and_end_times() {
        let result = TimeToolset::new()
            .invoke("wait", json!({ "seconds": 0.2 }), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(result["message"], "Waited 0.2 seconds");
        assert!(result.get("stopped_by").is_none());
        let started =
            chrono::DateTime::parse_from_rfc3339(result["started_at"].as_str().unwrap()).unwrap();
        let ended =
            chrono::DateTime::parse_from_rfc3339(result["ended_at"].as_str().unwrap()).unwrap();
        assert!(ended >= started);
    }

    /// A new-message signal that fires after `delay`.
    fn new_message_after(delay: Duration) -> CancellationToken {
        let new_message = CancellationToken::new();
        let signal = new_message.clone();
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            signal.cancel();
        });
        new_message
    }

    /// Invoke `tool` with a new-message signal that fires after 200 ms.
    async fn invoke_until_new_message(tool: &str, args: Value) -> (Value, Duration) {
        invoke_with_new_message(tool, args, new_message_after(Duration::from_millis(200))).await
    }

    async fn invoke_with_new_message(
        tool: &str,
        args: Value,
        new_message: CancellationToken,
    ) -> (Value, Duration) {
        let started = std::time::Instant::now();
        let result = TimeToolset::new()
            .invoke_with_context(harnx_toolset::ToolInvocation {
                tool: tool.into(),
                args,
                context: harnx_toolset::ToolInvocationContext {
                    new_message,
                    ..Default::default()
                },
                cancel: CancellationToken::new(),
            })
            .await
            .expect("a new message ends the wait with a result");
        (result, started.elapsed())
    }

    #[tokio::test]
    async fn wait_returns_early_on_new_message() {
        let (result, elapsed) = invoke_until_new_message("wait", json!({ "seconds": 30.0 })).await;
        assert!(elapsed < Duration::from_secs(5), "waited {elapsed:?}");
        assert_eq!(result["stopped_by"], "new_message");
        assert!(result["started_at"].is_string() && result["ended_at"].is_string());
    }

    #[tokio::test]
    async fn wait_until_returns_early_on_new_message() {
        let target = (Utc::now() + chrono::Duration::seconds(30))
            .format("%Y-%m-%dT%H:%M:%S")
            .to_string();
        let (result, elapsed) =
            invoke_until_new_message("wait_until", json!({ "time": target, "timezone": "UTC" }))
                .await;
        assert!(elapsed < Duration::from_secs(5), "waited {elapsed:?}");
        assert_eq!(result["stopped_by"], "new_message");
        assert!(result["waited_seconds"].as_f64().unwrap() < 5.0);
    }

    /// The tool server fires the signal before the call starts when a message
    /// was already pending at dispatch; the wait then returns at once.
    #[tokio::test]
    async fn wait_returns_at_once_when_a_new_message_is_already_pending() {
        let new_message = CancellationToken::new();
        new_message.cancel();
        let (result, elapsed) =
            invoke_with_new_message("wait", json!({ "seconds": 30.0 }), new_message).await;
        assert!(elapsed < Duration::from_secs(5), "waited {elapsed:?}");
        assert_eq!(result["stopped_by"], "new_message");
    }

    #[test]
    fn only_the_waits_stop_on_new_message() {
        let opted_in: Vec<String> = TimeToolset::new()
            .tools()
            .into_iter()
            .filter(|spec| spec.stops_on_new_message())
            .map(|spec| spec.name)
            .collect();
        assert_eq!(opted_in, ["wait", "wait_until"]);
    }

    #[tokio::test]
    async fn wait_until_reports_start_and_end_times() {
        // The target is two seconds out, truncated to whole seconds, so the
        // wait is between one and two seconds and never already in the past.
        let target = (Utc::now() + chrono::Duration::seconds(2))
            .format("%Y-%m-%dT%H:%M:%S")
            .to_string();
        let result = TimeToolset::new()
            .invoke(
                "wait_until",
                json!({ "time": target, "timezone": "UTC" }),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(result["target"], format!("{target}+00:00"));
        assert!(result["waited_seconds"].as_f64().unwrap() > 0.0);
        let started =
            chrono::DateTime::parse_from_rfc3339(result["started_at"].as_str().unwrap()).unwrap();
        let ended =
            chrono::DateTime::parse_from_rfc3339(result["ended_at"].as_str().unwrap()).unwrap();
        assert!(ended >= started);
    }
}
