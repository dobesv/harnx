//! Agent and session access rules matched against caller identities.
//!
//! Agent refs and identities are matched as whole strings using case-sensitive
//! globs. Matching rules grant the union of their scopes; no match grants nothing.
//!
//! Rules compile at startup; changes require a server restart.

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
///
/// At least one of `users`, `groups`, or `roles` must be non-empty.
/// Omitted `users`, `groups`, and `roles` default to empty lists.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AccessRuleSpec {
    pub agents: Vec<String>,
    /// Omitted scopes default to prompt; an empty list is invalid.
    pub scopes: Option<Vec<Scope>>,
    /// User identity selectors. Empty or omitted matches nothing.
    #[serde(default)]
    pub users: Vec<String>,
    /// Group name selectors. Empty or omitted matches nothing.
    #[serde(default)]
    pub groups: Vec<String>,
    /// Role name selectors. Empty or omitted matches nothing.
    #[serde(default)]
    pub roles: Vec<String>,
}

/// Caller identity collections passed to access rule evaluation.
///
/// Users, groups, and roles form distinct namespaces; the same string value
/// in different namespaces never matches the same selector. Callers must not
/// flatten or combine identity lists across namespaces before constructing
/// a CallerView — separate matching preserves namespace semantics.
#[derive(Debug, Clone, Copy, Default)]
pub struct CallerView<'a> {
    pub users: &'a [&'a str],
    pub groups: &'a [&'a str],
    pub roles: &'a [&'a str],
}

impl<'a> CallerView<'a> {
    /// Create from legacy user-only identity slice.
    pub fn from_users(identities: &'a [&'a str]) -> Self {
        Self {
            users: identities,
            groups: &[],
            roles: &[],
        }
    }
}

#[derive(Debug, Clone)]
struct CompiledAccessRule {
    agents: GlobSet,
    scopes: ScopeSet,
    users: GlobSet,
    groups: GlobSet,
    roles: GlobSet,
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

    /// Union scopes from rules matching the agent ref and any selector category.
    ///
    /// A rule matches when its agent glob matches AND at least one of its
    /// user/group/role selectors matches the corresponding caller collection.
    pub fn scopes_for(&self, agent_ref: &str, caller: CallerView<'_>) -> ScopeSet {
        let mut scopes = ScopeSet::default();
        for rule in &self.rules {
            if rule.agents.is_match(agent_ref) && self.caller_matches_selectors(rule, caller) {
                scopes.union_with(rule.scopes);
            }
        }
        scopes
    }

    /// Check if any selector category matches the caller's identity collections.
    fn caller_matches_selectors(&self, rule: &CompiledAccessRule, caller: CallerView<'_>) -> bool {
        caller.users.iter().any(|u| rule.users.is_match(u))
            || caller.groups.iter().any(|g| rule.groups.is_match(g))
            || caller.roles.iter().any(|r| rule.roles.is_match(r))
    }

    /// Either scope makes an agent visible.
    pub fn can_see_agent(&self, agent_ref: &str, caller: CallerView<'_>) -> bool {
        !self.scopes_for(agent_ref, caller).is_empty()
    }

    /// Admin alone does not grant permission to create sessions.
    pub fn can_create_session(&self, agent_ref: &str, caller: CallerView<'_>) -> bool {
        self.scopes_for(agent_ref, caller).contains(Scope::Prompt)
    }

    /// Admin can access any session; prompt requires user ownership.
    ///
    /// Groups and roles never satisfy ownership; only the users collection
    /// contributes to owner matching. Admin grants from group/role rules
    /// still allow full session access.
    pub fn can_access_session(
        &self,
        agent_ref: &str,
        caller: CallerView<'_>,
        session_owner: Option<&str>,
    ) -> bool {
        let scopes = self.scopes_for(agent_ref, caller);
        scopes.contains(Scope::Admin)
            || (scopes.contains(Scope::Prompt)
                && session_owner.is_some_and(|owner| caller.users.contains(&owner)))
    }
}

impl CompiledAccessRule {
    fn compile(spec: AccessRuleSpec, rule_index: usize) -> Result<Self> {
        let scopes = spec.scopes.clone().unwrap_or_else(|| vec![Scope::Prompt]);
        if scopes.is_empty() {
            bail!("access rule {rule_index}: scopes must not be empty");
        }
        let mut scope_set = ScopeSet::default();
        for scope in scopes {
            scope_set.insert(scope);
        }

        // Require at least one non-empty selector category.
        Self::check_selector_presence(&spec, rule_index)?;

        Ok(Self {
            agents: compile_rule_globs(&spec.agents, rule_index, RuleGlobField::Agents)?,
            scopes: scope_set,
            users: compile_rule_globs(&spec.users, rule_index, RuleGlobField::Users)?,
            groups: compile_rule_globs(&spec.groups, rule_index, RuleGlobField::Groups)?,
            roles: compile_rule_globs(&spec.roles, rule_index, RuleGlobField::Roles)?,
        })
    }

    fn check_selector_presence(spec: &AccessRuleSpec, rule_index: usize) -> Result<()> {
        fn has_any_selector(spec: &AccessRuleSpec) -> bool {
            !spec.users.is_empty() || !spec.groups.is_empty() || !spec.roles.is_empty()
        }
        if has_any_selector(spec) {
            return Ok(());
        }
        bail!(
            "access rule {rule_index}: at least one of users, groups, or roles must be non-empty"
        );
    }
}

enum RuleGlobField {
    Agents,
    Users,
    Groups,
    Roles,
}

fn compile_rule_globs(
    patterns: &[String],
    rule_index: usize,
    field: RuleGlobField,
) -> Result<GlobSet> {
    let field_name = match field {
        RuleGlobField::Agents => "agents",
        RuleGlobField::Users => "users",
        RuleGlobField::Groups => "groups",
        RuleGlobField::Roles => "roles",
    };
    // Agents must be non-empty; selectors can be empty.
    if matches!(field, RuleGlobField::Agents) && patterns.is_empty() {
        bail!("access rule {rule_index}: {field_name} must not be empty");
    }
    if patterns.is_empty() {
        // Build empty GlobSet for omitted/empty selector categories.
        return Ok(GlobSet::empty());
    }
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        if pattern.is_empty() {
            bail!(
                "access rule {rule_index}: empty {field_name} glob pattern {pattern:?} is not allowed"
            );
        }
        // Glob::new keeps matching case-sensitive and lets '*' span commas and slashes.
        let glob = Glob::new(pattern).map_err(|error| {
            anyhow!(
                "access rule {rule_index}: invalid {field_name} glob pattern {pattern:?}: {error}"
            )
        })?;
        builder.add(glob);
    }
    builder.build().with_context(|| {
        format!(
            "access rule {rule_index}: failed to compile {field_name} glob patterns {patterns:?}"
        )
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
            let scopes = rules.scopes_for("sisyphus", CallerView::from_users(&[identity]));
            assert!(scopes.contains(Scope::Prompt), "{identity}");
        }
    }

    #[test]
    fn agent_glob_matches_like_identity_glob() {
        let rules = AccessRules::from_yaml(
            r#"
rules:
  - agents: ["sisy*"]
    users: [bob]
"#,
        )
        .unwrap();

        for agent in ["sisyphus", "sisyphus-worker"] {
            assert!(
                rules.can_see_agent(agent, CallerView::from_users(&["bob"])),
                "{agent}"
            );
        }
    }

    #[test]
    fn basic_yml_parses_and_matches() {
        // Matches example_config/access.example.yaml structure.
        let rules = AccessRules::from_yaml(
            r#"
rules:
  - agents: [atlas]
    users: [bob]
    scopes: [prompt]
  - agents: [sisyphus, atlas]
    users: [alice]
    scopes: [admin]
"#,
        )
        .unwrap();

        for (agent, identity, prompt, admin) in [
            ("atlas", "bob", true, false),
            ("sisyphus", "alice", false, true),
            ("atlas", "alice", false, true),
        ] {
            let scopes = rules.scopes_for(agent, CallerView::from_users(&[identity]));
            for (scope, expected) in [(Scope::Prompt, prompt), (Scope::Admin, admin)] {
                assert_eq!(
                    scopes.contains(scope),
                    expected,
                    "{agent}/{identity}/{scope:?}"
                );
            }
        }
    }

    #[test]
    fn agent_glob_and_agent_ref_matching_cannot_change_scope_visibility() {
        for (agent, identity, users_pattern, expected_owner) in [
            ("sisyphus", "bob", "bob", "bob"),
            ("sisyphus", "CN=bob,OU=test", "'CN=*'", "CN=bob,OU=test"),
            ("cascade", "bob", "bob", "bob"),
        ] {
            let yaml = format!(
                r#"
rules:
  - agents: [sisyphus, "sisy*", cascade]
    users: [{users_pattern}]
"#,
            );
            let rules = AccessRules::from_yaml(&yaml).unwrap();
            let identities = [identity];
            let caller = CallerView::from_users(&identities);
            let scopes = rules.scopes_for(agent, caller);
            for (check, actual, expected) in [
                ("see agent", rules.can_see_agent(agent, caller), true),
                ("empty scopes", scopes.is_empty(), false),
                (
                    "create session",
                    rules.can_create_session(agent, caller),
                    true,
                ),
                (
                    "access own session",
                    rules.can_access_session(agent, caller, Some(expected_owner)),
                    true,
                ),
                (
                    "access legacy session",
                    rules.can_access_session(agent, caller, None),
                    false,
                ),
            ] {
                assert_eq!(
                    actual, expected,
                    "{check}: agent={agent}, identity={identity}"
                );
            }
        }
        let empty = AccessRules::from_yaml("rules: []").unwrap();
        assert!(empty
            .scopes_for("sisyphus", CallerView::from_users(&["bob"]))
            .is_empty());
    }

    #[test]
    fn invalid_agent_ref_globs_rejected() {
        check_glob_error(
            r#"
rules:
  - agents: ["[unclosed"]
    users: [bob]
"#,
            "rule 0: invalid agents glob",
            "[unclosed",
        );
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

        let scopes = rules.scopes_for("sisyphus", CallerView::from_users(&["bob"]));
        for (check, actual, expected) in [
            ("prompt scope", scopes.contains(Scope::Prompt), true),
            ("admin scope", scopes.contains(Scope::Admin), true),
            ("empty scopes", scopes.is_empty(), false),
            (
                "create session",
                rules.can_create_session("sisyphus", CallerView::from_users(&["bob"])),
                true,
            ),
            (
                "access legacy session",
                rules.can_access_session("sisyphus", CallerView::from_users(&["bob"]), None),
                true,
            ),
            (
                "unmatched agent scopes",
                rules
                    .scopes_for("daedalus", CallerView::from_users(&["bob"]))
                    .is_empty(),
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
        let scopes = rules.scopes_for("sisyphus", CallerView::from_users(&["bob"]));
        for (check, actual, expected) in [
            ("prompt scope", scopes.contains(Scope::Prompt), true),
            ("admin scope", scopes.contains(Scope::Admin), false),
            (
                "see agent",
                rules.can_see_agent("sisyphus", CallerView::from_users(&["bob"])),
                true,
            ),
            (
                "create session",
                rules.can_create_session("sisyphus", CallerView::from_users(&["bob"])),
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
    fn empty_agents_and_all_selectors_are_rejected() {
        // Empty agents is rejected - agents must have at least one pattern.
        let yaml = "rules:\n  - agents: []\n    users: [bob]\n";
        let result = AccessRules::from_yaml(yaml);
        assert!(result.is_err(), "empty agents should be rejected");
        let error = result.unwrap_err().to_string();
        assert!(
            error.contains("rule 0: agents must not be empty"),
            "{error}"
        );

        // Empty users with non-empty groups is valid.
        let rules =
            AccessRules::from_yaml("rules:\n  - agents: [sisyphus]\n    groups: [engineers]\n");
        assert!(rules.is_ok(), "empty users with groups should be valid");

        // Empty users with non-empty roles is valid.
        let rules = AccessRules::from_yaml("rules:\n  - agents: [sisyphus]\n    roles: [admin]\n");
        assert!(rules.is_ok(), "empty users with roles should be valid");

        // All selectors empty (users, groups, roles all explicitly empty) is rejected.
        let yaml = "rules:\n  - agents: [sisyphus]\n    users: []\n    groups: []\n    roles: []\n";
        let result = AccessRules::from_yaml(yaml);
        assert!(result.is_err(), "all selectors empty should be rejected");
        let error = result.unwrap_err().to_string();
        assert!(
            error.contains("rule 0: at least one of users, groups, or roles must be non-empty"),
            "{error}"
        );
    }

    #[test]
    fn selector_free_rules_are_rejected() {
        let error = AccessRules::from_yaml("rules:\n  - agents: [sisyphus]\n")
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("rule 0: at least one of users, groups, or roles must be non-empty"),
            "{error}"
        );
    }

    #[test]
    fn unknown_fields_are_rejected() {
        // Both AccessRulesFile and AccessRuleSpec have deny_unknown_fields.
        // Test that unknown fields in the rule spec cause a parse error.
        // Use a name that won't be recognized by any field.
        let yaml = "rules:\n  - agents: [sisyphus]\n    users: [bob]\n    zzzunknown: value\n";
        let result = AccessRules::from_yaml(yaml);
        assert!(result.is_err(), "expected error for unknown field");
    }

    #[test]
    fn invalid_glob_pattern_rejected() {
        check_glob_error(
            r#"
rules:
  - agents: [sisyphus]
    users: ["[unclosed"]
"#,
            "invalid users glob",
            "[unclosed",
        );
    }

    #[test]
    fn empty_pattern_rejected() {
        check_glob_error(
            r#"
rules:
  - agents: [sisyphus]
    users: [""]
"#,
            "empty users glob",
            "",
        );
    }

    fn check_glob_error(yaml: &str, expected: &str, contains: &str) {
        let error = AccessRules::from_yaml(yaml).unwrap_err().to_string();
        assert!(error.contains(expected), "{error}");
        if !contains.is_empty() {
            assert!(error.contains(contains), "{error}");
        }
    }

    #[test]
    fn can_access_session_scope_and_owner_matrix() {
        let rules = AccessRules::from_yaml(
            r#"
rules:
  - agents: [sisyphus]
    users: [bob]
    scopes: [prompt]
  - agents: [admin]
    users: [bob]
    scopes: [admin]
"#,
        )
        .unwrap();
        for (agent, owner, allowed) in [
            ("sisyphus", Some("bob"), true),
            ("sisyphus", Some("alice"), false),
            ("sisyphus", None, false),
            ("admin", Some("bob"), true),
            ("admin", Some("alice"), true),
            ("admin", None, true),
        ] {
            assert_eq!(
                rules.can_access_session(agent, CallerView::from_users(&["bob"]), owner),
                allowed,
                "{agent}/{owner:?}"
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
        assert!(rules.can_create_session("daedalus", CallerView::from_users(&identities)));
        for (owner, allowed) in [
            (Some("bob"), true),
            (Some("alias"), true),
            (Some("foreign"), false),
            (Some("BOB"), false),
            (None, false),
        ] {
            assert_eq!(
                rules.can_access_session("daedalus", CallerView::from_users(&identities), owner),
                allowed,
                "{owner:?}"
            );
        }

        for (identities, prompt, admin) in [
            (vec!["bob", "operator"], true, true),
            (vec!["operator"], false, true),
            (vec!["bob"], true, false),
        ] {
            let scopes = rules.scopes_for("sisyphus", CallerView::from_users(&identities));
            for (scope, expected) in [(Scope::Prompt, prompt), (Scope::Admin, admin)] {
                assert_eq!(scopes.contains(scope), expected, "{scope:?} {identities:?}");
            }
        }
        assert!(rules.can_access_session(
            "sisyphus",
            CallerView::from_users(&["bob", "operator"]),
            None
        ));
    }

    #[test]
    fn load_reads_rules_and_reports_path_on_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("access.yaml");
        std::fs::write(&path, "rules:\n  - agents: [sisyphus]\n    users: [bob]\n").unwrap();
        assert!(AccessRules::load(&path)
            .unwrap()
            .can_create_session("sisyphus", CallerView::from_users(&["bob"])));

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

    // === Group and role selector tests ===

    #[test]
    fn group_only_rules_grant_access() {
        check_group_only_rules_grant_access();
    }

    fn check_group_only_rules_grant_access() {
        let rules = AccessRules::from_yaml(
            r#"
rules:
  - agents: [sisyphus]
    groups: [engineers]
    scopes: [prompt]
"#,
        )
        .unwrap();

        // Group membership matches.
        let caller = CallerView {
            users: &[],
            groups: &["engineers"],
            roles: &[],
        };
        assert!(rules.can_create_session("sisyphus", caller));
        assert!(rules.can_see_agent("sisyphus", caller));

        // User identity alone does not match.
        let caller = CallerView::from_users(&["bob"]);
        assert!(!rules.can_see_agent("sisyphus", caller));
    }

    #[test]
    fn role_only_rules_grant_access() {
        check_role_only_rules_grant_access();
    }

    fn check_role_only_rules_grant_access() {
        let rules = AccessRules::from_yaml(
            r#"
rules:
  - agents: [sisyphus]
    roles: [admin]
    scopes: [admin]
"#,
        )
        .unwrap();

        let caller = CallerView {
            users: &[],
            groups: &[],
            roles: &["admin"],
        };
        assert!(rules.can_see_agent("sisyphus", caller));
        // Admin alone does not grant prompt.
        assert!(!rules.can_create_session("sisyphus", caller));
        // Admin can access any session.
        assert!(rules.can_access_session("sisyphus", caller, Some("foreign")));
        assert!(rules.can_access_session("sisyphus", caller, None));
    }

    #[test]
    fn mixed_user_group_role_selectors_or_semantics() {
        let rules = AccessRules::from_yaml(
            r#"
rules:
  - agents: [sisyphus]
    users: [alice]
    groups: [engineers]
    roles: [operator]
    scopes: [prompt]
"#,
        )
        .unwrap();

        // Each selector category alone matches.
        for caller in [
            CallerView::from_users(&["alice"]),
            CallerView {
                users: &[],
                groups: &["engineers"],
                roles: &[],
            },
            CallerView {
                users: &[],
                groups: &[],
                roles: &["operator"],
            },
        ] {
            assert!(rules.can_create_session("sisyphus", caller), "{caller:?}");
        }

        // Unrelated values in each category don't match.
        let caller = CallerView {
            users: &["bob"],
            groups: &["hr"],
            roles: &["guest"],
        };
        assert!(!rules.can_see_agent("sisyphus", caller));
    }

    #[test]
    fn cross_rule_scope_union_with_different_selectors() {
        let rules = AccessRules::from_yaml(
            r#"
rules:
  - agents: [sisyphus]
    users: [bob]
    scopes: [prompt]
  - agents: [sisyphus]
    groups: [engineers]
    scopes: [admin]
  - agents: [sisyphus]
    roles: [operator]
    scopes: [prompt]
"#,
        )
        .unwrap();

        // User-only match: prompt only.
        let caller = CallerView::from_users(&["bob"]);
        let scopes = rules.scopes_for("sisyphus", caller);
        assert!(scopes.contains(Scope::Prompt));
        assert!(!scopes.contains(Scope::Admin));

        // Group-only match: admin only.
        let caller = CallerView {
            users: &[],
            groups: &["engineers"],
            roles: &[],
        };
        let scopes = rules.scopes_for("sisyphus", caller);
        assert!(!scopes.contains(Scope::Prompt));
        assert!(scopes.contains(Scope::Admin));

        // Combined: union scopes.
        let caller = CallerView {
            users: &["bob"],
            groups: &["engineers"],
            roles: &[],
        };
        let scopes = rules.scopes_for("sisyphus", caller);
        assert!(scopes.contains(Scope::Prompt));
        assert!(scopes.contains(Scope::Admin));
    }

    #[test]
    fn same_text_different_namespaces_never_matches() {
        // "admin" as group vs role vs user are distinct.
        let rules = AccessRules::from_yaml(
            r#"
rules:
  - agents: [sisyphus]
    users: [admin]
    scopes: [prompt]
  - agents: [daedalus]
    groups: [admin]
    scopes: [prompt]
  - agents: [atlas]
    roles: [admin]
    scopes: [prompt]
"#,
        )
        .unwrap();

        // User "admin" matches user rule only.
        let caller = CallerView::from_users(&["admin"]);
        assert!(rules.can_see_agent("sisyphus", caller));
        assert!(!rules.can_see_agent("daedalus", caller));
        assert!(!rules.can_see_agent("atlas", caller));

        // Group "admin" matches group rule only.
        let caller = CallerView {
            users: &[],
            groups: &["admin"],
            roles: &[],
        };
        assert!(!rules.can_see_agent("sisyphus", caller));
        assert!(rules.can_see_agent("daedalus", caller));
        assert!(!rules.can_see_agent("atlas", caller));

        // Role "admin" matches role rule only.
        let caller = CallerView {
            users: &[],
            groups: &[],
            roles: &["admin"],
        };
        assert!(!rules.can_see_agent("sisyphus", caller));
        assert!(!rules.can_see_agent("daedalus", caller));
        assert!(rules.can_see_agent("atlas", caller));
    }

    #[test]
    fn ownership_checks_users_only_not_groups_or_roles() {
        let rules = AccessRules::from_yaml(
            r#"
rules:
  - agents: [sisyphus]
    groups: [engineers]
    scopes: [prompt]
"#,
        )
        .unwrap();

        // Group grant provides prompt scope.
        let caller = CallerView {
            users: &[],
            groups: &["engineers"],
            roles: &[],
        };
        assert!(rules.can_create_session("sisyphus", caller));

        // But cannot access sessions owned by others, even when the group name
        // matches the owner (groups don't satisfy ownership).
        assert!(!rules.can_access_session("sisyphus", caller, Some("engineers")));
        assert!(!rules.can_access_session("sisyphus", caller, Some("bob")));

        // No user identities means no ownership match possible.
        assert!(!rules.can_access_session("sisyphus", caller, None));
    }

    #[test]
    fn group_role_admin_overrides_foreign_session_ownership() {
        let rules = AccessRules::from_yaml(
            r#"
rules:
  - agents: [sisyphus]
    groups: [ops]
    scopes: [admin]
"#,
        )
        .unwrap();

        let caller = CallerView {
            users: &["bob"], // Bob is a user but not owner.
            groups: &["ops"],
            roles: &[],
        };

        // Admin from group allows access to any session.
        assert!(rules.can_access_session("sisyphus", caller, Some("foreign")));
        assert!(rules.can_access_session("sisyphus", caller, None));

        // Admin scope from group grant still doesn't allow session creation
        // without prompt scope.
        assert!(!rules.can_create_session("sisyphus", caller));
    }

    #[test]
    fn missing_memberships_empty_access() {
        let rules = AccessRules::from_yaml(
            r#"
rules:
  - agents: [sisyphus]
    groups: [engineers]
    scopes: [prompt]
"#,
        )
        .unwrap();

        // No groups provided: no match.
        let caller = CallerView::from_users(&["bob"]);
        assert!(!rules.can_see_agent("sisyphus", caller));
    }

    #[test]
    fn wildcard_selectors_match_like_users() {
        let rules = AccessRules::from_yaml(
            r#"
rules:
  - agents: [sisyphus]
    groups: ["team-*"]
    roles: ["*"]
    scopes: [prompt]
"#,
        )
        .unwrap();

        for group in ["team-alpha", "team-beta"] {
            let caller = CallerView {
                users: &[],
                groups: &[group],
                roles: &[],
            };
            assert!(
                rules.can_create_session("sisyphus", caller),
                "group={group}"
            );
        }

        for role in ["admin", "guest", "anything"] {
            let caller = CallerView {
                users: &[],
                groups: &[],
                roles: &[role],
            };
            assert!(rules.can_see_agent("sisyphus", caller), "role={role}");
        }
    }

    #[test]
    fn duplicate_memberships_do_not_change_grants() {
        let rules = AccessRules::from_yaml(
            r#"
rules:
  - agents: [sisyphus]
    groups: [engineers]
    scopes: [prompt]
"#,
        )
        .unwrap();

        let caller = CallerView {
            users: &[],
            groups: &["engineers", "engineers"],
            roles: &[],
        };
        assert!(rules.can_see_agent("sisyphus", caller));
    }

    #[test]
    fn invalid_glob_identifies_category_in_error() {
        let error = AccessRules::from_yaml(
            r#"
rules:
  - agents: [sisyphus]
    groups: ["[unclosed"]
"#,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("invalid groups glob"), "{error}");
        assert!(error.contains("[unclosed"), "{error}");
    }

    #[test]
    fn user_only_config_unchanged() {
        // Existing user-only YAML parses identically.
        let rules = AccessRules::from_yaml(
            r#"
rules:
  - agents: [sisyphus]
    users: [bob]
    scopes: [prompt]
"#,
        )
        .unwrap();

        let caller = CallerView::from_users(&["bob"]);
        assert!(rules.can_create_session("sisyphus", caller));
        assert!(rules.can_access_session("sisyphus", caller, Some("bob")));
        assert!(!rules.can_access_session("sisyphus", caller, Some("alice")));
    }
}
