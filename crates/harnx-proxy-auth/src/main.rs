use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use clap::Parser;

use harnx_proxy_auth::{
    ca, cli, exec_load, filter, fs_gen, hook, load, notice, proxy, sentinel,
    transform::{build_stages, TransformPipeline},
};

#[tokio::main]
async fn main() -> Result<()> {
    // Write tracing output to stderr so it never interleaves with the
    // readiness lines or JSONL responses on stdout.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();

    let args = <cli::Args as Parser>::parse();
    // Register the notice channel before starting the proxy, so exec hooks can
    // surface structured notices that the JSONL loop drains to stdout.
    let notice_rx = notice::init_channel();
    let sentinels = Arc::new(sentinel::Sentinels::generate());
    let load_specs = load::parse_load_specs(&args.load_yaml, &args.load_json, &args.load_raw)?;
    let loaded_vars: Vec<(String, jaq_all::json::Val)> = load_specs
        .iter()
        .map(|spec| (spec.name.clone(), load::load_value(spec)))
        .collect();

    let fs_temp_dir = if args.fs.is_empty() {
        None
    } else {
        Some(tempfile::Builder::new().prefix("harnx-fs-").tempdir()?)
    };
    let temp_file_root = fs_temp_dir
        .as_ref()
        .map(|dir| dir.path().to_string_lossy().into_owned())
        .unwrap_or_default();

    let mut loaded_vars = loaded_vars
        .into_iter()
        .map(|(name, value)| Ok((name, filter::jaq_value_to_json(value)?)))
        .collect::<Result<Vec<_>>>()?;
    let exec_specs = exec_load::parse_exec_specs(
        &args.load_exec,
        &loaded_vars
            .iter()
            .map(|(name, _)| name.clone())
            .collect::<Vec<_>>(),
    )?;
    for spec in &exec_specs {
        let value = exec_load::run_exec_value(spec, &exec_load::ShellRunner);
        loaded_vars.push((spec.name.clone(), value));
    }

    let jaq_vars = Arc::new(filter::JaqVars::new_from_values(
        &sentinels,
        temp_file_root.clone(),
        None,
        loaded_vars.clone(),
    )?);
    let stages = build_stages(&args.hook, &jaq_vars, args.hook_timeout_secs)?;
    let pipeline = Arc::new(TransformPipeline::new(stages));
    fs_gen::eval_and_write_files(&args.fs, &jaq_vars, &temp_file_root)?;

    let (ca_setup, _ca_temp_dir) = ca::setup()?;
    let ca_cert_path = ca_setup.cert_pem_path.clone();
    let ca_cert_pem = std::fs::read_to_string(&ca_cert_path)?;
    let port = proxy::start_proxy_with_log(pipeline.clone(), ca_setup, args.log_file).await?;
    let env_vars = filter::JaqVars::new_from_values(
        &sentinels,
        temp_file_root.clone(),
        Some(port),
        loaded_vars.clone(),
    )?;
    let mut extra_env = filter::eval_env_scripts(&args.env, &env_vars)?;
    let startup_vars = env_vars.safe_request_vars();
    let startup_env = pipeline
        .run_startup(startup_vars, Duration::from_secs(args.hook_timeout_secs))
        .await;
    for (key, value) in startup_env {
        if extra_env.contains_key(&key) {
            continue;
        }

        // SAFETY: startup hooks run after proxy listener tasks start but before
        // JSONL request loop begins. In this window, harnx-proxy-auth startup
        // path is sole code mutating process env, and proxy/background tasks
        // spawned by start_proxy_with_log do not read or write process env.
        // That keeps this mutation race-free in practice for current code.
        unsafe {
            std::env::set_var(&key, value.as_str().expect("startup env strings only"));
        }
        extra_env.insert(key, value);
    }

    // Write readiness lines then explicitly drop the lock before entering the
    // async JSONL loop. Holding a std::io::StdoutLock across an .await would
    // deadlock: tokio's async stdout shares the same fd and any tracing output
    // or response write that tries to acquire the lock would block forever.
    {
        let mut stdout = std::io::stdout().lock();
        writeln!(stdout, "PROXY_PORT={port}")?;
        writeln!(stdout, "CA_CERT_PATH={}", ca_cert_path.display())?;
        use base64::Engine as _;
        let ca_cert_b64 = base64::engine::general_purpose::STANDARD.encode(ca_cert_pem.as_bytes());
        writeln!(stdout, "CA_CERT_PEM_B64={ca_cert_b64}")?;
        stdout.flush()?;
    } // lock dropped here

    if let Some(temp_dir) = fs_temp_dir {
        tokio::select! {
            result = hook::run_jsonl_loop(port, ca_cert_path, extra_env, notice_rx) => result,
            _ = shutdown_signal() => {
                // `TempDir`'s Drop removes the directory on the way out.
                drop(temp_dir);
                Ok(())
            }
        }
    } else {
        hook::run_jsonl_loop(port, ca_cert_path, extra_env, notice_rx).await
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler");
        let _ = sigterm.recv().await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
}
