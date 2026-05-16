use std::io::Write;

use anyhow::Result;
use clap::Parser;

use harnx_proxy_auth::{ca, cli, filter, hook, proxy};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let args = <cli::Args as Parser>::parse();
    let filter_expr = args.combined_filter();
    let filter = filter::compile(&filter_expr)?;

    let (ca_setup, _ca_temp_dir) = ca::setup()?;
    let ca_cert_path = ca_setup.cert_pem_path.clone();
    let ca_cert_pem = std::fs::read_to_string(&ca_cert_path)?;
    let port = proxy::start_proxy(filter, ca_setup).await?;

    let mut stdout = std::io::stdout().lock();
    writeln!(stdout, "PROXY_PORT={port}")?;
    writeln!(stdout, "CA_CERT_PATH={}", ca_cert_path.display())?;
    use base64::Engine as _;
    let ca_cert_b64 = base64::engine::general_purpose::STANDARD.encode(ca_cert_pem.as_bytes());
    writeln!(stdout, "CA_CERT_PEM_B64={ca_cert_b64}")?;
    stdout.flush()?;

    hook::run_jsonl_loop(port, ca_cert_path).await
}
