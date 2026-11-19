use std::{future::Future, sync::Arc};

use async_trait::async_trait;

use crate::{bashkit_backend::RunControl, CancellationToken, ExtFunctionResult, MontyObject};

/// Host-only control for one run. Never exposed as a script value.
#[derive(Clone)]
pub struct ScriptControl {
    pub(crate) inner: Arc<RunControl>,
}

impl ScriptControl {
    /// Stop the interpreter with [`crate::ScriptStatus::Stopped`]. Store any typed
    /// outcome in caller-owned state before calling this method. The backend joins
    /// interpreter-owned work before returning; script code cannot catch this stop.
    pub fn stop(&self) {
        self.inner.stop_from_host();
    }

    /// Carry nesting admission into a separately spawned host task. Inline host
    /// futures inherit this context automatically; Tokio-spawned tasks don't.
    /// Call this *inside* the spawned task and await its result before returning
    /// from the host operation. Detached work still needs cancellation cleanup.
    pub async fn scope<F: Future>(&self, future: F) -> F::Output {
        crate::bashkit_backend::NESTING_DEPTH
            .scope(self.inner.child_depth, future)
            .await
    }
}

/// Context shared by Python functions and the host operations behind Bash commands.
#[derive(Clone)]
pub struct ScriptHostContext {
    /// Cancelled when the run ends, including host stops, failures and caller drop.
    pub cancel: CancellationToken,
    pub control: ScriptControl,
}

/// Python capabilities for one run. Names are an allowlist, not a tool catalog;
/// registering Bash commands alone never makes them callable from Python.
#[derive(Clone)]
pub struct ScriptPythonFunctions {
    pub names: Vec<String>,
    pub handler: Arc<dyn ScriptPythonHandler>,
}

/// Async bridge to trusted host operations. Implementations can share the same
/// host state as [`crate::ScriptCommand`] implementations. Calls aren't serialized.
#[async_trait]
pub trait ScriptPythonHandler: Send + Sync {
    /// Values cross the Monty boundary without JSON/string coercion. Return
    /// `ExtFunctionResult::Error` for a catchable Python exception. For suspension
    /// or typed fatal/cancel propagation, record host state and call `ctx.control.stop()`.
    async fn call(
        &self,
        name: String,
        args: Vec<MontyObject>,
        kwargs: Vec<(MontyObject, MontyObject)>,
        ctx: ScriptHostContext,
    ) -> ExtFunctionResult;
}
