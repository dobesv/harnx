use serde::Deserialize;

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct GrepQueryParams {
    pub query: String,
    pub language: Option<String>,
    pub repo: Option<String>,
    pub path: Option<String>,
}

impl GrepQueryParams {
    pub fn validate(&self) -> Result<(), String> {
        if self.query.is_empty() {
            return Err(
                "❌ Error: 'query' parameter is required and must be a non-empty string".into(),
            );
        }
        if self.query.trim().is_empty() {
            return Err("❌ Error: 'query' cannot be empty or only whitespace".into());
        }
        if self.query.chars().count() > 1000 {
            return Err(
                "❌ Error: 'query' is too long (max 1000 characters). Please use a shorter query."
                    .into(),
            );
        }

        if let Some(language) = &self.language {
            if language.trim().is_empty() {
                return Err(
                    "❌ Error: 'language' parameter must be a non-empty string when provided"
                        .into(),
                );
            }
            if language.chars().count() > 50 {
                return Err(
                    "❌ Error: 'language' parameter is too long (max 50 characters)".into(),
                );
            }
        }

        if let Some(repo) = &self.repo {
            if repo.trim().is_empty() {
                return Err(
                    "❌ Error: 'repo' parameter must be a non-empty string when provided".into(),
                );
            }
            if repo.matches('/').count() != 1 {
                return Err(
                    "❌ Error: 'repo' parameter must be in format 'owner/repository' (e.g., 'fastapi/fastapi')"
                        .into(),
                );
            }
            if repo.chars().count() > 100 {
                return Err("❌ Error: 'repo' parameter is too long (max 100 characters)".into());
            }
        }

        if let Some(path) = &self.path {
            if path.trim().is_empty() {
                return Err(
                    "❌ Error: 'path' parameter must be a non-empty string when provided".into(),
                );
            }
            if path.chars().count() > 200 {
                return Err("❌ Error: 'path' parameter is too long (max 200 characters)".into());
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::GrepQueryParams;

    fn params(query: impl Into<String>) -> GrepQueryParams {
        GrepQueryParams {
            query: query.into(),
            language: None,
            repo: None,
            path: None,
        }
    }

    #[test]
    fn rejects_invalid_query_with_exact_messages() {
        assert_eq!(
            params("").validate(),
            Err("❌ Error: 'query' parameter is required and must be a non-empty string".into())
        );
        assert_eq!(
            params(" \t\n").validate(),
            Err("❌ Error: 'query' cannot be empty or only whitespace".into())
        );
        assert_eq!(
            params("q".repeat(1001)).validate(),
            Err(
                "❌ Error: 'query' is too long (max 1000 characters). Please use a shorter query."
                    .into()
            )
        );
    }

    #[test]
    fn query_length_uses_characters_and_accepts_boundary() {
        assert!(params("é".repeat(1000)).validate().is_ok());
        assert_eq!(
            params("é".repeat(1001)).validate(),
            Err(
                "❌ Error: 'query' is too long (max 1000 characters). Please use a shorter query."
                    .into()
            )
        );
    }

    #[test]
    fn rejects_invalid_language_with_exact_messages_and_accepts_boundary() {
        let mut value = params("query");
        value.language = Some("  ".into());
        assert_eq!(
            value.validate(),
            Err("❌ Error: 'language' parameter must be a non-empty string when provided".into())
        );
        value.language = Some("l".repeat(50));
        assert!(value.validate().is_ok());
        value.language = Some("l".repeat(51));
        assert_eq!(
            value.validate(),
            Err("❌ Error: 'language' parameter is too long (max 50 characters)".into())
        );
    }

    #[test]
    fn rejects_invalid_repo_with_exact_messages_and_checks_slashes() {
        let mut value = params("query");
        value.repo = Some(" \t".into());
        assert_eq!(
            value.validate(),
            Err("❌ Error: 'repo' parameter must be a non-empty string when provided".into())
        );

        let format_error = Err("❌ Error: 'repo' parameter must be in format 'owner/repository' (e.g., 'fastapi/fastapi')".into());
        value.repo = Some("owner-repo".into());
        assert_eq!(value.validate(), format_error);
        value.repo = Some("owner/repo/extra".into());
        assert_eq!(value.validate(), format_error);
        value.repo = Some("owner/repo".into());
        assert!(value.validate().is_ok());
    }

    #[test]
    fn repo_length_accepts_100_and_rejects_101_characters() {
        let mut value = params("query");
        value.repo = Some(format!("{}/r", "o".repeat(98)));
        assert!(value.validate().is_ok());
        value.repo = Some(format!("{}/r", "o".repeat(99)));
        assert_eq!(
            value.validate(),
            Err("❌ Error: 'repo' parameter is too long (max 100 characters)".into())
        );
    }

    #[test]
    fn rejects_invalid_path_with_exact_messages_and_accepts_boundary() {
        let mut value = params("query");
        value.path = Some("\n ".into());
        assert_eq!(
            value.validate(),
            Err("❌ Error: 'path' parameter must be a non-empty string when provided".into())
        );
        value.path = Some("p".repeat(200));
        assert!(value.validate().is_ok());
        value.path = Some("p".repeat(201));
        assert_eq!(
            value.validate(),
            Err("❌ Error: 'path' parameter is too long (max 200 characters)".into())
        );
    }

    #[test]
    fn valid_filters_pass_validation() {
        let value = GrepQueryParams {
            query: " FastAPI ".into(),
            language: Some(" Python ".into()),
            repo: Some("fastapi/fastapi".into()),
            path: Some("src/".into()),
        };
        assert!(value.validate().is_ok());
    }
}
