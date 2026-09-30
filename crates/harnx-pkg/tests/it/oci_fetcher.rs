use std::collections::{HashMap, HashSet};
use std::io::Write as _;
use std::sync::Arc;

use axum::{
    body::Body,
    extract::{Path as AxumPath, Query, Request, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    middleware::{self, Next},
    response::Response,
    routing::{get, head, patch, post, put},
    Json, Router,
};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use bytes::Bytes;
use harnx_pkg::{
    credentials::resolve_oci_auth,
    fetch::{oci::OciFetcher, PackageFetcher},
};

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
use oci_client::{
    client::{ClientConfig, ClientProtocol, Config, ImageLayer},
    manifest,
    secrets::RegistryAuth,
    Client, Reference,
};
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::{net::TcpListener, sync::RwLock, task::JoinHandle};
use uuid::Uuid;

#[tokio::test]
async fn test_oci_fetch_basic() {
    let server = TestRegistry::new().start().await;
    let image = format!("localhost:{}/harnx-test-pkg", server.port());

    push_test_package(
        &server.registry_host(),
        &image,
        "v1.0.0",
        &[("agents/foo.md", "---\nmodel: test\n---\nHello")],
    )
    .await;

    let fetcher = OciFetcher::anonymous();
    let result = fetcher
        .fetch(&format!("oci://{image}"), "v1.0.0", None)
        .await
        .unwrap();

    assert!(result.dir.path().join("agents/foo.md").exists());
    assert!(!result.resolved_id.is_empty());
    assert_eq!(result.tag, "v1.0.0");
}

#[tokio::test]
async fn test_oci_list_tags() {
    let server = TestRegistry::new().start().await;
    let image = format!("localhost:{}/harnx-test-pkg", server.port());

    push_test_package(
        &server.registry_host(),
        &image,
        "v1.0.0",
        &[("README.md", "one")],
    )
    .await;
    push_test_package(
        &server.registry_host(),
        &image,
        "v2.0.0",
        &[("README.md", "two")],
    )
    .await;
    push_test_package(
        &server.registry_host(),
        &image,
        "latest",
        &[("README.md", "latest")],
    )
    .await;

    let fetcher = OciFetcher::anonymous();
    let versions = fetcher.list_tags(&format!("oci://{image}")).await.unwrap();

    assert_eq!(versions, vec![Version::new(1, 0, 0), Version::new(2, 0, 0)]);
}

#[tokio::test]
async fn test_oci_fetch_non_semver_rejected() {
    let fetcher = OciFetcher::anonymous();
    let result = fetcher
        .fetch("oci://localhost:5000/harnx/test-pkg", "latest", None)
        .await;
    assert!(result.is_err(), "Expected error for non-semver tag");
}

#[tokio::test]
async fn test_oci_fetch_with_basic_auth() {
    let server = TestRegistry::new()
        .with_auth("testuser", "testpass")
        .start()
        .await;
    let image = format!("localhost:{}/harnx-private-pkg", server.port());

    let push_auth = RegistryAuth::Basic("testuser".to_string(), "testpass".to_string());
    push_test_package_with_auth(
        &server.registry_host(),
        &image,
        "v1.0.0",
        &[("agents/private.md", "---\nmodel: test\n---\nPrivate")],
        &push_auth,
    )
    .await;

    let config_dir = tempfile::tempdir().unwrap();
    write_registry_config(
        config_dir.path(),
        &server.registry_host(),
        "testuser",
        "testpass",
    );

    let _env_lock = ENV_MUTEX.lock().await;
    let _env = EnvGuard::new("HARNX_CONFIG_DIR", config_dir.path());
    let auth = resolve_oci_auth(&format!("oci://{image}")).await.unwrap();

    let fetcher = OciFetcher::with_auth(auth);
    let result = fetcher
        .fetch(&format!("oci://{image}"), "v1.0.0", None)
        .await
        .unwrap();

    assert!(result.dir.path().join("agents/private.md").exists());
    assert_eq!(
        std::fs::read_to_string(result.dir.path().join("agents/private.md")).unwrap(),
        "---\nmodel: test\n---\nPrivate"
    );
}

#[tokio::test]
async fn test_oci_fetch_wrong_auth_fails() {
    let server = TestRegistry::new()
        .with_auth("testuser", "testpass")
        .start()
        .await;
    let image = format!("localhost:{}/harnx-private-pkg", server.port());

    let push_auth = RegistryAuth::Basic("testuser".to_string(), "testpass".to_string());
    push_test_package_with_auth(
        &server.registry_host(),
        &image,
        "v1.0.0",
        &[("agents/private.md", "---\nmodel: test\n---\nPrivate")],
        &push_auth,
    )
    .await;

    let config_dir = tempfile::tempdir().unwrap();
    write_registry_config(
        config_dir.path(),
        &server.registry_host(),
        "wronguser",
        "wrongpass",
    );

    let _env_lock = ENV_MUTEX.lock().await;
    let _env = EnvGuard::new("HARNX_CONFIG_DIR", config_dir.path());
    let auth = resolve_oci_auth(&format!("oci://{image}")).await.unwrap();

    let fetcher = OciFetcher::with_auth(auth);
    let result = fetcher
        .fetch(&format!("oci://{image}"), "v1.0.0", None)
        .await;

    assert!(
        result.is_err(),
        "Expected fetch to fail with wrong basic auth"
    );
}

#[tokio::test]
async fn test_oci_fetch_anon_against_private_fails() {
    let server = TestRegistry::new()
        .with_auth("testuser", "testpass")
        .start()
        .await;
    let image = format!("localhost:{}/harnx-private-pkg", server.port());

    let push_auth = RegistryAuth::Basic("testuser".to_string(), "testpass".to_string());
    push_test_package_with_auth(
        &server.registry_host(),
        &image,
        "v1.0.0",
        &[("agents/private.md", "---\nmodel: test\n---\nPrivate")],
        &push_auth,
    )
    .await;

    let fetcher = OciFetcher::anonymous();
    let result = fetcher
        .fetch(&format!("oci://{image}"), "v1.0.0", None)
        .await;

    assert!(
        result.is_err(),
        "Expected anonymous fetch to fail against private registry"
    );
}

fn write_registry_config(
    config_dir: &std::path::Path,
    registry_url: &str,
    username: &str,
    password: &str,
) {
    let repo_dir = config_dir.join("package_repos");
    std::fs::create_dir_all(&repo_dir).unwrap();
    std::fs::write(
        repo_dir.join("test-registry.yaml"),
        format!(
            "url: \"{registry_url}\"\nusername:\n  value: \"{username}\"\npassword:\n  value: \"{password}\"\n"
        ),
    )
    .unwrap();
}

async fn push_test_package(registry_host: &str, image: &str, tag: &str, files: &[(&str, &str)]) {
    push_test_package_with_auth(registry_host, image, tag, files, &RegistryAuth::Anonymous).await;
}

async fn push_test_package_with_auth(
    registry_host: &str,
    image: &str,
    tag: &str,
    files: &[(&str, &str)],
    auth: &RegistryAuth,
) {
    let archive = build_tar_gz(files);
    let reference: Reference = format!("{image}:{tag}").parse().unwrap();
    let client = Client::new(ClientConfig {
        protocol: ClientProtocol::Http,
        use_monolithic_push: true,
        ..Default::default()
    });

    let layers = vec![ImageLayer::new(
        archive,
        manifest::IMAGE_LAYER_GZIP_MEDIA_TYPE.to_string(),
        None,
    )];
    let config = Config::oci_v1(br#"{}"#.to_vec(), None);
    let image_manifest = manifest::OciImageManifest::build(&layers, &config, None);

    client
        .push(&reference, &layers, config, auth, Some(image_manifest))
        .await
        .unwrap_or_else(|err| panic!("push failed for {registry_host}: {err:?}"));
}

fn build_tar_gz(files: &[(&str, &str)]) -> Bytes {
    let mut tar_bytes = Vec::new();
    {
        let mut builder = tar::Builder::new(&mut tar_bytes);
        for (rel, content) in files {
            let data = content.as_bytes();
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append_data(&mut header, *rel, data).unwrap();
        }
        builder.finish().unwrap();
    }

    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(&tar_bytes).unwrap();
    Bytes::from(gz.finish().unwrap())
}

struct TestRegistry {
    expected_auth: Option<(String, String)>,
}

impl TestRegistry {
    fn new() -> Self {
        Self {
            expected_auth: None,
        }
    }

    fn with_auth(mut self, username: &str, password: &str) -> Self {
        self.expected_auth = Some((username.to_string(), password.to_string()));
        self
    }

    async fn start(self) -> RunningTestRegistry {
        let state = Arc::new(RegistryState {
            expected_auth: self.expected_auth,
            ..Default::default()
        });
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let registry_host = format!("localhost:{port}");

        let app = Router::new()
            .route("/v2/", get(api_version))
            .route("/v2/{name}/blobs/{digest}", head(check_blob))
            .route("/v2/{name}/blobs/{digest}", get(get_blob))
            .route("/v2/{name}/blobs/uploads/", post(start_upload))
            .route("/v2/{name}/blobs/uploads/{uuid}", patch(upload_chunk))
            .route("/v2/{name}/blobs/uploads/{uuid}", put(finish_upload))
            .route("/v2/{name}/manifests/{reference}", put(put_manifest))
            .route("/v2/{name}/manifests/{reference}", get(get_manifest))
            .route("/v2/{name}/manifests/{reference}", head(check_manifest))
            .route("/v2/{name}/tags/list", get(list_tags))
            .layer(middleware::from_fn_with_state(
                Arc::clone(&state),
                log_request,
            ))
            .with_state(state);

        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        RunningTestRegistry {
            _server: server,
            registry_host,
            port,
        }
    }
}

struct RunningTestRegistry {
    _server: JoinHandle<()>,
    registry_host: String,
    port: u16,
}

impl RunningTestRegistry {
    fn port(&self) -> u16 {
        self.port
    }

    fn registry_host(&self) -> String {
        self.registry_host.clone()
    }
}

#[derive(Default)]
struct RegistryState {
    expected_auth: Option<(String, String)>,
    blobs: RwLock<HashMap<String, Bytes>>,
    uploads: RwLock<HashMap<String, Vec<u8>>>,
    manifests: RwLock<HashMap<(String, String), ManifestEntry>>,
    repo_tags: RwLock<HashMap<String, HashSet<String>>>,
}

#[derive(Clone)]
struct ManifestEntry {
    body: Bytes,
    digest: String,
}

#[derive(Serialize)]
struct ApiVersion {
    version: String,
}

#[derive(Deserialize)]
struct UploadParams {
    digest: Option<String>,
}

#[derive(Deserialize)]
struct TagListQuery {
    n: Option<usize>,
    last: Option<String>,
}

#[derive(Serialize)]
struct TagListResponse {
    name: String,
    tags: Vec<String>,
}

async fn api_version() -> Json<ApiVersion> {
    Json(ApiVersion {
        version: "registry/2.0".to_string(),
    })
}

async fn check_blob(
    State(state): State<Arc<RegistryState>>,
    AxumPath((_name, digest)): AxumPath<(String, String)>,
) -> (StatusCode, HeaderMap) {
    let blobs = state.blobs.read().await;
    if let Some(blob) = blobs.get(&digest) {
        let mut headers = HeaderMap::new();
        headers.insert(
            "Content-Length",
            HeaderValue::from_str(&blob.len().to_string()).unwrap(),
        );
        (StatusCode::OK, headers)
    } else {
        (StatusCode::NOT_FOUND, HeaderMap::new())
    }
}

async fn get_blob(
    State(state): State<Arc<RegistryState>>,
    AxumPath((_name, digest)): AxumPath<(String, String)>,
) -> Response {
    let blobs = state.blobs.read().await;
    if let Some(blob) = blobs.get(&digest) {
        Response::builder()
            .status(StatusCode::OK)
            .body(Body::from(blob.clone()))
            .unwrap()
    } else {
        Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::empty())
            .unwrap()
    }
}

async fn start_upload(
    State(state): State<Arc<RegistryState>>,
    AxumPath(name): AxumPath<String>,
) -> (StatusCode, HeaderMap) {
    let uuid = Uuid::new_v4().to_string();
    state.uploads.write().await.insert(uuid.clone(), Vec::new());

    let mut headers = HeaderMap::new();
    headers.insert(
        "Location",
        HeaderValue::from_str(&format!("/v2/{name}/blobs/uploads/{uuid}")).unwrap(),
    );
    (StatusCode::ACCEPTED, headers)
}

async fn upload_chunk(
    State(state): State<Arc<RegistryState>>,
    AxumPath((name, uuid)): AxumPath<(String, String)>,
    body: Bytes,
) -> (StatusCode, HeaderMap) {
    let mut uploads = state.uploads.write().await;
    let Some(upload) = uploads.get_mut(&uuid) else {
        return (StatusCode::NOT_FOUND, HeaderMap::new());
    };
    upload.extend_from_slice(&body);

    let mut headers = HeaderMap::new();
    headers.insert(
        "Location",
        HeaderValue::from_str(&format!("/v2/{name}/blobs/uploads/{uuid}")).unwrap(),
    );
    headers.insert(
        "Range",
        HeaderValue::from_str(&format!("0-{}", upload.len().saturating_sub(1))).unwrap(),
    );
    headers.insert("Docker-Upload-UUID", HeaderValue::from_str(&uuid).unwrap());
    (StatusCode::ACCEPTED, headers)
}

async fn finish_upload(
    State(state): State<Arc<RegistryState>>,
    AxumPath((name, uuid)): AxumPath<(String, String)>,
    Query(params): Query<UploadParams>,
    body: Bytes,
) -> (StatusCode, HeaderMap) {
    let digest = params.digest.expect("digest query param required");
    let mut uploads = state.uploads.write().await;
    let mut data = uploads.remove(&uuid).unwrap_or_default();
    if !body.is_empty() {
        data.extend_from_slice(&body);
    }
    state
        .blobs
        .write()
        .await
        .insert(digest.clone(), Bytes::from(data));

    let mut headers = HeaderMap::new();
    headers.insert(
        "Location",
        HeaderValue::from_str(&format!("/v2/{name}/blobs/{digest}")).unwrap(),
    );
    headers.insert(
        "Docker-Content-Digest",
        HeaderValue::from_str(&digest).unwrap(),
    );
    (StatusCode::CREATED, headers)
}

async fn put_manifest(
    State(state): State<Arc<RegistryState>>,
    AxumPath((name, reference)): AxumPath<(String, String)>,
    body: Bytes,
) -> (StatusCode, HeaderMap) {
    let hash = Sha256::digest(&body);
    let digest = format!(
        "sha256:{}",
        hash.iter().map(|b| format!("{b:02x}")).collect::<String>()
    );
    state.manifests.write().await.insert(
        (name.clone(), reference.clone()),
        ManifestEntry {
            body: body.clone(),
            digest: digest.clone(),
        },
    );
    state
        .repo_tags
        .write()
        .await
        .entry(name.clone())
        .or_default()
        .insert(reference);

    let mut headers = HeaderMap::new();
    headers.insert(
        "Docker-Content-Digest",
        HeaderValue::from_str(&digest).unwrap(),
    );
    headers.insert(
        "Location",
        HeaderValue::from_str(&format!("/v2/{name}/manifests/{digest}")).unwrap(),
    );
    (StatusCode::CREATED, headers)
}

async fn get_manifest(
    State(state): State<Arc<RegistryState>>,
    AxumPath((name, reference)): AxumPath<(String, String)>,
) -> Response {
    let manifests = state.manifests.read().await;
    let Some(entry) = manifests.get(&(name, reference)) else {
        return Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::empty())
            .unwrap();
    };

    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", manifest::IMAGE_MANIFEST_MEDIA_TYPE)
        .header("Docker-Content-Digest", &entry.digest)
        .body(Body::from(entry.body.clone()))
        .unwrap()
}

async fn check_manifest(
    State(state): State<Arc<RegistryState>>,
    AxumPath((name, reference)): AxumPath<(String, String)>,
) -> (StatusCode, HeaderMap) {
    let manifests = state.manifests.read().await;
    if let Some(entry) = manifests.get(&(name, reference)) {
        let mut headers = HeaderMap::new();
        headers.insert(
            "Docker-Content-Digest",
            HeaderValue::from_str(&entry.digest).unwrap(),
        );
        headers.insert(
            "Content-Length",
            HeaderValue::from_str(&entry.body.len().to_string()).unwrap(),
        );
        (StatusCode::OK, headers)
    } else {
        (StatusCode::NOT_FOUND, HeaderMap::new())
    }
}

async fn list_tags(
    State(state): State<Arc<RegistryState>>,
    AxumPath(name): AxumPath<String>,
    Query(query): Query<TagListQuery>,
) -> Json<TagListResponse> {
    let repo_tags = state.repo_tags.read().await;
    let mut tags = repo_tags
        .get(&name)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .collect::<Vec<_>>();
    tags.sort();

    if let Some(last) = query.last {
        if let Some(index) = tags.iter().position(|tag| tag == &last) {
            tags = tags.into_iter().skip(index + 1).collect();
        }
    }
    if let Some(n) = query.n {
        tags.truncate(n);
    }

    Json(TagListResponse { name, tags })
}

async fn log_request(
    State(state): State<Arc<RegistryState>>,
    req: Request,
    next: Next,
) -> Response {
    eprintln!("test registry {} {}", req.method(), req.uri());

    let Some((expected_username, expected_password)) = state.expected_auth.as_ref() else {
        return next.run(req).await;
    };

    let Some(auth_header) = req.headers().get(header::AUTHORIZATION) else {
        return unauthorized_response();
    };

    let Ok(auth_header) = auth_header.to_str() else {
        return unauthorized_response();
    };

    let Some(encoded) = auth_header.strip_prefix("Basic ") else {
        return unauthorized_response();
    };

    let Ok(decoded) = STANDARD.decode(encoded) else {
        return unauthorized_response();
    };

    let Ok(decoded) = String::from_utf8(decoded) else {
        return unauthorized_response();
    };

    let Some((username, password)) = decoded.split_once(":") else {
        return unauthorized_response();
    };

    if username != expected_username || password != expected_password {
        return unauthorized_response();
    }

    next.run(req).await
}

fn unauthorized_response() -> Response {
    Response::builder()
        .status(StatusCode::UNAUTHORIZED)
        .header(header::WWW_AUTHENTICATE, "Basic realm=\"test\"")
        .body(Body::empty())
        .unwrap()
}
