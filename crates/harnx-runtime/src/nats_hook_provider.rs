use crate::config::{Config, LOCAL_CLUSTER_KEY};
use anyhow::{Context, Result};
use async_trait::async_trait;
use futures_util::TryStreamExt;
use harnx_core::event::{AgentEvent, NoticeEvent};
use harnx_core::hooks::{HookEvent, HookOutcome, HookPayload, HookResult, HookResultControl};
use harnx_core::instance::InstanceId;
use harnx_hookset::{FailPolicy, HookRegistration, HookSpec, HOOK_REGISTRY_BUCKET};
use harnx_hookset_server::hook_registration_key;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

const DEFAULT_HOOK_TIMEOUT: Duration = Duration::from_secs(30);

/// Session data needed to serialize a hook event for a remote hook server.
#[derive(Clone, Debug)]
pub struct HookDispatchMeta {
    pub session_id: String,
    pub cwd: PathBuf,
    pub resume_count: u32,
}

/// Parameters for one unified hook dispatch.
pub struct HookEventDispatch<'a> {
    pub event: HookEvent,
    pub provider: Option<&'a NatsHookProvider>,
    pub meta: HookDispatchMeta,
    pub pending_async_context: Option<Arc<Mutex<Option<String>>>>,
}

/// Dispatch one hook event through NATS, or use the inline runtime while no
/// NATS provider is available. The inline future is lazy and isn't polled when
/// NATS owns dispatch.
pub async fn dispatch_hook_event<F>(
    params: HookEventDispatch<'_>,
    inline_fallback: F,
) -> HookOutcome
where
    F: Future<Output = HookOutcome>,
{
    match params.provider {
        Some(provider) => {
            provider
                .dispatch_event(params.event, params.pending_async_context, params.meta)
                .await
        }
        None => inline_fallback.await,
    }
}

/// Discover hooks for binaries launched inside a worker process tree.
/// Frontends without `HARNX_INSTANCE_ID` keep the inline fallback.
pub async fn discover_process_nats_hook_provider(config: &Config) -> Option<Arc<NatsHookProvider>> {
    let instance_id = std::env::var(harnx_core::instance::HARNX_INSTANCE_ID).ok()?;
    NatsHookProvider::discover(config, InstanceId::from_string(instance_id))
        .await
        .ok()
        .map(Arc::new)
}

/// One hook route flattened from a hook server registration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveredHook {
    pub server: String,
    pub spec: HookSpec,
}

#[async_trait]
trait HookRequestDispatcher: Send + Sync {
    async fn request(
        &self,
        subject: String,
        payload: Vec<u8>,
        timeout: Duration,
    ) -> Result<HookOutcome>;
}

struct NatsHookRequester {
    client: async_nats::Client,
}

#[async_trait]
impl HookRequestDispatcher for NatsHookRequester {
    async fn request(
        &self,
        subject: String,
        payload: Vec<u8>,
        timeout: Duration,
    ) -> Result<HookOutcome> {
        let message = tokio::time::timeout(timeout, self.client.request(subject, payload.into()))
            .await
            .context("hook request timed out")??;
        serde_json::from_slice(&message.payload).context("deserialize hook reply")
    }
}

/// Discovers and dispatches hooks registered for one worker instance.
pub struct NatsHookProvider {
    client: Option<async_nats::Client>,
    instance_id: InstanceId,
    hooks: Vec<DiscoveredHook>,
    dispatcher: Arc<dyn HookRequestDispatcher>,
}

impl NatsHookProvider {
    pub async fn discover(config: &Config, instance_id: InstanceId) -> Result<Self> {
        let client = config.nats_client(LOCAL_CLUSTER_KEY).await?;
        let hooks = match registration_snapshot(&client, &instance_id).await {
            Ok(hooks) => hooks,
            Err(error) => {
                // Hook servers create the registry lazily, so a fresh instance has no bucket.
                log::debug!("NATS hook registry is not available yet: {error:#}");
                Vec::new()
            }
        };
        Ok(Self::from_hooks(client, instance_id, hooks))
    }

    pub fn from_hooks(
        client: async_nats::Client,
        instance_id: InstanceId,
        hooks: Vec<DiscoveredHook>,
    ) -> Self {
        let dispatcher = Arc::new(NatsHookRequester {
            client: client.clone(),
        });
        Self {
            client: Some(client),
            instance_id,
            hooks,
            dispatcher,
        }
    }

    pub fn hooks(&self) -> &[DiscoveredHook] {
        &self.hooks
    }

    /// Returns the client used for discovery and requests.
    pub fn client(&self) -> &async_nats::Client {
        self.client
            .as_ref()
            .expect("production NATS hook providers always carry a client")
    }

    #[cfg(test)]
    fn from_dispatcher(
        instance_id: InstanceId,
        hooks: Vec<DiscoveredHook>,
        dispatcher: Arc<dyn HookRequestDispatcher>,
    ) -> Self {
        Self {
            client: None,
            instance_id,
            hooks,
            dispatcher,
        }
    }

    /// Dispatches any hook event using its event-specific blocking policy.
    ///
    /// PreToolUse, session boundary, and turn-control events run sequentially.
    /// File-change and post-tool events start in background and return immediately.
    pub async fn dispatch_event(
        &self,
        event: HookEvent,
        pending: Option<Arc<Mutex<Option<String>>>>,
        meta: HookDispatchMeta,
    ) -> HookOutcome {
        match event {
            HookEvent::PreToolUse { .. } => {
                let outcome = self.dispatch_pre_tool_use(&event, meta).await;
                append_outcome_context(pending.as_ref(), &outcome).await;
                outcome
            }
            HookEvent::SessionStart { .. }
            | HookEvent::SessionEnd { .. }
            | HookEvent::UserPromptSubmit { .. }
            | HookEvent::Stop { .. }
            | HookEvent::StopFailure { .. } => {
                dispatch_blocking_event_with(
                    EventDispatch {
                        dispatcher: self.dispatcher.as_ref(),
                        instance_id: &self.instance_id,
                        hooks: &self.hooks,
                        meta: &meta,
                        pending,
                    },
                    &event,
                )
                .await
            }
            HookEvent::InstructionsLoaded { .. }
            | HookEvent::CwdChanged { .. }
            | HookEvent::PostToolUse { .. }
            | HookEvent::PostToolUseFailure { .. } => {
                self.dispatch_best_effort(event, pending, meta);
                continue_outcome(None)
            }
        }
    }

    pub async fn dispatch_pre_tool_use(
        &self,
        event: &HookEvent,
        meta: HookDispatchMeta,
    ) -> HookOutcome {
        dispatch_pre_tool_use_with(
            PreHookDispatch {
                dispatcher: self.dispatcher.as_ref(),
                instance_id: &self.instance_id,
                hooks: &self.hooks,
                meta: &meta,
            },
            event,
        )
        .await
    }

    /// Starts all matching PostToolUse hooks without waiting for their replies.
    pub fn dispatch_post_tool_use(
        &self,
        event: HookEvent,
        pending: Option<Arc<Mutex<Option<String>>>>,
        meta: HookDispatchMeta,
    ) {
        self.dispatch_best_effort(event, pending, meta);
    }

    fn dispatch_best_effort(
        &self,
        event: HookEvent,
        pending: Option<Arc<Mutex<Option<String>>>>,
        meta: HookDispatchMeta,
    ) {
        for hook in matching_hooks(&self.hooks, event.event_name(), event.matcher_text()) {
            let dispatcher = Arc::clone(&self.dispatcher);
            let subject = self
                .instance_id
                .hook_subject(&hook.server, event.event_name());
            let payload = HookPayload {
                session_id: meta.session_id.clone(),
                cwd: meta.cwd.clone(),
                resume_count: meta.resume_count,
                hook_event: event.clone(),
            };
            let params = BestEffortHookDispatch {
                subject,
                payload,
                hook: hook.clone(),
                pending: pending.clone(),
            };
            tokio::spawn(dispatch_one_best_effort_hook(dispatcher, params));
        }
    }
}

/// Filters hooks for an event and matcher, then orders them by dispatch precedence.
pub fn matching_hooks<'a>(
    hooks: &'a [DiscoveredHook],
    event_name: &str,
    tool_name: Option<&str>,
) -> Vec<&'a DiscoveredHook> {
    let mut matches: Vec<_> = hooks
        .iter()
        .enumerate()
        .filter(|(_, hook)| {
            hook.spec.event == event_name
                && match tool_name {
                    Some(tool_name) => harnx_hooks::CompiledMatcher::compile(&hook.spec.matcher)
                        .map(|matcher| matcher.matches_str(tool_name))
                        .unwrap_or(false),
                    None => hook.spec.matcher.is_none(),
                }
        })
        .collect();
    matches.sort_by(|(left_index, left), (right_index, right)| {
        left.spec
            .priority
            .cmp(&right.spec.priority)
            .then_with(|| left.server.cmp(&right.server))
            .then_with(|| left_index.cmp(right_index))
    });
    matches.into_iter().map(|(_, hook)| hook).collect()
}

struct PreHookDispatch<'a> {
    dispatcher: &'a dyn HookRequestDispatcher,
    instance_id: &'a InstanceId,
    hooks: &'a [DiscoveredHook],
    meta: &'a HookDispatchMeta,
}

#[derive(Default)]
struct ContinueResultAccumulator {
    additional_contexts: Vec<String>,
    system_messages: Vec<String>,
    resume: bool,
}

impl ContinueResultAccumulator {
    fn push(&mut self, result: &HookResult) {
        if let Some(context) = result
            .additional_context
            .as_ref()
            .filter(|context| !context.is_empty())
        {
            self.additional_contexts.push(context.clone());
        }
        if let Some(message) = result
            .system_message
            .as_ref()
            .filter(|message| !message.is_empty())
        {
            self.system_messages.push(message.clone());
        }
        self.resume |= result.resume.unwrap_or(false);
    }

    fn into_result(self, mutated_tool_input: Option<serde_json::Value>) -> HookResult {
        HookResult {
            additional_context: (!self.additional_contexts.is_empty())
                .then(|| self.additional_contexts.join("\n")),
            system_message: (!self.system_messages.is_empty())
                .then(|| self.system_messages.join("\n")),
            mutated_tool_input,
            resume: self.resume.then_some(true),
            ..HookResult::default()
        }
    }
}

async fn dispatch_pre_tool_use_with(params: PreHookDispatch<'_>, event: &HookEvent) -> HookOutcome {
    if !matches!(event, HookEvent::PreToolUse { .. }) {
        return continue_outcome(None);
    }

    let mut running_event = event.clone();
    let mut final_mutation = None;
    let mut accumulated = ContinueResultAccumulator::default();
    for hook in matching_hooks(params.hooks, event.event_name(), event.matcher_text()) {
        let payload = HookPayload {
            session_id: params.meta.session_id.clone(),
            cwd: params.meta.cwd.clone(),
            resume_count: params.meta.resume_count,
            hook_event: running_event.clone(),
        };
        let payload = match serde_json::to_vec(&payload) {
            Ok(payload) => payload,
            Err(error) => {
                return unavailable_outcome(&hook.server, hook.spec.fail_policy, error);
            }
        };
        let subject = params.instance_id.hook_subject(&hook.server, "PreToolUse");
        let timeout = hook
            .spec
            .timeout_secs
            .map(Duration::from_secs)
            .unwrap_or(DEFAULT_HOOK_TIMEOUT);
        let outcome = match params.dispatcher.request(subject, payload, timeout).await {
            Ok(outcome) => outcome,
            Err(error) if hook.spec.fail_policy == FailPolicy::Open => {
                log::warn!("{} hook unavailable; continuing: {error:#}", hook.server);
                continue;
            }
            Err(error) => return unavailable_outcome(&hook.server, FailPolicy::Closed, error),
        };

        match outcome.control {
            HookResultControl::Block { .. } | HookResultControl::Ask { .. } => return outcome,
            HookResultControl::Continue => {
                accumulated.push(&outcome.result);
                if let Some(tool_input) = outcome.result.mutated_tool_input {
                    running_event = with_pre_tool_input(&running_event, tool_input.clone());
                    final_mutation = Some(tool_input);
                }
            }
        }
    }
    HookOutcome {
        control: HookResultControl::Continue,
        result: accumulated.into_result(final_mutation),
    }
}

struct EventDispatch<'a> {
    dispatcher: &'a dyn HookRequestDispatcher,
    instance_id: &'a InstanceId,
    hooks: &'a [DiscoveredHook],
    meta: &'a HookDispatchMeta,
    pending: Option<Arc<Mutex<Option<String>>>>,
}

async fn dispatch_blocking_event_with(params: EventDispatch<'_>, event: &HookEvent) -> HookOutcome {
    let mut accumulated = ContinueResultAccumulator::default();
    for hook in matching_hooks(params.hooks, event.event_name(), event.matcher_text()) {
        let payload = HookPayload {
            session_id: params.meta.session_id.clone(),
            cwd: params.meta.cwd.clone(),
            resume_count: params.meta.resume_count,
            hook_event: event.clone(),
        };
        let payload = match serde_json::to_vec(&payload) {
            Ok(payload) => payload,
            Err(error) => {
                return unavailable_outcome(&hook.server, hook.spec.fail_policy, error);
            }
        };
        let subject = params
            .instance_id
            .hook_subject(&hook.server, event.event_name());
        let timeout = hook
            .spec
            .timeout_secs
            .map(Duration::from_secs)
            .unwrap_or(DEFAULT_HOOK_TIMEOUT);
        let outcome = match params.dispatcher.request(subject, payload, timeout).await {
            Ok(outcome) => outcome,
            Err(error) if hook.spec.fail_policy == FailPolicy::Open => {
                log::warn!("{} hook unavailable; continuing: {error:#}", hook.server);
                continue;
            }
            Err(error) => return unavailable_outcome(&hook.server, FailPolicy::Closed, error),
        };

        let texts = [
            outcome.result.additional_context.clone(),
            outcome.result.system_message.clone(),
        ];
        if let Some(pending) = params.pending.as_deref() {
            append_pending_context(pending, texts).await;
        }
        match outcome.control {
            HookResultControl::Block { .. } | HookResultControl::Ask { .. } => return outcome,
            HookResultControl::Continue => accumulated.push(&outcome.result),
        }
    }

    HookOutcome {
        control: HookResultControl::Continue,
        result: accumulated.into_result(None),
    }
}

struct BestEffortHookDispatch {
    subject: String,
    payload: HookPayload,
    hook: DiscoveredHook,
    pending: Option<Arc<Mutex<Option<String>>>>,
}

async fn dispatch_one_best_effort_hook(
    dispatcher: Arc<dyn HookRequestDispatcher>,
    params: BestEffortHookDispatch,
) {
    let server = &params.hook.server;
    let event_name = params.payload.hook_event.event_name();
    let timeout = params
        .hook
        .spec
        .timeout_secs
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_HOOK_TIMEOUT);
    let payload = match serde_json::to_vec(&params.payload) {
        Ok(payload) => payload,
        Err(error) => {
            emit_post_notice(format!(
                "{server} hook request could not be serialized: {error}"
            ));
            return;
        }
    };
    let outcome = match dispatcher.request(params.subject, payload, timeout).await {
        Ok(outcome) => outcome,
        Err(error) => {
            let policy = match params.hook.spec.fail_policy {
                FailPolicy::Closed => "fail-closed",
                FailPolicy::Open => "fail-open",
            };
            emit_post_notice(format!("{server} hook unavailable ({policy}): {error:#}"));
            return;
        }
    };

    if !matches!(outcome.control, HookResultControl::Continue) {
        emit_post_notice(format!(
            "{server} hook returned {:?} during best-effort {event_name} dispatch",
            outcome.control
        ));
    }
    if outcome.result.mutated_tool_input.is_some() || outcome.result.mutated_tool_response.is_some()
    {
        log::debug!("ignoring mutation from asynchronous {server} {event_name} hook");
    }
    if let Some(pending) = params.pending {
        append_pending_context(
            pending.as_ref(),
            [
                outcome.result.additional_context,
                outcome.result.system_message,
            ],
        )
        .await;
    }
}

async fn append_pending_context(pending: &Mutex<Option<String>>, texts: [Option<String>; 2]) {
    let mut guard = pending.lock().await;
    let mut accumulated = guard.take().unwrap_or_default();
    for text in texts.into_iter().flatten().filter(|text| !text.is_empty()) {
        if !accumulated.is_empty() {
            accumulated.push('\n');
        }
        accumulated.push_str(&text);
    }
    if !accumulated.is_empty() {
        *guard = Some(accumulated);
    }
}

async fn append_outcome_context(
    pending: Option<&Arc<Mutex<Option<String>>>>,
    outcome: &HookOutcome,
) {
    if let Some(pending) = pending {
        append_pending_context(
            pending.as_ref(),
            [
                outcome.result.additional_context.clone(),
                outcome.result.system_message.clone(),
            ],
        )
        .await;
    }
}

fn with_pre_tool_input(event: &HookEvent, tool_input: serde_json::Value) -> HookEvent {
    match event {
        HookEvent::PreToolUse {
            tool_name,
            tool_use_id,
            ..
        } => HookEvent::PreToolUse {
            tool_name: tool_name.clone(),
            tool_input,
            tool_use_id: tool_use_id.clone(),
        },
        _ => event.clone(),
    }
}

fn continue_outcome(mutated_tool_input: Option<serde_json::Value>) -> HookOutcome {
    HookOutcome {
        control: HookResultControl::Continue,
        result: HookResult {
            mutated_tool_input,
            ..HookResult::default()
        },
    }
}

fn unavailable_outcome(
    server: &str,
    fail_policy: FailPolicy,
    error: impl std::fmt::Display,
) -> HookOutcome {
    if fail_policy == FailPolicy::Open {
        log::warn!("{server} hook unavailable; continuing: {error}");
        return continue_outcome(None);
    }
    log::warn!("{server} hook unavailable; blocking: {error}");
    HookOutcome {
        control: HookResultControl::Block {
            reason: format!("{server} hook unavailable"),
        },
        result: HookResult::default(),
    }
}

fn emit_post_notice(message: String) {
    log::warn!("{message}");
    harnx_core::sink::emit_agent_event(AgentEvent::Notice(NoticeEvent::Error(message)));
}

async fn registration_snapshot(
    client: &async_nats::Client,
    instance_id: &InstanceId,
) -> Result<Vec<DiscoveredHook>> {
    let jetstream = async_nats::jetstream::new(client.clone());
    let store = jetstream.get_key_value(HOOK_REGISTRY_BUCKET).await?;
    let mut keys = store.keys().await?;
    let prefix = format!("{instance_id}.");
    let mut hooks = Vec::new();
    while let Some(key) = keys.try_next().await? {
        if !key.starts_with(&prefix) {
            continue;
        }
        let Some(value) = store.get(&key).await? else {
            continue;
        };
        let registration = match serde_json::from_slice::<HookRegistration>(&value) {
            Ok(registration) => registration,
            Err(error) => {
                log::warn!("ignoring invalid NATS hook registration '{key}': {error}");
                continue;
            }
        };
        if key != hook_registration_key(instance_id, &registration.server) {
            continue;
        }
        hooks.extend(registration.hooks.into_iter().map(|spec| DiscoveredHook {
            server: registration.server.clone(),
            spec,
        }));
    }
    Ok(hooks)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Notify;

    struct StubDispatcher {
        outcomes: Mutex<VecDeque<HookOutcome>>,
        seen_inputs: Mutex<Vec<Value>>,
        seen_subjects: Mutex<Vec<String>>,
        seen_resume_counts: Mutex<Vec<u32>>,
        delay: Option<Duration>,
        completed: Option<Arc<Notify>>,
        calls: AtomicUsize,
    }

    #[async_trait]
    impl HookRequestDispatcher for StubDispatcher {
        async fn request(
            &self,
            subject: String,
            payload: Vec<u8>,
            _timeout: Duration,
        ) -> Result<HookOutcome> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.seen_subjects.lock().await.push(subject);
            let payload: HookPayload = serde_json::from_slice(&payload)?;
            self.seen_resume_counts
                .lock()
                .await
                .push(payload.resume_count);
            if let HookEvent::PreToolUse { tool_input, .. } = payload.hook_event {
                self.seen_inputs.lock().await.push(tool_input);
            }
            if let Some(delay) = self.delay {
                tokio::time::sleep(delay).await;
            }
            let outcome = self
                .outcomes
                .lock()
                .await
                .pop_front()
                .context("stub outcome queue exhausted")?;
            if let Some(completed) = &self.completed {
                completed.notify_one();
            }
            Ok(outcome)
        }
    }

    fn hook(server: &str, event: &str, matcher: Option<&str>, priority: i32) -> DiscoveredHook {
        DiscoveredHook {
            server: server.to_string(),
            spec: HookSpec {
                event: event.to_string(),
                matcher: matcher.map(str::to_string),
                priority,
                timeout_secs: None,
                fail_policy: FailPolicy::Closed,
            },
        }
    }

    fn pre_event(input: Value) -> HookEvent {
        HookEvent::PreToolUse {
            tool_name: "exec".to_string(),
            tool_input: input,
            tool_use_id: "tool-1".to_string(),
        }
    }

    fn mutate(value: Value) -> HookOutcome {
        continue_outcome(Some(value))
    }

    fn stub(outcomes: Vec<HookOutcome>) -> Arc<StubDispatcher> {
        Arc::new(StubDispatcher {
            outcomes: Mutex::new(outcomes.into()),
            seen_inputs: Mutex::new(Vec::new()),
            seen_subjects: Mutex::new(Vec::new()),
            seen_resume_counts: Mutex::new(Vec::new()),
            delay: None,
            completed: None,
            calls: AtomicUsize::new(0),
        })
    }

    fn non_tool_events() -> Vec<HookEvent> {
        vec![
            HookEvent::SessionStart {
                source: "cli".to_string(),
                model: "model".to_string(),
            },
            HookEvent::SessionEnd {
                reason: "exit".to_string(),
            },
            HookEvent::UserPromptSubmit {
                prompt: "hello".to_string(),
            },
            HookEvent::Stop {
                stop_hook_active: true,
                last_assistant_message: Some("done".to_string()),
            },
            HookEvent::StopFailure {
                error: "failed".to_string(),
                error_type: "api_error".to_string(),
            },
            HookEvent::InstructionsLoaded {
                file_path: PathBuf::from("/tmp/CLAUDE.md"),
                memory_type: "Project".to_string(),
                load_reason: "session_start".to_string(),
            },
            HookEvent::CwdChanged {
                old_cwd: PathBuf::from("/tmp/old"),
                new_cwd: PathBuf::from("/tmp/new"),
            },
        ]
    }

    #[test]
    fn matcher_filter_selects_event_and_bare_tool_name() {
        let hooks = vec![
            hook("exact", "PreToolUse", Some("^exec$"), 0),
            hook("prefixed", "PreToolUse", Some("^mcp__exec$"), 0),
            hook("other-event", "PostToolUse", Some("^exec$"), 0),
            hook("all-tools", "PreToolUse", None, 0),
        ];

        let selected = matching_hooks(&hooks, "PreToolUse", Some("exec"));
        let servers: Vec<_> = selected.iter().map(|hook| hook.server.as_str()).collect();
        assert_eq!(servers, ["all-tools", "exact"]);

        let non_tool_hooks = vec![
            hook("unmatched", "SessionStart", Some("cli"), 0),
            hook("matched", "SessionStart", None, 0),
        ];
        let selected = matching_hooks(&non_tool_hooks, "SessionStart", None);
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].server, "matched");
    }

    #[test]
    fn priority_sort_uses_server_then_registry_order_as_tiebreakers() {
        let hooks = vec![
            hook("zeta", "PreToolUse", None, 10),
            hook("alpha", "PreToolUse", None, 10),
            hook("alpha", "PreToolUse", None, 10),
            hook("later", "PreToolUse", None, 20),
            hook("first", "PreToolUse", None, -1),
        ];

        let selected = matching_hooks(&hooks, "PreToolUse", Some("exec"));
        let positions: Vec<_> = selected
            .iter()
            .map(|hook| (hook.server.as_str(), hook.spec.priority))
            .collect();
        assert_eq!(
            positions,
            [
                ("first", -1),
                ("alpha", 10),
                ("alpha", 10),
                ("zeta", 10),
                ("later", 20),
            ]
        );
        assert!(std::ptr::eq(selected[1], &hooks[1]));
        assert!(std::ptr::eq(selected[2], &hooks[2]));
    }

    #[tokio::test]
    async fn pre_chain_applies_mutation_before_block_and_short_circuits() {
        let blocked = HookOutcome {
            control: HookResultControl::Block {
                reason: "denied".to_string(),
            },
            result: HookResult::default(),
        };
        let dispatcher = stub(vec![mutate(json!({"step": 1})), blocked]);
        let hooks = vec![
            hook("one", "PreToolUse", None, 0),
            hook("two", "PreToolUse", None, 1),
            hook("three", "PreToolUse", None, 2),
        ];

        let instance_id = InstanceId::from_string("test");
        let meta = HookDispatchMeta {
            session_id: "session".to_string(),
            cwd: PathBuf::from("/tmp"),
            resume_count: 0,
        };
        let outcome = dispatch_pre_tool_use_with(
            PreHookDispatch {
                dispatcher: dispatcher.as_ref(),
                instance_id: &instance_id,
                hooks: &hooks,
                meta: &meta,
            },
            &pre_event(json!({"step": 0})),
        )
        .await;

        assert!(matches!(outcome.control, HookResultControl::Block { .. }));
        assert_eq!(dispatcher.calls.load(Ordering::SeqCst), 2);
        assert_eq!(
            *dispatcher.seen_inputs.lock().await,
            vec![json!({"step": 0}), json!({"step": 1})]
        );
    }

    #[tokio::test]
    async fn pre_chain_returns_last_of_composed_mutations() {
        let dispatcher = stub(vec![
            mutate(json!({"step": "a"})),
            mutate(json!({"step": "b"})),
        ]);
        let hooks = vec![
            hook("one", "PreToolUse", None, 0),
            hook("two", "PreToolUse", None, 1),
        ];

        let instance_id = InstanceId::from_string("test");
        let meta = HookDispatchMeta {
            session_id: "session".to_string(),
            cwd: PathBuf::from("/tmp"),
            resume_count: 0,
        };
        let outcome = dispatch_pre_tool_use_with(
            PreHookDispatch {
                dispatcher: dispatcher.as_ref(),
                instance_id: &instance_id,
                hooks: &hooks,
                meta: &meta,
            },
            &pre_event(json!({"step": "initial"})),
        )
        .await;

        assert_eq!(
            outcome.result.mutated_tool_input,
            Some(json!({"step": "b"}))
        );
        assert_eq!(
            *dispatcher.seen_inputs.lock().await,
            vec![json!({"step": "initial"}), json!({"step": "a"})]
        );
    }

    #[tokio::test]
    async fn every_non_tool_event_dispatches_to_its_nats_subject() {
        let instance_id = InstanceId::from_string("test");
        let meta = HookDispatchMeta {
            session_id: "session".to_string(),
            cwd: PathBuf::from("/tmp"),
            resume_count: 0,
        };

        for event in non_tool_events() {
            let event_name = event.event_name();
            let dispatcher = stub(vec![continue_outcome(None)]);
            let hooks = vec![hook("server", event_name, None, 0)];
            let outcome = dispatch_blocking_event_with(
                EventDispatch {
                    dispatcher: dispatcher.as_ref(),
                    instance_id: &instance_id,
                    hooks: &hooks,
                    meta: &meta,
                    pending: None,
                },
                &event,
            )
            .await;

            assert!(matches!(outcome.control, HookResultControl::Continue));
            assert_eq!(dispatcher.calls.load(Ordering::SeqCst), 1, "{event_name}");
            let subjects = dispatcher.seen_subjects.lock().await;
            assert_eq!(subjects.len(), 1);
            assert!(subjects[0].ends_with(&format!(".hook.server.{event_name}")));
        }
    }

    #[tokio::test]
    async fn blocking_event_block_short_circuits_later_hooks() {
        let dispatcher = stub(vec![HookOutcome {
            control: HookResultControl::Block {
                reason: "denied".to_string(),
            },
            result: HookResult::default(),
        }]);
        let hooks = vec![
            hook("one", "UserPromptSubmit", None, 0),
            hook("two", "UserPromptSubmit", None, 1),
        ];
        let instance_id = InstanceId::from_string("test");
        let meta = HookDispatchMeta {
            session_id: "session".to_string(),
            cwd: PathBuf::from("/tmp"),
            resume_count: 0,
        };

        let outcome = dispatch_blocking_event_with(
            EventDispatch {
                dispatcher: dispatcher.as_ref(),
                instance_id: &instance_id,
                hooks: &hooks,
                meta: &meta,
                pending: None,
            },
            &HookEvent::UserPromptSubmit {
                prompt: "hello".to_string(),
            },
        )
        .await;

        assert!(matches!(outcome.control, HookResultControl::Block { .. }));
        assert_eq!(dispatcher.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn blocking_event_preserves_context_channels_and_appends_both_to_pending() {
        let dispatcher = stub(vec![HookOutcome {
            control: HookResultControl::Continue,
            result: HookResult {
                additional_context: Some("context".to_string()),
                system_message: Some("system".to_string()),
                ..HookResult::default()
            },
        }]);
        let hooks = vec![hook("server", "Stop", None, 0)];
        let instance_id = InstanceId::from_string("test");
        let meta = HookDispatchMeta {
            session_id: "session".to_string(),
            cwd: PathBuf::from("/tmp"),
            resume_count: 0,
        };
        let pending = Arc::new(Mutex::new(Some("existing".to_string())));

        let outcome = dispatch_blocking_event_with(
            EventDispatch {
                dispatcher: dispatcher.as_ref(),
                instance_id: &instance_id,
                hooks: &hooks,
                meta: &meta,
                pending: Some(Arc::clone(&pending)),
            },
            &HookEvent::Stop {
                stop_hook_active: true,
                last_assistant_message: Some("done".to_string()),
            },
        )
        .await;

        assert_eq!(
            outcome.result.additional_context.as_deref(),
            Some("context")
        );
        assert_eq!(outcome.result.system_message.as_deref(), Some("system"));
        assert_eq!(
            pending.lock().await.as_deref(),
            Some("existing\ncontext\nsystem")
        );
    }

    #[tokio::test]
    async fn lifecycle_event_fires_without_blocking() {
        let completed = Arc::new(Notify::new());
        let dispatcher = Arc::new(StubDispatcher {
            outcomes: Mutex::new(vec![continue_outcome(None)].into()),
            seen_inputs: Mutex::new(Vec::new()),
            seen_subjects: Mutex::new(Vec::new()),
            seen_resume_counts: Mutex::new(Vec::new()),
            delay: Some(Duration::from_millis(100)),
            completed: Some(Arc::clone(&completed)),
            calls: AtomicUsize::new(0),
        });
        let payload = HookPayload {
            session_id: "session".to_string(),
            cwd: PathBuf::from("/tmp"),
            resume_count: 0,
            hook_event: HookEvent::SessionStart {
                source: "cli".to_string(),
                model: "model".to_string(),
            },
        };

        let started = tokio::time::Instant::now();
        tokio::spawn(dispatch_one_best_effort_hook(
            dispatcher,
            BestEffortHookDispatch {
                subject: "test.hook.server.SessionStart".to_string(),
                payload,
                hook: hook("server", "SessionStart", None, 0),
                pending: None,
            },
        ));
        assert!(started.elapsed() < Duration::from_millis(50));
        completed.notified().await;
    }

    #[tokio::test]
    async fn post_dispatch_returns_before_hook_and_appends_shared_context() {
        let completed = Arc::new(Notify::new());
        let dispatcher = Arc::new(StubDispatcher {
            outcomes: Mutex::new(
                vec![HookOutcome {
                    control: HookResultControl::Continue,
                    result: HookResult {
                        additional_context: Some("context".to_string()),
                        system_message: Some("system".to_string()),
                        ..HookResult::default()
                    },
                }]
                .into(),
            ),
            seen_inputs: Mutex::new(Vec::new()),
            seen_subjects: Mutex::new(Vec::new()),
            seen_resume_counts: Mutex::new(Vec::new()),
            delay: Some(Duration::from_millis(100)),
            completed: Some(Arc::clone(&completed)),
            calls: AtomicUsize::new(0),
        });
        let pending = Arc::new(Mutex::new(Some("existing".to_string())));
        let payload = HookPayload {
            session_id: "session".to_string(),
            cwd: PathBuf::from("/tmp"),
            resume_count: 0,
            hook_event: HookEvent::PostToolUse {
                tool_name: "exec".to_string(),
                tool_input: json!({}),
                tool_response: json!({}),
                tool_use_id: "tool-1".to_string(),
            },
        };

        let started = tokio::time::Instant::now();
        tokio::spawn(dispatch_one_best_effort_hook(
            dispatcher,
            BestEffortHookDispatch {
                subject: "subject".to_string(),
                payload,
                hook: hook("server", "PostToolUse", None, 0),
                pending: Some(Arc::clone(&pending)),
            },
        ));
        assert!(started.elapsed() < Duration::from_millis(50));

        completed.notified().await;
        tokio::task::yield_now().await;
        assert_eq!(
            pending.lock().await.as_deref(),
            Some("existing\ncontext\nsystem")
        );
    }

    #[tokio::test]
    async fn unified_entrypoint_uses_inline_fallback_without_provider() {
        let outcome = HookOutcome {
            control: HookResultControl::Block {
                reason: "inline".to_string(),
            },
            result: HookResult::default(),
        };
        let actual = dispatch_hook_event(
            HookEventDispatch {
                event: HookEvent::SessionEnd {
                    reason: "test".to_string(),
                },
                provider: None,
                meta: HookDispatchMeta {
                    session_id: "session".to_string(),
                    cwd: PathBuf::from("/tmp"),
                    resume_count: 0,
                },
                pending_async_context: None,
            },
            std::future::ready(outcome.clone()),
        )
        .await;
        assert_eq!(
            serde_json::to_value(actual).unwrap(),
            serde_json::to_value(outcome).unwrap()
        );
    }

    #[tokio::test]
    async fn dispatch_event_queues_aggregated_pre_tool_context_for_next_turn() {
        let dispatcher = stub(vec![
            HookOutcome {
                control: HookResultControl::Continue,
                result: HookResult {
                    additional_context: Some("context-one".to_string()),
                    system_message: Some("system-one".to_string()),
                    mutated_tool_input: Some(json!({"step": 1})),
                    resume: Some(true),
                    ..HookResult::default()
                },
            },
            HookOutcome {
                control: HookResultControl::Continue,
                result: HookResult {
                    additional_context: Some("context-two".to_string()),
                    system_message: Some("system-two".to_string()),
                    mutated_tool_input: Some(json!({"step": 2})),
                    ..HookResult::default()
                },
            },
        ]);
        let provider = NatsHookProvider::from_dispatcher(
            InstanceId::from_string("test"),
            vec![
                hook("first", "PreToolUse", Some("exec"), 0),
                hook("second", "PreToolUse", Some("exec"), 1),
            ],
            dispatcher.clone(),
        );
        let pending = Arc::new(Mutex::new(Some("existing".to_string())));

        let outcome = provider
            .dispatch_event(
                pre_event(json!({"initial": true})),
                Some(Arc::clone(&pending)),
                HookDispatchMeta {
                    session_id: "session".to_string(),
                    cwd: PathBuf::from("/tmp"),
                    resume_count: 0,
                },
            )
            .await;

        assert_eq!(
            outcome.result.additional_context.as_deref(),
            Some("context-one\ncontext-two")
        );
        assert_eq!(
            outcome.result.system_message.as_deref(),
            Some("system-one\nsystem-two")
        );
        assert_eq!(outcome.result.mutated_tool_input, Some(json!({"step": 2})));
        assert_eq!(outcome.result.resume, Some(true));
        assert_eq!(
            dispatcher.seen_inputs.lock().await.as_slice(),
            [json!({"initial": true}), json!({"step": 1})]
        );
        assert_eq!(
            pending.lock().await.as_deref(),
            Some("existing\ncontext-one\ncontext-two\nsystem-one\nsystem-two")
        );
    }

    #[tokio::test]
    async fn session_end_is_delivered_before_dispatch_returns() {
        let dispatcher = Arc::new(StubDispatcher {
            outcomes: Mutex::new(vec![continue_outcome(None)].into()),
            seen_inputs: Mutex::new(Vec::new()),
            seen_subjects: Mutex::new(Vec::new()),
            seen_resume_counts: Mutex::new(Vec::new()),
            delay: Some(Duration::from_millis(100)),
            completed: None,
            calls: AtomicUsize::new(0),
        });
        let provider = NatsHookProvider::from_dispatcher(
            InstanceId::from_string("test"),
            vec![hook("lifecycle", "SessionEnd", None, 0)],
            dispatcher.clone(),
        );

        let started = tokio::time::Instant::now();
        let outcome = provider
            .dispatch_event(
                HookEvent::SessionEnd {
                    reason: "exit".to_string(),
                },
                None,
                HookDispatchMeta {
                    session_id: "session".to_string(),
                    cwd: PathBuf::from("/tmp"),
                    resume_count: 4,
                },
            )
            .await;

        assert!(matches!(outcome.control, HookResultControl::Continue));
        assert!(started.elapsed() >= Duration::from_millis(75));
        assert_eq!(dispatcher.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            dispatcher.seen_subjects.lock().await.as_slice(),
            ["harnx.v1.test.hook.lifecycle.SessionEnd"]
        );
        assert_eq!(dispatcher.seen_resume_counts.lock().await.as_slice(), [4]);
    }

    #[tokio::test]
    async fn resume_count_is_serialized_for_pre_tool_dispatch() {
        let dispatcher = stub(vec![continue_outcome(None)]);
        let instance_id = InstanceId::from_string("test");
        let hooks = vec![hook("server", "PreToolUse", Some("exec"), 0)];
        let meta = HookDispatchMeta {
            session_id: "session".to_string(),
            cwd: PathBuf::from("/tmp"),
            resume_count: 7,
        };

        dispatch_pre_tool_use_with(
            PreHookDispatch {
                dispatcher: dispatcher.as_ref(),
                instance_id: &instance_id,
                hooks: &hooks,
                meta: &meta,
            },
            &pre_event(json!({})),
        )
        .await;

        assert_eq!(dispatcher.seen_resume_counts.lock().await.as_slice(), [7]);
    }

    #[tokio::test]
    async fn pre_tool_ask_survives_nats_dispatch_for_approval_flow() {
        let dispatcher = stub(vec![HookOutcome {
            control: HookResultControl::Ask {
                reason: Some("approval needed".to_string()),
            },
            result: HookResult::default(),
        }]);
        let hooks = vec![hook("approval", "PreToolUse", Some("exec"), 0)];
        let instance_id = InstanceId::from_string("test");
        let meta = HookDispatchMeta {
            session_id: "session".to_string(),
            cwd: PathBuf::from("/tmp"),
            resume_count: 0,
        };

        let outcome = dispatch_pre_tool_use_with(
            PreHookDispatch {
                dispatcher: dispatcher.as_ref(),
                instance_id: &instance_id,
                hooks: &hooks,
                meta: &meta,
            },
            &pre_event(json!({})),
        )
        .await;

        assert!(matches!(
            outcome.control,
            HookResultControl::Ask { reason }
                if reason.as_deref() == Some("approval needed")
        ));
    }
}
