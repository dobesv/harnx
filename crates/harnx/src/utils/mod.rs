//! Shim for backward compat. Full implementation lives in
//! `harnx_runtime::utils` (plan P46 T2). Glob re-export preserves
//! every `crate::utils::X` callsite in harnx (main.rs, tui/, serve.rs,
//! acp/, agent_event_sink.rs, cli_event_sink.rs, test_utils/).

pub use harnx_runtime::utils::*;
