mod helpers;

use harnx_pkg::cli::{AddArgs, CheckForUpdatesArgs, UpdateArgs};
use harnx_pkg::commands;
use helpers::create_test_git_repo;

static ENV_MUTEX: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[tokio::test]
async fn test_add_command_success() {
    let _guard = ENV_MUTEX.lock().await;
    let tmp = tempfile::TempDir::new().unwrap();
    unsafe {
        std::env::set_var("HARNX_CONFIG_DIR", tmp.path());
    }

    let (_repo, url) = create_test_git_repo(
        &[("agents/helper.md", "---\nmodel: test\n---\nHello")],
        &["v1.0.0"],
    );
    let args = AddArgs {
        url,
        tag: "v1.0.0".to_string(),
        name: Some("testpkg".to_string()),
        subpath: None,
    };
    commands::add::run(&args).await.unwrap();
    assert!(tmp.path().join("packages/testpkg/manifest.yaml").exists());
    assert!(tmp
        .path()
        .join("packages/testpkg/agents/helper.md")
        .exists());

    unsafe {
        std::env::remove_var("HARNX_CONFIG_DIR");
    }
}

#[tokio::test]
async fn test_add_command_bad_tag() {
    let _guard = ENV_MUTEX.lock().await;
    let tmp = tempfile::TempDir::new().unwrap();
    unsafe {
        std::env::set_var("HARNX_CONFIG_DIR", tmp.path());
    }

    let (_repo, url) = create_test_git_repo(
        &[("agents/helper.md", "---\nmodel: test\n---\nHello")],
        &["v1.0.0"],
    );
    let args = AddArgs {
        url,
        tag: "notver".to_string(),
        name: Some("testpkg".to_string()),
        subpath: None,
    };
    let result = commands::add::run(&args).await;
    assert!(result.is_err(), "Expected error for bad semver tag");

    unsafe {
        std::env::remove_var("HARNX_CONFIG_DIR");
    }
}

#[tokio::test]
async fn test_add_command_duplicate() {
    let _guard = ENV_MUTEX.lock().await;
    let tmp = tempfile::TempDir::new().unwrap();
    unsafe {
        std::env::set_var("HARNX_CONFIG_DIR", tmp.path());
    }

    let (_repo, url) = create_test_git_repo(
        &[("agents/helper.md", "---\nmodel: test\n---\nHello")],
        &["v1.0.0"],
    );
    let args = AddArgs {
        url: url.clone(),
        tag: "v1.0.0".to_string(),
        name: Some("duplpkg".to_string()),
        subpath: None,
    };
    commands::add::run(&args).await.unwrap();

    // Try adding again - should fail
    let args2 = AddArgs {
        url,
        tag: "v1.0.0".to_string(),
        name: Some("duplpkg".to_string()),
        subpath: None,
    };
    let result = commands::add::run(&args2).await;
    assert!(result.is_err(), "Expected error for duplicate install");

    unsafe {
        std::env::remove_var("HARNX_CONFIG_DIR");
    }
}

#[tokio::test]
async fn test_check_updates_newer_available() {
    let _guard = ENV_MUTEX.lock().await;
    let tmp = tempfile::TempDir::new().unwrap();
    unsafe {
        std::env::set_var("HARNX_CONFIG_DIR", tmp.path());
    }

    // Repo has both v1.0.0 and v2.0.0
    let (_repo, url) = create_test_git_repo(
        &[("agents/helper.md", "---\nmodel: test\n---\nHello")],
        &["v1.0.0", "v2.0.0"],
    );

    // Install v1.0.0
    let args = AddArgs {
        url: url.clone(),
        tag: "v1.0.0".to_string(),
        name: Some("updpkg".to_string()),
        subpath: None,
    };
    commands::add::run(&args).await.unwrap();

    // Check for updates
    let check_args = CheckForUpdatesArgs {
        name: Some("updpkg".to_string()),
    };
    commands::check_updates::run(&check_args).await.unwrap();
    // Output says "update available v1.0.0 → v2.0.0"
    // We can verify the manifest is still at v1.0.0
    let manifest = harnx_pkg::install::load_manifest("updpkg").unwrap();
    match &manifest.source {
        harnx_core::package::PackageSource::Git { tag, .. } => {
            assert_eq!(tag, "v1.0.0");
        }
        _ => panic!("Expected Git source"),
    }

    unsafe {
        std::env::remove_var("HARNX_CONFIG_DIR");
    }
}

#[tokio::test]
async fn test_check_updates_up_to_date() {
    let _guard = ENV_MUTEX.lock().await;
    let tmp = tempfile::TempDir::new().unwrap();
    unsafe {
        std::env::set_var("HARNX_CONFIG_DIR", tmp.path());
    }

    // Repo has only v1.0.0
    let (_repo, url) = create_test_git_repo(
        &[("agents/helper.md", "---\nmodel: test\n---\nHello")],
        &["v1.0.0"],
    );

    // Install v1.0.0
    let args = AddArgs {
        url: url.clone(),
        tag: "v1.0.0".to_string(),
        name: Some("uptodate".to_string()),
        subpath: None,
    };
    commands::add::run(&args).await.unwrap();

    // Check for updates - should say up to date
    let check_args = CheckForUpdatesArgs {
        name: Some("uptodate".to_string()),
    };
    commands::check_updates::run(&check_args).await.unwrap();
    // Output says "up to date (v1.0.0)"

    unsafe {
        std::env::remove_var("HARNX_CONFIG_DIR");
    }
}

#[tokio::test]
async fn test_update_upgrades_package() {
    let _guard = ENV_MUTEX.lock().await;
    let tmp = tempfile::TempDir::new().unwrap();
    unsafe {
        std::env::set_var("HARNX_CONFIG_DIR", tmp.path());
    }

    // Repo has both v1.0.0 and v2.0.0
    let (_repo, url) = create_test_git_repo(
        &[("agents/helper.md", "---\nmodel: test\n---\nHello")],
        &["v1.0.0", "v2.0.0"],
    );

    // Install v1.0.0
    let args = AddArgs {
        url: url.clone(),
        tag: "v1.0.0".to_string(),
        name: Some("upgradepkg".to_string()),
        subpath: None,
    };
    commands::add::run(&args).await.unwrap();

    // Verify initial install at v1.0.0
    let manifest = harnx_pkg::install::load_manifest("upgradepkg").unwrap();
    match &manifest.source {
        harnx_core::package::PackageSource::Git { tag, .. } => {
            assert_eq!(tag, "v1.0.0");
        }
        _ => panic!("Expected Git source"),
    }

    // Update
    let update_args = UpdateArgs {
        name: Some("upgradepkg".to_string()),
    };
    commands::update::run(&update_args).await.unwrap();

    // Verify manifest now says v2.0.0
    let manifest = harnx_pkg::install::load_manifest("upgradepkg").unwrap();
    match &manifest.source {
        harnx_core::package::PackageSource::Git { tag, .. } => {
            assert_eq!(tag, "v2.0.0", "Expected tag to be upgraded to v2.0.0");
        }
        _ => panic!("Expected Git source"),
    }

    unsafe {
        std::env::remove_var("HARNX_CONFIG_DIR");
    }
}
