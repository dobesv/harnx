use harnx_core::execution_context::ExecutionContextObservation;
use std::collections::BTreeSet;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, PartialEq)]
pub struct SessionMeta {
    pub id: String,
    pub session_id: Option<String>,
    pub agent_name: Option<String>,
    pub title: Option<String>,
    pub modified: Option<SystemTime>,
    pub contexts: Vec<ExecutionContextObservation>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerMatchMode {
    Local,
    Remote,
}

#[derive(Debug, Clone)]
pub struct PickerQueryContext {
    pub observation: ExecutionContextObservation,
    pub mode: PickerMatchMode,
}

impl PickerQueryContext {
    pub async fn observe_current(mode: PickerMatchMode) -> Self {
        let current_dir = std::env::current_dir().unwrap_or_default();
        Self {
            observation: ExecutionContextObservation::observe_async(
                current_dir.clone(),
                current_dir,
            )
            .await,
            mode,
        }
    }
}

pub(crate) fn session_recency_key(session: &SessionMeta) -> u128 {
    if let Some(modified_ms) = session
        .modified
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis())
    {
        return u128::MAX - modified_ms;
    }
    if let Some(seconds) = crate::utils::session_name::decode_timestamp_session_id(&session.id) {
        return u128::MAX - (seconds as u128 * 1_000);
    }
    if let Ok(uuid) = uuid::Uuid::parse_str(&session.id) {
        if uuid.get_version_num() == 7 {
            if let Some(timestamp) = uuid.get_timestamp() {
                let (seconds, nanos) = timestamp.to_unix();
                return u128::MAX - ((seconds as u128 * 1_000) + (nanos as u128 / 1_000_000));
            }
        }
    }
    u128::MAX
}

/// Automatic selection intentionally remains recency-only.
pub fn find_matching_session(sessions: &[SessionMeta], agent_name: &str) -> Option<String> {
    let mut candidates: Vec<&SessionMeta> = sessions
        .iter()
        .filter(|session| session.agent_name.as_deref() == Some(agent_name))
        .collect();
    candidates.sort_by_key(|session| session_recency_key(session));
    candidates.first().map(|session| session.id.clone())
}

/// Existing recency-only ordering used outside the interactive picker.
pub fn sort_sessions_for_picker(mut sessions: Vec<SessionMeta>) -> Vec<SessionMeta> {
    sessions.sort_by_key(session_recency_key);
    sessions
}

/// Context-aware interactive picker ordering. Contexts are also reordered so
/// the first one is the safe context rendered for that row.
pub fn sort_sessions_for_picker_with_context(
    mut sessions: Vec<SessionMeta>,
    query: &PickerQueryContext,
) -> Vec<SessionMeta> {
    for session in &mut sessions {
        session.contexts.sort_by(|left, right| {
            context_match_tier(left, query)
                .cmp(&context_match_tier(right, query))
                .then_with(|| right.observed_at.cmp(&left.observed_at))
        });
    }
    sessions.sort_by(|left, right| {
        session_match_tier(left, query)
            .cmp(&session_match_tier(right, query))
            .then_with(|| session_recency_key(left).cmp(&session_recency_key(right)))
    });
    sessions
}

fn session_match_tier(session: &SessionMeta, query: &PickerQueryContext) -> u8 {
    session
        .contexts
        .iter()
        .map(|context| context_match_tier(context, query))
        .min()
        .unwrap_or(3)
}

fn context_match_tier(context: &ExecutionContextObservation, query: &PickerQueryContext) -> u8 {
    let repository_matches = same_present(
        context.primary_repository(),
        query.observation.primary_repository(),
    );
    let branch_matches = same_present(context.branch(), query.observation.branch());
    if local_directory_matches(context, query) {
        return 0;
    }
    if repository_matches {
        return if branch_matches { 0 } else { 1 };
    }
    if branch_matches {
        2
    } else {
        3
    }
}

fn same_present(left: Option<&str>, right: Option<&str>) -> bool {
    matches!((left, right), (Some(left), Some(right)) if left == right)
}

fn local_directory_matches(
    context: &ExecutionContextObservation,
    query: &PickerQueryContext,
) -> bool {
    query.mode == PickerMatchMode::Local
        && context.working_directory == query.observation.working_directory
}

impl SessionMeta {
    pub fn picker_label(&self) -> String {
        let mut label = match &self.title {
            Some(title) => format!("{}  {}", self.id, title),
            None => self.id.clone(),
        };
        let Some(context) = self
            .contexts
            .iter()
            .find(|context| context.primary_repository().is_some() || context.branch().is_some())
        else {
            return label;
        };
        let safe_context = match (context.primary_repository(), context.branch()) {
            (Some(repository), Some(branch)) => Some(format!("{repository} @ {branch}")),
            (Some(repository), None) => Some(repository.to_string()),
            (None, Some(branch)) => Some(format!("branch {branch}")),
            (None, None) => None,
        };
        if let Some(safe_context) = safe_context {
            label.push_str("  ·  ");
            label.push_str(&safe_context);
        }
        let repository_count = self
            .contexts
            .iter()
            .filter_map(ExecutionContextObservation::primary_repository)
            .collect::<BTreeSet<_>>()
            .len();
        if repository_count > 1 {
            label.push_str(&format!("  +{} repos", repository_count - 1));
        }
        label
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration as ChronoDuration, Utc};
    use harnx_core::execution_context::{
        GitRemoteObservation, GitRepositoryObservation, ToolObservationProvenance,
        EXECUTION_CONTEXT_VERSION,
    };
    use std::time::{Duration, UNIX_EPOCH};

    fn session_meta(name: &str) -> SessionMeta {
        SessionMeta {
            id: name.to_string(),
            session_id: None,
            agent_name: None,
            title: None,
            modified: None,
            contexts: Vec::new(),
        }
    }

    fn context(
        repo: Option<&str>,
        branch: Option<&str>,
        cwd: &str,
        age: i64,
    ) -> ExecutionContextObservation {
        ExecutionContextObservation {
            version: EXECUTION_CONTEXT_VERSION,
            observed_at: Utc::now() - ChronoDuration::seconds(age),
            workspace_root: cwd.to_string(),
            working_directory: cwd.to_string(),
            repository: Some(GitRepositoryObservation {
                worktree_root: cwd.to_string(),
                branch: branch.map(str::to_string),
                remotes: repo
                    .map(|repository| {
                        vec![GitRemoteObservation {
                            name: "origin".to_string(),
                            repository: repository.to_string(),
                            primary: true,
                        }]
                    })
                    .unwrap_or_default(),
            }),
            provenance: Some(ToolObservationProvenance::new(
                "scope", "fs", "read", "call",
            )),
        }
    }

    #[test]
    fn test_sort_recency_fallback() {
        let older = session_meta("018f0d1c-5b2a-7000-8000-000000000000");
        let newer = session_meta("018f0d1c-5b2b-7000-8000-000000000000");
        let sorted = sort_sessions_for_picker(vec![older, newer.clone()]);
        assert_eq!(sorted[0].id, newer.id);
    }

    #[test]
    fn test_sort_modified_beats_id_timestamp() {
        let mut first = session_meta("018f0d1c-5b2b-7000-8000-000000000000");
        first.modified = Some(UNIX_EPOCH + Duration::from_secs(10));
        let mut expected = session_meta("018f0d1c-5b2a-7000-8000-000000000000");
        expected.modified = Some(UNIX_EPOCH + Duration::from_secs(20));
        let sorted = sort_sessions_for_picker(vec![first, expected.clone()]);
        assert_eq!(sorted[0].id, expected.id);
    }

    #[test]
    fn test_find_matching_session_filters_agent_and_picks_most_recent() {
        let mut older = session_meta("018f0d1c-5b2a-7000-8000-000000000000");
        older.agent_name = Some("smith".to_string());
        older.modified = Some(UNIX_EPOCH + Duration::from_secs(1));
        let mut newer = older.clone();
        newer.id = "018f0d1c-5b2b-7000-8000-000000000000".to_string();
        newer.modified = Some(UNIX_EPOCH + Duration::from_secs(2));
        let mut other_agent = newer.clone();
        other_agent.id = "newest-other-agent".to_string();
        other_agent.agent_name = Some("neo".to_string());
        other_agent.modified = Some(UNIX_EPOCH + Duration::from_secs(3));
        assert_eq!(
            find_matching_session(&[older, newer.clone(), other_agent], "smith").as_deref(),
            Some(newer.id.as_str())
        );
    }

    #[test]
    fn ranks_repository_branch_repository_branch_then_recency() {
        let query = PickerQueryContext {
            observation: context(Some("github.com/acme/repo"), Some("main"), "/query", 0),
            mode: PickerMatchMode::Remote,
        };
        let mut exact = session_meta("exact");
        exact.contexts.push(context(
            Some("github.com/acme/repo"),
            Some("main"),
            "/remote",
            10,
        ));
        let mut repo = session_meta("repo");
        repo.contexts.push(context(
            Some("github.com/acme/repo"),
            Some("other"),
            "/query",
            0,
        ));
        let mut branch = session_meta("branch");
        branch.contexts.push(context(
            Some("github.com/elsewhere/repo"),
            Some("main"),
            "/elsewhere",
            0,
        ));
        let none = session_meta("none");
        let sorted = sort_sessions_for_picker_with_context(vec![none, branch, repo, exact], &query);
        assert_eq!(
            sorted
                .iter()
                .map(|session| session.id.as_str())
                .collect::<Vec<_>>(),
            vec!["exact", "repo", "branch", "none"]
        );
    }

    #[test]
    fn local_mode_matches_cwd_while_remote_mode_ignores_it() {
        let observation = context(None, None, "/query", 0);
        let mut cwd = session_meta("cwd");
        cwd.contexts.push(context(None, None, "/query", 0));
        let other = session_meta("other");
        let local = PickerQueryContext {
            observation: observation.clone(),
            mode: PickerMatchMode::Local,
        };
        assert_eq!(
            sort_sessions_for_picker_with_context(vec![other.clone(), cwd.clone()], &local)[0].id,
            "cwd"
        );
        let remote = PickerQueryContext {
            observation,
            mode: PickerMatchMode::Remote,
        };
        let sorted = sort_sessions_for_picker_with_context(vec![other, cwd], &remote);
        assert!(sorted
            .iter()
            .all(|session| session_match_tier(session, &remote) == 3));
    }

    #[test]
    fn uses_best_of_multiple_contexts_and_recency_fallback() {
        let query = PickerQueryContext {
            observation: context(Some("github.com/acme/repo"), Some("main"), "/query", 0),
            mode: PickerMatchMode::Remote,
        };
        let mut multiple = session_meta("multiple");
        multiple.modified = Some(UNIX_EPOCH + Duration::from_secs(1));
        multiple.contexts = vec![
            context(Some("github.com/elsewhere/old"), Some("old"), "/old", 0),
            context(Some("github.com/acme/repo"), Some("main"), "/match", 10),
        ];
        let mut newer_no_context = session_meta("newer-no-context");
        newer_no_context.modified = Some(UNIX_EPOCH + Duration::from_secs(3));
        let mut older_no_context = session_meta("older-no-context");
        older_no_context.modified = Some(UNIX_EPOCH + Duration::from_secs(2));

        let sorted = sort_sessions_for_picker_with_context(
            vec![older_no_context, multiple, newer_no_context],
            &query,
        );
        assert_eq!(
            sorted
                .iter()
                .map(|session| session.id.as_str())
                .collect::<Vec<_>>(),
            vec!["multiple", "newer-no-context", "older-no-context"]
        );
        assert_eq!(
            sorted[0].contexts[0].primary_repository(),
            Some("github.com/acme/repo")
        );
    }

    #[test]
    fn picker_label_never_includes_paths_and_marks_multiple_repositories() {
        let mut session = session_meta("session");
        session.title = Some("Fix tests".to_string());
        session.contexts = vec![
            context(None, None, "/secret/non-repository", 0),
            context(Some("github.com/acme/one"), Some("main"), "/secret/one", 1),
            context(
                Some("github.com/acme/two"),
                Some("feature"),
                "/secret/two",
                2,
            ),
        ];
        let label = session.picker_label();
        assert!(label.contains("github.com/acme/one @ main"));
        assert!(label.contains("+1 repos"));
        assert!(!label.contains("/secret"));
    }
}
