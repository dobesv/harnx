use chrono::{Datelike, Offset, TimeZone, Utc};
use chrono_tz::Tz;
use jiff::{civil, Span, Timestamp};
use rmcp::model::{
    CallToolRequestParams, CallToolResult, Content, ErrorData, Implementation, ListToolsResult,
    PaginatedRequestParams, Role, ServerCapabilities, ServerInfo, Tool, ToolAnnotations,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::ServerHandler;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};

#[derive(Clone)]
pub struct TimeServer {
    local_tz: String,
}

impl TimeServer {
    pub fn new() -> Self {
        let local_tz = iana_time_zone::get_timezone().unwrap_or_else(|_| "UTC".to_string());
        Self { local_tz }
    }

    fn get_current_time_impl(&self, timezone: &str) -> Result<CallToolResult, ErrorData> {
        let tz: Tz = timezone.parse().map_err(|_| {
            ErrorData::invalid_params(format!("Invalid timezone: {timezone}"), None)
        })?;

        let now = Utc::now().with_timezone(&tz);
        let current_offset = now.offset().fix().local_minus_utc();
        let jan1 = tz
            .with_ymd_and_hms(now.year(), 1, 1, 12, 0, 0)
            .single()
            .map(|dt| dt.offset().fix().local_minus_utc());
        let jul1 = tz
            .with_ymd_and_hms(now.year(), 7, 1, 12, 0, 0)
            .single()
            .map(|dt| dt.offset().fix().local_minus_utc());
        let standard_offset = match (jan1, jul1) {
            (Some(j), Some(ju)) => j.min(ju),
            _ => current_offset,
        };
        let is_dst = current_offset != standard_offset;

        let datetime_str = now.format("%Y-%m-%dT%H:%M:%S%:z").to_string();
        let result = serde_json::json!({
            "timezone": timezone,
            "datetime": &datetime_str,
            "day_of_week": now.format("%A").to_string(),
            "is_dst": is_dst,
        });

        let full = serde_json::to_string_pretty(&result).unwrap_or_default();
        let summary = format!("Current time in {timezone}: {datetime_str}");
        Ok(CallToolResult::success(vec![
            Content::text(full).with_audience(vec![Role::Assistant]),
            Content::text(summary).with_audience(vec![Role::User]),
        ]))
    }

    fn convert_time_impl(&self, args: ConvertTimeParams) -> Result<CallToolResult, ErrorData> {
        let base_inputs = [
            args.iso_timestamp.is_some(),
            args.unix_timestamp.is_some(),
            args.epoch_millis.is_some(),
        ];
        let provided_count = base_inputs.into_iter().filter(|provided| *provided).count();

        if provided_count > 1 {
            return Err(ErrorData::invalid_params(
                "Provide at most one of isoTimestamp, unixTimestamp, or epochMillis",
                None,
            ));
        }

        let timestamp = if let Some(iso_timestamp) = args.iso_timestamp.as_deref() {
            parse_iso_timestamp(iso_timestamp, args.source_timezone.as_deref())?
        } else if let Some(unix_timestamp) = args.unix_timestamp {
            parse_unix_timestamp(unix_timestamp)?
        } else if let Some(epoch_millis) = args.epoch_millis {
            parse_epoch_millis(epoch_millis)?
        } else {
            Timestamp::now()
        };

        let mut span = Span::new();
        if let Some(days) = args.offset_days {
            span = span.try_days(days).map_err(invalid_params)?;
        }
        if let Some(hours) = args.offset_hours {
            span = span.try_hours(hours).map_err(invalid_params)?;
        }
        if let Some(minutes) = args.offset_minutes {
            span = span.try_minutes(minutes).map_err(invalid_params)?;
        }
        if let Some(seconds) = args.offset_seconds {
            span = span.try_seconds(seconds).map_err(invalid_params)?;
        }

        let timestamp = timestamp.checked_add(span).map_err(invalid_params)?;

        let formatted_timestamp = if let Some(timezone) = args.timezone.as_deref() {
            timestamp
                .in_tz(timezone)
                .map_err(invalid_params)?
                .to_string()
        } else {
            timestamp.to_string()
        };

        let unix_timestamp = timestamp.as_second();
        let epoch_millis = timestamp.as_millisecond();

        let result = serde_json::json!({
            "timestamp": formatted_timestamp,
            "unixTimestamp": unix_timestamp,
            "epochMillis": epoch_millis,
        });

        let full = serde_json::to_string_pretty(&result).unwrap_or_default();
        let summary = format!(
            "{} (unix: {}, epochMillis: {})",
            result["timestamp"].as_str().unwrap_or_default(),
            unix_timestamp,
            epoch_millis,
        );
        Ok(CallToolResult::success(vec![
            Content::text(full).with_audience(vec![Role::Assistant]),
            Content::text(summary).with_audience(vec![Role::User]),
        ]))
    }

    async fn wait_impl(&self, seconds: f64) -> Result<CallToolResult, ErrorData> {
        if seconds <= 0.0 {
            return Err(ErrorData::invalid_params("seconds must be positive", None));
        }
        if seconds > 3600.0 {
            return Err(ErrorData::invalid_params(
                "maximum wait is 3600 seconds (1 hour)",
                None,
            ));
        }

        let duration = std::time::Duration::from_secs_f64(seconds);
        tokio::time::sleep(duration).await;

        Ok(CallToolResult::success(vec![Content::text(format!(
            "Waited {seconds:.1} seconds",
        ))]))
    }

    async fn wait_until_impl(
        &self,
        time_str: &str,
        timezone: Option<&str>,
    ) -> Result<CallToolResult, ErrorData> {
        let tz: Tz = timezone.unwrap_or(&self.local_tz).parse().map_err(|_| {
            ErrorData::invalid_params(
                format!("Invalid timezone: {}", timezone.unwrap_or(&self.local_tz)),
                None,
            )
        })?;

        let now = Utc::now().with_timezone(&tz);

        let target_naive = chrono::NaiveDateTime::parse_from_str(time_str, "%Y-%m-%dT%H:%M:%S")
            .or_else(|_| chrono::NaiveDateTime::parse_from_str(time_str, "%Y-%m-%dT%H:%M"))
            .or_else(|_| {
                time_str
                    .parse::<chrono::NaiveTime>()
                    .map(|t| now.date_naive().and_time(t))
            })
            .map_err(|_| {
                ErrorData::invalid_params(
                    "Invalid time format. Expected HH:MM, YYYY-MM-DDTHH:MM, or YYYY-MM-DDTHH:MM:SS",
                    None,
                )
            })?;

        let target_dt = tz
            .from_local_datetime(&target_naive)
            .single()
            .ok_or_else(|| ErrorData::invalid_params("Ambiguous or invalid local time", None))?;

        let mut actual_target = target_dt;
        let wait_duration = actual_target.signed_duration_since(now);

        let wait_duration = if wait_duration < chrono::Duration::zero() && !time_str.contains('-') {
            actual_target += chrono::Duration::days(1);
            actual_target.signed_duration_since(now)
        } else {
            wait_duration
        };

        if wait_duration < chrono::Duration::zero() {
            return Err(ErrorData::invalid_params(
                format!(
                    "Target time {} is in the past (current: {})",
                    actual_target.format("%Y-%m-%dT%H:%M:%S%:z"),
                    now.format("%Y-%m-%dT%H:%M:%S%:z"),
                ),
                None,
            ));
        }

        let max_wait = chrono::Duration::hours(24);
        if wait_duration > max_wait {
            return Err(ErrorData::invalid_params(
                format!(
                    "Wait duration ({:.0} minutes) exceeds maximum (24 hours)",
                    wait_duration.num_seconds() as f64 / 60.0,
                ),
                None,
            ));
        }

        let secs = wait_duration.num_seconds().max(0) as u64;
        let nanos = (wait_duration.num_milliseconds().max(0) as u64 % 1000) * 1_000_000;
        let duration = std::time::Duration::new(secs, nanos as u32);
        tokio::time::sleep(duration).await;

        let arrived = Utc::now().with_timezone(&tz);
        Ok(CallToolResult::success(vec![Content::text(format!(
            "Waited until {} ({:.0} seconds). Current time: {}",
            actual_target.format("%Y-%m-%dT%H:%M:%S%:z"),
            duration.as_secs_f64(),
            arrived.format("%Y-%m-%dT%H:%M:%S%:z"),
        ))]))
    }
}

impl ServerHandler for TimeServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "harnx-mcp-time",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "Time utilities: get current time, convert timestamps, and wait/sleep.",
            )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let read_only = ToolAnnotations::new()
            .read_only(true)
            .destructive(false)
            .idempotent(true)
            .open_world(false);

        let local_tz = &self.local_tz;

        let tools = vec![
            Tool::new(
                "get_current_time",
                "Get current time in a specific timezone",
                schema_object(
                    vec![(
                        "timezone",
                        "string",
                        Some(format!(
                            "IANA timezone name (e.g. 'America/New_York', 'Europe/London'). \
                             Use '{local_tz}' for local timezone if none specified."
                        )),
                    )],
                    &["timezone"],
                ),
            )
            .annotate(read_only.clone()),
            Tool::new(
                "convert_time",
                "Convert, offset, and reformat timestamps",
                schema_object(
                    vec![
                        (
                            "isoTimestamp",
                            "string",
                            Some("ISO formatted timestamp string. If it omits a timezone, sourceTimezone may be used to interpret it.".to_string()),
                        ),
                        (
                            "unixTimestamp",
                            "number",
                            Some("Unix timestamp in epoch seconds.".to_string()),
                        ),
                        (
                            "epochMillis",
                            "integer",
                            Some("JavaScript-style timestamp in epoch milliseconds.".to_string()),
                        ),
                        (
                            "offsetSeconds",
                            "integer",
                            Some("Number of seconds to add.".to_string()),
                        ),
                        (
                            "offsetMinutes",
                            "integer",
                            Some("Number of minutes to add.".to_string()),
                        ),
                        (
                            "offsetHours",
                            "integer",
                            Some("Number of hours to add.".to_string()),
                        ),
                        (
                            "offsetDays",
                            "integer",
                            Some("Number of days to add.".to_string()),
                        ),
                        (
                            "timezone",
                            "string",
                            Some(format!(
                                "Target IANA timezone for output formatting. Defaults to UTC. Use '{local_tz}' for local timezone."
                            )),
                        ),
                        (
                            "sourceTimezone",
                            "string",
                            Some(format!(
                                "If isoTimestamp has no timezone, interpret it in this IANA timezone before converting. Use '{local_tz}' for local timezone."
                            )),
                        ),
                    ],
                    &[],
                ),
            )
            .annotate(read_only.clone()),
            Tool::new(
                "wait",
                "Wait/sleep for a specified number of seconds (max 3600). Useful for polling or rate-limiting.",
                schema_object(
                    vec![(
                        "seconds",
                        "number",
                        Some("Duration to wait in seconds (max 3600)".to_string()),
                    )],
                    &["seconds"],
                ),
            )
            .annotate(
                ToolAnnotations::new()
                    .read_only(true)
                    .destructive(false)
                    .idempotent(false)
                    .open_world(false),
            ),
            Tool::new(
                "wait_until",
                "Wait until a specific time. Accepts HH:MM (today/tomorrow), or full datetime YYYY-MM-DDTHH:MM. Max 24 hours.",
                schema_object(
                    vec![
                        (
                            "time",
                            "string",
                            Some("Target time: HH:MM (today, or tomorrow if past), YYYY-MM-DDTHH:MM, or YYYY-MM-DDTHH:MM:SS".to_string()),
                        ),
                        (
                            "timezone",
                            "string",
                            Some(format!(
                                "IANA timezone for the target time. Defaults to '{local_tz}'."
                            )),
                        ),
                    ],
                    &["time"],
                ),
            )
            .annotate(
                ToolAnnotations::new()
                    .read_only(true)
                    .destructive(false)
                    .idempotent(false)
                    .open_world(false),
            ),
        ];

        Ok(ListToolsResult {
            meta: None,
            tools,
            next_cursor: None,
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        match request.name.as_ref() {
            "get_current_time" => {
                let args = parse_arguments::<GetCurrentTimeParams>(request.arguments)?;
                self.get_current_time_impl(&args.timezone)
            }
            "convert_time" => {
                let args = parse_arguments::<ConvertTimeParams>(request.arguments)?;
                self.convert_time_impl(args)
            }
            "wait" => {
                let args = parse_arguments::<WaitParams>(request.arguments)?;
                self.wait_impl(args.seconds).await
            }
            "wait_until" => {
                let args = parse_arguments::<WaitUntilParams>(request.arguments)?;
                self.wait_until_impl(&args.time, args.timezone.as_deref())
                    .await
            }
            other => Err(ErrorData::invalid_params(
                format!("unknown tool: {other}"),
                None,
            )),
        }
    }
}

#[derive(serde::Deserialize)]
struct GetCurrentTimeParams {
    timezone: String,
}

#[derive(serde::Deserialize)]
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

#[derive(serde::Deserialize)]
struct WaitParams {
    seconds: f64,
}

#[derive(serde::Deserialize)]
struct WaitUntilParams {
    time: String,
    #[serde(default)]
    timezone: Option<String>,
}

fn parse_iso_timestamp(
    iso_timestamp: &str,
    source_timezone: Option<&str>,
) -> Result<Timestamp, ErrorData> {
    if let Ok(timestamp) = iso_timestamp.parse::<Timestamp>() {
        return Ok(timestamp);
    }

    let source_timezone = source_timezone.ok_or_else(|| {
        ErrorData::invalid_params(
            "isoTimestamp is missing timezone information; provide sourceTimezone",
            None,
        )
    })?;

    let datetime = iso_timestamp.parse::<civil::DateTime>().map_err(|_| {
        ErrorData::invalid_params(format!("Invalid isoTimestamp: {iso_timestamp}"), None)
    })?;

    datetime
        .in_tz(source_timezone)
        .map(|zoned| zoned.timestamp())
        .map_err(invalid_params)
}

fn parse_unix_timestamp(unix_timestamp: f64) -> Result<Timestamp, ErrorData> {
    if !unix_timestamp.is_finite() {
        return Err(ErrorData::invalid_params(
            "unixTimestamp must be a finite number",
            None,
        ));
    }

    let total_nanos = unix_timestamp * 1_000_000_000.0;
    if !total_nanos.is_finite() || total_nanos < i128::MIN as f64 || total_nanos > i128::MAX as f64
    {
        return Err(ErrorData::invalid_params(
            "unixTimestamp is out of range",
            None,
        ));
    }

    Timestamp::from_nanosecond(total_nanos.round() as i128).map_err(invalid_params)
}

fn parse_epoch_millis(epoch_millis: i64) -> Result<Timestamp, ErrorData> {
    Timestamp::from_millisecond(epoch_millis).map_err(invalid_params)
}

fn invalid_params<E: std::fmt::Display>(err: E) -> ErrorData {
    ErrorData::invalid_params(err.to_string(), None)
}

fn parse_arguments<T: DeserializeOwned>(
    arguments: Option<Map<String, Value>>,
) -> Result<T, ErrorData> {
    serde_json::from_value(Value::Object(arguments.unwrap_or_default()))
        .map_err(|err| ErrorData::invalid_params(format!("invalid tool arguments: {err}"), None))
}

fn schema_object(
    properties: Vec<(&str, &str, Option<String>)>,
    required: &[&str],
) -> Map<String, Value> {
    let mut schema = Map::new();
    schema.insert("type".to_string(), Value::String("object".to_string()));

    let mut props = Map::new();
    for (name, ty, desc) in properties {
        let mut prop = Map::new();
        prop.insert("type".to_string(), Value::String(ty.to_string()));
        if let Some(d) = desc {
            prop.insert("description".to_string(), Value::String(d));
        }
        props.insert(name.to_string(), Value::Object(prop));
    }
    schema.insert("properties".to_string(), Value::Object(props));
    schema.insert("additionalProperties".to_string(), Value::Bool(false));
    schema.insert(
        "required".to_string(),
        Value::Array(
            required
                .iter()
                .map(|n| Value::String(n.to_string()))
                .collect(),
        ),
    );

    schema
}

#[cfg(test)]
mod tests {
    use super::*;

    use rmcp::handler::client::ClientHandler;
    use rmcp::model::{ClientCapabilities, InitializeRequestParams};
    use rmcp::service::{serve_client, serve_server, RoleClient, RoleServer, RunningService};
    use tokio::io::duplex;

    #[derive(Clone, Default)]
    struct TestClientHandler;

    impl ClientHandler for TestClientHandler {
        fn get_info(&self) -> InitializeRequestParams {
            InitializeRequestParams::new(
                ClientCapabilities::builder()
                    .enable_roots()
                    .enable_roots_list_changed()
                    .build(),
                Implementation::new("test", "0.1"),
            )
        }
    }

    struct TestConnection {
        _server_service: RunningService<RoleServer, TimeServer>,
        client_service: RunningService<RoleClient, TestClientHandler>,
    }

    async fn connect_server(server: TimeServer) -> TestConnection {
        let (client_transport, server_transport) = duplex(65_536);
        let server_fut = serve_server(server, server_transport);
        let client_fut = serve_client(TestClientHandler, client_transport);
        let (server_res, client_res) = tokio::join!(server_fut, client_fut);
        TestConnection {
            _server_service: server_res.unwrap(),
            client_service: client_res.unwrap(),
        }
    }

    fn text_content(result: &CallToolResult) -> String {
        result
            .content
            .iter()
            .find_map(|content| content.raw.as_text().map(|text| text.text.clone()))
            .unwrap()
    }

    fn tool_args(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    #[tokio::test]
    async fn test_time_server_list_tools() {
        let TestConnection {
            _server_service,
            client_service,
        } = connect_server(TimeServer::new()).await;
        let peer = client_service.peer().clone();
        let _client_task = tokio::spawn(async move {
            let _ = client_service.waiting().await;
        });

        let tools = peer.list_tools(Default::default()).await.unwrap();
        let names = tools
            .tools
            .iter()
            .map(|tool| tool.name.to_string())
            .collect::<Vec<_>>();

        assert_eq!(
            names,
            vec!["get_current_time", "convert_time", "wait", "wait_until"]
        );
    }

    #[tokio::test]
    async fn test_time_server_get_current_time() {
        let TestConnection {
            _server_service,
            client_service,
        } = connect_server(TimeServer::new()).await;
        let peer = client_service.peer().clone();
        let _client_task = tokio::spawn(async move {
            let _ = client_service.waiting().await;
        });

        let result = peer
            .call_tool(
                CallToolRequestParams::new("get_current_time")
                    .with_arguments(tool_args(serde_json::json!({ "timezone": "UTC" }))),
            )
            .await
            .unwrap();

        let text = text_content(&result);
        assert_eq!(result.is_error, Some(false));
        assert!(text.contains("\"timezone\": \"UTC\""));
    }

    #[test]
    fn test_get_current_time_utc() {
        let server = TimeServer::new();
        let result = server.get_current_time_impl("UTC").unwrap();
        let text = text_content(&result);
        let json: Value = serde_json::from_str(&text).unwrap();

        assert_eq!(json["timezone"], "UTC");
        assert!(json["datetime"].as_str().unwrap().ends_with("+00:00"));
    }

    #[test]
    fn test_convert_time_from_unix_timestamp() {
        let server = TimeServer::new();
        let result = server
            .convert_time_impl(ConvertTimeParams {
                iso_timestamp: None,
                unix_timestamp: Some(1_704_067_200.0),
                epoch_millis: None,
                offset_seconds: None,
                offset_minutes: None,
                offset_hours: None,
                offset_days: None,
                timezone: None,
                source_timezone: None,
            })
            .unwrap();
        let text = text_content(&result);
        let json: Value = serde_json::from_str(&text).unwrap();

        assert_eq!(json["timestamp"], "2024-01-01T00:00:00Z");
        assert_eq!(json["unixTimestamp"], 1_704_067_200);
        assert_eq!(json["epochMillis"], 1_704_067_200_000i64);
    }

    #[test]
    fn test_convert_time_with_timezone_and_offset() {
        let server = TimeServer::new();
        let result = server
            .convert_time_impl(ConvertTimeParams {
                iso_timestamp: Some("2024-01-02T00:00:00Z".to_string()),
                unix_timestamp: None,
                epoch_millis: None,
                offset_seconds: None,
                offset_minutes: Some(30),
                offset_hours: Some(1),
                offset_days: None,
                timezone: Some("America/New_York".to_string()),
                source_timezone: None,
            })
            .unwrap();
        let text = text_content(&result);
        let json: Value = serde_json::from_str(&text).unwrap();

        assert_eq!(
            json["timestamp"],
            "2024-01-01T20:30:00-05:00[America/New_York]"
        );
        assert_eq!(json["unixTimestamp"], 1_704_159_000);
        assert_eq!(json["epochMillis"], 1_704_159_000_000i64);
    }

    #[test]
    fn test_convert_time_naive_iso_with_source_timezone() {
        let server = TimeServer::new();
        let result = server
            .convert_time_impl(ConvertTimeParams {
                iso_timestamp: Some("2024-01-02T00:00:00".to_string()),
                unix_timestamp: None,
                epoch_millis: None,
                offset_seconds: None,
                offset_minutes: None,
                offset_hours: None,
                offset_days: None,
                timezone: None,
                source_timezone: Some("America/New_York".to_string()),
            })
            .unwrap();
        let text = text_content(&result);
        let json: Value = serde_json::from_str(&text).unwrap();

        assert_eq!(json["timestamp"], "2024-01-02T05:00:00Z");
        assert_eq!(json["unixTimestamp"], 1_704_171_600);
        assert_eq!(json["epochMillis"], 1_704_171_600_000i64);
    }

    #[test]
    fn test_convert_time_rejects_conflicting_inputs() {
        let server = TimeServer::new();
        let error = server
            .convert_time_impl(ConvertTimeParams {
                iso_timestamp: Some("2024-01-02T00:00:00Z".to_string()),
                unix_timestamp: Some(1_704_067_200.0),
                epoch_millis: None,
                offset_seconds: None,
                offset_minutes: None,
                offset_hours: None,
                offset_days: None,
                timezone: None,
                source_timezone: None,
            })
            .unwrap_err();

        assert!(error
            .message
            .contains("Provide at most one of isoTimestamp, unixTimestamp, or epochMillis"));
    }

    #[test]
    fn test_convert_time_negative_fractional_unix_timestamp() {
        let server = TimeServer::new();
        let result = server
            .convert_time_impl(ConvertTimeParams {
                iso_timestamp: None,
                unix_timestamp: Some(-0.5),
                epoch_millis: None,
                offset_seconds: None,
                offset_minutes: None,
                offset_hours: None,
                offset_days: None,
                timezone: None,
                source_timezone: None,
            })
            .unwrap();
        let text = text_content(&result);
        let json: Value = serde_json::from_str(&text).unwrap();

        assert_eq!(json["timestamp"], "1969-12-31T23:59:59.5Z");
        assert_eq!(json["unixTimestamp"], 0);
        assert_eq!(json["epochMillis"], -500);
    }

    #[test]
    fn test_convert_time_negative_fractional_unix_timestamp_with_more_precision() {
        let server = TimeServer::new();
        let result = server
            .convert_time_impl(ConvertTimeParams {
                iso_timestamp: None,
                unix_timestamp: Some(-1.25),
                epoch_millis: None,
                offset_seconds: None,
                offset_minutes: None,
                offset_hours: None,
                offset_days: None,
                timezone: None,
                source_timezone: None,
            })
            .unwrap();
        let text = text_content(&result);
        let json: Value = serde_json::from_str(&text).unwrap();

        assert_eq!(json["timestamp"], "1969-12-31T23:59:58.75Z");
        assert_eq!(json["unixTimestamp"], -1);
        assert_eq!(json["epochMillis"], -1250);
    }

    #[test]
    fn test_convert_time_preserves_sub_millisecond_unix_precision() {
        let server = TimeServer::new();
        let result = server
            .convert_time_impl(ConvertTimeParams {
                iso_timestamp: None,
                unix_timestamp: Some(1_704_153_600.000_4),
                epoch_millis: None,
                offset_seconds: None,
                offset_minutes: None,
                offset_hours: None,
                offset_days: None,
                timezone: None,
                source_timezone: None,
            })
            .unwrap();
        let text = text_content(&result);
        let json: Value = serde_json::from_str(&text).unwrap();

        assert_eq!(json["timestamp"], "2024-01-02T00:00:00.000400128Z");
        assert_eq!(json["unixTimestamp"], 1_704_153_600);
        assert_eq!(json["epochMillis"], 1_704_153_600_000i64);
    }

    #[tokio::test]
    async fn test_wait_until_past_date_rejected() {
        let server = TimeServer::new();
        let past = (Utc::now() - chrono::Duration::minutes(1))
            .format("%Y-%m-%dT%H:%M:%S")
            .to_string();

        let error = server
            .wait_until_impl(&past, Some("UTC"))
            .await
            .unwrap_err();
        assert!(error.message.contains("is in the past"));
    }
}
