use std::borrow::Cow;

/// Parsed agent selector from CLI/config surfaces.
///
/// `agent@cluster` selects a remote agent routed through cluster `cluster`.
/// Parsing splits once on the LAST `@`, so `team@agent@prod` becomes
/// `Remote { agent: "team@agent", cluster: "prod" }`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentRef<'a> {
    Local(Cow<'a, str>),
    Remote {
        agent: Cow<'a, str>,
        cluster: Cow<'a, str>,
    },
}

impl<'a> AgentRef<'a> {
    /// Parse raw `-a/--agent` value before any sanitization.
    pub fn parse(raw: &'a str) -> Self {
        match raw.rsplit_once('@') {
            Some((agent, cluster)) => Self::Remote {
                agent: Cow::Borrowed(agent),
                cluster: Cow::Borrowed(cluster),
            },
            None => Self::Local(Cow::Borrowed(raw)),
        }
    }

    pub fn local_name(&self) -> Option<&str> {
        match self {
            Self::Local(name) => Some(name.as_ref()),
            Self::Remote { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::AgentRef;

    #[test]
    fn parse_local_bare_name() {
        assert_eq!(AgentRef::parse("foo"), AgentRef::Local("foo".into()));
    }

    #[test]
    fn parse_local_package_name() {
        assert_eq!(
            AgentRef::parse("pkg/bar"),
            AgentRef::Local("pkg/bar".into())
        );
    }

    #[test]
    fn parse_remote_name() {
        assert_eq!(
            AgentRef::parse("bar@foo"),
            AgentRef::Remote {
                agent: "bar".into(),
                cluster: "foo".into(),
            }
        );
    }

    #[test]
    fn parse_remote_splits_on_last_at() {
        assert_eq!(
            AgentRef::parse("team@agent@prod"),
            AgentRef::Remote {
                agent: "team@agent".into(),
                cluster: "prod".into(),
            }
        );
    }
}
