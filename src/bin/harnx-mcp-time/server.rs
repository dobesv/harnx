use chrono::{Datelike, Offset, TimeZone, Utc};
use chrono_tz::Tz;
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

    fn convert_time_impl(
        &self,
        source_tz: &str,
        time_str: &str,
        target_tz: &str,
    ) -> Result<CallToolResult, ErrorData> {
        let source: Tz = source_tz.parse().map_err(|_| {
            ErrorData::invalid_params(format!("Invalid source timezone: {source_tz}"), None)
        })?;
        let target: Tz = target_tz.parse().map_err(|_| {
            ErrorData::invalid_params(format!("Invalid target timezone: {target_tz}"), None)
        })?;

        let parts: Vec<&str> = time_str.split(':').collect();
        if parts.len() != 2 {
            return Err(ErrorData::invalid_params(
                "Invalid time format. Expected HH:MM (24-hour)",
                None,
            ));
        }
        let hour: u32 = parts[0]
            .parse()
            .map_err(|_| ErrorData::invalid_params("Invalid hour", None))?;
        let minute: u32 = parts[1]
            .parse()
            .map_err(|_| ErrorData::invalid_params("Invalid minute", None))?;

        if hour >= 24 || minute >= 60 {
            return Err(ErrorData::invalid_params(
                "Hour must be 0-23, minute must be 0-59",
                None,
            ));
        }

        let now_utc = Utc::now();
        let now_source = now_utc.with_timezone(&source);
        let source_dt = source
            .with_ymd_and_hms(
                now_source.year(),
                now_source.month(),
                now_source.day(),
                hour,
                minute,
                0,
            )
            .single()
            .ok_or_else(|| ErrorData::invalid_params("Ambiguous or invalid local time", None))?;

        let target_dt = source_dt.with_timezone(&target);

        let source_offset = source_dt.offset().fix().local_minus_utc() as f64 / 3600.0;
        let target_offset = target_dt.offset().fix().local_minus_utc() as f64 / 3600.0;
        let diff = target_offset - source_offset;

        let time_diff_str = if diff.fract() == 0.0 {
            format!("{:+.1}h", diff)
        } else {
            let s = format!("{:+.2}", diff);
            format!("{}h", s.trim_end_matches('0').trim_end_matches('.'))
        };

        let source_time_str = source_dt.format("%H:%M %Z").to_string();
        let target_time_str = target_dt.format("%H:%M %Z").to_string();
        let result = serde_json::json!({
            "source": {
                "timezone": source_tz,
                "datetime": source_dt.format("%Y-%m-%dT%H:%M:%S%:z").to_string(),
                "day_of_week": source_dt.format("%A").to_string(),
            },
            "target": {
                "timezone": target_tz,
                "datetime": target_dt.format("%Y-%m-%dT%H:%M:%S%:z").to_string(),
                "day_of_week": target_dt.format("%A").to_string(),
            },
            "time_difference": time_diff_str,
        });

        let full = serde_json::to_string_pretty(&result).unwrap_or_default();
        let summary = format!("{source_time_str} → {target_time_str} ({time_diff_str})");
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
                // HH:MM — assume today (or tomorrow if already passed)
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

        // If HH:MM was given and the time already passed today, target tomorrow
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
                "Time utilities: get current time, convert between timezones, and wait/sleep.",
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
                "Convert time between timezones",
                schema_object(
                    vec![
                        (
                            "source_timezone",
                            "string",
                            Some(format!(
                                "Source IANA timezone name. Use '{local_tz}' for local timezone."
                            )),
                        ),
                        (
                            "time",
                            "string",
                            Some("Time in 24-hour format (HH:MM)".to_string()),
                        ),
                        (
                            "target_timezone",
                            "string",
                            Some(format!(
                                "Target IANA timezone name. Use '{local_tz}' for local timezone."
                            )),
                        ),
                    ],
                    &["source_timezone", "time", "target_timezone"],
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
                self.convert_time_impl(&args.source_timezone, &args.time, &args.target_timezone)
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
    source_timezone: String,
    time: String,
    target_timezone: String,
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
            .call_tool(CallToolRequestParams::new("get_current_time")
                .with_arguments(tool_args(serde_json::json!({ "timezone": "UTC" }))))
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
    fn test_convert_time_basic() {
        let server = TimeServer::new();
        let result = server
            .convert_time_impl("UTC", "12:30", "America/New_York")
            .unwrap();
        let text = text_content(&result);
        let json: Value = serde_json::from_str(&text).unwrap();

        assert_eq!(json["source"]["timezone"], "UTC");
        assert_eq!(json["target"]["timezone"], "America/New_York");
        assert!(json["time_difference"].as_str().unwrap().ends_with('h'));
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
