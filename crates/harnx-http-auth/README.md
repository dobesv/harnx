# harnx-http-auth

Use `ClientCredentialsProvider` for a service account's OAuth 2.0
`client_credentials` grant. It reads the secret from a file, exchanges it
only when a token is needed, and shares one in-flight exchange among callers.
There is no discovery, interactive login, credential store, or resource
request replay in this crate.

## Sending a request

Construct the provider on a blocking thread after validating the configured
token endpoint. Pass a dedicated token client that has no API credentials.
`tls::credentialed_client_builder` applies the CA and optional client identity,
and disables redirects and ambient proxies; use the same TLS settings for the
resource client. Configure destination checks and client retry policies before
building either client. Do not reuse these credentials or the client identity
for unrelated destinations such as a remote specification host.

- Call `provider.token().await?` before sending. Keep the returned `AccessToken`
  with the request; stamp its sensitive `authorization_header()` on the request.
- On an explicit 401 that must not replay, call `provider.reject(&sent)` and
  return the failure. `reject` retires only the exact generation sent, even
  when a later exchange returned the same bearer value. It never does I/O.
- If policy permits a single replay, use `provider.renew_rejected(&sent).await?`
  to get its credential. A newer token can be reused until actual expiry.
  The caller sends the replay and can reject its token if that request also
  returns 401, without requesting another replay.

Use `provider.redact` for echoes of the OAuth client secret and the current or
recently retired tokens (four retired generations). Use
`redact::redact_secret_variants` for other configured credentials. Apply these
to decoded strings before serializing structured results. Neither helper is a
substitute for removing sensitive URL components and response headers or for
keeping raw network errors and dependency traces out of diagnostics.

`net` supplies typed URL/literal checks, address classification, and DNS answer
checks. These are building blocks, not a destination policy. A guarded resolver
does not see IP literal URLs. A `Private` classification alone doesn't permit
connecting to an address; consumers must enforce destination-scoped permission
and exclude metadata addresses. MCP keeps its existing policy and resend rules.
