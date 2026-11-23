//! Publish, refresh and delete user-scoped and ownerless-only registrations.
//!
//! A credentialed tool server publishes one global registration for its
//! management tools plus one registration per connected principal (decision 5
//! of the user-credentials plan). These extra registrations share the main
//! registration's 30 s refresh / 90 s TTL lifecycle, but they must never be
//! written blindly: a stale writer (a disconnected credential's left-behind
//! refresh loop) must not overwrite or delete a newer connection's
//! registration. Every write here is a CAS on a KV revision the caller read,
//! and every delete additionally checks the stored `connection_id`.

use crate::TOOL_REGISTRY_BUCKET;
use anyhow::{Context, Result};
use async_nats::jetstream::kv;
use harnx_core::instance::ServerScope;
use harnx_toolset::{
    server_identity_token, user_key_segment, Registration, REGISTRATION_OWNERLESS_SEGMENT,
    REGISTRATION_USER_SEGMENT,
};

/// How long an extra registration survives without a refresh; shared with the
/// main registration so all of one server's registrations expire together.
pub use harnx_nats_common::registry::REGISTRATION_TTL as REGISTRATION_HANDLE_TTL;

/// Which extra registration one handle addresses. The global registration is
/// owned by `serve_with_shutdown`'s refresh loop; a handle only manages the
/// per-principal additions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RegistrationAudience {
    /// A user-scoped registration for this user id, published under
    /// `{scope}.{identity_token}.u.{U}`.
    User(String),
    /// The ownerless-only registration (`--single-user` servers), published
    /// under `{scope}.{identity_token}.anon`.
    OwnerlessOnly,
}

/// One stored registration plus the KV revision it was read at, for CAS writes.
#[derive(Clone, Debug)]
pub struct StoredRegistration {
    pub registration: Registration,
    pub revision: u64,
}

/// Outcome of a fenced delete.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConditionalDelete {
    /// The registration was removed at the expected revision.
    Deleted,
    /// The stored registration's `connection_id` differs from the expected
    /// one, so a newer connection owns it and it was left in place.
    ConnectionMismatch,
}

#[derive(Debug)]
pub enum RegistrationHandleError {
    /// The CAS failed: the stored revision moved since the caller last read
    /// it, or the registration already exists. Re-read and decide again;
    /// never retry blindly.
    Conflict,
    /// The registration payload does not match the audience it publishes
    /// under (e.g. a user audience carrying an ownerless-only payload).
    Invalid(String),
    Other(anyhow::Error),
}

impl std::fmt::Display for RegistrationHandleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Conflict => {
                write!(f, "registration revision conflict; re-read and retry")
            }
            Self::Invalid(reason) => write!(f, "invalid registration: {reason}"),
            Self::Other(error) => write!(f, "{error:#}"),
        }
    }
}

impl std::error::Error for RegistrationHandleError {}

/// Publisher handle for one tool server's user-scoped and ownerless-only
/// registrations in the shared registry bucket.
#[derive(Clone)]
pub struct RegistrationHandle {
    registry: kv::Store,
    scope: ServerScope,
    identity_token: String,
}

impl RegistrationHandle {
    /// Build a handle over an already-open registry bucket.
    pub fn new(registry: kv::Store, scope: ServerScope, identity_token: String) -> Self {
        Self {
            registry,
            scope,
            identity_token,
        }
    }

    /// Open (creating on demand) the registry bucket and build a handle for
    /// one server identity. Use this from a toolset factory that received the
    /// hosting NATS client.
    pub async fn open(
        client: &async_nats::Client,
        replicas: usize,
        scope: ServerScope,
        identity: crate::RegistrationIdentity,
        server_name: &str,
    ) -> Result<Self> {
        // Same bucket and TTL `serve_with_shutdown` reconciles, so a server
        // publishing only extra registrations still heals a stale bucket.
        let jetstream = async_nats::jetstream::new(client.clone());
        let store = harnx_nats_common::registry::ensure_bucket_with_ttl(
            &jetstream,
            TOOL_REGISTRY_BUCKET,
            REGISTRATION_HANDLE_TTL,
            replicas,
        )
        .await?;
        let identity_token =
            server_identity_token(identity.package.as_deref(), &identity.config, server_name);
        Ok(Self::new(store, scope, identity_token))
    }

    /// The KV key one audience publishes under.
    pub fn key(&self, audience: &RegistrationAudience) -> String {
        match audience {
            RegistrationAudience::User(user_id) => format!(
                "{}.{identity_token}.{REGISTRATION_USER_SEGMENT}.{segment}",
                self.scope,
                identity_token = self.identity_token,
                segment = user_key_segment(user_id)
            ),
            RegistrationAudience::OwnerlessOnly => format!(
                "{}.{identity_token}.{REGISTRATION_OWNERLESS_SEGMENT}",
                self.scope,
                identity_token = self.identity_token
            ),
        }
    }

    /// Read the stored registration and its KV revision, for CAS writes.
    pub async fn read(
        &self,
        audience: &RegistrationAudience,
    ) -> Result<Option<StoredRegistration>, RegistrationHandleError> {
        let key = self.key(audience);
        let entry = self
            .registry
            .entry(&key)
            .await
            .with_context(|| format!("read registration '{key}'"))
            .map_err(RegistrationHandleError::Other)?;
        let Some(entry) = live_entry(entry) else {
            return Ok(None);
        };
        let registration = decode_registration(&key, &entry.value)?;
        Ok(Some(StoredRegistration {
            registration,
            revision: entry.revision,
        }))
    }

    /// Publish a registration that must not exist as a live value.
    ///
    /// An absent key (including one carrying only a deletion marker — a
    /// strict create stays blocked behind a marker until the bucket's TTL
    /// reaps it) is written with a CAS: absent keys go through a strict
    /// create, marker-only keys update at the marker's revision, so two
    /// racing publishers still resolve to exactly one winner. A live value
    /// fails with [`RegistrationHandleError::Conflict`]; the caller re-reads
    /// and uses [`Self::update`].
    pub async fn publish(
        &self,
        audience: &RegistrationAudience,
        registration: Registration,
    ) -> Result<u64, RegistrationHandleError> {
        let key = self.key(audience);
        self.validate(audience, &registration)?;
        let payload = encode_registration(&registration).map_err(RegistrationHandleError::Other)?;
        let entry = self
            .registry
            .entry(&key)
            .await
            .with_context(|| format!("read registration '{key}' before publish"))
            .map_err(RegistrationHandleError::Other)?;
        match live_entry(entry.clone()) {
            Some(_) => Err(RegistrationHandleError::Conflict),
            None => {
                // Absent or marker-only: a CAS create either way. `create` is
                // the strict form for a subject with no history at all; over a
                // marker, the update at the marker's revision is the CAS.
                let revision = match entry {
                    Some(marker) => self
                        .registry
                        .update(&key, payload.into(), marker.revision)
                        .await
                        .map_err(|error| match error.kind() {
                            kv::UpdateErrorKind::WrongLastRevision => {
                                RegistrationHandleError::Conflict
                            }
                            _ => RegistrationHandleError::Other(anyhow::Error::new(error)),
                        })?,
                    None => self
                        .registry
                        .create(&key, payload.into())
                        .await
                        .map_err(|error| match error.kind() {
                            kv::CreateErrorKind::AlreadyExists => RegistrationHandleError::Conflict,
                            _ => RegistrationHandleError::Other(anyhow::Error::new(error)),
                        })?,
                };
                Ok(revision)
            }
        }
    }

    /// CAS-update the registration at `expected_revision`; a refresh tick.
    /// A stale revision fails with [`RegistrationHandleError::Conflict`]:
    /// re-read, re-check the connection, decide again.
    pub async fn update(
        &self,
        audience: &RegistrationAudience,
        registration: Registration,
        expected_revision: u64,
    ) -> Result<u64, RegistrationHandleError> {
        let key = self.key(audience);
        self.validate(audience, &registration)?;
        let payload = encode_registration(&registration).map_err(RegistrationHandleError::Other)?;
        self.registry
            .update(&key, payload.into(), expected_revision)
            .await
            .map_err(|error| match error.kind() {
                kv::UpdateErrorKind::WrongLastRevision => RegistrationHandleError::Conflict,
                _ => RegistrationHandleError::Other(anyhow::Error::new(error)),
            })
    }

    /// Delete the registration, but only while the KV revision is still
    /// `expected_revision` and the stored payload's `connection_id` equals
    /// `expected_connection_id` (decision 5 fencing: a stale writer must not
    /// remove a newer connection's registration).
    ///
    /// The caller reads the current state first (via [`Self::read`]) and
    /// passes the revision it observed; the revision CAS keeps the
    /// `connection_id` check from racing a concurrent write.
    pub async fn delete(
        &self,
        audience: &RegistrationAudience,
        expected_revision: u64,
        expected_connection_id: &str,
    ) -> Result<ConditionalDelete, RegistrationHandleError> {
        let key = self.key(audience);
        let entry = self
            .registry
            .entry(&key)
            .await
            .with_context(|| format!("read registration '{key}' before delete"))
            .map_err(RegistrationHandleError::Other)?;
        let Some(entry) = live_entry(entry) else {
            // Already gone: the caller's goal is achieved, not a conflict.
            return Ok(ConditionalDelete::Deleted);
        };
        if entry.revision != expected_revision {
            return Err(RegistrationHandleError::Conflict);
        }
        let registration = decode_registration(&key, &entry.value)?;
        if registration.connection_id.as_deref() != Some(expected_connection_id) {
            return Ok(ConditionalDelete::ConnectionMismatch);
        }
        self.registry
            .delete_expect_revision(&key, Some(expected_revision))
            .await
            .map_err(|error| match error.kind() {
                kv::DeleteErrorKind::WrongLastRevision => RegistrationHandleError::Conflict,
                _ => RegistrationHandleError::Other(anyhow::Error::new(error)),
            })?;
        Ok(ConditionalDelete::Deleted)
    }

    fn validate(
        &self,
        audience: &RegistrationAudience,
        registration: &Registration,
    ) -> Result<(), RegistrationHandleError> {
        let token = server_identity_token(
            registration.package.as_deref(),
            &registration.config,
            &registration.server,
        );
        if token != self.identity_token {
            return Err(RegistrationHandleError::Invalid(format!(
                "registration identity '{token}' does not match the handle's '{}'",
                self.identity_token
            )));
        }
        let matches = match audience {
            RegistrationAudience::User(user_id) => {
                registration.user_id.as_deref() == Some(user_id.as_str())
                    && !registration.ownerless_only
            }
            RegistrationAudience::OwnerlessOnly => {
                registration.user_id.is_none() && registration.ownerless_only
            }
        };
        if !matches {
            return Err(RegistrationHandleError::Invalid(
                "registration payload does not match its audience".to_string(),
            ));
        }
        Ok(())
    }
}

/// The live value behind a raw entry, or `None` for absent and deleted keys.
fn live_entry(entry: Option<kv::Entry>) -> Option<kv::Entry> {
    entry.filter(|entry| entry.operation == kv::Operation::Put)
}

fn encode_registration(registration: &Registration) -> Result<Vec<u8>> {
    serde_json::to_vec(registration).context("encode tool registration")
}

fn decode_registration(key: &str, value: &[u8]) -> Result<Registration, RegistrationHandleError> {
    serde_json::from_slice(value)
        .with_context(|| format!("decode registration '{key}'"))
        .map_err(RegistrationHandleError::Other)
}
