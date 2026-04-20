//! Re-exports of system-variable interpolation helpers from `harnx-core`.
//! The implementation moved to `harnx-core::system_vars` alongside
//! `AgentConfig`, which depends on it. Kept as a re-export so any future
//! consumer in harnx can use `crate::utils::interpolate_variables`.
#![allow(unused_imports)]

pub use harnx_core::system_vars::{interpolate_variables, RE_VARIABLE};
