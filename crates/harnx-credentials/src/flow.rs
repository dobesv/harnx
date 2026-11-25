//! Encrypted, restart-safe connect flows. Claim before exchanging a code or saving tokens.

use async_nats::jetstream;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use chrono::{TimeDelta, Utc};
use ring::{
    digest,
    rand::{SecureRandom, SystemRandom},
};

use std::{sync::Arc, time::Duration};

use crate::{
    backend::{self, EncryptedBucket},
    model::validate_server,
    Error, Keyring, Principal, Result, Secret, Snapshot, Versioned,
};

pub const FLOWS_BUCKET: &str = "harnx_tool_oauth_states";
pub const FLOW_TTL: Duration = Duration::from_secs(10 * 60);

mod types;
pub use types::{FlowOptions, FlowOutcome, FlowStatus, OAuthContext, PendingFlow};

const RANDOM_SECRET_BYTES: usize = 32;

#[derive(Clone, Debug)]
pub struct PendingFlowStore {
    bucket: EncryptedBucket,
    server: String,
}

impl PendingFlowStore {
    pub async fn open(
        js: &jetstream::Context,
        replicas: usize,
        keys: Arc<Keyring>,
        server: impl Into<String>,
    ) -> Result<Self> {
        let server = server.into();
        validate_server(&server)?;
        let kv = backend::ensure_bucket(js, FLOWS_BUCKET, FLOW_TTL, replicas).await?;
        Ok(Self {
            bucket: EncryptedBucket { kv, keys },
            server,
        })
    }

    /// Persist both records before returning a usable challenge. Failure never returns a link.
    pub async fn create(
        &self,
        owner: Principal,
        options: FlowOptions,
    ) -> Result<Versioned<PendingFlow>> {
        let expires_at = Utc::now() + TimeDelta::seconds(FLOW_TTL.as_secs() as i64);
        self.create_with_expiry(owner, options, expires_at).await
    }

    /// Test hook for the connect-page expiry gate: create a flow with an
    /// explicit absolute expiry. Production flows always use the 10-minute TTL
    /// via [`Self::create`]; only integration tests drive a shorter or past
    /// expiry, so an expired link is testable without waiting.
    #[doc(hidden)]
    pub async fn create_with_expiry(
        &self,
        owner: Principal,
        options: FlowOptions,
        expires_at: chrono::DateTime<Utc>,
    ) -> Result<Versioned<PendingFlow>> {
        owner.validate()?;
        let now = Utc::now();
        let state = options.authorization_state.as_ref().map(Secret::expose);
        let oauth_state = match state {
            Some(state) => Secret::from(state.csrf_token.clone()),
            None => random_secret()?,
        };
        let flow = PendingFlow {
            owner,
            server: self.server.clone(),
            challenge_id: random_secret()?,
            created_at: now,
            expires_at,
            status: FlowStatus::Pending,
            outcome: None,
            csrf_token: random_secret()?,
            oauth_state,
            pkce_verifier: state.map(|state| Secret::from(state.pkce_verifier.clone())),
            redirect_uri: options.redirect_uri,
            scopes: options.scopes,
            oauth_client: options.oauth_client,
            oauth_context: options.oauth_context,
            authorization_state: options.authorization_state,
        };
        let key = challenge_key(flow.challenge_id.expose());
        let revision = self.bucket.write(&key, &flow, 0).await?;
        if let Err(error) = backend::update(
            &self.bucket.kv,
            &state_key(flow.oauth_state.expose()),
            key.clone().into_bytes(),
            0,
        )
        .await
        {
            let _ = backend::delete(&self.bucket.kv, &key, revision).await;
            return Err(error);
        }
        Ok(Versioned {
            revision,
            record: flow,
        })
    }

    pub async fn get_by_challenge(
        &self,
        challenge: &str,
    ) -> Result<Option<Versioned<PendingFlow>>> {
        let key = challenge_key(challenge);
        let flow = self.read(&key).await?;
        if let Some(flow) = &flow {
            if flow.record.challenge_id.expose() != challenge {
                return Err(Error::InvalidRecord);
            }
        }
        Ok(flow)
    }

    /// The index contains only a hashed-flow key; compare the decrypted state before use.
    pub async fn get_by_state(&self, state: &str) -> Result<Option<Versioned<PendingFlow>>> {
        let Some(index) = backend::entry(&self.bucket.kv, &state_key(state)).await? else {
            return Ok(None);
        };
        if index.operation != jetstream::kv::Operation::Put {
            return Ok(None);
        }
        let key = std::str::from_utf8(&index.value).map_err(|_| Error::InvalidRecord)?;
        if !valid_flow_key(key) {
            return Err(Error::InvalidRecord);
        }
        let flow = self.read(key).await?;
        if let Some(flow) = &flow {
            if flow.record.oauth_state.expose() != state {
                return Err(Error::AuthenticationFailed);
            }
        }
        Ok(flow)
    }

    /// Exactly one CAS winner may perform the completing route's side effects.
    pub async fn claim(&self, challenge: &str, revision: u64) -> Result<Versioned<PendingFlow>> {
        self.transition(
            challenge,
            revision,
            FlowStatus::Pending,
            FlowStatus::Claimed,
            None,
        )
        .await
    }

    pub async fn complete(
        &self,
        challenge: &str,
        revision: u64,
        connection_id: String,
    ) -> Result<Versioned<PendingFlow>> {
        if connection_id.is_empty() {
            return Err(Error::InvalidRecord);
        }
        self.transition(
            challenge,
            revision,
            FlowStatus::Claimed,
            FlowStatus::Completed,
            Some(FlowOutcome::Connected { connection_id }),
        )
        .await
    }

    /// Pass only a safe, user-facing message, never a raw provider response.
    pub async fn fail(
        &self,
        challenge: &str,
        revision: u64,
        message: String,
    ) -> Result<Versioned<PendingFlow>> {
        self.transition(
            challenge,
            revision,
            FlowStatus::Claimed,
            FlowStatus::Failed,
            Some(FlowOutcome::Failed {
                error_message: Secret::new(message),
            }),
        )
        .await
    }

    /// Non-pending flows are returned unchanged, including a flow completed during Cancel.
    pub async fn cancel(&self, challenge: &str, revision: u64) -> Result<Versioned<PendingFlow>> {
        let flow = self
            .get_by_challenge(challenge)
            .await?
            .ok_or(Error::NotFound)?;
        if flow.record.status != FlowStatus::Pending {
            return Ok(flow);
        }
        self.transition(
            challenge,
            revision,
            FlowStatus::Pending,
            FlowStatus::Cancelled,
            None,
        )
        .await
    }

    pub(crate) async fn save_authorization_state(
        &self,
        challenge: &str,
        state: rmcp::transport::auth::StoredAuthorizationState,
    ) -> Result<()> {
        let mut flow = self
            .get_by_challenge(challenge)
            .await?
            .ok_or(Error::NotFound)?;
        flow.record.validate_authorization_state(&state)?;
        flow.record.pkce_verifier = Some(Secret::from(state.pkce_verifier.clone()));
        flow.record.authorization_state = Some(Secret::new(state));
        self.bucket
            .write(&challenge_key(challenge), &flow.record, flow.revision)
            .await?;
        Ok(())
    }

    pub(crate) async fn delete_authorization_state(&self, challenge: &str) -> Result<()> {
        let Some(mut flow) = self.get_by_challenge(challenge).await? else {
            return Ok(());
        };
        flow.record.authorization_state = None;
        flow.record.pkce_verifier = None;
        self.bucket
            .write(&challenge_key(challenge), &flow.record, flow.revision)
            .await?;
        Ok(())
    }

    async fn read(&self, key: &str) -> Result<Option<Versioned<PendingFlow>>> {
        let snapshot: Snapshot<PendingFlow> = self.bucket.read(key).await?;
        if let Some(flow) = &snapshot.record {
            if flow.server != self.server || challenge_key(flow.challenge_id.expose()) != key {
                return Err(Error::InvalidRecord);
            }
            if flow.expires_at <= Utc::now() {
                return Err(Error::Expired);
            }
        }
        Ok(snapshot.into_versioned())
    }

    async fn transition(
        &self,
        challenge: &str,
        revision: u64,
        from: FlowStatus,
        to: FlowStatus,
        outcome: Option<FlowOutcome>,
    ) -> Result<Versioned<PendingFlow>> {
        let mut flow = self
            .get_by_challenge(challenge)
            .await?
            .ok_or(Error::NotFound)?;
        if flow.revision != revision {
            return Err(Error::Conflict);
        }
        if flow.record.status != from {
            return Err(Error::InvalidTransition);
        }
        flow.record.status = to;
        flow.record.outcome = outcome;
        let revision = self
            .bucket
            .write(&challenge_key(challenge), &flow.record, revision)
            .await?;
        Ok(Versioned {
            revision,
            record: flow.record,
        })
    }
}

fn random_secret() -> Result<Secret<String>> {
    let mut bytes = [0; RANDOM_SECRET_BYTES];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| Error::Crypto)?;
    Ok(Secret::new(URL_SAFE_NO_PAD.encode(bytes)))
}

fn hash(value: &str) -> String {
    digest::digest(&digest::SHA256, value.as_bytes())
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub fn challenge_key(challenge: &str) -> String {
    format!("v1.flow.{}", hash(challenge))
}
pub fn state_key(state: &str) -> String {
    format!("v1.state.{}", hash(state))
}

fn valid_flow_key(key: &str) -> bool {
    key.strip_prefix("v1.flow.")
        .is_some_and(|hash| hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()))
}
