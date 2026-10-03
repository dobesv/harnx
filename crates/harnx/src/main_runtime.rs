//! Bootstrap the CLI without constructing its async state on the OS main stack.

use anyhow::{Context, Result};
use std::future::Future;

const MAIN_STACK_BYTES: usize = 8 * 1024 * 1024;

pub(super) fn run<F, Fut, T>(create_future: F) -> Result<T>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = Result<T>>,
    T: Send + 'static,
{
    // Windows executables start with a 1 MiB stack. Deep debug-build NATS poll
    // frames need the same explicit budget already used by harnx-worker.
    let handle = std::thread::Builder::new()
        .name("harnx-main".into())
        .stack_size(MAIN_STACK_BYTES)
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .thread_stack_size(MAIN_STACK_BYTES)
                .enable_all()
                .build()
                .context("build CLI runtime")?;
            runtime.block_on(create_future())
        })
        .context("spawn CLI runtime thread")?;
    match handle.join() {
        Ok(result) => result,
        Err(panic) => std::panic::resume_unwind(panic),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn async_state_is_created_on_runtime_thread_and_joined_before_return() {
        let caller = std::thread::current().id();
        let result = run(move || {
            assert_ne!(std::thread::current().id(), caller);
            assert_eq!(std::thread::current().name(), Some("harnx-main"));
            Box::pin(async { Ok(42) })
        })
        .unwrap();
        assert_eq!(result, 42);
    }

    #[test]
    fn runtime_errors_and_panics_keep_their_original_outcomes() {
        let error =
            run::<_, _, ()>(|| async { anyhow::bail!("CLI operation failed") }).unwrap_err();
        assert_eq!(error.to_string(), "CLI operation failed");
        let panic = std::panic::catch_unwind(|| {
            run::<_, _, ()>(|| async { panic!("CLI operation panicked") })
        });
        assert!(panic.is_err());
    }
}
