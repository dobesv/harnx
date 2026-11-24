//! Fault-injection hook infrastructure for crash and recovery integration tests.
//! Feature-gated by `#[cfg(feature = "fault-injection")]`.

#[cfg(feature = "fault-injection")]
mod implementation {
    use parking_lot::Mutex;
    use std::{
        collections::{HashMap, HashSet},
        sync::LazyLock,
        time::Duration,
    };
    use tokio::sync::oneshot;

    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    pub enum RecoveryFaultPoint {
        RoundStartBefore,
        RoundStartAfter,
        AttemptStartBefore,
        AttemptStartAfter,
        DispatchIntentBefore,
        DispatchIntentAfter,
        DownstreamSnapshotBefore,
        DownstreamSnapshotAfter,
        DownstreamCompletionBefore,
        DownstreamCompletionAfter,
        OuterCompletionRecordBefore,
        OuterCompletionRecordAfter,
        CheckpointBefore,
        CheckpointAfter,
        InteractionRequestedBefore,
        InteractionRequestedAfter,
        SuspensionReadyBefore,
        SuspensionReadyAfter,
        AcceptedResponseBefore,
        AcceptedResponseAfter,
        RerunBefore,
        RerunAfter,
        FinalToolResultsBefore,
        FinalToolResultsAfter,
        HandoffDispatchBefore,
        HandoffTargetPromptAfter,
        HandoffCommittedBefore,
        HandoffCommittedAfter,
    }

    #[derive(Debug)]
    pub enum PauseAction {
        Resume,
        Crash,
    }

    struct RegisteredPause {
        session_filter: String,
        point: RecoveryFaultPoint,
        reached_tx: oneshot::Sender<()>,
        action_rx: oneshot::Receiver<PauseAction>,
    }

    struct FaultState {
        pauses: Vec<RegisteredPause>,
        simulated_store_errors: HashSet<String>,
        store_error_reads: HashMap<String, usize>,
        t2_1_workers: HashSet<String>,
        t2_1_preflights: HashMap<String, usize>,
    }

    static STATE: LazyLock<Mutex<FaultState>> = LazyLock::new(|| {
        Mutex::new(FaultState {
            pauses: Vec::new(),
            simulated_store_errors: HashSet::new(),
            store_error_reads: HashMap::new(),
            t2_1_workers: HashSet::new(),
            t2_1_preflights: HashMap::new(),
        })
    });

    pub struct PauseHandle {
        reached_rx: oneshot::Receiver<()>,
        action_tx: Option<oneshot::Sender<PauseAction>>,
    }

    impl PauseHandle {
        // qual:test_helper
        pub async fn wait_reached(&mut self) -> anyhow::Result<()> {
            tokio::time::timeout(Duration::from_secs(10), &mut self.reached_rx)
                .await
                .map_err(|_| anyhow::anyhow!("timed out waiting for fault point reached"))?
                .map_err(|_| {
                    anyhow::anyhow!("pause sender dropped without reaching fault point")
                })?;
            Ok(())
        }

        // qual:test_helper
        pub fn release(mut self) {
            if let Some(tx) = self.action_tx.take() {
                let _ = tx.send(PauseAction::Resume);
            }
        }

        // qual:test_helper
        pub fn crash(mut self) {
            if let Some(tx) = self.action_tx.take() {
                let _ = tx.send(PauseAction::Crash);
            }
        }
    }

    impl Drop for PauseHandle {
        fn drop(&mut self) {
            if let Some(tx) = self.action_tx.take() {
                let _ = tx.send(PauseAction::Crash);
            }
        }
    }

    fn matches_session(session: &str, filter: &str) -> bool {
        !filter.is_empty() && !session.is_empty() && session == filter
    }

    // qual:test_helper
    pub fn pause_point(session_id: &str, point: RecoveryFaultPoint) -> PauseHandle {
        let (reached_tx, reached_rx) = oneshot::channel();
        let (action_tx, action_rx) = oneshot::channel();
        let mut state = STATE.lock();
        state.pauses.push(RegisteredPause {
            session_filter: session_id.to_string(),
            point,
            reached_tx,
            action_rx,
        });
        PauseHandle {
            reached_rx,
            action_tx: Some(action_tx),
        }
    }

    // qual:test_helper
    pub fn simulate_store_error(session_id: &str, simulate: bool) {
        let mut state = STATE.lock();
        if simulate {
            state.simulated_store_errors.insert(session_id.to_string());
        } else {
            state.simulated_store_errors.remove(session_id);
        }
    }

    pub fn is_store_error_simulated(session_id: &str) -> bool {
        let mut state = STATE.lock();
        let unavailable = state.simulated_store_errors.contains(session_id);
        if unavailable {
            *state
                .store_error_reads
                .entry(session_id.to_owned())
                .or_default() += 1;
        }
        unavailable
    }

    // qual:test_helper
    pub fn store_error_count(session_id: &str) -> usize {
        STATE
            .lock()
            .store_error_reads
            .get(session_id)
            .copied()
            .unwrap_or(0)
    }

    // qual:test_helper
    pub fn simulate_t2_1_worker(session_id: &str, simulate: bool) {
        let mut state = STATE.lock();
        if simulate {
            state.t2_1_workers.insert(session_id.to_owned());
        } else {
            state.t2_1_workers.remove(session_id);
        }
    }

    pub fn t2_1_worker_preflight(session_id: &str) -> bool {
        let mut state = STATE.lock();
        let legacy = state.t2_1_workers.contains(session_id);
        if legacy {
            *state
                .t2_1_preflights
                .entry(session_id.to_owned())
                .or_default() += 1;
        }
        legacy
    }

    // qual:test_helper
    pub fn t2_1_preflight_count(session_id: &str) -> usize {
        STATE
            .lock()
            .t2_1_preflights
            .get(session_id)
            .copied()
            .unwrap_or(0)
    }

    pub async fn hit(session_id: &str, point: RecoveryFaultPoint) {
        let matched = {
            let mut state = STATE.lock();
            let index = state
                .pauses
                .iter()
                .position(|p| p.point == point && matches_session(session_id, &p.session_filter));
            index.map(|idx| state.pauses.swap_remove(idx))
        };

        if let Some(pause) = matched {
            let _ = pause.reached_tx.send(());
            match pause.action_rx.await {
                Ok(PauseAction::Crash) | Err(_) => {
                    panic!("simulated worker crash via fault-injection hook at {point:?}");
                }
                Ok(PauseAction::Resume) => {}
            }
        }
    }
}

#[cfg(not(feature = "fault-injection"))]
mod implementation {
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    pub enum RecoveryFaultPoint {
        RoundStartBefore,
        RoundStartAfter,
        AttemptStartBefore,
        AttemptStartAfter,
        DispatchIntentBefore,
        DispatchIntentAfter,
        DownstreamSnapshotBefore,
        DownstreamSnapshotAfter,
        DownstreamCompletionBefore,
        DownstreamCompletionAfter,
        OuterCompletionRecordBefore,
        OuterCompletionRecordAfter,
        CheckpointBefore,
        CheckpointAfter,
        InteractionRequestedBefore,
        InteractionRequestedAfter,
        SuspensionReadyBefore,
        SuspensionReadyAfter,
        AcceptedResponseBefore,
        AcceptedResponseAfter,
        RerunBefore,
        RerunAfter,
        FinalToolResultsBefore,
        FinalToolResultsAfter,
        HandoffDispatchBefore,
        HandoffTargetPromptAfter,
        HandoffCommittedBefore,
        HandoffCommittedAfter,
    }

    #[inline(always)]
    pub async fn hit(_session_id: &str, _point: RecoveryFaultPoint) {}

    #[inline(always)]
    pub fn is_store_error_simulated(_session_id: &str) -> bool {
        false
    }
}

pub use implementation::*;
