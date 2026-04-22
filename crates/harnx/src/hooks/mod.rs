//! Re-export shim. The real hooks implementation lives in the
//! `harnx-hooks` crate — moved in Plan 43 (step 9, β+ progressive peel).
//! Call sites like `crate::hooks::PersistentHookManager` continue to
//! resolve through this shim.

pub use harnx_hooks::*;
