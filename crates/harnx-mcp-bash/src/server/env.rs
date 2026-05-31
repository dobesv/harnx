// Auto-split from server.rs / handlers.rs for cohesion. See server/mod.rs.
use super::*;

impl BashServer {
    /// Safe defaults for non-interactive child processes: (key, fallback).
    ///
    /// Each entry is seeded as the host-env value when set, otherwise the
    /// fallback.  This makes the host environment authoritative — if the user
    /// already has e.g. `PAGER=bat` or `NO_COLOR=0` in their shell, that wins.
    /// Later layers (.env.bash, extra_env_passthrough, env_overrides) can
    /// override further.
    pub(crate) const NON_INTERACTIVE_ENV_DEFAULTS: &[(&str, &str)] = &[
        // credential/interactive-prompt suppression
        ("GIT_TERMINAL_PROMPT", "0"),
        ("GIT_ASKPASS", "true"),
        ("SSH_ASKPASS", "true"),
        ("SSH_ASKPASS_REQUIRE", "force"),
        ("DEBIAN_FRONTEND", "noninteractive"),
        // pager suppression
        ("PAGER", "cat"),
        ("GIT_PAGER", "cat"),
        ("MANPAGER", "cat"),
        ("SYSTEMD_PAGER", "cat"),
        ("GH_PAGER", "cat"),
        // ANSI color suppression
        ("TERM", "dumb"),
        ("NO_COLOR", "1"),
        ("CLICOLOR", "0"),
        ("FORCE_COLOR", "0"),
    ];

    /// Build the child process environment from the configured sources.
    ///
    /// Layers, applied in order from lowest to highest precedence (later
    /// entries replace earlier ones with the same key):
    /// 1. Non-interactive safe defaults (`NON_INTERACTIVE_ENV_DEFAULTS`):
    ///    each key is seeded with the host-env value when set, otherwise the
    ///    fallback.  The host environment therefore beats these fallbacks, and
    ///    all later layers beat the host.
    /// 2. Default allowlist values inherited from the host process env.
    /// 3. `XDG_*` variables inherited from the host process env.
    /// 4. `.env.bash` dotfile values.
    /// 5. `extra_env_passthrough` — host values for explicitly named vars.
    /// 6. `env_overrides` — explicit `KEY=VALUE` overrides, highest
    ///    precedence.
    ///
    /// This applies on every platform; sandbox-specific behaviour
    /// (birdcage exceptions, `sandbox_run` helper) remains Unix-only.
    pub(crate) fn build_child_env(&self) -> Vec<(String, String)> {
        let mut env_vars: Vec<(String, String)> = Vec::new();

        // 1. Non-interactive safe defaults — lowest precedence.
        //
        // Child commands run non-interactively under an LLM agent.  Without
        // sensible defaults, tools can:
        //
        //   • Write credential prompts via /dev/tty, bypassing
        //     stdin(Stdio::null()) and corrupting the TUI display.
        //   • Spawn interactive pagers (less, more) that hang forever waiting
        //     for keystrokes that will never come.
        //   • Emit ANSI escape sequences that clutter tool-result text sent
        //     back to the model.
        //
        // Each default is seeded as: host-env value if set, otherwise the
        // fallback.  This makes the host environment authoritative for all of
        // these keys — if the user already has PAGER=bat or NO_COLOR=0 in
        // their shell, that value is used.  Later layers (.env.bash,
        // extra_env_passthrough, env_overrides) can still override further.
        //
        // --- Credential / interactive-prompt suppression ---
        //
        //   GIT_TERMINAL_PROMPT=0       — git won't open /dev/tty for
        //                                 credential prompts; fails cleanly.
        //   GIT_ASKPASS=true            — fallback no-op askpass helper
        //                                 (exits 0 with empty output).
        //   SSH_ASKPASS=true            — same for SSH passphrase prompts.
        //   SSH_ASKPASS_REQUIRE=force   — force SSH to use SSH_ASKPASS even
        //                                 when a terminal is available.
        //   DEBIAN_FRONTEND=noninteractive — suppress apt/dpkg prompts.
        //
        // --- Pager suppression ---
        //
        //   PAGER=cat                   — generic pager (man, less wrappers).
        //   GIT_PAGER=cat               — git log/diff/show/etc.
        //   MANPAGER=cat                — man pages.
        //   SYSTEMD_PAGER=cat           — journalctl, systemctl status, etc.
        //   GH_PAGER=cat                — GitHub CLI.
        //
        // --- ANSI color suppression ---
        //
        //   TERM=dumb                   — the most universal signal: tools
        //                                 that check $TERM will detect no
        //                                 color support; less/more refuse to
        //                                 run on a dumb terminal (fail-fast
        //                                 rather than hang); readline still
        //                                 works for the non-interactive -c
        //                                 bash invocation we use.
        //   NO_COLOR=1                  — canonical no-color.org standard,
        //                                 adopted by 300+ tools.
        //   CLICOLOR=0                  — BSD/macOS convention (ls, etc.).
        //   FORCE_COLOR=0               — Node/chalk ecosystem override.
        Self::insert_non_interactive_env_defaults(&mut env_vars);

        // 2. Default allowlist.
        Self::insert_default_env_allowlist(&mut env_vars);

        // 3. XDG_* vars from host env.
        Self::insert_xdg_env_vars(&mut env_vars);

        // 4. .env.bash dotfile.
        Self::insert_dotenv_bash_vars(&mut env_vars);

        // 5. Explicit passthrough names — host value wins over dotfile.
        self.insert_passthrough_env_vars(&mut env_vars);

        // 6. Explicit overrides — highest precedence.
        self.insert_env_overrides(&mut env_vars);

        env_vars
    }

    pub(crate) fn insert_non_interactive_env_defaults(env_vars: &mut Vec<(String, String)>) {
        for (key, fallback) in Self::NON_INTERACTIVE_ENV_DEFAULTS {
            let value = std::env::var(key).unwrap_or_else(|_| (*fallback).to_string());
            env_vars.push(((*key).to_string(), value));
        }
    }

    pub(crate) fn insert_default_env_allowlist(env_vars: &mut Vec<(String, String)>) {
        for name in Self::DEFAULT_ENV_ALLOWLIST {
            if let Ok(value) = std::env::var(name) {
                Self::upsert_env_var(env_vars, (*name).to_string(), value);
            }
        }
    }

    pub(crate) fn insert_xdg_env_vars(env_vars: &mut Vec<(String, String)>) {
        for (name, value) in std::env::vars() {
            if name.starts_with("XDG_") {
                Self::upsert_env_var(env_vars, name, value);
            }
        }
    }

    pub(crate) fn insert_dotenv_bash_vars(env_vars: &mut Vec<(String, String)>) {
        for (key, value) in load_bash_env_file() {
            Self::upsert_env_var(env_vars, key, value);
        }
    }

    pub(crate) fn insert_passthrough_env_vars(&self, env_vars: &mut Vec<(String, String)>) {
        for name in &self.inner.sandbox_config.extra_env_passthrough {
            if let Ok(value) = std::env::var(name) {
                Self::upsert_env_var(env_vars, name.clone(), value);
            }
        }
    }

    pub(crate) fn insert_env_overrides(&self, env_vars: &mut Vec<(String, String)>) {
        for (key, value) in &self.inner.sandbox_config.env_overrides {
            Self::upsert_env_var(env_vars, key.clone(), value.clone());
        }
    }

    pub(crate) fn upsert_env_var(env_vars: &mut Vec<(String, String)>, key: String, value: String) {
        if let Some((_, existing)) = env_vars.iter_mut().find(|(k, _)| k == &key) {
            *existing = value;
        } else {
            env_vars.push((key, value));
        }
    }

    // -----------------------------------------------------------------------
    // env helpers
    // -----------------------------------------------------------------------

    /// Validate that every key in a per-call `env` map is a legal environment
    /// variable name.  Keys must be non-empty and must not contain `=` or NUL
    /// bytes, because:
    /// - In sandbox mode the key is embedded in `--env KEY=VALUE`; a `=` in
    ///   the key makes the `KEY` / `VALUE` split in `sandbox-run` ambiguous.
    /// - On all platforms, the OS env API rejects keys with NUL bytes.
    pub(crate) fn validate_extra_env(env: &HashMap<String, String>) -> Result<(), ErrorData> {
        for key in env.keys() {
            if key.is_empty() || key.contains('=') || key.contains('\0') {
                return Err(ErrorData::invalid_params(
                    format!(
                        "invalid env key {key:?}: keys must be non-empty and must not contain '=' or NUL"
                    ),
                    None,
                ));
            }
        }
        Ok(())
    }
}
