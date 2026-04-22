//! Re-export shim. The real RAG engine lives in the `harnx-rag` crate —
//! moved in Plan 45 (β+ progressive peel). Call sites like
//! `crate::rag::Rag` continue to resolve through this shim.

pub use harnx_rag::*;
