//! Explicit `header-csv:` lists. Opaque `header:` values never use this parser.

use super::IdentityError;

/// Repeated fields must be comma-joined before parsing so field boundaries don't
/// change quoting or escaping. Empty items are retained for fail-closed user IDs.
pub(super) fn parse_header_list(mut input: &str) -> Result<Vec<String>, IdentityError> {
    let mut values = Vec::new();
    loop {
        let item = input.trim_start();
        let (value, remaining) = match item.strip_prefix('"') {
            Some(quoted) => quoted_item(quoted)?,
            None => unquoted_item(item)?,
        };
        values.push(value);
        match remaining {
            Some(rest) => input = rest,
            None => return Ok(values),
        }
    }
}

fn quoted_item(input: &str) -> Result<(String, Option<&str>), IdentityError> {
    let mut value = String::new();
    let mut chars = input.char_indices().peekable();
    while let Some((index, ch)) = chars.next() {
        match ch {
            '"' => {
                let remaining = input[index + 1..].trim_start();
                return match remaining.strip_prefix(',') {
                    Some(rest) => Ok((value, Some(rest))),
                    None if remaining.is_empty() => Ok((value, None)),
                    _ => Err(IdentityError::InvalidValue),
                };
            }
            '\\' if matches!(chars.peek(), Some((_, '"' | '\\'))) => {
                // Only quote and backslash are decoded. DN escapes stay literal.
                if let Some((_, escaped)) = chars.next() {
                    value.push(escaped);
                }
            }
            _ => value.push(ch),
        }
    }
    Err(IdentityError::InvalidValue)
}

fn unquoted_item(input: &str) -> Result<(String, Option<&str>), IdentityError> {
    let mut chars = input.char_indices();
    while let Some((index, ch)) = chars.next() {
        match ch {
            '\\' => {
                // Keep both bytes, but an escaped comma isn't a list separator.
                chars.next();
            }
            ',' => return Ok((input[..index].trim().to_owned(), Some(&input[index + 1..]))),
            '"' => return Err(IdentityError::InvalidValue),
            _ => {}
        }
    }
    Ok((input.trim().to_owned(), None))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoted_dns_and_plain_items_can_share_a_list() {
        harnx_core::require_nextest();
        assert_eq!(
            parse_header_list(r#""CN=admins,OU=eng,O=Acme", "CN=ops,O=Acme", plain-group"#)
                .unwrap(),
            ["CN=admins,OU=eng,O=Acme", "CN=ops,O=Acme", "plain-group"]
        );
    }

    #[test]
    fn quoted_escapes_decode_only_quote_and_backslash() {
        harnx_core::require_nextest();
        assert_eq!(
            parse_header_list(r#""CN=the \"admins\",O=Acme", "CN=Smith\, Alice,O=Acme", "path\\group", "literal\x""#).unwrap(),
            [r#"CN=the "admins",O=Acme"#, r"CN=Smith\, Alice,O=Acme", r"path\group", r"literal\x"]
        );
    }

    #[test]
    fn unquoted_escaped_commas_and_backslashes_stay_literal() {
        harnx_core::require_nextest();
        assert_eq!(
            parse_header_list(r"CN=admins\,OU=eng\,O=Acme, ops, path\\, final\").unwrap(),
            [r"CN=admins\,OU=eng\,O=Acme", "ops", r"path\\", r"final\"]
        );
    }

    #[test]
    fn whitespace_empty_items_order_and_duplicates_are_preserved() {
        harnx_core::require_nextest();
        assert_eq!(
            parse_header_list(" , \t plain \t, \" spaced \" \t, \"\", plain,, ").unwrap(),
            ["", "plain", " spaced ", "", "plain", "", ""]
        );
        for empty in ["", " \t ", ",,", " , \"\", ,"] {
            assert!(parse_header_list(empty)
                .unwrap()
                .iter()
                .all(String::is_empty));
        }
    }

    #[test]
    fn malformed_quotes_reject_the_whole_list() {
        harnx_core::require_nextest();
        for input in [
            "valid, \"unterminated",
            "\"dangling\\",
            r#""escaped closing quote\""#,
            r#""closed"junk, later"#,
            r#""closed" "extra", later"#,
            r#"unquoted"quote, later"#,
        ] {
            assert_eq!(parse_header_list(input), Err(IdentityError::InvalidValue));
        }
    }
}
