//! Shared tool admission and evaluation for middleware and event-hook scripts.

use anyhow::{anyhow, ensure, Result};
use harnx_core::{abort::AbortSignal, tool::ToolCall};
use harnx_engine::tool::{
    eval_inner_tool_call, EvaluatedInnerCall, InnerToolEvalOptions, ToolEvalContext,
    ToolMiddlewareContext,
};
use harnx_script::{
    commands::{CallOutcome, InnerExecutor, ToolSpec},
    CancellationToken, ScriptCommand,
};
use serde_json::Value;
use std::{collections::HashSet, sync::Arc};

#[derive(Clone, Copy)]
enum Origin {
    Middleware,
    EventHook,
}

impl Origin {
    fn label(self) -> &'static str {
        match self {
            Self::Middleware => "middleware",
            Self::EventHook => "event-hook",
        }
    }
}

pub(crate) struct ScriptTools {
    context: ToolMiddlewareContext,
    allowed_names: HashSet<String>,
    origin: Origin,
}

impl ScriptTools {
    pub(super) fn middleware(
        context: ToolMiddlewareContext,
        allowed_names: HashSet<String>,
    ) -> Self {
        Self {
            context,
            allowed_names,
            origin: Origin::Middleware,
        }
    }

    pub(crate) fn event_hook(
        evaluation: Arc<ToolEvalContext>,
        abort: AbortSignal,
        allowed_names: HashSet<String>,
    ) -> Self {
        Self {
            context: ToolMiddlewareContext { evaluation, abort },
            allowed_names,
            origin: Origin::EventHook,
        }
    }

    // qual:allow(iosp) — admission and the origin's round/recursion policy must be applied together before constructing the inner call.
    pub(super) fn prepare(
        &self,
        name: &str,
        arguments: Value,
    ) -> Result<(ToolEvalContext, ToolCall)> {
        ensure!(
            self.allowed_names.contains(name),
            "{} tool '{name}' is not allowlisted",
            self.origin.label()
        );
        ensure!(
            arguments.is_object(),
            "tool arguments must be a JSON object"
        );
        let base = &self.context.evaluation;
        let mut ctx = base.fork_for_inner_calls(InnerToolEvalOptions {
            providers: base.providers.clone(),
            tool_round: match self.origin {
                Origin::Middleware => base.tool_round,
                Origin::EventHook => None,
            },
            emit_tool_call_fn: Arc::new(|_, _| {}),
            emit_tool_result_fn: Arc::new(|_, _| {}),
            emit_tool_blocked_fn: Arc::new(|_, _| {}),
            emit_tool_update_fn: Arc::new(|_, _| {}),
        });
        // Only middleware helpers bypass the chain to avoid recursing into it.
        // Hooks must retain policy and credential middleware.
        if matches!(self.origin, Origin::Middleware) {
            ctx.middleware_fn = None;
        }
        ctx.allowed_tool_names = self.allowed_names.clone();
        let call = ToolCall::new(
            name.to_string(),
            arguments,
            Some(uuid::Uuid::new_v4().to_string()),
            None,
        );
        Ok((ctx, call))
    }

    pub(super) async fn evaluate(
        &self,
        ctx: &ToolEvalContext,
        call: ToolCall,
    ) -> Result<EvaluatedInnerCall> {
        eval_inner_tool_call(ctx, call, &self.context.abort).await
    }

    pub(crate) async fn execute(
        &self,
        name: &str,
        arguments: Value,
        cancel: CancellationToken,
    ) -> Result<Value> {
        let (ctx, call) = self.prepare(name, arguments)?;
        let evaluated = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(anyhow!("{} tool call cancelled", self.origin.label())),
            result = self.evaluate(&ctx, call) => result?,
        };
        Ok(evaluated.result.output)
    }
}

#[async_trait::async_trait]
impl InnerExecutor for ScriptTools {
    async fn call(&self, name: &str, args: Value, cancel: CancellationToken) -> CallOutcome {
        match self.execute(name, args, cancel).await {
            Ok(output) => CallOutcome::Ok(output),
            Err(error) => CallOutcome::Fatal {
                message: format!("{error:#}"),
            },
        }
    }
}

pub(crate) fn tool_commands(
    tools: &[harnx_core::tool::ToolDeclaration],
    executor: Arc<dyn InnerExecutor>,
) -> Result<Vec<Arc<dyn ScriptCommand>>> {
    let specs = tools
        .iter()
        .map(|tool| ToolSpec {
            name: tool.name.clone(),
            description: tool.description.clone(),
            schema: tool.parameters.as_value().clone(),
        })
        .collect();
    Ok(harnx_script::commands::tool_commands(specs, executor)?)
}
