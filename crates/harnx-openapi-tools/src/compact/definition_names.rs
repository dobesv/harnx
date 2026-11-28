use std::collections::HashSet;

use crate::error::{SpecError, SpecResult};

use super::short_hash;

const MAX_DISAMBIGUATION_ATTEMPTS: usize = 256;

pub(super) fn allocate_def_name(
    reference: &str,
    occupied: &mut HashSet<String>,
    context: &str,
) -> SpecResult<String> {
    let base = reference
        .rsplit('/')
        .next()
        .map(sanitize_def_name)
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "def".to_string());
    if occupied.insert(base.clone()) {
        return Ok(base);
    }

    let hash = short_hash(reference);
    for attempt in 0..MAX_DISAMBIGUATION_ATTEMPTS {
        let candidate = if attempt == 0 {
            format!("{base}_{hash}")
        } else {
            format!("{base}_{hash}_{attempt}")
        };
        if occupied.insert(candidate.clone()) {
            return Ok(candidate);
        }
    }

    Err(SpecError::unsupported(
        context,
        format!(
            "could not allocate a unique definition name for {reference:?} after \
             {MAX_DISAMBIGUATION_ATTEMPTS} deterministic suffix candidates"
        ),
    ))
}

fn sanitize_def_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}
