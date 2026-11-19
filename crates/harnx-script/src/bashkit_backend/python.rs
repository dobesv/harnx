use std::sync::{atomic::Ordering, Arc};

use crate::{
    ExcType, ExtFunctionResult, MontyException, ScriptControl, ScriptHostContext,
    ScriptPythonFunctions,
};

use super::RunControl;

pub(super) fn handler(
    functions: ScriptPythonFunctions,
    control: Arc<RunControl>,
) -> bashkit::PythonExternalFnHandler {
    Arc::new(move |name, args, kwargs| {
        let functions = functions.clone();
        let control = control.clone();
        Box::pin(async move {
            // NameLookup is checked by BashKit too. Don't trust a callable value
            // supplied through another external function to bypass the allowlist.
            if !functions.names.contains(&name) {
                return ExtFunctionResult::Error(MontyException::new(
                    ExcType::NameError,
                    Some(format!("unregistered Python host function: {name}")),
                ));
            }
            let ctx = ScriptHostContext {
                cancel: control.stop.clone(),
                control: ScriptControl {
                    inner: control.clone(),
                },
            };
            let result = functions.handler.call(name, args, kwargs, ctx).await;
            if control.host_stopped.load(Ordering::Acquire) {
                // A Python exception could be caught. Instead yield to the stop
                // branch without ever resuming Monty with a result or exception.
                return std::future::pending().await;
            }
            result
        })
    })
}
