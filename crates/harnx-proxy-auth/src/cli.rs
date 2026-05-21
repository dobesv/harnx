use clap::Parser;

#[derive(Debug, Parser)]
pub struct Args {
    /// jq/jaq filter applied to each request. Input is a JSON object with
    /// fields: host, path, method, headers (object of lowercase header names).
    /// Output should be same object, optionally with headers modified.
    /// Multiple --hook flags are piped together.
    /// Example: 'if .host == "github.com" then .headers.authorization = "Bearer \(env.GITHUB_TOKEN)" else . end'
    #[arg(long, value_name = "JQ_FILTER")]
    pub hook: Vec<String>,
}

impl Args {
    /// Combine multiple --hook expressions into single piped filter.
    pub fn combined_filter(&self) -> String {
        if self.hook.is_empty() {
            ".".to_string()
        } else {
            self.hook.join(" | ")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Args;

    #[test]
    fn combined_filter_defaults_to_identity() {
        let args = Args { hook: Vec::new() };
        assert_eq!(args.combined_filter(), ".");
    }

    #[test]
    fn combined_filter_pipes_multiple_hooks() {
        let args = Args {
            hook: vec![".foo = 1".into(), ".bar = 2".into()],
        };
        assert_eq!(args.combined_filter(), ".foo = 1 | .bar = 2");
    }
}
