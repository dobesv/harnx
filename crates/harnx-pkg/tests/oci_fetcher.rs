use std::collections::{HashMap, HashSet};
use std::io::Write as _;
use std::sync::Arc;

use axum::{
    body::Body,
    extract::{Path as AxumPath, Query, Request, State},
    http::{HeaderMap, HeaderValue, StatusCode},
    middleware::{self, Next},
    response::Response,
    routing::{get, head, patch, post, put},
    Json, Router,
};
use bytes::Bytes;
use harnx_pkg::fetch::{oci::OciFetcher, PackageFetcher};
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
    let server = TestRegistry::new().await;
    let image = format!("localhost:{}/harnx-test-pkg", server.port());

    push_test_package(
        &server.registry_host(),
        &image,
        "v1.0.0",
        &[("agents/foo.md", "---\nmodel: test\n---\nHello")],
    )
    .await;

    let fetcher = OciFetcher;
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
    let server = TestRegistry::new().await;
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

    let fetcher = OciFetcher;
    let versions = fetcher.list_tags(&format!("oci://{image}")).await.unwrap();

    assert_eq!(versions, vec![Version::new(1, 0, 0), Version::new(2, 0, 0)]);
}

#[tokio::test]
async fn test_oci_fetch_non_semver_rejected() {
    let fetcher = OciFetcher;
    let result = fetcher
        .fetch("oci://localhost:5000/harnx/test-pkg", "latest", None)
        .await;
    assert!(result.is_err(), "Expected error for non-semver tag");
}

async fn push_test_package(registry_host: &str, image: &str, tag: &str, files: &[(&str, &str)]) {
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
        .push(
            &reference,
            &layers,
            config,
            &RegistryAuth::Anonymous,
            Some(image_manifest),
        )
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
    _server: JoinHandle<()>,
    registry_host: String,
    port: u16,
}

impl TestRegistry {
    async fn new() -> Self {
        let state = Arc::new(RegistryState::default());
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
            .layer(middleware::from_fn(log_request))
            .with_state(state);

        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        Self {
            _server: server,
            registry_host,
            port,
        }
    }

    fn port(&self) -> u16 {
        self.port
    }

    fn registry_host(&self) -> String {
        self.registry_host.clone()
    }
}

#[derive(Default)]
struct RegistryState {
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

async fn log_request(req: Request, next: Next) -> Response {
    eprintln!("test registry {} {}", req.method(), req.uri());
    next.run(req).await
}
