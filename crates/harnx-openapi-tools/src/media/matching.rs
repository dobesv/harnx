//! Shared media predicates for runtime selection and JSON Schema.

use serde_json::{json, Value};

use super::MediaKind;

const TOKEN: &str = "[a-z0-9!#$&^_.+-]";
// Unlike `$`, this cannot match before a final line break in any regex dialect.
const END: &str = r"(?![\s\S])";
const JSON: &str = "application/json";

pub(super) fn is_token(token: &str) -> bool {
    !token.is_empty()
        && token.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"!#$&^_.+-".contains(&byte)
        })
}

pub(super) fn is_concrete(media: &str) -> bool {
    media
        .split_once('/')
        .is_some_and(|(ty, sub)| is_token(ty) && is_token(sub))
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum MediaMatch<'a> {
    Any,
    Type(&'a str),
    Exact(&'a str),
}

impl<'a> MediaMatch<'a> {
    pub(crate) fn new(media: &'a str) -> Self {
        if media == "*/*" {
            Self::Any
        } else if let Some(ty) = media.strip_suffix("/*") {
            Self::Type(ty)
        } else {
            Self::Exact(media)
        }
    }

    pub(crate) fn precedence(self) -> u8 {
        match self {
            Self::Any => 0,
            Self::Type(_) => 1,
            Self::Exact(_) => 2,
        }
    }

    pub(crate) fn covers(self, concrete: &str) -> bool {
        match self {
            Self::Any => true,
            Self::Type(ty) => concrete.split_once('/').is_some_and(|(top, _)| top == ty),
            Self::Exact(exact) => concrete == exact,
        }
    }

    pub(crate) fn shadows(self, other: Self) -> bool {
        match self {
            Self::Exact(concrete) => {
                other.precedence() < self.precedence() && other.covers(concrete)
            }
            Self::Type(_) => other == Self::Any,
            Self::Any => false,
        }
    }

    pub(crate) fn schema(self) -> Value {
        match self {
            Self::Any => pattern(format!("{TOKEN}+/{TOKEN}+")),
            Self::Type(ty) => pattern(format!("{}/{TOKEN}+", escape_token(ty))),
            Self::Exact(ty) => json!({"const": ty}),
        }
    }
}

fn escape_token(token: &str) -> String {
    token
        .replace('.', "\\.")
        .replace('+', "\\+")
        .replace('^', "\\^")
        .replace('$', "\\$")
}

fn pattern(body: String) -> Value {
    json!({"pattern": format!("^{body}{END}")})
}

#[derive(Clone, Copy)]
enum EncoderMatch {
    Exact(&'static str),
    Json,
    Type(&'static str),
    Any,
}

impl EncoderMatch {
    fn covers(self, media: &str) -> bool {
        match self {
            Self::Exact(exact) => exact == media,
            Self::Json => media == JSON || media.ends_with("+json"),
            Self::Type(ty) => MediaMatch::Type(ty).covers(media),
            Self::Any => true,
        }
    }

    fn domain(self, range: MediaMatch<'_>, shadowed: &[MediaMatch<'_>]) -> Option<Value> {
        match self {
            Self::Exact(media) => {
                let visible = range.covers(media) && !shadowed.iter().any(|key| key.covers(media));
                visible.then(|| MediaMatch::Exact(media).schema())
            }
            Self::Json => Some(json!({"anyOf": [
                MediaMatch::Exact(JSON).schema(),
                pattern(format!(r"{TOKEN}+/{TOKEN}*\+json")),
            ]})),
            Self::Type(ty) => {
                let matched = MediaMatch::Type(ty);
                let visible =
                    (range == MediaMatch::Any || range == matched) && !shadowed.contains(&matched);
                visible.then(|| matched.schema())
            }
            Self::Any if matches!(range, MediaMatch::Type("text" | "multipart")) => None,
            Self::Any => Some(Value::Bool(true)),
        }
    }
}

// Earlier rules win, including +json suffixes on concrete types from a range.
const ENCODERS: &[(MediaKind, EncoderMatch)] = &[
    (
        MediaKind::JsonPatch,
        EncoderMatch::Exact("application/json-patch+json"),
    ),
    (MediaKind::Json, EncoderMatch::Json),
    (MediaKind::Text, EncoderMatch::Type("text")),
    (
        MediaKind::UrlencodedForm,
        EncoderMatch::Exact("application/x-www-form-urlencoded"),
    ),
    (MediaKind::Multipart, EncoderMatch::Type("multipart")),
    (MediaKind::Base64, EncoderMatch::Any),
];

pub(super) fn classify_concrete(media: &str) -> MediaKind {
    ENCODERS
        .iter()
        .find(|(_, rule)| rule.covers(media))
        .map_or(MediaKind::Base64, |(kind, _)| *kind)
}

pub(crate) fn encoding_domains<'a>(
    range: MediaMatch<'a>,
    shadowed: &'a [MediaMatch<'a>],
) -> impl Iterator<Item = (MediaKind, Value)> + 'a {
    let mut exclusions = Vec::new();
    ENCODERS.iter().filter_map(move |(kind, rule)| {
        let domain = rule.domain(range, shadowed)?;
        let selected = excluding(domain.clone(), &exclusions);
        exclusions.push(domain);
        Some((*kind, selected))
    })
}

pub(crate) fn excluding(domain: Value, exclusions: &[Value]) -> Value {
    if exclusions.is_empty() {
        domain
    } else {
        json!({"allOf": [domain, {"not": {"anyOf": exclusions}}]})
    }
}
