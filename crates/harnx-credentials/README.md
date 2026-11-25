# harnx-credentials

Encrypted per-principal tool credentials, durable connect flows, refresh leases and tool catalogs over NATS JetStream. This crate owns storage and fencing. It doesn't host HTTP routes, exchange OAuth codes, publish tool registrations or choose a request's principal.

## Key file

Generate a key file explicitly, before starting a tool server:

```sh
umask 077
cargo run -p harnx-credentials --bin harnx-credentials-keygen > credentials-key.json
```

`Keyring::load(path)` accepts JSON `{ "active": "kid", "keys": { "kid": "base64-encoded 32-byte key" } }`. Missing files and invalid keys are errors. Each server identity has its own keyring; replicas of that server share it. Keep older keys for decryption when changing `active`. Back up the file: losing every decrypting key requires users to reconnect. No startup code creates keys or derives them from NATS credentials.

Envelopes use `ring::aead` ChaCha20-Poly1305 and fresh random 96-bit nonces. AAD binds bucket, full KV key, version and key ID. Copying ciphertext to a different principal, server or bucket fails authentication. Errors contain no plaintext or provider response bodies.

## Credentials

Open `CredentialStore::open(&jetstream, replicas, Arc<Keyring>, server_identity, Principal)` once, then use `for_principal` to reuse that server's bucket/keyring. `Principal::User` takes the canonical user ID, not an already-encoded segment. `Principal::Ownerless` is distinct from every user ID.

- `inspect()` returns `Snapshot { revision, record }`, including expired/needs-login records and deletion tombstones. Use this only for management and CAS preparation.
- `get()` returns an active, unexpired `Versioned<CredentialRecord>`, `None` if absent, or typed `Expired` / `NeedsLogin` errors. Recheck on every upstream use, including cached clients.
- `put_connect(snapshot.revision, NewCredential)` assigns a fresh random `connection_id`. Default and maximum absolute lifetime is 365 days; `with_credential_ttl` changes it for future connects. A supplied earlier expiry is accepted; a later one is rejected.
- `put_refresh(revision, material)` preserves `connection_id`, creation time, absolute expiry and account label. Missing OAuth refresh tokens retain the previous token. Prefer the lease commit API below for provider exchanges.
- `mark_needs_login(revision)` and `delete(revision)` are CAS operations. Every stale writer gets `Error::Conflict`; reread and decide again. After deletion, use `inspect`'s tombstone revision to reconnect, not zero.
- `list_active_principals()` supports startup recovery for this server. `list_for_user(&principal)` resolves within this server/keyring scope, not across other servers' keyrings.

Read paths use `harnx_nats_common::leader_reads::retry_transient` rather than follower/direct gets. Buckets are created on demand with the supplied replica count and history 1. Existing incompatible history/TTL settings fail rather than silently changing live lease lifetimes.

## Refresh

`RefreshLeaseStore::open(&jetstream, replicas)` uses a dedicated 60-second-TTL bucket. `acquire(&credential_store)` returns `LeaseBusy` when another instance owns that principal's lease. It reads the credential revision **after** acquiring ownership. Use `lease.credential()` for the exchange, not a pre-acquisition snapshot.

Hold the lease across exchange and save. `lease.commit(new_material)` consumes it and saves only against the captured credential revision. Reconnect, disconnect or another credential mutation fences the result with `Conflict`. A lease that expired or changed returns `LeaseLost`. `mark_needs_login()` on the lease has the same fencing for definitive `invalid_grant` responses. Explicit `release()` is available; drop releases best-effort, with TTL handling process death. Leases aren't renewed. Bound provider calls within the lease lifetime, discard failed commit results and reread rather than retrying a token exchange blindly.

## Connect flows

`PendingFlowStore::open(&jetstream, replicas, keys, server_identity)` stores encrypted flow records plus hashed-state lookup entries. Both entries exist before `create(principal, FlowOptions)` returns. Challenges, CSRF tokens and generated OAuth state each use 32 random bytes. Absolute expiry and bucket TTL are ten minutes; updating a flow doesn't extend absolute expiry.

For OAuth, prepare rmcp's `StoredAuthorizationState` first, wrap it in `Secret`, and include it with the full `ClientRegistration` in `FlowOptions`. Its CSRF token becomes the flow's OAuth state. This lets another instance serve OAuth start and callback with the same state, PKCE verifier, issuer expectations and client registration. Use a cryptographically random state when constructing that rmcp value.

`get_by_challenge` and `get_by_state` return `Versioned<PendingFlow>`. Before `claim(challenge, revision)`, the HTTP caller must check trusted user identity and form CSRF where applicable. Exactly one pending-to-claimed CAS winner may exchange a code or write a credential. Then call `complete(challenge, current_revision, connection_id)` or `fail(challenge, current_revision, safe_message)`. Failure messages must not contain provider response bodies. `cancel` changes only pending flows; a completed/claimed/failed flow stays unchanged. CAS conflicts require rereading, not replaying side effects.

## Catalogs and derived state

`CatalogStore` stores non-secret `Catalog { connection_id, tools: Vec<rmcp::model::Tool> }` values per server/principal.

- `publish(&credentials, expected_catalog_revision, catalog)` checks the scoped credential is active, unexpired and has the catalog's connection ID, then CAS-writes. Use it for initial publication or reconnect replacement.
- `update(expected_revision, connection_id, tools)` requires both the existing catalog's connection ID and revision to match. A delayed notification from an old client can't replace a newer catalog.
- `delete_if_connection(connection_id)` rereads on CAS conflicts and leaves another connection's catalog intact.

Credential and catalog keys aren't one transaction. For connect completion, reread credential before every publication attempt; stop if superseded. Server-side authorization and registration refresh must still check the active credential and its connection ID. Disconnect/needs-login must mutate the credential first, then conditionally remove derived state. Registrations are outside this crate.

## rmcp 3.8 adapters

`rmcp_adapters::RmcpCredentialStore` implements rmcp's `CredentialStore` for an **existing** connection. Construct it with the credential handle, lease store and connection ID, and give it to that connection's `AuthorizationManager`. It rejects unguarded saves. rmcp's refresh path acquires `acquire_refresh_guard`, reloads, exchanges and saves under that guard. The adapter keeps the full client registration and provider-specific token fields in the encrypted record; `StoredCredentials` alone doesn't contain them. A new connection needs a new adapter.

Initial authorization-code acquisition is separate: use rmcp with temporary credential storage for the exchange, build full `OAuthMaterial`, CAS-write via `put_connect`, then install the connection-bound adapter for refresh. Don't send the initial code-exchange save through a refresh-only adapter. `to_rmcp_credentials` converts persisted OAuth tokens for manager setup without dropping stored registration data.

`RmcpStateStore::new(flows, challenge)` implements rmcp's `StateStore` for one flow. It refuses a different state's challenge. Saving validates the original state, PKCE verifier and issuer expectations; deleting removes only OAuth exchange state, not the flow's outcome. State adapter mutations advance the flow revision: reread before `complete` / `fail` after rmcp finishes callback processing.

`Secret<T>` redacts `Debug` and `Display`, but intentionally serializes plaintext for encrypted persistence. `expose()` is the explicit plaintext boundary. Never log serialized records, URLs containing challenges, authorization codes, tokens or provider error bodies. Catalogs are plaintext: don't put secrets into tool definitions.

## Storage keys

`S` is unpadded base64url of server identity; `U` is unpadded base64url of canonical user ID, or literal `=` for ownerless.

| Bucket | Key | Contents / TTL |
|---|---|---|
| `harnx_tool_credentials` | `v1.{S}.{U}` | encrypted credential, absolute expiry enforced on use |
| `harnx_tool_oauth_states` | `v1.flow.{sha256-hex(challenge)}` | encrypted flow, ten-minute TTL |
| `harnx_tool_oauth_states` | `v1.state.{sha256-hex(state)}` | flow key only, ten-minute TTL |
| `harnx_tool_credential_locks` | `lock.v1.{S}.{U}` | lease ownership, 60-second TTL |
| `harnx_tool_user_catalogs` | `catalog.v1.{S}.{U}` | non-secret catalog |

NATS credentials remain the deployment's trust boundary. Encryption protects stored secret material; this crate doesn't authenticate tool-request principals or grant per-component NATS permissions.

## Verification

```sh
NATS_SERVER_BIN=/home/dobes/.local/bin/nats-server PROTOC=/usr/bin/protoc cargo nextest run -p harnx-credentials
PROTOC=/usr/bin/protoc cargo clippy -p harnx-credentials --all-targets -- -D warnings
```

Tests use `harnx_test_bins::spawn_nats_server` to start isolated brokers. Missing `nats-server` fails these tests instead of silently skipping.
