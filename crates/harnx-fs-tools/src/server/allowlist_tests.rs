use super::{FsServer, RollbackParams};
use harnx_tool_allow::ResolvedAllowlist;

fn path_string(path: &std::path::Path) -> String {
    path.to_string_lossy().into_owned()
}

#[tokio::test]
async fn rollback_rejects_repo_root_outside_write_grant() {
    let temp = tempfile::tempdir().expect("tempdir");
    let base = temp.path().canonicalize().expect("canonical tempdir");
    let repo = base.join("repo");
    let allowed = repo.join("allowed");
    std::fs::create_dir_all(&allowed).expect("create allowed subdirectory");
    let init = std::process::Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(&repo)
        .status()
        .expect("run git init");
    assert!(init.success());
    let mut allowlist = ResolvedAllowlist::new();
    allowlist.insert_write(&allowed);
    let server = FsServer::new(allowlist);

    let denied = server
        .rollback_file_impl(RollbackParams {
            commit_id: "0000000000000000000000000000000000000000".to_string(),
            repo_path: path_string(&allowed),
        })
        .await
        .unwrap_err();

    assert!(denied.message.contains("outside allowed write paths"));
    assert!(denied.message.contains(&path_string(&repo)));
}

#[cfg(unix)]
#[test]
fn default_search_path_prefers_allowed_cwd_over_common_defaults() {
    let cwd = std::env::current_dir().expect("current dir");
    let inputs = harnx_tool_allow::AllowInputs {
        read: vec![cwd.clone()],
        common_default: true,
        ..Default::default()
    };
    let allowlist = harnx_tool_allow::resolve_allowlist(
        &inputs,
        &cwd,
        &harnx_tool_allow::AllowEnv::from_current_process(),
    );

    assert!(allowlist.contains_read("/usr/bin"));
    assert_eq!(super::default_search_path(&allowlist).unwrap(), cwd);
}
