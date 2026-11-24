//! NATS URL-credential redaction for log lines: error chains and cluster
//! addresses echo configured URLs verbatim, and those can carry userinfo
//! (`nats://user:pass@host`), schemeless credentials (`user:pass@host`) or
//! query-string tokens (`?token=...`) on schemed and schemeless endpoints.

/// Redact secrets from every URL or bare endpoint in `text`: the userinfo
/// of `scheme://user:pass@host` (including comma-separated multi-URL lists),
/// the credentials of a schemeless `user:pass@host`, and any query string
/// (`?token=...` can carry NATS auth tokens). Non-credential text and URLs
/// without credentials pass through unchanged.
pub(crate) fn redact_url_credentials(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut rest = text;
    while let Some((position, delimiter)) = next_redaction_split(rest) {
        result.push_str(&redact_token_credentials(&rest[..position]));
        result.push(delimiter);
        rest = &rest[position + delimiter.len_utf8()..];
    }
    result.push_str(&redact_token_credentials(rest));
    result
}

/// The next delimiter to split on. Whitespace always separates URL-shaped
/// tokens. A comma does only when a `scheme://` URL follows it: commas
/// occur inside credentials (`nats://u:sample,tail@host`) and query
/// strings (`?auth_token=sample,tail`), and splitting there would hand
/// the tail after the comma to the redactor as a token of its own —
/// unmasked, or with only the `?`-prefix masked (R3-1).
fn next_redaction_split(text: &str) -> Option<(usize, char)> {
    text.char_indices().find(|&(position, delimiter)| {
        if delimiter.is_whitespace() {
            return true;
        }
        delimiter == ',' && starts_scheme_url(&text[position + 1..])
    })
}

/// Whether `text` starts a `scheme://` URL (RFC 3986 first/`+`/`-`/`.`).
fn starts_scheme_url(text: &str) -> bool {
    let bytes = text.as_bytes();
    if bytes.is_empty() || !bytes[0].is_ascii_alphabetic() {
        return false;
    }
    let Some(colon) = bytes
        .iter()
        .position(|&b| !b.is_ascii_alphanumeric() && !matches!(b, b'+' | b'-' | b'.'))
    else {
        return false;
    };
    bytes[colon] == b':'
        && bytes.get(colon + 1) == Some(&b'/')
        && bytes.get(colon + 2) == Some(&b'/')
}

/// Redact one whitespace/comma-delimited token. Tokens are how credential
/// material actually appears in chains: inside one URL, in comma-separated
/// server lists, or bare after a scheme was dropped by a context message.
fn redact_token_credentials(token: &str) -> String {
    let Some(scheme_end) = token.find("://") else {
        // Schemeless endpoint: redact a userinfo-shaped prefix (it contains a
        // colon, so a plain email address stays untouched) and, for
        // endpoint-shaped text, any query string — `?token=...` can carry
        // the NATS auth token also on an address without a scheme.
        return redact_schemeless_token(token);
    };
    let mut redacted = String::with_capacity(token.len());
    redacted.push_str(&token[..scheme_end + "://".len()]);
    let authority_end = token[scheme_end + "://".len()..]
        .find(['/', '?'])
        .map(|offset| scheme_end + "://".len() + offset)
        .unwrap_or(token.len());
    let authority = &token[scheme_end + "://".len()..authority_end];
    if let Some(at) = authority.rfind('@') {
        redacted.push_str("***@");
        redacted.push_str(&authority[at + 1..]);
    } else {
        redacted.push_str(authority);
    }
    // A query string can carry the NATS auth token: redact it whole, keeping
    // only its presence visible.
    match token[authority_end..].find('?') {
        Some(query_start) => {
            redacted.push_str(&token[authority_end..authority_end + query_start + 1]);
            redacted.push_str("***");
        }
        None => redacted.push_str(&token[authority_end..]),
    }
    redacted
}

/// Redact a token without a `scheme://`: a userinfo-shaped prefix, and a
/// query string when the text before it is endpoint-shaped. Ordinary prose
/// containing a question mark passes through unchanged.
fn redact_schemeless_token(token: &str) -> String {
    let query = token
        .find('?')
        .filter(|&position| endpoint_shaped(&token[..position]));
    let (body, has_query) = match query {
        Some(position) => (&token[..position], true),
        None => (token, false),
    };
    let mut redacted = body.to_string();
    if let Some(at) = body.rfind('@') {
        let credentials = &body[..at];
        if credentials.contains(':') && !credentials.contains('/') {
            redacted = format!("***@{}", &body[at + 1..]);
        }
    }
    if has_query {
        redacted.push('?');
        redacted.push_str("***");
    }
    redacted
}

/// Whether schemeless text before a `?` is an endpoint: credentials or a
/// user are attached with `@`, or it ends in a `host:port` — a colon whose
/// tail is only digits, or digits then a path (`host:port/path?token=...`
/// carries the token the same way).
fn endpoint_shaped(before_query: &str) -> bool {
    before_query.contains('@')
        || before_query
            .rfind(':')
            .filter(|&position| !before_query[position + 1..].is_empty())
            .is_some_and(|position| {
                let tail = &before_query[position + 1..];
                let digits = tail
                    .bytes()
                    .take_while(|byte| byte.is_ascii_digit())
                    .count();
                digits > 0
                    && (digits == tail.len()
                        || (tail.as_bytes()[digits] == b'/' && digits + 1 < tail.len()))
            })
}
