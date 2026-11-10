//! Every shipped agent must load and render its prompt through the same code
//! the runtime uses at activation time.
//!
//! Resolving `variables: [{name, path}]` by hand in the test would only prove
//! the markdown files line up; it would not catch a caller that installs an
//! agent without running the resolver. So this test installs the packages into
//! a temp config dir and drives `resolve_variables` — the production path — for
//! each agent.

use std::{
    collections::BTreeSet,
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
};

use harnx_core::tool::ToolDeclaration;
use harnx_runtime::client::{retrieve_model, ModelType};
use harnx_runtime::config::agent::{load_with_qualified_name, resolve_variables};
use harnx_runtime::config::Config;

#[path = "support/shipped_model_policy.rs"]
mod shipped_model_policy;

const PACKAGES: [&str; 2] = ["pantheon", "coding"];

struct EnvVarGuard {
    key: &'static str,
    previous: Option<std::ffi::OsString>,
}

impl EnvVarGuard {
    fn set_path(key: &'static str, value: &Path) -> Self {
        let previous = std::env::var_os(key);
        // SAFETY: nextest gives each test its own process, so nothing else
        // mutates the environment concurrently.
        unsafe { std::env::set_var(key, value) };
        Self { key, previous }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        // SAFETY: see EnvVarGuard::set_path.
        unsafe {
            match &self.previous {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }
}

fn workspace_root() -> Option<PathBuf> {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .find(|ancestor| ancestor.join("packages").is_dir())
        .map(Path::to_path_buf)
}

/// Load an agent, resolve its variables, and render the prompt with tools.
fn render_agent_prompt(qualified_name: &str, tools: &[ToolDeclaration]) -> Result<String, String> {
    let agent_path = Config::agent_file(qualified_name);
    let mut agent = load_with_qualified_name(&agent_path, qualified_name)
        .map_err(|e| format!("{qualified_name}: failed to load: {e}"))?;
    resolve_variables(&mut agent)
        .map_err(|e| format!("{qualified_name}: failed to resolve variables: {e}"))?;
    agent
        .system_text_with_tools(tools)
        .map_err(|e| format!("{qualified_name}: failed to render prompt: {e}"))
}

/// Test case for sandbox branching behavior verification.
struct BranchTestCase {
    agent: &'static str,
    with_sandbox: bool,
    must_contain: &'static str,
    must_not_contain: Option<&'static str>,
}

/// Evaluate a single branch test case, returning an error message if it fails.
fn evaluate_branch_test_case(
    test_case: &BranchTestCase,
    sandbox_tool: &ToolDeclaration,
) -> Option<String> {
    let tools = if test_case.with_sandbox {
        std::slice::from_ref(sandbox_tool)
    } else {
        &[]
    };

    let prompt = match render_agent_prompt(test_case.agent, tools) {
        Ok(prompt) => prompt,
        Err(err) => return Some(err),
    };

    if !prompt.contains(test_case.must_contain) {
        return Some(format!(
            "{}: with_sandbox={}, expected to contain '{}' but did not",
            test_case.agent, test_case.with_sandbox, test_case.must_contain
        ));
    }

    if let Some(not_contain) = test_case.must_not_contain {
        if prompt.contains(not_contain) {
            return Some(format!(
                "{}: with_sandbox={}, expected NOT to contain '{}' but did",
                test_case.agent, test_case.with_sandbox, not_contain
            ));
        }
    }

    None
}

struct FragmentChecker<'a> {
    file_name: &'a str,
    content: &'a str,
    failures: &'a mut Vec<String>,
}

impl<'a> FragmentChecker<'a> {
    fn check_jinja(&mut self) {
        for tag in ["{%", "{{"] {
            if self.content.contains(tag) {
                self.failures.push(format!(
                    "{}: contains forbidden Jinja tag '{tag}'",
                    self.file_name
                ));
            }
        }
    }

    fn check_terms(&mut self) {
        const FORBIDDEN_TERMS: [&str; 6] = [
            "icap",
            "github_token",
            "caching-proxy",
            "sidecar",
            "agentgateway",
            "formative",
        ];
        let lower = self.content.to_lowercase();
        for term in FORBIDDEN_TERMS {
            if lower.contains(term) {
                self.failures.push(format!(
                    "{}: contains forbidden term '{term}'",
                    self.file_name
                ));
            }
        }
        if lower.contains("extend") && lower.contains("expiry") {
            self.failures.push(format!(
                "{}: contains forbidden expiry extension guidance",
                self.file_name
            ));
        }
    }
}

/// Verify sandbox workflow fragments contain no forbidden content.
fn check_fragment_hygiene(file_name: &str, content: &str) -> Vec<String> {
    let mut failures = Vec::new();
    let mut checker = FragmentChecker {
        file_name,
        content,
        failures: &mut failures,
    };
    checker.check_jinja();
    checker.check_terms();
    failures
}

/// Point `HARNX_CONFIG_DIR` at a temp dir whose `packages/` mirrors the
/// workspace's, so `Config::agent_file("pantheon/sisyphus")` resolves to the
/// real shipped agent.
fn install_packages(workspace_root: &Path) -> (tempfile::TempDir, EnvVarGuard) {
    let temp = tempfile::tempdir().expect("temp config dir");
    let packages_dir = temp.path().join("packages");
    fs::create_dir_all(&packages_dir).expect("create packages dir");
    for package in PACKAGES {
        let source = workspace_root.join("packages").join(package);
        let link = packages_dir.join(package);
        #[cfg(unix)]
        std::os::unix::fs::symlink(&source, &link).expect("link package into config dir");
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(&source, &link).expect("link package into config dir");
    }
    let guard = EnvVarGuard::set_path("HARNX_CONFIG_DIR", temp.path());
    (temp, guard)
}

fn agent_stems(agents_dir: &Path, failures: &mut Vec<String>) -> Vec<String> {
    let entries = match fs::read_dir(agents_dir) {
        Ok(entries) => entries,
        Err(error) => {
            failures.push(format!("{}: {error}", agents_dir.display()));
            return Vec::new();
        }
    };
    let mut stems: Vec<String> = entries
        .filter_map(|entry| match entry {
            Ok(entry) => {
                let path = entry.path();
                if path.is_file() && path.extension() == Some(OsStr::new("md")) {
                    path.file_stem()
                        .and_then(OsStr::to_str)
                        .map(ToOwned::to_owned)
                } else {
                    None
                }
            }
            Err(error) => {
                failures.push(format!("{}: {error}", agents_dir.display()));
                None
            }
        })
        .collect();
    stems.sort();
    stems
}

/// What one agent contributed to the run: how many `path:` variables it
/// resolved, and whether its prompt rendered.
#[derive(Default)]
struct AgentOutcome {
    file_backed_variables: usize,
    rendered: bool,
}

/// Every `path:` variable must come back with content. An empty value means
/// the resolver ran but did not actually read the file.
fn check_file_backed_variables(
    agent: &harnx_runtime::config::Agent,
    qualified_name: &str,
    failures: &mut Vec<String>,
) -> usize {
    let declared: Vec<&str> = agent
        .defined_variables()
        .iter()
        .filter(|variable| variable.path.is_some())
        .map(|variable| variable.name.as_str())
        .collect();
    for name in &declared {
        let resolved = agent.variables().get(*name).is_some_and(|v| !v.is_empty());
        if !resolved {
            failures.push(format!(
                "{qualified_name}: variable '{name}' declares a path but resolved to nothing"
            ));
        }
    }
    declared.len()
}

fn check_agent(qualified_name: &str, failures: &mut Vec<String>) -> AgentOutcome {
    let agent_path = Config::agent_file(qualified_name);
    let mut agent = match load_with_qualified_name(&agent_path, qualified_name) {
        Ok(agent) => agent,
        Err(error) => {
            failures.push(format!("{qualified_name}: {error:#}"));
            return AgentOutcome::default();
        }
    };
    if let Err(error) = resolve_variables(&mut agent) {
        failures.push(format!("{qualified_name}: {error:#}"));
        return AgentOutcome::default();
    }

    let file_backed_variables = check_file_backed_variables(&agent, qualified_name, failures);
    let rendered = match agent.system_text() {
        Ok(_) => true,
        Err(error) => {
            failures.push(format!("{qualified_name}: {error:#}"));
            false
        }
    };
    AgentOutcome {
        file_backed_variables,
        rendered,
    }
}

#[test]
fn agent_prompt_rendering_renders_all_shipped_agents() {
    // `install_packages` sets HARNX_CONFIG_DIR process-wide; the guards are
    // only sound because nextest runs each test in its own process.
    harnx_core::require_nextest();
    let Some(workspace_root) = workspace_root() else {
        // Published crates do not include workspace package assets.
        return;
    };
    let (_temp, _config_guard) = install_packages(&workspace_root);

    let mut failures = Vec::new();
    let mut file_backed_variables = 0usize;
    let mut rendered = 0usize;

    for package in PACKAGES {
        let agents_dir = workspace_root.join("packages").join(package).join("agents");
        for stem in agent_stems(&agents_dir, &mut failures) {
            let outcome = check_agent(&format!("{package}/{stem}"), &mut failures);
            file_backed_variables += outcome.file_backed_variables;
            rendered += usize::from(outcome.rendered);
        }
    }

    assert!(
        failures.is_empty(),
        "failed to load or render shipped agent prompts:\n{}",
        failures.join("\n")
    );
    assert!(rendered > 0, "no shipped agents were rendered");
    assert!(
        file_backed_variables > 0,
        "no shipped agent exercised a file-backed variable"
    );
}

const ELIGIBLE_SHELL_AGENTS: [&str; 34] = [
    "coding/coder",
    "pantheon/aeacus",
    "pantheon/apollo",
    "pantheon/argus",
    "pantheon/aristarchus",
    "pantheon/athena",
    "pantheon/atlas",
    "pantheon/calliope",
    "pantheon/clio",
    "pantheon/erato",
    "pantheon/euterpe",
    "pantheon/hephaestus",
    "pantheon/hermes",
    "pantheon/hestia",
    "pantheon/iris",
    "pantheon/laocoon",
    "pantheon/melpomene",
    "pantheon/metis",
    "pantheon/minos",
    "pantheon/mnemosyne",
    "pantheon/momus",
    "pantheon/nemesis",
    "pantheon/opis",
    "pantheon/peitho",
    "pantheon/plato",
    "pantheon/polyhymnia",
    "pantheon/pytheas",
    "pantheon/rhadamanthus",
    "pantheon/sisyphus",
    "pantheon/terpsichore",
    "pantheon/thalia",
    "pantheon/tyche",
    "pantheon/urania",
    "pantheon/zosimus",
];

const INELIGIBLE_SHELL_AGENTS: [&str; 12] = [
    "coding/compact-coder",
    "pantheon/compact-argus",
    "pantheon/compact-deploy",
    "pantheon/compact-dev",
    "pantheon/compact-mnemosyne",
    "pantheon/compact-planner",
    "pantheon/compact-reliability",
    "pantheon/compact-researcher",
    "pantheon/compact-reviewer",
    "pantheon/daedalus",
    "pantheon/librarian",
    "pantheon/oracle",
];

const SHELL_EXECUTION_TOOLS: [&str; 3] = ["bash_spawn", "bash_wait", "bash_terminate"];

fn assert_eligible_shell_prompt(qualified_name: &str, prompt: &str) {
    assert!(
        prompt.contains("bash_spawn"),
        "{qualified_name}: missing bash_spawn"
    );
    assert!(
        prompt.contains("wait: { timeout_secs: 30 }"),
        "{qualified_name}: missing wait example"
    );
    let lower = prompt.to_lowercase();
    assert!(
        lower.contains("prefer") && lower.contains("nested `wait`"),
        "{qualified_name}: missing wait preference"
    );
    assert!(
        prompt.contains("bash_wait") && prompt.contains("bash_terminate"),
        "{qualified_name}: missing follow-up tools"
    );
    assert!(
        lower.contains("wait expiry is not process termination")
            || lower.contains("wait expiry does not terminate")
            || lower.contains("wait expires, the process continues running"),
        "{qualified_name}: missing expiry lifecycle guidance"
    );
    assert!(
        lower.contains("under 5 minutes"),
        "{qualified_name}: missing cache-TTL recommendation"
    );
    assert!(
        lower.contains("120 seconds") || lower.contains("120s"),
        "{qualified_name}: missing 120-second default"
    );
}

fn assert_ineligible_shell_prompt(qualified_name: &str, prompt: &str, granted: &[String]) {
    assert!(
        !SHELL_EXECUTION_TOOLS
            .iter()
            .any(|tool| granted.iter().any(|name| name == tool)),
        "{qualified_name}: ineligible agent has shell-tool grants: {granted:?}"
    );
    let lower = prompt.to_lowercase();
    assert!(
        !SHELL_EXECUTION_TOOLS
            .iter()
            .any(|tool| lower.contains(tool)),
        "{qualified_name}: ineligible prompt mentions unavailable shell tools"
    );
    assert!(
        !lower.contains("wait: { timeout_secs: 30 }")
            && !lower.contains("nested `wait`")
            && !lower.contains("wait expiry is not process termination"),
        "{qualified_name}: ineligible prompt contains nested-wait guidance"
    );
}

fn rendered_shell_agent_inventory(workspace_root: &Path) -> (BTreeSet<String>, BTreeSet<String>) {
    let mut eligible = BTreeSet::new();
    let mut ineligible = BTreeSet::new();
    for package in PACKAGES {
        let agents_dir = workspace_root.join("packages").join(package).join("agents");
        let mut failures = Vec::new();
        let stems = agent_stems(&agents_dir, &mut failures);
        assert!(
            failures.is_empty(),
            "{}: {failures:?}",
            agents_dir.display()
        );
        for stem in stems {
            let qualified_name = format!("{package}/{stem}");
            let agent =
                load_with_qualified_name(&Config::agent_file(&qualified_name), &qualified_name)
                    .unwrap_or_else(|error| panic!("{qualified_name}: failed to load: {error}"));
            let granted = agent.use_tools().unwrap_or_default();
            let prompt = render_agent_prompt(&qualified_name, &[])
                .unwrap_or_else(|error| panic!("{qualified_name}: {error}"));
            if SHELL_EXECUTION_TOOLS
                .iter()
                .all(|tool| granted.iter().any(|name| name == tool))
            {
                eligible.insert(qualified_name.clone());
                assert_eligible_shell_prompt(&qualified_name, &prompt);
            } else {
                ineligible.insert(qualified_name.clone());
                assert_ineligible_shell_prompt(&qualified_name, &prompt, &granted);
            }
        }
    }
    (eligible, ineligible)
}

/// Check deployed guidance and tool grants on fully rendered activation prompts.
#[test]
fn rendered_shell_guidance_matches_agent_tool_eligibility() {
    harnx_core::require_nextest();
    let Some(workspace_root) = workspace_root() else {
        return;
    };
    let (_temp, _config_guard) = install_packages(&workspace_root);
    let (eligible, ineligible) = rendered_shell_agent_inventory(&workspace_root);
    let expected_eligible: BTreeSet<_> = ELIGIBLE_SHELL_AGENTS
        .into_iter()
        .map(str::to_owned)
        .collect();
    let expected_ineligible: BTreeSet<_> = INELIGIBLE_SHELL_AGENTS
        .into_iter()
        .map(str::to_owned)
        .collect();
    assert_eq!(
        eligible, expected_eligible,
        "eligible agent inventory changed"
    );
    assert_eq!(
        ineligible, expected_ineligible,
        "ineligible agent inventory changed"
    );
}

async fn check_agent_models(config: &Config, qualified_name: &str) -> (usize, Vec<String>) {
    let agent_path = Config::agent_file(qualified_name);
    let agent = match load_with_qualified_name(&agent_path, qualified_name) {
        Ok(agent) => agent,
        Err(error) => return (0, vec![format!("{qualified_name}: {error:#}")]),
    };
    let model_ids: Vec<_> = agent
        .model_id()
        .into_iter()
        .chain(agent.model_fallbacks().iter().map(String::as_str))
        .collect();
    let mut checked = 0;
    let mut failures = Vec::new();
    for model_id in &model_ids {
        checked += 1;
        if let Err(error) = check_model_metadata(config, model_id) {
            failures.push(format!("{qualified_name}: {error}"));
        }
    }
    if let Err(error) = shipped_model_policy::check_chain(config, &model_ids) {
        failures.push(format!("{qualified_name}: {error:#}"));
    }
    if let Err(error) = shipped_model_policy::check_single_provider_fallbacks(config, &agent).await
    {
        failures.push(format!("{qualified_name}: {error:#}"));
    }
    (checked, failures)
}

fn check_model_metadata(config: &Config, model_id: &str) -> Result<(), String> {
    let model = retrieve_model(&config.clients, model_id, ModelType::Chat)
        .map_err(|error| format!("model {model_id} does not resolve: {error:#}"))?;
    if model.max_input_tokens().is_none() || !model.supports_tool_use() {
        return Err(format!(
            "{model_id} lost its catalog limits or tool capabilities"
        ));
    }
    if model.real_name().starts_with("gpt-") && model.endpoint() != Some("responses") {
        return Err(format!("{model_id} lost its Responses endpoint metadata"));
    }
    Ok(())
}

#[tokio::test]
async fn shipped_agent_models_resolve_with_required_endpoint_metadata() {
    harnx_core::require_nextest();
    let Some(workspace_root) = workspace_root() else {
        return;
    };
    let (temp, _config_guard) = install_packages(&workspace_root);
    let config = Config::load_from_file(&temp.path().join("config.yaml"))
        .expect("load package client configs");
    let mut failures = Vec::new();
    let mut checked = 0usize;

    for package in PACKAGES {
        let agents_dir = workspace_root.join("packages").join(package).join("agents");
        for stem in agent_stems(&agents_dir, &mut failures) {
            let qualified_name = format!("{package}/{stem}");
            let (agent_checked, agent_failures) =
                check_agent_models(&config, &qualified_name).await;
            checked += agent_checked;
            failures.extend(agent_failures);
        }
    }

    assert!(
        checked > 0,
        "no shipped agent model references were checked"
    );
    assert!(
        failures.is_empty(),
        "invalid shipped agent model references:\n{}",
        failures.join("\n")
    );
}

#[tokio::test]
async fn optional_zai_patch_prefers_direct_glm_and_retains_bedrock() {
    harnx_core::require_nextest();
    let Some(root) = workspace_root() else {
        return;
    };
    let (temp, _guard) = install_packages(&root);
    for package in PACKAGES {
        fs::copy(
            root.join("packages")
                .join(package)
                .join("zai.patch.example.yaml"),
            temp.path()
                .join("packages")
                .join(format!("{package}.patch.yaml")),
        )
        .unwrap();
    }
    let config = Config::load_from_file(&temp.path().join("config.yaml")).unwrap();
    for (name, expected) in [
        ("pantheon/athena", true),
        ("pantheon/mnemosyne", true),
        ("pantheon/atlas", true),
        ("coding/coder", true),
        ("coding/compact-coder", false),
    ] {
        let agent = load_with_qualified_name(&Config::agent_file(name), name).unwrap();
        let ids: Vec<_> = agent
            .model_id()
            .into_iter()
            .chain(agent.model_fallbacks().iter().map(String::as_str))
            .collect();
        let direct = ids.iter().position(|id| id.ends_with("/zai:glm-5.3"));
        assert_eq!(direct.is_some(), expected, "{name}: {ids:?}");
        if let Some(index) = direct {
            assert!(
                ids[index + 1].ends_with("/bedrock:us.zai.glm-5.3"),
                "{name}: {ids:?}"
            );
        }
        let (_, failures) = check_agent_models(&config, name).await;
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }
}

#[test]
fn clio_prompt_checks_for_an_existing_pull_request_after_push() {
    harnx_core::require_nextest();
    let Some(workspace_root) = workspace_root() else {
        return;
    };
    let (_temp, _config_guard) = install_packages(&workspace_root);

    let qualified_name = "pantheon/clio";
    let agent_path = Config::agent_file(qualified_name);
    let mut agent = load_with_qualified_name(&agent_path, qualified_name).expect("load Clio");
    resolve_variables(&mut agent).expect("resolve Clio variables");
    let prompt = agent.system_text().expect("render Clio prompt");

    assert!(
        prompt.contains("gh pr list --head \"$branch\" --state open --limit 1"),
        "Clio must query for an open pull request on the pushed branch"
    );
    assert!(
        prompt.contains("url,state,isDraft,mergeStateStatus,reviewDecision,statusCheckRollup"),
        "Clio must retrieve the existing pull request's link and status"
    );
    assert!(
        prompt.contains("Only when no open pull request exists for the branch"),
        "Clio must reserve the compare-link fallback for branches without an open pull request"
    );
}

#[test]
fn shipped_agent_prompts_use_canonical_plan_urls_and_plan_id_trailers() {
    harnx_core::require_nextest();
    let Some(workspace_root) = workspace_root() else {
        return;
    };
    let (_temp, _config_guard) = install_packages(&workspace_root);

    let clio = render_agent_prompt("pantheon/clio", &[]).expect("render Clio prompt");
    assert!(clio.contains("Plan-Id: cid:plan:<agent>/<sid>/<slug>"));

    let atlas = render_agent_prompt("pantheon/atlas", &[]).expect("render Atlas prompt");
    assert!(atlas.contains("Plan-Id: cid:plan:"));
    assert!(atlas.contains("cid:plan:"));
    assert!(!atlas.contains("when given a plan name:"));
    assert!(!atlas.contains("with the extracted plan name"));

    let daedalus = render_agent_prompt("pantheon/daedalus", &[]).expect("render Daedalus prompt");
    assert!(daedalus.contains("plans_add_plan"));
    assert!(daedalus.contains("cid:plan:"));

    let sisyphus = render_agent_prompt("pantheon/sisyphus", &[]).expect("render Sisyphus prompt");
    assert!(sisyphus.contains("Plan-Id: cid:plan:"));
    assert!(sisyphus.contains("cid:plan:"));

    let aristarchus =
        render_agent_prompt("pantheon/aristarchus", &[]).expect("render Aristarchus prompt");
    assert!(aristarchus.contains("Plan-Id: cid:plan:"));
    assert!(aristarchus.contains("cid:plan:"));

    let coder = render_agent_prompt("coding/coder", &[]).expect("render Coder prompt");
    assert!(coder.contains("plans_add_plan"));
    assert!(coder.contains("cid:plan:"));
}

#[test]
fn plan_creating_agents_expose_add_plan() {
    harnx_core::require_nextest();
    let Some(workspace_root) = workspace_root() else {
        return;
    };
    let (_temp, _config_guard) = install_packages(&workspace_root);

    for qualified_name in ["pantheon/sisyphus", "pantheon/daedalus"] {
        let agent_path = Config::agent_file(qualified_name);
        let agent = load_with_qualified_name(&agent_path, qualified_name)
            .unwrap_or_else(|error| panic!("failed to load {qualified_name}: {error}"));
        let tools = agent.use_tools().expect("agent must declare use_tools");
        assert!(
            tools.iter().any(|tool| tool == "plans_add_plan"),
            "{qualified_name} must expose plans_add_plan to create session-owned plans"
        );

        let prompt = render_agent_prompt(qualified_name, &[])
            .unwrap_or_else(|error| panic!("failed to render {qualified_name}: {error}"));
        assert!(prompt.contains("plans_add_plan(name=\"slug\")"));
        assert!(
            prompt.contains("Use the returned canonical `cid:plan:...` URL"),
            "{qualified_name} must tell agents to use the returned canonical plan URL"
        );
        assert!(
            prompt.contains("do not construct one yourself"),
            "{qualified_name} must not require agents to construct session-owned plan URLs"
        );
    }
}

/// Check the assembled prompts: stale reminders in an outer agent must not
/// contradict its shared policy. This does not establish model behavior.
#[test]
fn planning_prompts_do_not_restore_question_quotas_or_early_approval_gates() {
    harnx_core::require_nextest();
    let Some(workspace_root) = workspace_root() else {
        return;
    };
    let (_temp, _config_guard) = install_packages(&workspace_root);

    for (name, policy) in [
        ("pantheon/daedalus", "zero questions is valid"),
        ("pantheon/metis", "zero candidate questions is valid"),
        ("pantheon/momus", "without a numeric quota"),
    ] {
        let prompt = render_agent_prompt(name, &[]).expect("render planning agent");
        let normalized = prompt
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase();
        assert!(normalized.contains(policy), "{name}: missing {policy}");
        for obsolete in [
            "3-5 key questions maximum",
            "maximum 5 questions",
            "approve by default",
            "80% clear is good enough",
            "explicit confirmation before moving to phase 2",
            "max 3 issues",
            "maximum 3 issues per rejection",
            "do not proceed without user confirmation",
        ] {
            assert!(
                !normalized.contains(obsolete),
                "{name}: assembled prompt restores obsolete policy: {obsolete}"
            );
        }
    }
}

#[test]
fn daedalus_research_and_review_tools_keep_the_execution_approval_hook() {
    harnx_core::require_nextest();
    let Some(workspace_root) = workspace_root() else {
        return;
    };
    let (_temp, _config_guard) = install_packages(&workspace_root);
    let agent = load_with_qualified_name(
        &Config::agent_file("pantheon/daedalus"),
        "pantheon/daedalus",
    )
    .expect("load Daedalus");
    let tools = agent.use_tools().expect("Daedalus tools");
    // async_delegation_guidance_renders_and_selects_tools_for_all_integrating_agents
    // checks send/wait/load through the production selector for every prompt target.
    for required in [
        "metis_session_prompt",
        "pytheas_session_prompt",
        "momus_session_prompt",
        "atlas_session_handoff",
    ] {
        assert!(
            tools.iter().any(|tool| tool == required),
            "missing {required}"
        );
    }
    let hooks = agent.hooks().expect("execution approval hook");
    let hook = hooks
        .entries
        .iter()
        .find(|hook| hook.command.contains("^atlas_session_handoff$"))
        .expect("Atlas handoff hook");
    assert_ne!(hook.async_hook, Some(true));
    let args = shell_words::split(&hook.command).expect("valid hook command quoting");
    assert_eq!(args.len(), 7);
    assert_eq!(
        args[..6],
        [
            "harnx-claude-compatible-hook-server",
            "--event",
            "PreToolUse",
            "--matcher",
            "^atlas_session_handoff$",
            "--jaq",
        ]
    );
    let output: serde_json::Value = serde_json::from_str(&args[6]).expect("literal hook output");
    assert_eq!(output["hookSpecificOutput"]["permissionDecision"], "ask");
}

/// Target agents that should declare `sandbox_connect` in their front-matter `use_tools`.
const TARGET_AGENTS: [&str; 11] = [
    "atlas",
    "apollo",
    "athena",
    "hephaestus",
    "hermes",
    "hestia",
    "iris",
    "peitho",
    "plato",
    "pytheas",
    "zosimus",
];

/// Synthetic sandbox_connect tool declaration for testing.
fn sandbox_connect_tool() -> ToolDeclaration {
    ToolDeclaration {
        name: "sandbox_connect".to_string(),
        description: "Connect to sandbox".to_string(),
        parameters: Default::default(),
        mcp_tool_name: None,
        mcp_server_name: None,
        call_template: None,
        result_template: None,
        idempotent_hint: None,
        read_only_hint: None,
        kind: None,
    }
}

#[test]
fn target_agents_declare_sandbox_connect_in_use_tools() {
    harnx_core::require_nextest();
    let Some(workspace_root) = workspace_root() else {
        return;
    };
    let (_temp, _config_guard) = install_packages(&workspace_root);

    let mut failures = Vec::new();

    for agent_name in TARGET_AGENTS {
        let qualified_name = format!("pantheon/{}", agent_name);
        let agent_path = Config::agent_file(&qualified_name);
        let agent = match load_with_qualified_name(&agent_path, &qualified_name) {
            Ok(agent) => agent,
            Err(error) => {
                failures.push(format!("{}: failed to load: {error}", qualified_name));
                continue;
            }
        };

        let use_tools = agent.use_tools();
        let has_sandbox_connect = use_tools
            .as_ref()
            .map(|tools| tools.iter().any(|t| t == "sandbox_connect"))
            .unwrap_or(false);

        if !has_sandbox_connect {
            failures.push(format!(
                "{}: does not declare sandbox_connect in front-matter use_tools",
                qualified_name
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "target agents missing sandbox_connect in use_tools:\n{}",
        failures.join("\n")
    );
}

/// All 22 test cases for sandbox branching behavior verification.
/// Categories:
/// - CREATE: atlas (2 test cases)
/// - INHERIT/Workers: apollo, athena, hephaestus, hermes, hestia, iris, peitho, plato (16 test cases)
/// - DUAL/Research: pytheas, zosimus (4 test cases)
fn branch_test_cases() -> Vec<BranchTestCase> {
    let mut cases = vec![
        // CREATE category: Atlas
        BranchTestCase {
            agent: "pantheon/atlas",
            with_sandbox: true,
            must_contain: "## Sandbox environment workflow",
            must_not_contain: Some("## Default Repository"),
        },
        BranchTestCase {
            agent: "pantheon/atlas",
            with_sandbox: false,
            must_contain: "## Default Repository",
            must_not_contain: Some("## Sandbox environment workflow"),
        },
    ];

    const WORKERS: [&str; 8] = [
        "pantheon/apollo",
        "pantheon/athena",
        "pantheon/hephaestus",
        "pantheon/hermes",
        "pantheon/hestia",
        "pantheon/iris",
        "pantheon/peitho",
        "pantheon/plato",
    ];
    for agent in WORKERS {
        cases.push(BranchTestCase {
            agent,
            with_sandbox: true,
            must_contain: "## Sandbox environment workflow",
            must_not_contain: Some("You work locally"),
        });
        cases.push(BranchTestCase {
            agent,
            with_sandbox: false,
            must_contain: "You work locally",
            must_not_contain: Some("## Sandbox environment workflow"),
        });
    }

    const RESEARCH: [&str; 2] = ["pantheon/pytheas", "pantheon/zosimus"];
    for agent in RESEARCH {
        cases.push(BranchTestCase {
            agent,
            with_sandbox: true,
            must_contain: "## Sandbox environment workflow",
            must_not_contain: Some("## Local environment Workflow"),
        });
        cases.push(BranchTestCase {
            agent,
            with_sandbox: false,
            must_contain: "## Local environment Workflow",
            must_not_contain: Some("## Sandbox environment workflow"),
        });
    }

    cases
}

#[test]
fn sandbox_connect_tool_branches_correctly_for_category_agents() {
    harnx_core::require_nextest();
    let Some(workspace_root) = workspace_root() else {
        return;
    };
    let (_temp, _config_guard) = install_packages(&workspace_root);

    let sandbox_tool = sandbox_connect_tool();
    let mut failures = Vec::new();

    for test_case in branch_test_cases() {
        if let Some(err) = evaluate_branch_test_case(&test_case, &sandbox_tool) {
            failures.push(err);
        }
    }

    assert!(
        failures.is_empty(),
        "sandbox branching behavior incorrect:\n{}",
        failures.join("\n")
    );
}
/// Verify all shared sandbox-workflow-*.md fragments are pure markdown and harnx-native:
/// - NO Jinja tags ({% or {{)
/// - NO forbidden terms: ICAP, GITHUB_TOKEN, caching-proxy, sidecar, agentgateway, formative, "extend .*expiry"
#[test]
fn shared_sandbox_workflow_fragments_are_pure_markdown_and_harnx_native() {
    let Some(workspace_root) = workspace_root() else {
        return;
    };

    let fragments_dir = workspace_root.join("packages/pantheon/agents/shared");
    let sandbox_workflow_files: Vec<_> = fs::read_dir(&fragments_dir)
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with("sandbox-workflow-"))
        })
        .collect();

    assert!(
        !sandbox_workflow_files.is_empty(),
        "no sandbox-workflow-*.md files found in {}",
        fragments_dir.display()
    );

    let mut failures = Vec::new();

    for entry in sandbox_workflow_files {
        let path = entry.path();
        let content = fs::read_to_string(&path).unwrap_or_default();
        let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("?");
        failures.extend(check_fragment_hygiene(file_name, &content));
    }

    assert!(
        failures.is_empty(),
        "sandbox workflow fragments contain forbidden content:\n{}",
        failures.join("\n")
    );
}

const COMPACTION_PROMPT_PHRASES: [&str; 2] = [
    "someone reading only the summary should be able to tell what the user last asked and what the agent last answered",
    "If the latest user prompt isn't kept verbatim after the summary, quote it",
];

fn compaction_prompt_paths(workspace_root: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for package in PACKAGES {
        let agents_dir = workspace_root.join("packages").join(package).join("agents");
        let entries = fs::read_dir(&agents_dir)
            .unwrap_or_else(|err| panic!("failed to read {}: {err}", agents_dir.display()));
        paths.extend(
            entries
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| {
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.starts_with("compact-") && name.ends_with(".md"))
                }),
        );
    }
    paths.push(workspace_root.join("example_config/agents/example-compaction-agent.md"));
    paths
}

fn assert_compaction_prompt_contains_phrases(path: &Path) {
    let content = fs::read_to_string(path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));
    let missing: Vec<_> = COMPACTION_PROMPT_PHRASES
        .iter()
        .filter(|phrase| !content.contains(**phrase))
        .collect();
    assert!(
        missing.is_empty(),
        "{} is missing compaction instruction phrases: {:?}",
        path.display(),
        missing
    );
}

/// Verify all packaged compaction prompts and the example compaction agent.
#[test]
fn compaction_prompts_contain_shared_request_reply_instruction() {
    let Some(workspace_root) = workspace_root() else {
        return;
    };
    let prompt_paths = compaction_prompt_paths(&workspace_root);
    assert!(
        prompt_paths.len() >= 10,
        "expected at least 10 markdown compaction prompts to be checked"
    );
    prompt_paths
        .iter()
        .for_each(|path| assert_compaction_prompt_contains_phrases(path));
}

/// Agents that integrate the shared async-delegation protocol. Every one of
/// them must resolve the `{{async_delegation}}` include and select the
/// send/wait/load tools for each of its delegation targets.
const ASYNC_DELEGATION_AGENTS: [&str; 16] = [
    "apollo",
    "aristarchus",
    "athena",
    "atlas",
    "daedalus",
    "hephaestus",
    "hermes",
    "hestia",
    "iris",
    "melpomene",
    "metis",
    "peitho",
    "plato",
    "polyhymnia",
    "sisyphus",
    "urania",
];

/// Heading the shared async-delegation include contributes. A rendered prompt
/// that resolves the include contains it exactly once; a duplicate include
/// renders it twice, and an unresolved one (or a missing protocol) renders it
/// zero times.
const ASYNC_DELEGATION_HEADING: &str = "Async Sub-Agent Delegation (session_send / session_wait)";

/// Semantic phrases the shared async-delegation protocol must contribute to
/// every integrating agent's rendered prompt: preferred 120-second soft wait,
/// inherited hard policy, durable handle, re-wait safety, explicit
/// next_index→read.index_min paging, and cancellation only when the tool is
/// available.
const ASYNC_DELEGATION_PHRASES: [&str; 8] = [
    "wait: {seconds: 120}",
    "hard execution deadline stays inherited from the target agent's policy",
    "durable handle: `session_id` + `prompt_id`",
    "Repeated waits recover the same result and never consume it",
    "returned `next_index` as `read.index_min`",
    "If a `{agent}_session_cancel` tool is available to you",
    "Soft wait expiry alone never stops the child",
    "`{agent}_session_prompt` remains fully supported for synchronous delegation",
];

/// Every session tool name the pantheon worker registers per configured agent,
/// spelled the way the provider exposes them to same-package agents.
const SESSION_TOOL_SUFFIXES: [&str; 6] = [
    "session_new",
    "session_prompt",
    "session_send",
    "session_wait",
    "session_load",
    "session_cancel",
];

/// Load and resolve one integrating agent; returns it with its `use_tools`.
fn loaded_async_agent(
    qualified_name: &str,
) -> Result<(harnx_runtime::config::Agent, Vec<String>), String> {
    let agent_path = Config::agent_file(qualified_name);
    let mut agent = load_with_qualified_name(&agent_path, qualified_name)
        .map_err(|error| format!("{qualified_name}: failed to load: {error}"))?;
    resolve_variables(&mut agent)
        .map_err(|error| format!("{qualified_name}: failed to resolve variables: {error}"))?;
    let use_tools = agent.use_tools().unwrap_or_default();
    Ok((agent, use_tools))
}

/// Per-agent inputs shared by the async-guidance checks below: the agent's
/// own selectors and what the production selector actually picked for it.
struct AsyncAgentSelection<'a> {
    qualified_name: &'a str,
    use_tools: &'a [String],
    selected_names: &'a std::collections::HashSet<&'a str>,
}

/// Selection failures for one agent: send/wait/load missing for a declared
/// prompt target, or cancellation granted without a selector of its own.
fn async_selection_failures(agent: &AsyncAgentSelection<'_>) -> Vec<String> {
    let mut failures = Vec::new();
    for selector in agent.use_tools {
        let Some(target) = selector.strip_suffix("_session_prompt") else {
            continue;
        };
        for suffix in ["session_send", "session_wait", "session_load"] {
            let expected = format!("{target}_{suffix}");
            if !agent.selected_names.contains(expected.as_str()) {
                failures.push(format!(
                    "{}: {selector} is selected but {expected} is not; \
                     async guidance requires send/wait/load for every prompt target",
                    agent.qualified_name
                ));
            }
        }
    }

    failures.extend(
        agent
            .selected_names
            .iter()
            .filter(|name| name.ends_with("_session_cancel"))
            .filter(|name| !agent.use_tools.iter().any(|selector| selector == *name))
            .map(|name| {
                format!(
                    "{}: {name} selected without a matching use_tools selector",
                    agent.qualified_name
                )
            }),
    );
    failures
}

/// Rendered-prompt semantics: every policy phrase present, the shared
/// protocol heading exactly once (resolved, not duplicated), and no
/// unresolved template markers leaking into the prompt.
fn async_prompt_semantic_failures(agent: &AsyncAgentSelection<'_>, prompt: &str) -> Vec<String> {
    let qualified_name = agent.qualified_name;
    let mut failures: Vec<String> = ASYNC_DELEGATION_PHRASES
        .iter()
        .filter(|phrase| !prompt.contains(**phrase))
        .map(|phrase| {
            format!(
                "{qualified_name}: rendered prompt is missing async delegation phrase '{phrase}'"
            )
        })
        .collect();

    let headings = prompt.matches(ASYNC_DELEGATION_HEADING).count();
    if headings != 1 {
        failures.push(format!(
            "{qualified_name}: async delegation heading appears {headings} times, \
             expected exactly one resolved include"
        ));
    }

    failures.extend(
        ["{{", "{%"]
            .iter()
            .filter(|marker| prompt.contains(*marker))
            .map(|marker| {
                format!(
                    "{qualified_name}: rendered prompt still contains template marker '{marker}'"
                )
            }),
    );
    failures
}

/// Worker-style declarations for every pantheon agent, spelled the way the
/// provider exposes the session tools to same-package agents.
fn worker_session_declarations(agents_dir: &Path) -> Vec<ToolDeclaration> {
    let mut scratch = Vec::new();
    let mut declarations = Vec::new();
    for stem in agent_stems(agents_dir, &mut scratch) {
        for tool in SESSION_TOOL_SUFFIXES {
            declarations.push(ToolDeclaration {
                name: format!("{stem}_{tool}"),
                description: "subagent session tool".to_string(),
                parameters: Default::default(),
                mcp_tool_name: None,
                mcp_server_name: None,
                call_template: None,
                result_template: None,
                idempotent_hint: None,
                read_only_hint: None,
                kind: None,
            });
        }
    }
    declarations
}

/// Production render + tool-selection coverage for the shipped async
/// delegation guidance (review B1): for every integrating agent, the
/// production selector must pick send/wait/load for each declared prompt
/// target without granting unrequested cancellation, and the production
/// renderer must expand the shared protocol with no unresolved or
/// duplicated include.
#[test]
fn async_delegation_guidance_renders_and_selects_tools_for_all_integrating_agents() {
    harnx_core::require_nextest();
    let Some(workspace_root) = workspace_root() else {
        // Published crates do not include workspace package assets.
        return;
    };
    let (_temp, _config_guard) = install_packages(&workspace_root);

    // Production select path: `Config::select_tools` reading the worker-owned
    // NATS declaration cache (`refresh_nats_tool_declarations` writes this
    // exact field in production). Seed it with the six session tools for
    // every pantheon agent so selectors resolve the way they do against a
    // running worker.
    let config = Config::default();
    let agents_dir = workspace_root.join("packages/pantheon/agents");
    *config.nats_tool_declarations.write() = worker_session_declarations(&agents_dir);

    let mut failures = Vec::new();
    for agent_name in ASYNC_DELEGATION_AGENTS {
        let qualified_name = format!("pantheon/{agent_name}");

        let (agent, use_tools) = match loaded_async_agent(&qualified_name) {
            Ok(loaded) => loaded,
            Err(failure) => {
                failures.push(failure);
                continue;
            }
        };

        let selected = config.select_tools(&agent).unwrap_or_default();
        let selected_names: std::collections::HashSet<&str> =
            selected.iter().map(|tool| tool.name.as_str()).collect();
        let selection = AsyncAgentSelection {
            qualified_name: &qualified_name,
            use_tools: &use_tools,
            selected_names: &selected_names,
        };
        failures.extend(async_selection_failures(&selection));

        // Production render path with the actually selected tools.
        match agent.system_text_with_tools(&selected) {
            Ok(prompt) => failures.extend(async_prompt_semantic_failures(&selection, &prompt)),
            Err(error) => failures.push(format!(
                "{qualified_name}: failed to render prompt: {error}"
            )),
        }
    }

    assert!(
        failures.is_empty(),
        "async delegation guidance or tool selection broken:\n{}",
        failures.join("\n")
    );
}

/// Fragment hygiene for the shared async-delegation include (review B1):
/// like the sandbox fragments it must stay pure markdown — no Jinja tags —
/// and keep the policy anchors the integrating agents inherit verbatim.
/// The sandbox fragment's forbidden-term list does not apply here: this
/// fragment must say the wait never *extends* the deadline and that a soft
/// *expiry* never cancels the child, so async-specific checks replace it.
#[test]
fn shared_async_delegation_fragment_is_pure_markdown_and_policy_aligned() {
    let Some(workspace_root) = workspace_root() else {
        return;
    };

    let path = workspace_root.join("packages/pantheon/agents/shared/async-delegation.md");
    let content = fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));

    let mut failures = Vec::new();
    let mut checker = FragmentChecker {
        file_name: "async-delegation.md",
        content: &content,
        failures: &mut failures,
    };
    checker.check_jinja();

    // Unconditional cancellation or unmapped paging wording would be copied
    // verbatim into every integrating agent's prompt.
    failures.extend(
        [
            "Stop the child with",
            "page forward with the returned",
            "86400",
            "604800",
        ]
        .iter()
        .filter(|forbidden| content.contains(*forbidden))
        .map(|forbidden| format!("async-delegation.md: contains forbidden wording '{forbidden}'")),
    );

    failures.extend(
        ASYNC_DELEGATION_PHRASES
            .iter()
            .filter(|phrase| !content.contains(**phrase))
            .map(|phrase| {
                format!("async-delegation.md: missing required policy phrase '{phrase}'")
            }),
    );

    assert!(
        failures.is_empty(),
        "shared async delegation fragment failed hygiene or policy checks:\n{}",
        failures.join("\n")
    );
}
