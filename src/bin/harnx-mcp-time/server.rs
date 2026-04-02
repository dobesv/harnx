use chrono::{Datelike, Offset, TimeZone, Utc};
use chrono_tz::Tz;
use rmcp::model::{
    CallToolRequestParam, CallToolResult, Content, ErrorData, Implementation, ListToolsResult,
    PaginatedRequestParam, ServerCapabilities, ServerInfo, Tool, ToolAnnotations,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::ServerHandler;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};
use std::future::Future;

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
        let tz: Tz = timezone
            .parse()
            .map_err(|_| ErrorData::invalid_params(format!("Invalid timezone: {timezone}"), None))?;

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

        let result = serde_json::json!({
            "timezone": timezone,
            "datetime": now.format("%Y-%m-%dT%H:%M:%S%:z").to_string(),
            "day_of_week": now.format("%A").to_string(),
            "is_dst": is_dst,
        });

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap_or_default(),
        )]))
    }

    fn convert_time_impl(
        &self,
        source_tz: &str,
        time_str: &str,
        target_tz: &str,
    ) -> Result<CallToolResult, ErrorData> {
        let source: Tz = source_tz
            .parse()
            .map_err(|_| ErrorData::invalid_params(format!("Invalid source timezone: {source_tz}"), None))?;
        let target: Tz = target_tz
            .parse()
            .map_err(|_| ErrorData::invalid_params(format!("Invalid target timezone: {target_tz}"), None))?;

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

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap_or_default(),
        )]))
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
        let tz: Tz = timezone
            .unwrap_or(&self.local_tz)
            .parse()
            .map_err(|_| {
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
                time_str.parse::<chrono::NaiveTime>().map(|t| now.date_naive().and_time(t))
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
            actual_target = actual_target + chrono::Duration::days(1);
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
        ServerInfo {
            protocol_version: Default::default(),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            server_info: Implementation {
                name: "harnx-mcp-time".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                title: None,
                website_url: None,
                icons: None,
            },
            instructions: Some(
                "Time utilities: get current time, convert between timezones, and wait/sleep."
                    .to_string(),
            ),
        }
    }

    fn list_tools(
        &self,
        _request: Option<PaginatedRequestParam>,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListToolsResult, ErrorData>> + Send + '_ {
        async move {
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
                tools,
                next_cursor: None,
            })
        }
    }

    fn call_tool(
        &self,
        request: CallToolRequestParam,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<CallToolResult, ErrorData>> + Send + '_ {
        async move {
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
                    self.wait_until_impl(&args.time, args.timezone.as_deref()).await
                }
                other => Err(ErrorData::invalid_params(
                    format!("unknown tool: {other}"),
                    None,
                )),
            }
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
    serde_json::from_value(Value::Object(arguments.unwrap_or_default())).map_err(|err| {
        ErrorData::invalid_params(format!("invalid tool arguments: {err}"), None)
    })
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
