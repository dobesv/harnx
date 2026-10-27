//! Launch delivery handles for a new or resumed segment of the same task.
use super::*;

pub(super) struct SegmentRequest {
    pub slot: Arc<ContextSlot>,
    pub session: NatsSession,
    pub input: Option<Input>,
    pub admission: AdmissionOwnership,
}

struct SegmentPublication {
    channels: StreamChannels,
    resumed: bool,
}

impl Runner {
    pub(super) async fn launch_segment(
        self: &Arc<Self>,
        request: SegmentRequest,
    ) -> Result<StartTurnResult> {
        let SegmentRequest {
            slot,
            session,
            input,
            admission,
        } = request;
        let AdmissionOwnership {
            guard: mut active,
            authority,
        } = admission;
        let ticket = authority.ticket(session.storage_key())?;
        let task_id = segment_task_id(&authority)?;
        let (events, rx) = broadcast::channel(EVENT_CAPACITY);
        let (cancel_tx, cancel_rx) = mpsc::channel(1);
        let (done_tx, done) = watch::channel(false);
        let live = Arc::new(parking_lot::Mutex::new(LiveState::default()));
        *active = Some(Arc::new(RunnerHandle {
            task_id: task_id.clone(),
            lease: authority.lease.clone(),
            fence: authority.context.version()?.owner,
            session: session
                .clone()
                .with_admission_id(ticket.invocation_id().into()),
            cancel_tx,
            done,
        }));
        let (admitted_tx, admitted_rx) = oneshot::channel();
        let publisher = self.segment_publisher(
            &session,
            SegmentPublication {
                channels: StreamChannels { events, live },
                resumed: input.is_none(),
            },
            AdmissionOwnership {
                guard: active,
                authority,
            },
        );
        let detached = DetachedTurn {
            publisher,
            slot,
            session,
            input,
            task_id,
            cancel_rx,
            admitted_tx,
            done_tx,
        };
        tokio::spawn(detached.run());
        let snapshot = admitted_rx
            .await
            .context("turn stopped before admission")??;
        Ok(StartTurnResult {
            snapshot,
            events: rx,
            deduped: false,
        })
    }

    fn segment_publisher(
        &self,
        session: &NatsSession,
        publication: SegmentPublication,
        admission: AdmissionOwnership,
    ) -> Publisher {
        let constructor = if !publication.resumed {
            Publisher::new
        } else {
            Publisher::resume
        };
        let publisher = constructor(self.store.clone(), session, publication.channels, admission);
        #[cfg(feature = "fault-injection")]
        let mut publisher = publisher;
        #[cfg(feature = "fault-injection")]
        {
            publisher.hooks = self.hooks.clone();
        }
        publisher
    }
}

fn segment_task_id(authority: &OwnedTask) -> Result<String> {
    Ok(authority
        .context
        .document
        .state
        .active
        .as_ref()
        .context("segment task missing")?
        .snapshot
        .task
        .id
        .clone())
}
