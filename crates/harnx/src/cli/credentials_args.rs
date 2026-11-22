use clap::{Args, Subcommand};

/// Manage your own credentials on tool servers that ask for them
/// (`<server>_connect` tools). Secrets are never entered here: `connect`
/// prints a link you complete in a browser.
#[derive(Args, Debug, PartialEq, Eq)]
pub struct CredentialsArgs {
    #[command(subcommand)]
    pub command: CredentialsSubcommands,
}

#[derive(Subcommand, Debug, PartialEq, Eq)]
pub enum CredentialsSubcommands {
    /// Show connected/expiry status for every credentialed tool server
    List,
    /// Start a connect flow for a tool server; prints a link to open in a
    /// browser and waits until the connection completes, fails or is
    /// cancelled (Ctrl-C)
    Connect {
        /// Tool server name (the `<server>` in `<server>_connect`)
        server: String,
    },
    /// Remove your stored credential for a tool server
    Disconnect {
        /// Tool server name (the `<server>` in `<server>_disconnect`)
        server: String,
    },
}
