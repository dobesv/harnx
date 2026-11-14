//! Joined-dispatch records and the void-recovery bookkeeping that releases
//! them (issue #2672). One responsibility: the durable record a joining
//! caller leaves, and the CAS moves that hand a proven-void join to its
//! delivering invocation. Moved out of `admission.rs` when this branch's
//! additions pushed that module over CodeScene's cohesion limit; the public
//! API paths are unchanged (re-exported through `admission`).
use super::{intent_key, prompt_key, InvocationAdmission};
use crate::nats_session_metadata::SessionMetadataStore;
use anyhow::{ensure, Context, Result};
use async_nats::jetstream::kv;
use serde::{Deserialize, Serialize};

/// What one proven-void join releases (issue #2672, review F1): the prompt
/// whose record and binding still name `dead_invocation`, and the invocation
/// that must deliver it instead. Grouped so the release entry points stay
/// within the argument budget (CodeScene policy, like
/// `RecordedDispatchPrompt`).
#[derive(Clone, Copy, Debug)]
pub struct VoidJoinRelease<'a> {
    pub storage: &'a str,
    pub prompt: &'a str,
    pub dead_invocation: &'a str,
    pub target: &'a str,
}

/// Per-prompt dispatch identity for a caller that joined a session's active
/// turn instead of reserving a new invocation (issue #2672). Stored create-only
/// in session metadata KV under `(storage_key, prompt_id)`.
///
/// The execution admission chain (`prompts.{storage}.{prompt_id}` -> bound
/// invocation -> `InvocationAdmission`) keeps supplying execution facts —
/// origin, run identity, ancestry, admitted_at, timeout and terminal
/// classification. This record is authoritative for everything the dispatcher
/// and observer attribute to the *caller*: parent identity and the original
/// composed prompt content.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct JoinedDispatch {
    /// Joining caller's parent storage key, the identity `Scope::resolve` and
    /// `validate_dispatch_admission` compare against the trusted caller scope.
    pub parent_storage_key: Option<String>,
    /// Joining caller's parent invocation id (its `RunLimitsRecord`
    /// identity), the invocation-level identity `validate_dispatch_admission`
    /// compares on the new-invocation path.
    pub parent_invocation_id: Option<String>,
    /// Original composed prompt content after attachment resolution — the same
    /// form the invocation admission's `prompt_content` stores for new
    /// invocations. Authoritative for replay; never re-composed or re-fetched.
    pub prompt_content: harnx_core::message::MessageContent,
    /// The invocation id of the active turn this prompt joined. The prompt
    /// admission binding must point at exactly this invocation.
    pub invocation_id: String,
}

impl JoinedDispatch {
    /// Whether `parent_storage_key`/`parent_invocation_id` name exactly the
    /// caller this record belongs to. One predicate for every replay and
    /// activation site (issue #2672, review F7); observation does not check
    /// caller identity and must not use this.
    ///
    /// Both fields must match: the storage scopes compare exactly (absence
    /// matches absence, `None == None`, though every stored record carries
    /// `Some` and dispatch callers must present one), and the caller's parent
    /// invocation must match the recorded one exactly, including its absence.
    pub fn matches_caller(
        &self,
        parent_storage_key: Option<&str>,
        parent_invocation_id: Option<&str>,
    ) -> bool {
        self.parent_storage_key.as_deref() == parent_storage_key
            && self.parent_invocation_id.as_deref() == parent_invocation_id
    }
}

fn joined_dispatch_key(storage: &str, id: &str) -> String {
    // Same opaque-id encoding as the admission binding: prompt ids are
    // transcript call ids, not KV-safe identifiers.
    let key = harnx_core::session_identity::session_key(Some(storage), id);
    format!("sessions/{storage}/joined_dispatches/{key}")
}

impl SessionMetadataStore {
    /// Per-prompt dispatch identity of a caller that joined an active turn
    /// instead of reserving a new invocation (issue #2672).
    pub async fn joined_dispatch(
        &self,
        storage: &str,
        prompt: &str,
    ) -> Result<Option<JoinedDispatch>> {
        let key = joined_dispatch_key(storage, prompt);
        let bytes = self.leader_value(&key).await?;
        bytes
            .map(|bytes| {
                let record: JoinedDispatch =
                    serde_json::from_slice(&bytes).context("invalid joined dispatch record")?;
                Ok(record)
            })
            .transpose()
    }

    /// Create-only join record. On conflict the existing record is returned so
    /// the caller replays through the joined path instead of double-joining.
    pub async fn create_joined_dispatch(
        &self,
        storage: &str,
        prompt: &str,
        record: &JoinedDispatch,
    ) -> Result<JoinedDispatch> {
        ensure!(
            record.parent_storage_key.is_some(),
            "joined dispatch requires the caller's trusted parent scope"
        );
        let key = joined_dispatch_key(storage, prompt);
        let payload = serde_json::to_vec(record)?;
        if self
            .kv_store()
            .update(&key, payload.into(), 0)
            .await
            .is_err()
        {
            let stored = self
                .joined_dispatch(storage, prompt)
                .await?
                .context("joined dispatch create unconfirmed")?;
            return Ok(stored);
        }
        Ok(record.clone())
    }

    /// Hand a proven-void join to one writer (issue #2672, review F1).
    ///
    /// The caller has proven on fresh entries that the dead record's turn is
    /// terminal and the recorded message was never appended — a fact the
    /// append backstop keeps stable, since nothing can be appended behind a
    /// terminal row. The record is revision-CAS rewritten to name
    /// `release.target` only while it still equals `dead`, so exactly one
    /// retry of the same call id wins; the loser yields (`false`) and re-reads
    /// fresh state.
    pub async fn retarget_joined_dispatch(
        &self,
        release: VoidJoinRelease<'_>,
        dead: &JoinedDispatch,
    ) -> Result<bool> {
        ensure!(
            dead.invocation_id == release.dead_invocation,
            "joined dispatch release names a different dead invocation"
        );
        let key = joined_dispatch_key(release.storage, release.prompt);
        let redirected = JoinedDispatch {
            invocation_id: release.target.to_owned(),
            ..dead.clone()
        };
        for _ in 0..16 {
            let Some(entry) = self.leader_entry(&key).await? else {
                return Ok(false);
            };
            if entry.operation != kv::Operation::Put {
                // A delete/purge marker cannot be a join record; nothing to
                // retarget (review round 2, C3, matching the sibling
                // `redirect_prompt_admission` guard).
                return Ok(false);
            }
            let stored = serde_json::from_slice::<JoinedDispatch>(&entry.value)
                .context("invalid joined dispatch record")?;
            if stored == redirected {
                return Ok(true);
            }
            if stored != *dead {
                // A concurrent retry already moved the record; it owns the
                // binding move too.
                return Ok(false);
            }
            let payload = serde_json::to_vec(&redirected)?;
            if harnx_nats_common::cas::update(
                self.kv_store(),
                key.clone(),
                payload.into(),
                entry.revision,
            )
            .await
            .is_ok()
            {
                return Ok(true);
            }
        }
        anyhow::bail!("session busy: joined dispatch release kept changing")
    }

    /// Move a prompt's admission binding off an invocation that provably ended
    /// before the prompt's message was appended (issue #2672, review F1).
    ///
    /// The binding is revision-CAS replaced only while it still names
    /// `release.dead_invocation`, so a concurrent retry's redirect is never
    /// clobbered, and a binding that names a live invocation is never
    /// touched. Returns the invocation the binding names now — the target on
    /// success, or a different invocation a concurrent retry of the same call
    /// already moved it to, which is equally valid for delivery.
    pub async fn redirect_prompt_admission(&self, release: VoidJoinRelease<'_>) -> Result<String> {
        let key = prompt_key(release.storage, release.prompt);
        let target = release.target;
        let dead = release.dead_invocation;
        for _ in 0..16 {
            let current = match self.leader_entry(&key).await? {
                // Never bound: the delivery path's create-only bind will
                // write the target (create-only writers cannot re-create a
                // key after a delete marker, which is why this repair
                // redirects instead of deleting).
                None => return Ok(target.to_owned()),
                Some(entry) => entry,
            };
            if current.operation != kv::Operation::Put {
                return Ok(target.to_owned());
            }
            let named = std::str::from_utf8(&current.value)
                .with_context(|| "invalid prompt admission binding")?
                .to_owned();
            if named == target {
                return Ok(named);
            }
            if named != dead {
                // A concurrent retry of the same dispatch moved it first;
                // deliver under its target rather than clobbering a live one.
                return Ok(named);
            }
            if harnx_nats_common::cas::update(
                self.kv_store(),
                key.clone(),
                target.to_owned().into(),
                current.revision,
            )
            .await
            .is_ok()
            {
                return Ok(target.to_owned());
            }
        }
        anyhow::bail!("session busy: prompt admission binding kept changing")
    }

    /// Supersede a never-delivered candidate admission's prompt content with
    /// the joined record's authoritative content (issue #2672, CodeRabbit I3).
    /// The candidate predates the join and the void proof guarantees the
    /// prompt's row is absent, so nothing ever referenced the old content.
    /// Only the content field changes — identity, limits and admitted-at stay
    /// frozen — and a fixed ticket refuses, keeping its immutable content
    /// contract. Revision CAS: a concurrent writer is never clobbered.
    pub async fn recontent_candidate_admission(
        &self,
        storage: &str,
        prompt: &str,
        content: &harnx_core::message::MessageContent,
    ) -> Result<()> {
        let key = intent_key(storage, prompt);
        for _ in 0..16 {
            let Some(entry) = self.leader_entry(&key).await? else {
                // Nothing stored yet; the create-only reserve writes the
                // joined content itself.
                return Ok(());
            };
            if entry.operation != kv::Operation::Put {
                // A purged candidate cannot be repaired here; the reserve
                // surfaces its create-unconfirmed refusal.
                return Ok(());
            }
            let stored = serde_json::from_slice::<InvocationAdmission>(&entry.value)
                .context("invalid dispatch admission record")?;
            ensure!(
                stored.fixed_ticket.is_none(),
                "leftover dispatch admission content disagrees with the joined record"
            );
            if stored.prompt_content.as_ref() == Some(content) {
                return Ok(());
            }
            let mut updated = stored.clone();
            updated.prompt_content = Some(content.clone());
            let payload = serde_json::to_vec(&updated)?;
            if harnx_nats_common::cas::update(
                self.kv_store(),
                key.clone(),
                payload.into(),
                entry.revision,
            )
            .await
            .is_ok()
            {
                return Ok(());
            }
        }
        anyhow::bail!("dispatch admission content kept changing")
    }
}
