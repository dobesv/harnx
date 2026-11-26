use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use clap::Parser;

use harnx_proxy_auth::{
    ca, cli, exec_load, filter, fs_gen, load, notice, proxy, sentinel,
    transform::{build_stages, TransformPipeline},
};

const MAIN_STACK_BYTES: usize = 8 * 1024 * 1024;

fn main() -> Result<()> {
    // Transport futures exceed Windows' small main-thread stack in debug builds.
    std::thread::Builder::new()
        .name("proxy-auth".into())
        .stack_size(MAIN_STACK_BYTES)
        .spawn(|| {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?
                .block_on(Box::pin(run()))
        })?
        .join()
        .map_err(|_| anyhow::anyhow!("proxy auth server thread panicked"))?
}

async fn run() -> Result<()> {
    let args = <cli::Args as Parser>::parse();
    let _ = harnx_core::logging::init(harnx_core::logging::LogSink::Stderr);
    run_tool_service(&args).await
}

// qual:allow(iosp) — the listing branch only picks which toolset to build
async fn run_tool_service(args: &cli::Args) -> Result<()> {
    if args.list_tools {
        // Listing must not start the proxy or run hooks, which can have side effects.
        let toolset = harnx_proxy_auth::toolset::ProxyAuthToolset::new(
            0,
            std::path::Path::new(""),
            &serde_json::Map::new(),
            None,
        );
        return Box::pin(harnx_toolset_server::run_toolset_main(toolset)).await;
    }
    let mut runtime = build_runtime(args).await?;
    let toolset = harnx_proxy_auth::toolset::ProxyAuthToolset::new(
        runtime.port,
        &runtime.ca_cert_path,
        &runtime.extra_env,
        runtime.fs_temp_dir.as_ref().map(|dir| dir.path()),
    );
    // The server owns the proxy, CA and Jira config for background commands too.
    // Drain notices to stderr, never onto the tool transport's stdout.
    let result = tokio::select! {
        result = Box::pin(harnx_toolset_server::run_toolset_main(toolset)) => result,
        () = log_notices(&mut runtime.notice_rx) => anyhow::bail!("proxy notice channel closed"),
    };
    drop(runtime);
    result
}

async fn log_notices(notices: &mut notice::HookNoticeReceiver) {
    while let Some(notice) = notices.recv().await {
        log::warn!("proxy auth [{}]: {}", notice.level, notice.message);
    }
}

/// Assembles the dispatch runtime from CLI args: loads variables, generates
/// files, starts the proxy, and runs startup hooks to populate extra_env.
async fn build_runtime(args: &cli::Args) -> Result<DispatchRuntime> {
    // Register the notice channel before starting the proxy, so exec hooks can
    // surface structured notices that log_notices drains to stderr.
    let notice_rx = notice::init_channel();
    let sentinels = Arc::new(sentinel::Sentinels::generate());
    let loaded_vars = load_variables(args)?;
    let fs_temp_dir = if args.fs.is_empty() {
        None
    } else {
        Some(tempfile::Builder::new().prefix("harnx-fs-").tempdir()?)
    };
    let temp_file_root = fs_temp_dir
        .as_ref()
        .map(|dir| dir.path().to_string_lossy().into_owned())
        .unwrap_or_default();

    let jaq_vars = Arc::new(filter::JaqVars::new_from_values(
        &sentinels,
        temp_file_root.clone(),
        None,
        loaded_vars.clone(),
    )?);
    let hooks = args.resolved_hooks()?;
    let stages = build_stages(&hooks, &jaq_vars, args.hook_timeout_secs)?;
    let pipeline = Arc::new(TransformPipeline::new(stages));
    fs_gen::eval_and_write_files(&args.fs, &jaq_vars, &temp_file_root)?;

    let (ca_setup, ca_temp_dir) = ca::setup()?;
    let ca_cert_path = ca_setup.cert_pem_path.clone();
    let port =
        proxy::start_proxy_with_log(pipeline.clone(), ca_setup, args.log_file.clone()).await?;
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
        // tool-service request handling begins. This startup path is the only
        // code mutating process env, and proxy/background tasks spawned by
        // start_proxy_with_log do not read or write process env.
        // That keeps this mutation race-free in practice for current code.
        unsafe {
            std::env::set_var(&key, value.as_str().expect("startup env strings only"));
        }
        extra_env.insert(key, value);
    }

    Ok(DispatchRuntime {
        port,
        ca_cert_path,
        extra_env,
        notice_rx,
        fs_temp_dir,
        _ca_temp_dir: ca_temp_dir,
    })
}

fn load_variables(args: &cli::Args) -> Result<Vec<(String, serde_json::Value)>> {
    let load_specs = load::parse_load_specs(&args.load_yaml, &args.load_json, &args.load_raw)?;
    let loaded_vars: Vec<(String, jaq_all::json::Val)> = load_specs
        .iter()
        .map(|spec| (spec.name.clone(), load::load_value(spec)))
        .collect();
    let mut loaded_vars = loaded_vars
        .into_iter()
        .map(|(name, value)| Ok((name, filter::jaq_value_to_json(value)?)))
        .collect::<Result<Vec<_>>>()?;
    let names = loaded_vars
        .iter()
        .map(|(name, _)| name.clone())
        .collect::<Vec<_>>();
    for spec in exec_load::parse_exec_specs(&args.load_exec, &names)? {
        let value = exec_load::run_exec_value(&spec, &exec_load::ShellRunner);
        loaded_vars.push((spec.name, value));
    }
    Ok(loaded_vars)
}

struct DispatchRuntime {
    port: u16,
    ca_cert_path: std::path::PathBuf,
    extra_env: serde_json::Map<String, serde_json::Value>,
    notice_rx: notice::HookNoticeReceiver,
    fs_temp_dir: Option<tempfile::TempDir>,
    // Owns the on-disk CA bundle (ca.pem). The exported SSL_CERT_FILE /
    // CURL_CA_BUNDLE / NODE_EXTRA_CA_CERTS / REQUESTS_CA_BUNDLE / GIT_SSL_CAINFO
    // env vars point at this file, so it MUST stay alive for the whole server
    // lifetime, not just build_runtime(). Dropping it early deletes ca.pem while
    // the proxy keeps running (the proxy holds the CA in memory), leaving every
    // TLS client pointed at a missing file. See issue #1622.
    _ca_temp_dir: tempfile::TempDir,
}
