//! Bounded discovery; task settlement lives in reconciler.
use super::*;

const SWEEP_BATCH: usize = 32;
const SWEEP_INTERVAL: Duration = Duration::from_secs(1);
const REGISTRY_READ_TIMEOUT: Duration = Duration::from_secs(5);

pub struct SupervisionConfig {
    pub exports: Vec<Export>,
    pub config: GlobalConfig,
    pub route: SessionActivationRoute,
    pub abort: AbortSignal,
}

impl Runner {
    /// Start once per backend. Weak reference avoids retaining a stopped backend.
    pub fn start_supervision(self: &Arc<Self>, settings: SupervisionConfig) {
        let runner = Arc::downgrade(self);
        tokio::spawn(async move {
            let mut cursor = 0;
            let mut event_cursor = 0;
            let mut pass_started = std::time::Instant::now();
            while !settings.abort.aborted() {
                let Some(runner) = runner.upgrade() else {
                    break;
                };
                if runner.shutting_down.load(Ordering::SeqCst) {
                    break;
                }
                let scan = settings.sweep(&runner, &mut cursor).await;
                let complete = scan.is_ok() && cursor == 0;
                crate::diagnostics::outcome("sweep", scan.is_ok());
                if let Err(error) = scan {
                    warn!(%error, "A2A recovery registry scan unresolved");
                }
                if let Err(error) = runner
                    .store
                    .cleanup_terminal_events(&mut event_cursor)
                    .await
                {
                    warn!(%error, "A2A terminal event cleanup unresolved");
                }
                if complete {
                    metrics::histogram!("harnx_a2a_sweep_lag_seconds")
                        .record(pass_started.elapsed().as_secs_f64());
                    pass_started = std::time::Instant::now();
                }
                drop(runner);
                tokio::select! {
                    _ = tokio::time::sleep(SWEEP_INTERVAL) => (),
                    _ = harnx_core::abort::wait_abort_signal(&settings.abort) => break,
                }
            }
        });
    }
}

impl SupervisionConfig {
    async fn sweep(&self, runner: &Runner, cursor: &mut u64) -> Result<()> {
        for _ in 0..SWEEP_BATCH {
            if self.abort.aborted() || runner.shutting_down.load(Ordering::SeqCst) {
                break;
            }
            let Some(entry) = tokio::time::timeout(
                REGISTRY_READ_TIMEOUT,
                runner.store.next_recovery_registration(cursor),
            )
            .await??
            else {
                break;
            };
            let Some(export) = self
                .exports
                .iter()
                .find(|export| export.agent == entry.agent && export.public_name == entry.export)
            else {
                continue;
            };
            // A fixed whole-transcript deadline could starve the same long
            // session forever. Runtime operations have bounded broker requests.
            let result = tokio::select! {
                result = runner.reconcile_registration(export, &entry, self) => result,
                _ = harnx_core::abort::wait_abort_signal(&self.abort) => break,
            };
            crate::diagnostics::outcome("recovery", result.is_ok());
            if let Err(error) = result {
                warn!(storage = %entry.allocation.storage_key, %error, "A2A background recovery unresolved");
            }
        }
        Ok(())
    }
}
