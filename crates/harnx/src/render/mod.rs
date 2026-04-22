//! Re-export shim. The real render code lives in the `harnx-render`
//! crate — moved in Plan 42 (step 9, β+ progressive peel). Call sites
//! like `crate::render::MarkdownRender` continue to resolve through
//! this shim.

pub use harnx_render::*;
