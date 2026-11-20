//! Shared header specifications and extraction for user IDs and memberships.

use super::IdentityError;
use anyhow::{bail, Context};
use http::{HeaderMap, HeaderName};
use std::str::FromStr;

mod list;

/// A trusted header and its explicit comma-handling policy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeaderSpec {
    name: HeaderName,
    csv: bool,
}

pub(super) enum HeaderFields {
    First,
    All,
}

impl HeaderSpec {
    pub fn name(&self) -> &HeaderName {
        &self.name
    }

    /// User IDs use the first field; memberships use all repeated fields.
    /// Keep empty items so a CSV user ID can't skip an empty first item.
    pub(super) fn values(
        &self,
        headers: &HeaderMap,
        fields: HeaderFields,
    ) -> Result<Vec<String>, IdentityError> {
        let limit = match fields {
            HeaderFields::First => 1,
            HeaderFields::All => usize::MAX,
        };
        let fields = headers
            .get_all(&self.name)
            .iter()
            .take(limit)
            .map(|field| field.to_str())
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| IdentityError::InvalidValue)?;
        if self.csv {
            list::parse_header_list(&fields.join(","))
        } else {
            Ok(fields
                .into_iter()
                .map(|value| value.trim().to_owned())
                .collect())
        }
    }
}

impl FromStr for HeaderSpec {
    type Err = anyhow::Error;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let (name, csv) = match raw.split_once(':') {
            Some((prefix, name)) if prefix.eq_ignore_ascii_case("header") => (name, false),
            Some((prefix, name)) if prefix.eq_ignore_ascii_case("header-csv") => (name, true),
            Some(_) => {
                bail!("invalid header source '{raw}': expected header:NAME or header-csv:NAME")
            }
            None => (raw, false),
        };
        let name = HeaderName::from_bytes(name.as_bytes())
            .with_context(|| format!("invalid header source name '{name}'"))?;
        Ok(Self { name, csv })
    }
}
