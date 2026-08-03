use clap::Parser;

#[derive(Debug, Parser)]
pub struct Args {
    /// Hook server name assigned by the supervisor.
    #[arg(long, value_name = "NAME")]
    pub name: Option<String>,

    /// jq/jaq filter or exec hook applied to each request. Input is a JSON
    /// object with fields: host, path, method, headers (object of lowercase
    /// header names). Output should be same object, optionally with headers
    /// modified. A `--hook` value starting with `#!` becomes an inline resident
    /// exec stage and must include a shebang so temp file is runnable. Values
    /// starting with `/`, `./`, or `../` are treated as executable file paths
    /// relative to proxy CWD; they only need execute permission and may be any
    /// language or compiled binary. All other values are jaq stages. Elements
    /// apply in CLI order.
    /// Example: 'if .host == "github.com" then .headers.authorization = "Bearer \(env.GITHUB_TOKEN)" else . end'
    #[arg(long, value_name = "HOOK")]
    pub hook: Vec<String>,

    /// Per-request timeout in seconds for exec `--hook` stages.
    #[arg(long, default_value_t = 30)]
    pub hook_timeout_secs: u64,

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
    /// `$fake_hex_key`, `$fake_email`, `$temp_file_root`, and after proxy
    /// startup `$proxy_port` as a string. Must output a JSON object.
    /// Multiple `--env` flags run in order and merge (later keys win).
    #[arg(long, value_name = "JQ_FILTER")]
    pub env: Vec<String>,

    /// jq/jaq filter that must output an object mapping relative file paths to
    /// file contents. Each value may be a string, object/array (written as JSON),
    /// or null to skip. Files are written under a fresh temporary root (unique
    /// per proxy instance, auto-deleted on exit) whose path is available to
    /// `--env`/`--fs`/`--hook` jq filters as `$temp_file_root` and to executable
    /// hooks as `vars.temp_file_root` on each request.
    /// Multiple `--fs` flags run in order and merge (later keys win).
    #[arg(long, value_name = "JQ_FILTER")]
    pub fs: Vec<String>,

    /// Load a YAML file into a jaq variable before evaluating hooks.
    /// Format: name=path
    #[arg(long = "load-yaml", value_name = "NAME=PATH")]
    pub load_yaml: Vec<String>,

    /// Load a JSON file into a jaq variable before evaluating hooks.
    /// Format: name=path
    #[arg(long = "load-json", value_name = "NAME=PATH")]
    pub load_json: Vec<String>,

    /// Load a raw text file into a jaq variable before evaluating hooks.
    /// Format: name=path
    #[arg(long = "load-raw", value_name = "NAME=PATH")]
    pub load_raw: Vec<String>,

    /// Execute a shell command and load stdout as a jaq variable.
    /// Format: name=command
    #[arg(long = "load-exec", value_name = "NAME=COMMAND")]
    pub load_exec: Vec<String>,
}
