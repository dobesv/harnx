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

    /// Write a JSON log line for every proxied request. Each line is a JSON
    /// object with fields: host, method, path, auth, changed.
    /// `auth` is a truncated summary of the outgoing Authorization header
    /// (never the full token) and `changed` lists header names the hook
    /// added, replaced, or removed. Useful for debugging auth injection
    /// without exposing full tokens.
    #[arg(long, value_name = "PATH")]
    pub log_file: Option<std::path::PathBuf>,

    /// jq/jaq filter that receives sentinel values as jaq variables:
    /// `$fake_uuid_key`, `$fake_base64_key`, `$fake_url_base64_key`,
    /// `$fake_hex_key`, `$fake_email`. Must output a JSON object.
    /// Multiple `--env` flags run in order and merge (later keys win).
    /// Error aborts startup.
    #[arg(long, value_name = "JQ_FILTER")]
    pub env: Vec<String>,

    /// Load YAML file at startup and expose parsed value as jaq variable `$<name>`.
    #[arg(long, value_name = "NAME=PATH")]
    pub load_yaml: Vec<String>,

    /// Load JSON file at startup and expose parsed value as jaq variable `$<name>`.
    #[arg(long, value_name = "NAME=PATH")]
    pub load_json: Vec<String>,

    /// Load raw UTF-8 file at startup and expose contents as jaq variable `$<name>`.
    #[arg(long, value_name = "NAME=PATH")]
    pub load_raw: Vec<String>,

    /// jq/jaq transformer that builds files under `$temp_file_root`.
    #[arg(long, value_name = "JQ_FILTER")]
    pub fs: Vec<String>,

    /// Run shell command at startup and expose stdout as jaq variable `$<name>`.
    #[arg(long, value_name = "NAME=COMMAND")]
    pub load_exec: Vec<String>,
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
        let args = Args {
            hook: Vec::new(),
            log_file: None,
            env: Vec::new(),
            load_yaml: Vec::new(),
            load_json: Vec::new(),
            load_raw: Vec::new(),
            fs: Vec::new(),
            load_exec: Vec::new(),
        };
        assert_eq!(args.combined_filter(), ".");
    }

    #[test]
    fn combined_filter_pipes_multiple_hooks() {
        let args = Args {
            hook: vec![".foo = 1".into(), ".bar = 2".into()],
            log_file: None,
            env: Vec::new(),
            load_yaml: Vec::new(),
            load_json: Vec::new(),
            load_raw: Vec::new(),
            fs: Vec::new(),
            load_exec: Vec::new(),
        };
        assert_eq!(args.combined_filter(), ".foo = 1 | .bar = 2");
    }
}
