//! The strict metadata update a parent supplies with a child-session tool
//! call.
//!
//! [`ChildSessionMetadataPatch`] is a deliberately narrow view of the typed
//! metadata patch ([`SessionMetadataPatch`]): title and stored variables reuse
//! its field semantics, properties reuse the validator an agent's own
//! `harnx_write_session_meta` write goes through, and execution overrides and
//! raw extensions are not settable from tool input at all. Deserialization is
//! strict (`deny_unknown_fields`), so an `overrides` or extension payload is
//! rejected at the tool boundary before any reservation or mutation happens.
//!
//! The same type drives both write paths:
//! - creation: [`apply_to_initializer`] folds the patch into the
//!   [`SessionInitializer`], so the first canonical record already carries
//!   the requested state — never a follow-up mutation after the KV create;
//! - existing sessions: `SessionMetadataStore::apply_child_patch` applies
//!   title, variables and properties in one compare-and-set, with the
//!   revision guard and normalized-state check a retried invocation needs.

use super::store::{is_wrong_last_revision, PatchGuard};
use super::{
    session_properties, MetadataRecord, SessionInitializer, SessionMetadata, SessionMetadataPatch,
    SessionMetadataStore, SessionPropertiesUpdate, SessionTitlePatch,
};
use anyhow::Result;
use harnx_core::agent_config::AgentVariables;
use serde::Deserialize;

/// The metadata a parent may set on a child session through the session
/// tools. Field semantics match the descriptive metadata a child can set
/// itself; execution settings are out of scope here.
///
/// Every field is optional. An omitted field keeps the session's current
/// state, an omitted `properties` update keeps the stored properties, and a
/// supplied `variables` map replaces the stored overrides wholesale —
/// including an explicit empty map, which clears them. Values are stored as
/// given, never truncated.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ChildSessionMetadataPatch {
    /// Session title. `value` is stored trimmed; a null or blank value clears
    /// the title and its manual flag; `manual: true` (default false) keeps a
    /// later automatic title replacement from overwriting it.
    pub title: Option<SessionTitlePatch>,
    /// Stored session variables, replaced wholesale when supplied.
    pub variables: Option<AgentVariables>,
    /// Session properties, with the same names, values, inheritance flags
    /// and limits an agent's own property write goes through. Read-only
    /// identity properties are rejected here exactly as they are there.
    pub properties: Option<SessionPropertiesUpdate>,
}

/// What applying a [`ChildSessionMetadataPatch`] to an existing record did.
#[derive(Debug)]
pub enum ChildPatchOutcome {
    /// The record now carries the requested state; `revision` is the revision
    /// this call wrote.
    Applied(super::MetadataRecord),
    /// The requested state already held at the observed revision, so nothing
    /// was written. A retried invocation that lands here must proceed
    /// without re-applying, and must never reset what the record gained
    /// since.
    AlreadyHeld(super::MetadataRecord),
    /// The record moved past `expected_revision` without carrying the
    /// requested state. Nothing was written. The caller must re-check durable
    /// admission before deciding anything, and must never silently rebase
    /// onto the newer record.
    Conflict {
        current: super::MetadataRecord,
        expected_revision: u64,
    },
}

impl ChildSessionMetadataPatch {
    /// Whether the patch requests nothing at all.
    pub fn is_empty(&self) -> bool {
        self.title.is_none() && self.variables.is_none() && self.properties.is_none()
    }

    /// Check every name and value before any session state is touched.
    ///
    /// Catches everything that can be judged without the session's real
    /// properties: unknown and read-only property names, malformed values,
    /// inheritance requests for a non-inheritable property. The
    /// custom-property count limit depends on the actual (possibly inherited)
    /// properties and is enforced atomically at apply time.
    pub fn validate(&self) -> Result<()> {
        if let Some(update) = &self.properties {
            update.validate()?;
        }
        Ok(())
    }
}

/// Fold the patch into the initializer's in-memory state, so the first
/// canonical record a create persists already carries the requested title,
/// variables and properties.
///
/// Property operations apply against the initializer's actual properties —
/// the ones inheritance and destination routing resolved — so limits and
/// inheritance see real state, not a placeholder. An existing creation patch
/// (set by trusted creation code) keeps its fields except where this patch
/// supplies its own; its execution overrides are never touched.
///
/// Fails without touching the initializer when a property operation is
/// invalid for that state, or when the resulting extensions exceed the
/// serialized limits the canonical record enforces (a property set within
/// the count and per-value caps can still overflow the 64 KiB extension
/// namespace). Failing here keeps the rejection before any reservation or
/// mutation, instead of surfacing at the store's `create` after T2 has
/// reserved a session.
pub fn apply_to_initializer(
    initializer: &mut SessionInitializer,
    patch: &ChildSessionMetadataPatch,
) -> Result<()> {
    patch.validate()?;
    let mut candidate = initializer.clone();
    if let Some(update) = &patch.properties {
        update.apply(&mut candidate.properties)?;
    }
    if patch.title.is_some() || patch.variables.is_some() {
        let requested = SessionMetadataPatch {
            title: patch.title.clone(),
            variables: patch.variables.clone(),
            overrides: None,
        };
        match &mut candidate.metadata_patch {
            Some(existing) => {
                if requested.title.is_some() {
                    existing.title = requested.title;
                }
                if requested.variables.is_some() {
                    existing.variables = requested.variables;
                }
            }
            None => candidate.metadata_patch = Some(requested),
        }
    }
    validate_prospective_extensions(&candidate)?;
    *initializer = candidate;
    Ok(())
}

/// The extension limits the first canonical record must satisfy, checked
/// against exactly what `SessionMetadata::new` would persist for
/// `initializer` (its tool context and properties). Building the record
/// with a placeholder id and running the full validation keeps this check
/// from drifting from what the store's `create` enforces later.
fn validate_prospective_extensions(initializer: &SessionInitializer) -> Result<()> {
    const UNRESERVED: &str = "unreserved";
    SessionMetadata::new(UNRESERVED, initializer.clone()).validate(UNRESERVED)
}

/// Apply the patch in place to existing canonical metadata, returning
/// whether anything changed. One pass: title and variables through the
/// shared typed patch — so field semantics cannot drift from the metadata
/// PATCH path — and properties through the property validator, against the
/// record's actual stored properties.
///
/// Atomic: a validation failure leaves `metadata` exactly as it was, so a
/// rejected mixed update (valid title plus invalid property operation)
/// changes nothing.
pub(in crate::nats_session_metadata) fn apply_to_metadata(
    metadata: &mut SessionMetadata,
    patch: &ChildSessionMetadataPatch,
) -> Result<bool> {
    let mut candidate = metadata.clone();
    super::view::apply_typed_patch(
        &mut candidate,
        &SessionMetadataPatch {
            title: patch.title.clone(),
            variables: patch.variables.clone(),
            overrides: None,
        },
    );
    if let Some(update) = &patch.properties {
        let mut properties = session_properties(&candidate)?;
        update.apply(&mut properties)?;
        super::session_properties::store_properties(&mut candidate, &properties)?;
    }
    let changed = candidate != *metadata;
    if changed {
        *metadata = candidate;
    }
    Ok(changed)
}

/// Whether `metadata` already carries the complete normalized patch.
///
/// The recovery check for a retried invocation: applying the patch to the
/// current record changes nothing, so the requested state already holds and
/// the retry must proceed without writing — whatever else the record gained
/// since (a later child edit) stays in place.
pub fn already_holds(metadata: &SessionMetadata, patch: &ChildSessionMetadataPatch) -> bool {
    let mut candidate = metadata.clone();
    matches!(apply_to_metadata(&mut candidate, patch), Ok(false))
}

impl SessionMetadataStore {
    /// Apply a parent-supplied child metadata patch to an existing session's
    /// record: title, stored variables and properties in one compare-and-set,
    /// with unrelated fields preserved under concurrency.
    ///
    /// Without an expected revision, the store retries lost revision races,
    /// re-applying the patch to the latest committed record the way the other
    /// metadata mutations do; the patch can still fail validation against the
    /// record it finally settles on (an invalid or over-limit property
    /// operation), and that failure writes nothing.
    ///
    /// With one, the patch applies only at exactly that revision. A record
    /// that already carries the complete normalized patch satisfies the call
    /// without writing — a retried invocation must not reset later child
    /// edits — and a record that moved past the revision without carrying it
    /// is a [`ChildPatchOutcome::Conflict`], never a silent rebase onto the
    /// newer state.
    pub async fn apply_child_patch(
        &self,
        session_id: &str,
        patch: &ChildSessionMetadataPatch,
        expected_revision: Option<u64>,
    ) -> Result<ChildPatchOutcome> {
        patch.validate()?;
        match expected_revision {
            None => self.apply_child_patch_fresh(session_id, patch).await,
            Some(expected_revision) => {
                self.apply_child_patch_pinned(session_id, patch, expected_revision)
                    .await
            }
        }
    }

    /// Retry-loop CAS against whatever record is latest, like the other
    /// metadata mutations.
    async fn apply_child_patch_fresh(
        &self,
        session_id: &str,
        patch: &ChildSessionMetadataPatch,
    ) -> Result<ChildPatchOutcome> {
        let mut applied = false;
        let record = self
            .patch_guarded_if_changed(session_id, PatchGuard::default(), |metadata| {
                let changed = apply_to_metadata(metadata, patch)?;
                applied = changed;
                Ok(changed)
            })
            .await?;
        Ok(if applied {
            ChildPatchOutcome::Applied(record)
        } else {
            ChildPatchOutcome::AlreadyHeld(record)
        })
    }

    /// One CAS attempt at exactly `expected_revision`, so a retried
    /// invocation can neither rebase onto newer state nor reset a later
    /// child edit.
    async fn apply_child_patch_pinned(
        &self,
        session_id: &str,
        patch: &ChildSessionMetadataPatch,
        expected_revision: u64,
    ) -> Result<ChildPatchOutcome> {
        let record = self
            .record_for_patch(session_id, PatchGuard::default())
            .await?;
        // Already-carried state satisfies the call at any revision: a retry
        // must never write over what the record gained since.
        if already_holds(&record.metadata, patch) {
            return Ok(ChildPatchOutcome::AlreadyHeld(record));
        }
        // A record that moved past the expected revision without carrying
        // the state is a recoverable conflict. Deciding this before applying
        // the patch matters: an apply that would fail against the newer
        // record (say, the custom-property count filled up) must surface as
        // a conflict the caller can re-check admission on, not as a hard
        // validation error.
        if record.revision != expected_revision {
            return Ok(ChildPatchOutcome::Conflict {
                current: record,
                expected_revision,
            });
        }
        let mut candidate = record.metadata.clone();
        apply_to_metadata(&mut candidate, patch)?;
        // This path writes outside the store's CAS loop, so it must run the
        // record-level validation itself — extension namespace and aggregate
        // limits, identity — the same checks patch_guarded_if_changed
        // enforces before every other metadata write.
        candidate.validate_storage_key(session_id)?;
        match self.update_metadata(&candidate, record.revision).await {
            Ok(revision) => {
                self.publish_invalidation(session_id, revision).await;
                Ok(ChildPatchOutcome::Applied(MetadataRecord {
                    metadata: candidate,
                    revision,
                }))
            }
            // Lost a write race between the read and the CAS: the winning
            // writer may have applied the same state, otherwise the caller
            // has to re-check admission before retrying.
            Err(error) if is_wrong_last_revision(&error) => Ok(self
                .child_patch_race_outcome(session_id, patch, expected_revision)
                .await?),
            Err(error) => Err(error),
        }
    }

    /// Re-read after a lost CAS race: same state means the request already
    /// holds, anything else is a recoverable conflict.
    async fn child_patch_race_outcome(
        &self,
        session_id: &str,
        patch: &ChildSessionMetadataPatch,
        expected_revision: u64,
    ) -> Result<ChildPatchOutcome> {
        let current = self
            .record_for_patch(session_id, PatchGuard::default())
            .await?;
        Ok(if already_holds(&current.metadata, patch) {
            ChildPatchOutcome::AlreadyHeld(current)
        } else {
            ChildPatchOutcome::Conflict {
                current,
                expected_revision,
            }
        })
    }
}

#[cfg(test)]
#[path = "child_patch_tests.rs"]
mod tests;
