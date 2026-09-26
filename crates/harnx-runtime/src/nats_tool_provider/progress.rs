//! Routes shared control-subject progress messages to call-bound runtime sinks.
use futures_util::StreamExt;
use harnx_core::api_types::CompletionTokenUsage;
use harnx_core::event::{ContentBlock, ToolKind, ToolLocation, ToolStatus};
use harnx_core::tool::{ToolProgress, ToolUpdatePatch};
use harnx_toolset::{
    ProgressChunk, ProgressMessage, ToolProgressContent, ToolProgressKind, ToolProgressPatch,
    ToolProgressStatus,
};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::task::JoinHandle;

struct ProgressRouteSink {
    state: Mutex<ProgressRouteState>,
}

struct ProgressRouteState {
    open: bool,
    target: Arc<dyn ToolProgress>,
}

impl ProgressRouteSink {
    fn new(target: Arc<dyn ToolProgress>) -> Self {
        Self {
            state: Mutex::new(ProgressRouteState { open: true, target }),
        }
    }

    fn update(&self, patch: ToolProgressPatch) {
        let state = self.state.lock().expect("NATS progress route poisoned");
        if state.open {
            state.target.update(to_runtime_patch(patch));
        }
    }

    fn close(&self, final_progress: Option<ToolProgressPatch>) {
        let mut state = self.state.lock().expect("NATS progress route poisoned");
        if !state.open {
            return;
        }
        state.open = false;
        if let Some(patch) = final_progress {
            state.target.update(to_runtime_patch(patch));
        }
    }
}

type ProgressRoutes = Arc<Mutex<HashMap<String, Arc<ProgressRouteSink>>>>;

/// One consumer owns the shared control subscription and fans messages out by wire call ID.
pub(super) struct ProgressDispatcher {
    routes: ProgressRoutes,
    task: JoinHandle<()>,
}

impl ProgressDispatcher {
    pub(super) fn new(subscription: async_nats::Subscriber) -> Self {
        let routes = ProgressRoutes::default();
        let task = tokio::spawn(run_dispatcher(subscription, routes.clone()));
        Self { routes, task }
    }

    pub(super) fn register(&self, call_id: String, target: Arc<dyn ToolProgress>) -> ProgressRoute {
        let sink = Arc::new(ProgressRouteSink::new(target));
        let replaced = self
            .routes
            .lock()
            .expect("NATS progress routes poisoned")
            .insert(call_id.clone(), sink.clone());
        if let Some(replaced) = replaced {
            replaced.close(None);
        }
        ProgressRoute {
            call_id,
            routes: self.routes.clone(),
            sink,
            open: true,
        }
    }
}

impl Drop for ProgressDispatcher {
    fn drop(&mut self) {
        self.task.abort();
        let routes =
            std::mem::take(&mut *self.routes.lock().expect("NATS progress routes poisoned"));
        for sink in routes.into_values() {
            sink.close(None);
        }
    }
}

/// Registration guard. Dropping a pending call removes its route on every exit path.
pub(super) struct ProgressRoute {
    call_id: String,
    routes: ProgressRoutes,
    sink: Arc<ProgressRouteSink>,
    open: bool,
}

impl ProgressRoute {
    pub(super) fn finish(mut self, final_progress: Option<ToolProgressPatch>) {
        self.remove();
        self.sink.close(final_progress);
    }

    fn remove(&mut self) {
        if !self.open {
            return;
        }
        self.open = false;
        let mut routes = self.routes.lock().expect("NATS progress routes poisoned");
        if routes
            .get(&self.call_id)
            .is_some_and(|registered| Arc::ptr_eq(registered, &self.sink))
        {
            routes.remove(&self.call_id);
        }
    }
}

impl Drop for ProgressRoute {
    fn drop(&mut self) {
        self.remove();
        self.sink.close(None);
    }
}

async fn run_dispatcher(mut subscription: async_nats::Subscriber, routes: ProgressRoutes) {
    while let Some(message) = subscription.next().await {
        let Ok(message) = serde_json::from_slice::<ProgressMessage>(&message.payload) else {
            // Cancellation shares this subject and is consumed by tool servers, not this dispatcher.
            continue;
        };
        let sink = routes
            .lock()
            .expect("NATS progress routes poisoned")
            .get(&message.call_id)
            .cloned();
        if let Some(sink) = sink {
            let ProgressChunk::V1(patch) = message.chunk;
            sink.update(patch);
        }
    }
}

fn to_runtime_patch(patch: ToolProgressPatch) -> ToolUpdatePatch {
    ToolUpdatePatch {
        markdown: patch.markdown,
        title: patch.title,
        status: patch.status.map(to_runtime_status),
        content: patch
            .content
            .map(|content| content.into_iter().map(to_runtime_content).collect()),
        kind: patch.kind.map(to_runtime_kind),
        locations: patch.locations.map(|locations| {
            locations
                .into_iter()
                .map(|location| ToolLocation {
                    path: location.path,
                    line: location.line,
                })
                .collect()
        }),
        usage: patch.usage.map(|usage| CompletionTokenUsage {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cached_tokens: usage.cached_tokens,
            cache_write_tokens: usage.cache_write_tokens,
        }),
    }
}

fn to_runtime_status(status: ToolProgressStatus) -> ToolStatus {
    match status {
        ToolProgressStatus::Pending => ToolStatus::Pending,
        ToolProgressStatus::InProgress => ToolStatus::InProgress,
        ToolProgressStatus::Completed => ToolStatus::Completed,
        ToolProgressStatus::Failed => ToolStatus::Failed,
    }
}

fn to_runtime_kind(kind: ToolProgressKind) -> ToolKind {
    match kind {
        ToolProgressKind::Read => ToolKind::Read,
        ToolProgressKind::Edit => ToolKind::Edit,
        ToolProgressKind::Delete => ToolKind::Delete,
        ToolProgressKind::Move => ToolKind::Move,
        ToolProgressKind::Search => ToolKind::Search,
        ToolProgressKind::Execute => ToolKind::Execute,
        ToolProgressKind::Think => ToolKind::Think,
        ToolProgressKind::Fetch => ToolKind::Fetch,
        ToolProgressKind::SwitchMode => ToolKind::SwitchMode,
        ToolProgressKind::Other => ToolKind::Other,
    }
}

fn to_runtime_content(content: ToolProgressContent) -> ContentBlock {
    match content {
        ToolProgressContent::Text { text } => ContentBlock::Text(text),
        ToolProgressContent::Image { data, mime } => ContentBlock::Image { data, mime },
        ToolProgressContent::ResourceLink { uri, name } => ContentBlock::ResourceLink { uri, name },
        ToolProgressContent::Opaque { kind, value } => ContentBlock::Opaque { kind, value },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harnx_core::tool::ToolProgress;
    use std::sync::Mutex;

    #[derive(Default)]
    struct RecordingProgress(Mutex<Vec<ToolUpdatePatch>>);

    impl ToolProgress for RecordingProgress {
        fn update(&self, patch: ToolUpdatePatch) {
            self.0.lock().unwrap().push(patch);
        }
    }

    #[test]
    fn final_snapshot_closes_route_before_late_messages() {
        let target = Arc::new(RecordingProgress::default());
        let sink = ProgressRouteSink::new(target.clone());
        sink.update(ToolProgressPatch {
            title: Some("live".into()),
            ..Default::default()
        });
        sink.close(Some(ToolProgressPatch {
            title: Some("final".into()),
            ..Default::default()
        }));
        sink.update(ToolProgressPatch {
            title: Some("late".into()),
            ..Default::default()
        });

        let updates = target.0.lock().unwrap();
        assert_eq!(updates.len(), 2);
        assert_eq!(updates[0].title.as_deref(), Some("live"));
        assert_eq!(updates[1].title.as_deref(), Some("final"));
    }

    #[test]
    fn maps_every_status_and_kind_variant() {
        let statuses = [
            (ToolProgressStatus::Pending, ToolStatus::Pending),
            (ToolProgressStatus::InProgress, ToolStatus::InProgress),
            (ToolProgressStatus::Completed, ToolStatus::Completed),
            (ToolProgressStatus::Failed, ToolStatus::Failed),
        ];
        for (wire, runtime) in statuses {
            assert!(
                std::mem::discriminant(&to_runtime_status(wire))
                    == std::mem::discriminant(&runtime)
            );
        }

        let kinds = [
            (ToolProgressKind::Read, ToolKind::Read),
            (ToolProgressKind::Edit, ToolKind::Edit),
            (ToolProgressKind::Delete, ToolKind::Delete),
            (ToolProgressKind::Move, ToolKind::Move),
            (ToolProgressKind::Search, ToolKind::Search),
            (ToolProgressKind::Execute, ToolKind::Execute),
            (ToolProgressKind::Think, ToolKind::Think),
            (ToolProgressKind::Fetch, ToolKind::Fetch),
            (ToolProgressKind::SwitchMode, ToolKind::SwitchMode),
            (ToolProgressKind::Other, ToolKind::Other),
        ];
        for (wire, runtime) in kinds {
            assert!(
                std::mem::discriminant(&to_runtime_kind(wire)) == std::mem::discriminant(&runtime)
            );
        }
    }

    #[test]
    fn maps_every_content_variant() {
        let opaque_value = serde_json::json!({"field": "value"});
        let mapped = [
            to_runtime_content(ToolProgressContent::Text {
                text: "hello".into(),
            }),
            to_runtime_content(ToolProgressContent::Image {
                data: vec![1, 2, 3],
                mime: "image/png".into(),
            }),
            to_runtime_content(ToolProgressContent::ResourceLink {
                uri: "file:///tmp/result".into(),
                name: Some("result".into()),
            }),
            to_runtime_content(ToolProgressContent::Opaque {
                kind: "vendor".into(),
                value: opaque_value.clone(),
            }),
        ];

        assert!(matches!(&mapped[0], ContentBlock::Text(text) if text == "hello"));
        assert!(matches!(
            &mapped[1],
            ContentBlock::Image { data, mime }
                if data == &[1, 2, 3] && mime == "image/png"
        ));
        assert!(matches!(
            &mapped[2],
            ContentBlock::ResourceLink { uri, name }
                if uri == "file:///tmp/result" && name.as_deref() == Some("result")
        ));
        assert!(matches!(
            &mapped[3],
            ContentBlock::Opaque { kind, value }
                if kind == "vendor" && value == &opaque_value
        ));
    }

    #[test]
    fn maps_full_patch_fields_without_losing_values() {
        use harnx_toolset::{ToolProgressLocation, ToolProgressUsage};

        let patch = to_runtime_patch(ToolProgressPatch {
            title: Some("Indexing".into()),
            status: Some(ToolProgressStatus::InProgress),
            kind: Some(ToolProgressKind::Search),
            locations: Some(vec![ToolProgressLocation {
                path: "src/lib.rs".into(),
                line: Some(42),
            }]),
            usage: Some(ToolProgressUsage {
                input_tokens: 11,
                output_tokens: 7,
                cached_tokens: 3,
                cache_write_tokens: 2,
            }),
            markdown: Some("**working**".into()),
            content: Some(vec![ToolProgressContent::Text {
                text: "chunk".into(),
            }]),
        });

        assert_eq!(patch.title.as_deref(), Some("Indexing"));
        assert_eq!(patch.markdown.as_deref(), Some("**working**"));
        assert!(matches!(patch.status, Some(ToolStatus::InProgress)));
        assert!(matches!(patch.kind, Some(ToolKind::Search)));
        let locations = patch.locations.expect("locations map");
        assert_eq!(locations.len(), 1);
        assert_eq!(locations[0].path, std::path::PathBuf::from("src/lib.rs"));
        assert_eq!(locations[0].line, Some(42));
        let usage = patch.usage.expect("usage maps");
        assert_eq!(usage.input_tokens, 11);
        assert_eq!(usage.output_tokens, 7);
        assert_eq!(usage.cached_tokens, 3);
        assert_eq!(usage.cache_write_tokens, 2);
        assert!(matches!(
            patch.content.as_deref(),
            Some([ContentBlock::Text(text)]) if text == "chunk"
        ));
    }

    #[tokio::test]
    async fn dropping_dispatcher_aborts_task_and_closes_registered_routes() {
        let routes = ProgressRoutes::default();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (dropped_tx, dropped_rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            struct DropSignal(Option<tokio::sync::oneshot::Sender<()>>);
            impl Drop for DropSignal {
                fn drop(&mut self) {
                    if let Some(sender) = self.0.take() {
                        let _ = sender.send(());
                    }
                }
            }

            let _drop_signal = DropSignal(Some(dropped_tx));
            let _ = started_tx.send(());
            std::future::pending::<()>().await;
        });
        started_rx.await.expect("dispatcher task starts");
        let dispatcher = ProgressDispatcher {
            routes: routes.clone(),
            task,
        };
        let target = Arc::new(RecordingProgress::default());
        let route = dispatcher.register("call-drop".into(), target);
        let sink = route.sink.clone();

        drop(dispatcher);

        assert!(routes.lock().unwrap().is_empty());
        assert!(!sink.state.lock().unwrap().open);
        tokio::time::timeout(std::time::Duration::from_secs(2), dropped_rx)
            .await
            .expect("dispatcher task is aborted promptly")
            .expect("dispatcher task drop signal arrives");
        drop(route);
    }
}
