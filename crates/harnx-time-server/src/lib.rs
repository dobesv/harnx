use async_trait::async_trait;
use chrono::{Datelike, Offset, TimeZone, Utc};
use chrono_tz::Tz;
use harnx_toolset::{ToolInvokeError, ToolSpec, Toolset};
use jiff::{civil, Span, Timestamp};
use serde::Deserialize;
use serde_json::{json, Value};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

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

        let timestamp = if let Some(value) = args.iso_timestamp.as_deref() {
            parse_iso_timestamp(value, args.source_timezone.as_deref())?
        } else if let Some(value) = args.unix_timestamp {
            parse_unix_timestamp(value)?
        } else if let Some(value) = args.epoch_millis {
            Timestamp::from_millisecond(value).map_err(recoverable)?
        } else {
            Timestamp::now()
        };

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
        let timestamp = timestamp.checked_add(span).map_err(recoverable)?;
        let formatted = if let Some(timezone) = args.timezone.as_deref() {
            timestamp.in_tz(timezone).map_err(recoverable)?.to_string()
        } else {
            timestamp.to_string()
        };

        Ok(json!({
            "timestamp": formatted,
            "unixTimestamp": timestamp.as_second(),
            "epochMillis": timestamp.as_millisecond(),
        }))
    }

    async fn wait(&self, args: Value, cancel: CancellationToken) -> Result<Value, ToolInvokeError> {
        let args: WaitParams = parse_args(args)?;
        if !(0.0..=3600.0).contains(&args.seconds) || args.seconds == 0.0 {
            return Err(ToolInvokeError::Recoverable(
                "seconds must be positive and no greater than 3600".to_string(),
            ));
        }
        let duration = Duration::from_secs_f64(args.seconds);
        tokio::select! {
            _ = tokio::time::sleep(duration) => Ok(json!({
                "message": format!("Waited {:.1} seconds", args.seconds)
            })),
            _ = cancel.cancelled() => Err(ToolInvokeError::Fatal("tool call cancelled".to_string())),
        }
    }

    async fn wait_until(
        &self,
        args: Value,
        cancel: CancellationToken,
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
        tokio::select! {
            _ = tokio::time::sleep(duration) => Ok(json!({
                "target": target.format("%Y-%m-%dT%H:%M:%S%:z").to_string(),
                "waited_seconds": duration.as_secs_f64(),
            })),
            _ = cancel.cancelled() => Err(ToolInvokeError::Fatal("tool call cancelled".to_string())),
        }
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

    fn tools(&self) -> Vec<ToolSpec> {
        vec![
            ToolSpec {
                name: "get_current_time".to_string(),
                description: "Get current time in a specific timezone".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": { "timezone": { "type": "string" } },
                    "required": ["timezone"]
                }),
                idempotent_hint: true,
                read_only_hint: true,
            },
            ToolSpec {
                name: "convert_time".to_string(),
                description: "Convert timestamps, timezones, and time offsets".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "isoTimestamp": { "type": "string" },
                        "unixTimestamp": { "type": "number" },
                        "epochMillis": { "type": "integer" },
                        "offsetSeconds": { "type": "integer" },
                        "offsetMinutes": { "type": "integer" },
                        "offsetHours": { "type": "integer" },
                        "offsetDays": { "type": "integer" },
                        "timezone": { "type": "string" },
                        "sourceTimezone": { "type": "string" }
                    }
                }),
                idempotent_hint: true,
                read_only_hint: true,
            },
            ToolSpec {
                name: "wait".to_string(),
                description: "Wait for a specified number of seconds".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": { "seconds": { "type": "number" } },
                    "required": ["seconds"]
                }),
                idempotent_hint: false,
                read_only_hint: true,
            },
            ToolSpec {
                name: "wait_until".to_string(),
                description: "Wait until a target time, up to 24 hours".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "time": { "type": "string" },
                        "timezone": { "type": "string" }
                    },
                    "required": ["time"]
                }),
                idempotent_hint: false,
                read_only_hint: true,
            },
        ]
    }

    async fn invoke(
        &self,
        tool: &str,
        args: Value,
        cancel: CancellationToken,
    ) -> Result<Value, ToolInvokeError> {
        match tool {
            "get_current_time" => self.current_time(args),
            "convert_time" => self.convert_time(args),
            "wait" => self.wait(args, cancel).await,
            "wait_until" => self.wait_until(args, cancel).await,
            _ => Err(ToolInvokeError::Recoverable(format!(
                "unknown time tool: {tool}"
            ))),
        }
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
    let nanos = value * 1_000_000_000.0;
    if !nanos.is_finite() || nanos < i128::MIN as f64 || nanos > i128::MAX as f64 {
        return Err(ToolInvokeError::Recoverable(
            "unixTimestamp is out of range".to_string(),
        ));
    }
    Timestamp::from_nanosecond(nanos.round() as i128).map_err(recoverable)
}

fn recoverable(error: impl std::fmt::Display) -> ToolInvokeError {
    ToolInvokeError::Recoverable(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[tokio::test]
    async fn wait_honors_cancellation() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let result = TimeToolset::new()
            .invoke("wait", json!({ "seconds": 30.0 }), cancel)
            .await;
        assert!(matches!(result, Err(ToolInvokeError::Fatal(_))));
    }
}
