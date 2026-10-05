//! All of this crate's integration tests, built as one binary. A separate
//! binary per file links the whole dependency graph again for each, which
//! dominated build time; nextest still runs every test in its own process.

#[allow(dead_code)]
mod common;
#[allow(dead_code)]
#[path = "common/generation.rs"]
mod generation;
#[allow(dead_code)]
#[path = "common/worker.rs"]
mod worker;

mod agent_prompt_rendering;
mod frontend_affine_workers;
mod hook_supervisor;
mod interruption;
mod interruption_fencing;
mod local_worker_supervisor;
mod manage_servers;
mod nats_activation_replicas;
mod nats_agent_variables_e2e;
mod nats_attachments;
mod nats_connect;
mod nats_control;
mod nats_deadline_admission;
mod nats_event_fanout;
mod nats_hooks_e2e;
mod nats_lease;
mod nats_local_server;
mod nats_macro_run_limits;
mod nats_read_convergence;
mod nats_read_reconcile_heal;
mod nats_read_restart_persistence;
mod nats_read_state_list;
mod nats_remote_transcript;
mod nats_run_limits;
mod nats_session;
mod nats_session_agent_identity;
mod nats_session_delete;
mod nats_session_dump;
mod nats_session_log;
mod nats_session_metadata;
mod nats_session_metadata_stale_replica;
mod nats_session_properties;
mod nats_session_stream_identity;
mod nats_tool_confirmation;
mod nats_tool_provider;
mod nats_worker;
mod nats_worker_attention;
mod nats_worker_handoff_hooks;
mod nats_worker_handoffs;
mod nats_worker_sigterm_e2e;
mod package_loading;
mod package_tool_naming_e2e;
mod scope_discovery;
mod title_generation;
mod tls_client_config;
mod tool_reconciler;
mod tool_reconciler_e2e;
mod tool_reconciler_race;
mod tool_supervisor;
mod worker_remote_session_cleanup;
