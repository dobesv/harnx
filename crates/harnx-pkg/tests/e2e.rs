mod helpers;

use harnx_core::config_paths::packages_dir;
use harnx_pkg::cli::{AddArgs, CheckForUpdatesArgs, RemoveArgs, UpdateArgs};
use harnx_pkg::commands;
use harnx_runtime::config::list_agents;

static ENV_MUTEX: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct EnvGuard {
    key: &'static str,
    prev: Option<std::ffi::OsString>,
}

impl EnvGuard {
    fn new(key: &'static str, val: impl AsRef<std::path::Path>) -> Self {
        let prev = std::env::var_os(key);
        unsafe { std::env::set_var(key, val.as_ref()) };
        Self { key, prev }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.prev {
            Some(v) => unsafe { std::env::set_var(self.key, v) },
            None => unsafe { std::env::remove_var(self.key) },
        }
    }
}

#[tokio::test]
async fn test_e2e_git_add_then_runtime_loads() {
    let _guard = ENV_MUTEX.lock().await;
    let tmp = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::new("HARNX_CONFIG_DIR", tmp.path());

    let (_repo_dir, url) = helpers::create_test_git_repo(
        &[("agents/helper.md", "---\nmodel: test\n---\nHelp here.")],
        &["v1.0.0"],
    );

    let args = AddArgs {
        url,
        tag: "v1.0.0".to_string(),
        name: Some("e2e-pkg".to_string()),
        subpath: None,
    };
    commands::add::run(&args).await.unwrap();

    let pkg_dir = packages_dir().join("e2e-pkg");
    assert!(pkg_dir.join("manifest.yaml").exists());
    assert!(pkg_dir.join("agents/helper.md").exists());

    let agents = list_agents();
    assert!(
        agents.contains(&"e2e-pkg/helper".to_string()),
        "Expected 'e2e-pkg/helper' in agents, got: {:?}",
        agents
    );
}

#[tokio::test]
async fn test_e2e_git_update() {
    let _guard = ENV_MUTEX.lock().await;
    let tmp = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::new("HARNX_CONFIG_DIR", tmp.path());

    let (_repo_dir, url) = helpers::create_test_git_repo(
        &[("agents/helper.md", "---\nmodel: test\n---\nv1 content.")],
        &["v1.0.0", "v2.0.0"],
    );

    let add_args = AddArgs {
        url: url.clone(),
        tag: "v1.0.0".to_string(),
        name: Some("update-pkg".to_string()),
        subpath: None,
    };
    commands::add::run(&add_args).await.unwrap();

    let update_args = UpdateArgs {
        name: Some("update-pkg".to_string()),
    };
    commands::update::run(&update_args).await.unwrap();

    let manifest_content =
        std::fs::read_to_string(packages_dir().join("update-pkg/manifest.yaml")).unwrap();
    assert!(
        manifest_content.contains("v2.0.0"),
        "Manifest should record v2.0.0 after update, got:\n{manifest_content}"
    );
}

#[tokio::test]
async fn test_e2e_check_updates_finds_newer() {
    let _guard = ENV_MUTEX.lock().await;
    let tmp = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::new("HARNX_CONFIG_DIR", tmp.path());

    let (_repo_dir, url) = helpers::create_test_git_repo(
        &[("agents/helper.md", "---\nmodel: test\n---\nHello.")],
        &["v1.0.0", "v2.0.0"],
    );

    let add_args = AddArgs {
        url,
        tag: "v1.0.0".to_string(),
        name: Some("check-pkg".to_string()),
        subpath: None,
    };
    commands::add::run(&add_args).await.unwrap();

    let check_args = CheckForUpdatesArgs {
        name: Some("check-pkg".to_string()),
    };
    commands::check_updates::run(&check_args).await.unwrap();
}

#[tokio::test]
async fn test_e2e_list_output() {
    let _guard = ENV_MUTEX.lock().await;
    let tmp = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::new("HARNX_CONFIG_DIR", tmp.path());

    let (_repo1, url1) =
        helpers::create_test_git_repo(&[("agents/a.md", "---\nmodel: test\n---\nA")], &["v1.0.0"]);
    let (_repo2, url2) =
        helpers::create_test_git_repo(&[("agents/b.md", "---\nmodel: test\n---\nB")], &["v1.0.0"]);

    commands::add::run(&AddArgs {
        url: url1,
        tag: "v1.0.0".to_string(),
        name: Some("list-alpha".to_string()),
        subpath: None,
    })
    .await
    .unwrap();
    commands::add::run(&AddArgs {
        url: url2,
        tag: "v1.0.0".to_string(),
        name: Some("list-beta".to_string()),
        subpath: None,
    })
    .await
    .unwrap();

    commands::list::run().await.unwrap();

    let agents = list_agents();
    assert!(agents.iter().any(|a| a.starts_with("list-alpha/")));
    assert!(agents.iter().any(|a| a.starts_with("list-beta/")));
}

#[tokio::test]
async fn test_e2e_remove_cleans_runtime() {
    let _guard = ENV_MUTEX.lock().await;
    let tmp = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::new("HARNX_CONFIG_DIR", tmp.path());

    let (_repo, url) = helpers::create_test_git_repo(
        &[("agents/worker.md", "---\nmodel: test\n---\nWork.")],
        &["v1.0.0"],
    );

    commands::add::run(&AddArgs {
        url,
        tag: "v1.0.0".to_string(),
        name: Some("rm-pkg".to_string()),
        subpath: None,
    })
    .await
    .unwrap();

    assert!(list_agents().contains(&"rm-pkg/worker".to_string()));

    commands::remove::run(&RemoveArgs {
        name: "rm-pkg".to_string(),
    })
    .await
    .unwrap();

    assert!(!list_agents().contains(&"rm-pkg/worker".to_string()));
    assert!(!packages_dir().join("rm-pkg").exists());
}
