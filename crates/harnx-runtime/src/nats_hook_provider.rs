use crate::config::{Config, LOCAL_CLUSTER_KEY};
use anyhow::{Context, Result};
use async_trait::async_trait;
use futures_util::TryStreamExt;
use harnx_core::event::{AgentEvent, NoticeEvent};
use harnx_core::hooks::{HookEvent, HookOutcome, HookPayload, HookResult, HookResultControl};
use harnx_core::instance::InstanceId;
use harnx_hookset::{FailPolicy, HookRegistration, HookSpec, HOOK_REGISTRY_BUCKET};
use harnx_hookset_server::hook_registration_key;
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
    client: async_nats::Client,
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
            client,
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
        &self.client
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
        for hook in matching_hooks(&self.hooks, event.event_name(), event.matcher_text()) {
            let dispatcher = Arc::clone(&self.dispatcher);
            let subject = self
                .instance_id
                .hook_subject(&hook.server, event.event_name());
            let payload = HookPayload {
                session_id: meta.session_id.clone(),
                cwd: meta.cwd.clone(),
                resume_count: 0,
                hook_event: event.clone(),
            };
            let params = PostHookDispatch {
                subject,
                payload,
                hook: hook.clone(),
                pending: pending.clone(),
            };
            tokio::spawn(dispatch_one_post_hook(dispatcher, params));
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

async fn dispatch_pre_tool_use_with(params: PreHookDispatch<'_>, event: &HookEvent) -> HookOutcome {
    if !matches!(event, HookEvent::PreToolUse { .. }) {
        return continue_outcome(None);
    }

    let mut running_event = event.clone();
    let mut final_mutation = None;
    for hook in matching_hooks(params.hooks, event.event_name(), event.matcher_text()) {
        let payload = HookPayload {
            session_id: params.meta.session_id.clone(),
            cwd: params.meta.cwd.clone(),
            resume_count: 0,
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
                if let Some(tool_input) = outcome.result.mutated_tool_input {
                    running_event = with_pre_tool_input(&running_event, tool_input.clone());
                    final_mutation = Some(tool_input);
                }
            }
        }
    }
    continue_outcome(final_mutation)
}

struct PostHookDispatch {
    subject: String,
    payload: HookPayload,
    hook: DiscoveredHook,
    pending: Option<Arc<Mutex<Option<String>>>>,
}

async fn dispatch_one_post_hook(
    dispatcher: Arc<dyn HookRequestDispatcher>,
    params: PostHookDispatch,
) {
    let server = &params.hook.server;
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
            "{server} hook returned {:?} after tool completion",
            outcome.control
        ));
    }
    if outcome.result.mutated_tool_response.is_some() {
        log::debug!("ignoring mutated tool response from asynchronous {server} hook");
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
        delay: Option<Duration>,
        completed: Option<Arc<Notify>>,
        calls: AtomicUsize,
    }

    #[async_trait]
    impl HookRequestDispatcher for StubDispatcher {
        async fn request(
            &self,
            _subject: String,
            payload: Vec<u8>,
            _timeout: Duration,
        ) -> Result<HookOutcome> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let payload: HookPayload = serde_json::from_slice(&payload)?;
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
            delay: None,
            completed: None,
            calls: AtomicUsize::new(0),
        })
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
        assert!(matching_hooks(&hooks, "SessionStart", None).is_empty());
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
        tokio::spawn(dispatch_one_post_hook(
            dispatcher,
            PostHookDispatch {
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
}
