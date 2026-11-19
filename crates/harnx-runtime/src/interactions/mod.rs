//! In-process tool interactions and durable response acceptance.

pub mod decision;
pub mod pending;
mod summary;
pub use summary::derive_pending_interaction_summary;

pub use decision::{
    accept_interaction_response, apply_interaction_response, InteractionDecisionOutcome,
};
pub use pending::{
    derive_pending_interactions, interaction_request_from_entry, PendingInteraction,
};

#[cfg(test)]
mod tests;

#[cfg(test)]
mod summary_tests;
