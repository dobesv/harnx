use harnx_core::package_namespace::{mcp_tool_prefix, sanitize_for_tool_name};
use harnx_toolset::{server_identity_token, Registration};

/// Worker-side source of truth for NATS tool names and route identities.
pub struct ServerIdentity;

impl ServerIdentity {
    /// Return `<package>__<config>__<server>` for packaged servers.
    ///
    /// Top-level servers use `__<config>__<server>`, including both leading
    /// separators so their identity cannot collide with a packaged server.
    pub fn identity_token(registration: &Registration) -> String {
        server_identity_token(
            registration.package.as_deref(),
            &registration.config,
            &registration.server,
        )
    }

    /// Return the tool name exposed to an agent in `agent_package`.
    pub fn agent_visible_name(
        agent_package: Option<&str>,
        registration: &Registration,
        raw_tool: &str,
    ) -> String {
        let server = sanitize_for_tool_name(&registration.server);
        let raw_tool = sanitize_for_tool_name(raw_tool);
        let prefix = match registration.package.as_deref() {
            Some(package) if Some(package) != agent_package => mcp_tool_prefix(package, &server),
            _ => server,
        };
        format!("{prefix}_{raw_tool}")
    }

    /// Resolve an agent-visible name to `(identity token, raw tool name)`.
    ///
    /// Qualified cross-package names are checked first. Bare names prefer
    /// registrations from `active_package` and top-level registrations before
    /// falling back to another package. Broader collision handling is tracked
    /// by #1356.
    pub fn parse_agent_tool_name(
        name: &str,
        known: &[Registration],
        active_package: Option<&str>,
    ) -> Option<(String, String)> {
        Self::resolve_qualified_name(name, known)
            .or_else(|| Self::resolve_local_name(name, known, active_package))
            .or_else(|| Self::resolve_cross_package_fallback(name, known, active_package))
    }

    fn resolve_qualified_name(name: &str, known: &[Registration]) -> Option<(String, String)> {
        Self::resolve_from(
            name,
            known
                .iter()
                .filter(|registration| registration.package.is_some()),
            |registration, tool| {
                let package = registration.package.as_deref()?;
                let server = sanitize_for_tool_name(&registration.server);
                let raw_tool = sanitize_for_tool_name(&tool.name);
                Some(format!("{}_{raw_tool}", mcp_tool_prefix(package, &server)))
            },
        )
    }

    fn resolve_local_name(
        name: &str,
        known: &[Registration],
        active_package: Option<&str>,
    ) -> Option<(String, String)> {
        Self::resolve_from(
            name,
            known.iter().filter(|registration| {
                registration.package.is_none() || registration.package.as_deref() == active_package
            }),
            |registration, tool| {
                Some(Self::agent_visible_name(
                    registration.package.as_deref(),
                    registration,
                    &tool.name,
                ))
            },
        )
    }

    fn resolve_cross_package_fallback(
        name: &str,
        known: &[Registration],
        active_package: Option<&str>,
    ) -> Option<(String, String)> {
        Self::resolve_from(
            name,
            known.iter().filter(|registration| {
                registration.package.is_some() && registration.package.as_deref() != active_package
            }),
            |registration, tool| {
                Some(Self::agent_visible_name(
                    registration.package.as_deref(),
                    registration,
                    &tool.name,
                ))
            },
        )
    }

    fn resolve_from<'a>(
        name: &str,
        registrations: impl Iterator<Item = &'a Registration>,
        candidate_name: impl Fn(&Registration, &harnx_toolset::ToolSpec) -> Option<String>,
    ) -> Option<(String, String)> {
        for registration in registrations {
            for tool in &registration.tools {
                if candidate_name(registration, tool).as_deref() == Some(name) {
                    return Some((Self::identity_token(registration), tool.name.clone()));
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::ServerIdentity;
    use harnx_toolset::{Registration, ToolSpec};
    use serde_json::json;

    fn registration(package: Option<&str>) -> Registration {
        Registration {
            package: package.map(str::to_string),
            config: "tools".to_string(),
            server: "fs".to_string(),
            tools: vec![ToolSpec {
                name: "read".to_string(),
                description: String::new(),
                input_schema: json!({ "type": "object" }),
                idempotent_hint: true,
                read_only_hint: true,
                timeout_secs: None,
                meta: None,
            }],
            schema_version: 1,
            proto_version: 1,
        }
    }

    #[test]
    fn same_package_name_is_bare() {
        harnx_core::require_nextest();
        let registration = registration(Some("p"));
        assert_eq!(
            ServerIdentity::agent_visible_name(Some("p"), &registration, "read"),
            "fs_read"
        );
    }

    #[test]
    fn cross_package_name_is_qualified() {
        harnx_core::require_nextest();
        let registration = registration(Some("p"));
        assert_eq!(
            ServerIdentity::agent_visible_name(Some("other"), &registration, "read"),
            "p__fs_read"
        );
    }

    #[test]
    fn top_level_name_is_bare() {
        harnx_core::require_nextest();
        let registration = registration(None);
        assert_eq!(
            ServerIdentity::agent_visible_name(Some("p"), &registration, "read"),
            "fs_read"
        );
    }

    #[test]
    fn reverse_parse_round_trips_visible_names() {
        harnx_core::require_nextest();
        let same_package = registration(Some("active"));
        let top_level = registration(None);
        let cross_package = registration(Some("p"));
        let known = [
            same_package.clone(),
            top_level.clone(),
            cross_package.clone(),
        ];

        assert_eq!(
            ServerIdentity::parse_agent_tool_name("fs_read", &known, Some("active")),
            Some((
                ServerIdentity::identity_token(&same_package),
                "read".to_string()
            ))
        );
        assert_eq!(
            ServerIdentity::parse_agent_tool_name("p__fs_read", &known, Some("active")),
            Some((
                ServerIdentity::identity_token(&cross_package),
                "read".to_string()
            ))
        );
        assert_eq!(
            ServerIdentity::parse_agent_tool_name(
                "fs_read",
                std::slice::from_ref(&top_level),
                None,
            ),
            Some((
                ServerIdentity::identity_token(&top_level),
                "read".to_string()
            ))
        );
        assert_eq!(
            ServerIdentity::parse_agent_tool_name(
                "fs_read",
                std::slice::from_ref(&cross_package),
                Some("active"),
            ),
            Some((
                ServerIdentity::identity_token(&cross_package),
                "read".to_string()
            ))
        );
    }
}
