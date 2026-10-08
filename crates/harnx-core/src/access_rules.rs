//! Agent and session access rules matched against caller identities.
//!
//! Agent refs and identities are matched as whole strings using case-sensitive
//! globs. Matching rules grant the union of their scopes; no match grants nothing.
//!
//! Rules are compiled once at startup; changes require a server restart.

use anyhow::{anyhow, bail, Context, Result};
use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Permission granted on a matching agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    /// Create sessions and operate sessions owned by a caller identity.
    Prompt,
    /// Operate any session, including sessions without an owner.
    Admin,
}

/// Union of the scopes granted by matching access rules.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScopeSet {
    prompt: bool,
    admin: bool,
}

impl ScopeSet {
    pub fn contains(&self, scope: Scope) -> bool {
        match scope {
            Scope::Prompt => self.prompt,
            Scope::Admin => self.admin,
        }
    }

    pub fn is_empty(&self) -> bool {
        !self.prompt && !self.admin
    }

    fn insert(&mut self, scope: Scope) {
        match scope {
            Scope::Prompt => self.prompt = true,
            Scope::Admin => self.admin = true,
        }
    }

    fn union_with(&mut self, other: Self) {
        self.prompt |= other.prompt;
        self.admin |= other.admin;
    }
}

/// YAML representation of an access rules file.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AccessRulesFile {
    pub rules: Vec<AccessRuleSpec>,
}

/// An access rule before validation and glob compilation.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AccessRuleSpec {
    pub agents: Vec<String>,
    /// Omitted scopes default to prompt; an empty list is invalid.
    pub scopes: Option<Vec<Scope>>,
    pub users: Vec<String>,
}

#[derive(Debug, Clone)]
struct CompiledAccessRule {
    agents: GlobSet,
    scopes: ScopeSet,
    users: GlobSet,
}

/// Validated access rules with precompiled agent and identity globs.
#[derive(Debug, Clone)]
pub struct AccessRules {
    rules: Vec<CompiledAccessRule>,
}

impl AccessRules {
    /// Parse and validate access rules. Validation errors use zero-based rule indices.
    pub fn from_yaml(yaml: &str) -> Result<Self> {
        let file: AccessRulesFile =
            serde_yaml::from_str(yaml).context("failed to parse access rules YAML")?;
        let rules = file
            .rules
            .into_iter()
            .enumerate()
            .map(|(index, spec)| CompiledAccessRule::compile(spec, index))
            .collect::<Result<Vec<_>>>()?;
        Ok(Self { rules })
    }

    /// Read and validate a rules file, including its path in any error.
    pub fn load(path: &Path) -> Result<Self> {
        let yaml = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read access rules from {}", path.display()))?;
        Self::from_yaml(&yaml)
            .with_context(|| format!("failed to load access rules from {}", path.display()))
    }

    /// Union scopes from rules matching the agent ref and any caller identity.
    pub fn scopes_for(&self, agent_ref: &str, identities: &[&str]) -> ScopeSet {
        let mut scopes = ScopeSet::default();
        for rule in &self.rules {
            if rule.agents.is_match(agent_ref)
                && identities
                    .iter()
                    .any(|identity| rule.users.is_match(identity))
            {
                scopes.union_with(rule.scopes);
            }
        }
        scopes
    }

    /// Either scope makes an agent visible.
    pub fn can_see_agent(&self, agent_ref: &str, identities: &[&str]) -> bool {
        !self.scopes_for(agent_ref, identities).is_empty()
    }

    /// Admin alone does not grant permission to create sessions.
    pub fn can_create_session(&self, agent_ref: &str, identities: &[&str]) -> bool {
        self.scopes_for(agent_ref, identities)
            .contains(Scope::Prompt)
    }

    /// Admin can access any session; prompt requires an exact caller identity owner.
    /// Sessions without an owner require admin.
    pub fn can_access_session(
        &self,
        agent_ref: &str,
        identities: &[&str],
        session_owner: Option<&str>,
    ) -> bool {
        let scopes = self.scopes_for(agent_ref, identities);
        scopes.contains(Scope::Admin)
            || (scopes.contains(Scope::Prompt)
                && session_owner.is_some_and(|owner| identities.contains(&owner)))
    }
}

impl CompiledAccessRule {
    fn compile(spec: AccessRuleSpec, rule_index: usize) -> Result<Self> {
        let scopes = spec.scopes.unwrap_or_else(|| vec![Scope::Prompt]);
        if scopes.is_empty() {
            bail!("access rule {rule_index}: scopes must not be empty");
        }
        let mut scope_set = ScopeSet::default();
        for scope in scopes {
            scope_set.insert(scope);
        }
        Ok(Self {
            agents: compile_rule_globs(&spec.agents, rule_index, RuleGlobField::Agents)?,
            scopes: scope_set,
            users: compile_rule_globs(&spec.users, rule_index, RuleGlobField::Users)?,
        })
    }
}

enum RuleGlobField {
    Agents,
    Users,
}

fn compile_rule_globs(
    patterns: &[String],
    rule_index: usize,
    field: RuleGlobField,
) -> Result<GlobSet> {
    let field = match field {
        RuleGlobField::Agents => "agents",
        RuleGlobField::Users => "users",
    };
    if patterns.is_empty() {
        bail!("access rule {rule_index}: {field} must not be empty");
    }
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        if pattern.is_empty() {
            bail!(
                "access rule {rule_index}: empty {field} glob pattern {pattern:?} is not allowed"
            );
        }
        // Glob::new keeps matching case-sensitive and lets '*' span commas and slashes.
        let glob = Glob::new(pattern).map_err(|error| {
            anyhow!("access rule {rule_index}: invalid {field} glob pattern {pattern:?}: {error}")
        })?;
        builder.add(glob);
    }
    builder.build().with_context(|| {
        format!("access rule {rule_index}: failed to compile {field} glob patterns {patterns:?}")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_and_glob_identity_matches() {
        let rules = AccessRules::from_yaml(
            r#"
rules:
  - agents: [sisyphus]
    users: ["CN=bob,OU=test", "CN=*,OU=mycorp,O=slack.com"]
"#,
        )
        .unwrap();

        for identity in [
            "CN=bob,OU=test",
            "CN=alice,OU=mycorp,O=slack.com",
            "CN=alice,OU=engineering,OU=mycorp,O=slack.com",
        ] {
            assert!(rules
                .scopes_for("sisyphus", &[identity])
                .contains(Scope::Prompt));
        }
    }

    #[test]
    fn matching_is_case_sensitive_and_whole_string() {
        let rules = AccessRules::from_yaml(
            r#"
rules:
  - agents: [sisyphus]
    users: ["CN=bob,OU=test", "CN=*,OU=mycorp,O=slack.com"]
"#,
        )
        .unwrap();

        for identity in [
            "cn=bob,OU=test",
            "prefix-CN=bob,OU=test",
            "CN=bob,OU=test-suffix",
            "CN=bob,OU=mycorp,O=slack.com,extra",
            "CN=bob,OU=MYCORP,O=slack.com",
        ] {
            assert!(rules.scopes_for("sisyphus", &[identity]).is_empty());
        }
        for agent in ["Sisyphus", "prefix-sisyphus", "sisyphus-suffix"] {
            assert!(rules.scopes_for(agent, &["CN=bob,OU=test"]).is_empty());
        }
    }

    #[test]
    fn no_matching_rule_grants_no_scopes() {
        let rules = AccessRules::from_yaml(
            "rules:\n  - agents: [sisyphus]\n    scopes: [prompt, admin]\n    users: [bob]\n",
        )
        .unwrap();

        for (agent, identities) in [
            ("unknown", vec!["bob"]),
            ("sisyphus", vec!["unknown"]),
            ("sisyphus", vec![]),
        ] {
            assert!(rules.scopes_for(agent, &identities).is_empty());
            for (action, allowed) in [
                ("see agent", rules.can_see_agent(agent, &identities)),
                (
                    "create session",
                    rules.can_create_session(agent, &identities),
                ),
                (
                    "access owned session",
                    rules.can_access_session(agent, &identities, Some("bob")),
                ),
                (
                    "access legacy session",
                    rules.can_access_session(agent, &identities, None),
                ),
            ] {
                assert!(
                    !allowed,
                    "{action}: agent={agent}, identities={identities:?}"
                );
            }
        }
        let empty = AccessRules::from_yaml("rules: []").unwrap();
        assert!(empty.scopes_for("sisyphus", &["bob"]).is_empty());
    }

    #[test]
    fn scopes_are_unioned_across_all_matching_rules() {
        let rules = AccessRules::from_yaml(
            r#"
rules:
  - agents: [sisyphus]
    scopes: [prompt]
    users: [bob]
  - agents: ["sisy*"]
    scopes: [admin, admin]
    users: [bob]
  - agents: [daedalus]
    scopes: [admin]
    users: [alice]
"#,
        )
        .unwrap();

        let scopes = rules.scopes_for("sisyphus", &["bob"]);
        for (check, actual, expected) in [
            ("prompt scope", scopes.contains(Scope::Prompt), true),
            ("admin scope", scopes.contains(Scope::Admin), true),
            ("empty scopes", scopes.is_empty(), false),
            (
                "create session",
                rules.can_create_session("sisyphus", &["bob"]),
                true,
            ),
            (
                "access legacy session",
                rules.can_access_session("sisyphus", &["bob"], None),
                true,
            ),
            (
                "unmatched agent scopes",
                rules.scopes_for("daedalus", &["bob"]).is_empty(),
                true,
            ),
        ] {
            assert_eq!(actual, expected, "{check}");
        }
    }

    #[test]
    fn omitted_scopes_default_to_prompt() {
        let rules =
            AccessRules::from_yaml("rules:\n  - agents: [sisyphus]\n    users: [bob]\n").unwrap();
        let scopes = rules.scopes_for("sisyphus", &["bob"]);
        for (check, actual, expected) in [
            ("prompt scope", scopes.contains(Scope::Prompt), true),
            ("admin scope", scopes.contains(Scope::Admin), false),
            ("see agent", rules.can_see_agent("sisyphus", &["bob"]), true),
            (
                "create session",
                rules.can_create_session("sisyphus", &["bob"]),
                true,
            ),
        ] {
            assert_eq!(actual, expected, "{check}");
        }
    }

    #[test]
    fn empty_scopes_are_rejected() {
        let error = AccessRules::from_yaml(
            "rules:\n  - agents: [sisyphus]\n    scopes: []\n    users: [bob]\n",
        )
        .unwrap_err();
        assert!(error
            .to_string()
            .contains("rule 0: scopes must not be empty"));
    }

    #[test]
    fn empty_agents_and_users_are_rejected() {
        for (agents, users, field) in [("[]", "[bob]", "agents"), ("[sisyphus]", "[]", "users")] {
            let error = AccessRules::from_yaml(&format!(
                "rules:\n  - agents: {agents}\n    users: {users}\n"
            ))
            .unwrap_err();
            assert!(error
                .to_string()
                .contains(&format!("rule 0: {field} must not be empty")));
        }
    }

    #[test]
    fn empty_and_invalid_globs_name_rule_index_field_and_pattern() {
        for field in ["agents", "users"] {
            for pattern in ["", "["] {
                let (agents, users) = if field == "agents" {
                    (format!("[{pattern:?}]"), "[bob]".to_owned())
                } else {
                    ("[sisyphus]".to_owned(), format!("[{pattern:?}]"))
                };
                let yaml = format!(
                    "rules:\n  - agents: [valid]\n    users: [bob]\n  - agents: {agents}\n    users: {users}\n"
                );
                let error = AccessRules::from_yaml(&yaml).unwrap_err().to_string();
                assert!(error.contains("rule 1:"), "{error}");
                assert!(
                    error.contains(&format!("{field} glob pattern {pattern:?}")),
                    "{error}"
                );
                assert!(
                    error.contains(if pattern.is_empty() {
                        "empty"
                    } else {
                        "invalid"
                    }),
                    "{error}"
                );
            }
        }
    }

    #[test]
    fn unknown_fields_and_scopes_are_rejected() {
        for (yaml, expected) in [
            ("rules: []\nunknown: true\n", "unknown field `unknown`"),
            (
                "rules:\n  - agents: [sisyphus]\n    users: [bob]\n    scope: [admin]\n",
                "unknown field `scope`",
            ),
            (
                "rules:\n  - agents: [sisyphus]\n    users: [bob]\n    scopes: [Prompt]\n",
                "unknown variant `Prompt`",
            ),
        ] {
            let error = AccessRules::from_yaml(yaml).unwrap_err();
            assert!(format!("{error:#}").contains(expected), "{error:#}");
        }
    }

    #[test]
    fn yaml_anchors_and_aliases_are_reused_across_rules() {
        let rules = AccessRules::from_yaml(
            r#"
rules:
  - agents: &agents [sisyphus, daedalus]
    scopes: &scopes [prompt]
    users: &users ["CN=bob,OU=test", "CN=*,OU=mycorp,O=slack.com"]
  - agents: *agents
    scopes: [admin]
    users: *users
  - agents: [hephaestus]
    scopes: *scopes
    users: *users
"#,
        )
        .unwrap();

        for agent in ["sisyphus", "daedalus"] {
            for identity in ["CN=bob,OU=test", "CN=alice,OU=mycorp,O=slack.com"] {
                let scopes = rules.scopes_for(agent, &[identity]);
                assert!(scopes.contains(Scope::Prompt));
                assert!(scopes.contains(Scope::Admin));
            }
        }
        let scopes = rules.scopes_for("hephaestus", &["CN=bob,OU=test"]);
        assert!(scopes.contains(Scope::Prompt));
        assert!(!scopes.contains(Scope::Admin));
    }

    #[test]
    fn agent_cluster_refs_match_literally() {
        let rules = AccessRules::from_yaml(
            r#"
rules:
  - agents: [sisyphus]
    scopes: [prompt]
    users: [bob]
  - agents: ["sisyphus@remote"]
    scopes: [admin]
    users: [alice]
"#,
        )
        .unwrap();

        for (agent, identity, visible) in [
            ("sisyphus", "bob", true),
            ("sisyphus@remote", "bob", false),
            ("sisyphus@remote", "alice", true),
            ("sisyphus", "alice", false),
            ("sisyphus@other", "alice", false),
        ] {
            assert_eq!(
                rules.can_see_agent(agent, &[identity]),
                visible,
                "{agent} {identity}"
            );
        }
    }

    #[test]
    fn wildcard_agent_glob_matches_all_agent_refs() {
        let rules =
            AccessRules::from_yaml("rules:\n  - agents: ['*']\n    users: [bob]\n").unwrap();
        for agent in [
            "sisyphus",
            "daedalus",
            "sisyphus@remote",
            "pantheon/sisyphus",
        ] {
            assert!(rules.can_create_session(agent, &["bob"]));
        }
        assert!(!rules.can_see_agent("sisyphus", &["alice"]));
        assert!(!rules.can_see_agent("sisyphus", &[]));
    }

    #[test]
    fn can_access_session_scope_and_owner_matrix() {
        let rules = AccessRules::from_yaml(
            r#"
rules:
  - agents: [prompt]
    scopes: [prompt]
    users: [bob]
  - agents: [admin]
    scopes: [admin]
    users: [bob]
  - agents: [both]
    scopes: [prompt, admin]
    users: [bob]
"#,
        )
        .unwrap();

        for (agent, owner, expected) in [
            ("prompt", Some("bob"), true),
            ("prompt", Some("alice"), false),
            ("prompt", None, false),
            ("admin", Some("bob"), true),
            ("admin", Some("alice"), true),
            ("admin", None, true),
            ("both", Some("bob"), true),
            ("both", Some("alice"), true),
            ("both", None, true),
            ("unknown", Some("bob"), false),
            ("unknown", Some("alice"), false),
            ("unknown", None, false),
        ] {
            assert_eq!(
                rules.can_access_session(agent, &["bob"], owner),
                expected,
                "agent={agent}, owner={owner:?}"
            );
        }
        assert!(rules.can_see_agent("admin", &["bob"]));
        for (agent, can_create) in [("admin", false), ("prompt", true), ("both", true)] {
            assert_eq!(
                rules.can_create_session(agent, &["bob"]),
                can_create,
                "{agent}"
            );
        }
    }

    #[test]
    fn multiple_caller_identities_match_rules_and_session_owners() {
        let rules = AccessRules::from_yaml(
            r#"
rules:
  - agents: [sisyphus, daedalus]
    scopes: [prompt]
    users: [bob]
  - agents: [sisyphus]
    scopes: [admin]
    users: [operator]
"#,
        )
        .unwrap();

        let identities = ["unmatched", "bob", "alias"];
        assert!(rules.can_create_session("daedalus", &identities));
        for (owner, allowed) in [
            (Some("bob"), true),
            (Some("alias"), true),
            (Some("foreign"), false),
            (Some("BOB"), false),
            (None, false),
        ] {
            assert_eq!(
                rules.can_access_session("daedalus", &identities, owner),
                allowed,
                "{owner:?}"
            );
        }

        for (identities, prompt, admin) in [
            (vec!["bob", "operator"], true, true),
            (vec!["operator"], false, true),
            (vec!["bob"], true, false),
        ] {
            let scopes = rules.scopes_for("sisyphus", &identities);
            for (scope, expected) in [(Scope::Prompt, prompt), (Scope::Admin, admin)] {
                assert_eq!(scopes.contains(scope), expected, "{scope:?} {identities:?}");
            }
        }
        assert!(rules.can_access_session("sisyphus", &["bob", "operator"], None));
    }

    #[test]
    fn load_reads_rules_and_reports_path_on_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("access.yaml");
        std::fs::write(&path, "rules:\n  - agents: [sisyphus]\n    users: [bob]\n").unwrap();
        assert!(AccessRules::load(&path)
            .unwrap()
            .can_create_session("sisyphus", &["bob"]));

        std::fs::write(
            &path,
            "rules:\n  - agents: [sisyphus]\n    scopes: []\n    users: [bob]\n",
        )
        .unwrap();
        let error = AccessRules::load(&path).unwrap_err();
        assert!(error.to_string().contains(&path.display().to_string()));
        assert!(format!("{error:#}").contains("scopes must not be empty"));

        std::fs::write(&path, "rules: [").unwrap();
        let error = AccessRules::load(&path).unwrap_err();
        assert!(error.to_string().contains(&path.display().to_string()));
        assert!(format!("{error:#}").contains("failed to parse access rules YAML"));

        let missing = dir.path().join("missing.yaml");
        let error = AccessRules::load(&missing).unwrap_err();
        assert!(error.to_string().contains(&missing.display().to_string()));
        assert!(error.to_string().contains("failed to read access rules"));
    }
}
