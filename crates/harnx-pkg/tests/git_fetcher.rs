mod helpers;

use harnx_pkg::fetch::{git::GitFetcher, PackageFetcher};
use helpers::create_test_git_repo;
use semver::Version;

#[tokio::test]
async fn test_git_fetch_basic() {
    let (_repo_dir, url) = create_test_git_repo(
        &[("agents/foo.md", "---\nmodel: test\n---\nHello")],
        &["v1.0.0"],
    );
    let fetcher = GitFetcher;
    let result = fetcher.fetch(&url, "v1.0.0", None).await.unwrap();
    assert!(result.dir.path().join("agents/foo.md").exists());
    assert!(!result.resolved_id.is_empty());
}

#[tokio::test]
async fn test_git_fetch_subpath() {
    let (_repo_dir, url) = create_test_git_repo(
        &[
            ("agents/foo.md", "agent content"),
            ("other/bar.txt", "other content"),
        ],
        &["v1.0.0"],
    );
    let fetcher = GitFetcher;
    let result = fetcher.fetch(&url, "v1.0.0", Some("agents")).await.unwrap();
    assert!(result.dir.path().join("foo.md").exists());
    assert!(!result.dir.path().join("other").exists());
}

#[tokio::test]
async fn test_git_list_tags() {
    let (_repo_dir, url) = create_test_git_repo(
        &[("README.md", "hello")],
        &["v1.0.0", "v2.0.0", "notver", "v1"],
    );
    let fetcher = GitFetcher;
    let versions = fetcher.list_tags(&url).await.unwrap();
    assert_eq!(versions, vec![Version::new(1, 0, 0), Version::new(2, 0, 0)]);
}

#[tokio::test]
async fn test_git_fetch_bad_tag() {
    let (_repo_dir, url) = create_test_git_repo(&[("README.md", "hello")], &["v1.0.0"]);
    let fetcher = GitFetcher;
    let result = fetcher.fetch(&url, "v999.0.0", None).await;
    assert!(result.is_err(), "Expected error for missing tag");
}

#[tokio::test]
async fn test_git_fetch_non_semver_tag_rejected() {
    let (_repo_dir, url) = create_test_git_repo(&[("README.md", "hello")], &["v1.0.0"]);
    let fetcher = GitFetcher;
    let result = fetcher.fetch(&url, "latest", None).await;
    assert!(result.is_err(), "Expected error for non-semver tag");
}
