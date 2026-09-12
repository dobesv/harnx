use crate::lifecycle::{
    CreateSandboxClaim, SandboxApi, SandboxCondition, SandboxRecord, LAST_ACTIVITY_ANNOTATION,
};
use crate::policy::{operation_metric, FailureKind};
use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use kube::api::{
    ApiResource, DeleteParams, DynamicObject, GroupVersionKind, ListParams, Patch, PatchParams,
    PostParams,
};
use kube::{Api, Client, ResourceExt};
use serde_json::{json, Value};
use std::error::Error as _;
use std::future::Future;
use std::time::Duration;

const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_COMPOSITE_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug)]
pub(crate) struct KubernetesRequestError {
    pub kind: FailureKind,
    pub retry_after: Option<Duration>,
    status_code: Option<u16>,
    status_reason: Option<String>,
    status_message: Option<String>,
    resource_name: Option<String>,
    operation: &'static str,
    source: anyhow::Error,
}

impl std::fmt::Display for KubernetesRequestError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "Kubernetes {}: {:#}",
            self.operation, self.source
        )
    }
}

impl std::error::Error for KubernetesRequestError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}

pub(crate) fn kubernetes_failure(error: &anyhow::Error) -> (FailureKind, Option<Duration>) {
    error
        .chain()
        .find_map(|source| source.downcast_ref::<KubernetesRequestError>())
        .map_or((FailureKind::Transport, None), |error| {
            (error.kind, error.retry_after)
        })
}

#[derive(Clone)]
pub struct KubernetesSandboxApi {
    claims: Api<DynamicObject>,
    sandboxes: Api<DynamicObject>,
    claim_resource: ApiResource,
    request_timeout: Duration,
    composite_timeout: Duration,
}

impl KubernetesSandboxApi {
    pub fn new(client: Client, namespace: &str) -> Self {
        Self::with_timeouts(
            client,
            namespace,
            DEFAULT_REQUEST_TIMEOUT,
            DEFAULT_COMPOSITE_TIMEOUT,
        )
    }

    pub fn with_timeouts(
        client: Client,
        namespace: &str,
        request_timeout: Duration,
        composite_timeout: Duration,
    ) -> Self {
        let claim = ApiResource::from_gvk_with_plural(
            &GroupVersionKind::gvk("extensions.agents.x-k8s.io", "v1alpha1", "SandboxClaim"),
            "sandboxclaims",
        );
        let sandbox = ApiResource::from_gvk_with_plural(
            &GroupVersionKind::gvk("agents.x-k8s.io", "v1alpha1", "Sandbox"),
            "sandboxes",
        );
        Self {
            claims: Api::namespaced_with(client.clone(), namespace, &claim),
            sandboxes: Api::namespaced_with(client, namespace, &sandbox),
            claim_resource: claim,
            request_timeout,
            composite_timeout,
        }
    }

    async fn request<T>(
        &self,
        operation: &'static str,
        future: impl Future<Output = std::result::Result<T, kube::Error>>,
    ) -> Result<T> {
        match tokio::time::timeout(self.request_timeout, future).await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(error)) => Err(request_error_from_kube(operation, error)),
            Err(error) => Err(KubernetesRequestError {
                kind: FailureKind::Timeout,
                retry_after: None,
                status_code: None,
                status_reason: None,
                status_message: None,
                resource_name: None,
                operation,
                source: anyhow::Error::new(error),
            }
            .into()),
        }
    }

    async fn composite<T>(
        &self,
        operation: &'static str,
        future: impl Future<Output = Result<T>>,
    ) -> Result<T> {
        match tokio::time::timeout(self.composite_timeout, future).await {
            Ok(result) => result,
            Err(error) => Err(KubernetesRequestError {
                kind: FailureKind::Timeout,
                retry_after: None,
                status_code: None,
                status_reason: None,
                status_message: None,
                resource_name: None,
                operation,
                source: anyhow::Error::new(error),
            }
            .into()),
        }
    }

    async fn claim(&self, id: &str) -> Result<Option<DynamicObject>> {
        match self.request("get_claim", self.claims.get(id)).await {
            Ok(claim) => Ok(Some(claim)),
            Err(error) if is_absent_resource(&error, id) => Ok(None),
            Err(error) => Err(error).with_context(|| format!("get SandboxClaim '{id}'")),
        }
    }

    async fn record(&self, claim: DynamicObject) -> Result<SandboxRecord> {
        let id = claim.name_any();
        let sandbox_name = pointer_string(&claim.data, "/status/sandbox/name");
        let pod_ips = claim
            .data
            .pointer("/status/sandbox/podIPs")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect();
        let replicas = if let Some(name) = sandbox_name.as_deref() {
            match self.request("get_sandbox", self.sandboxes.get(name)).await {
                Ok(sandbox) => sandbox
                    .data
                    .pointer("/spec/replicas")
                    .and_then(Value::as_i64),
                Err(error) if is_absent_resource(&error, name) => None,
                Err(error) => return Err(error).with_context(|| format!("get Sandbox '{name}'")),
            }
        } else {
            None
        };
        let conditions = claim
            .data
            .pointer("/status/conditions")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|condition| SandboxCondition {
                kind: object_string(condition, "type"),
                status: object_string(condition, "status"),
                reason: object_string(condition, "reason"),
                message: object_string(condition, "message"),
            })
            .collect();
        let shutdown_time = pointer_string(&claim.data, "/spec/lifecycle/shutdownTime")
            .map(|time| parse_time(&time))
            .transpose()?;
        let created_at = claim
            .metadata
            .creation_timestamp
            .as_ref()
            .map(|time| {
                DateTime::<Utc>::from_timestamp(
                    time.0.as_second(),
                    time.0.subsec_nanosecond() as u32,
                )
                .context("SandboxClaim creation timestamp is outside Chrono's supported range")
            })
            .transpose()?;
        let last_activity = last_activity(&claim);
        Ok(SandboxRecord {
            id,
            sandbox_name,
            pod_ips,
            replicas,
            conditions,
            shutdown_time,
            created_at,
            last_activity,
        })
    }

    async fn sandbox_name(&self, id: &str) -> Result<String> {
        self.claim(id)
            .await?
            .with_context(|| format!("sandbox not found: {id}"))?
            .data
            .pointer("/status/sandbox/name")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
            .map(str::to_string)
            .with_context(|| format!("sandbox name not found in claim {id} status"))
    }

    fn record_outcome<T>(operation: &'static str, result: &Result<T>) {
        let outcome = match result {
            Ok(_) => "success",
            Err(error) => match kubernetes_failure(error).0 {
                FailureKind::Timeout => "timeout",
                FailureKind::Permanent | FailureKind::Internal => "permanent_error",
                FailureKind::Transport | FailureKind::RemoteTransient => "transport_error",
            },
        };
        operation_metric("k8s", operation, outcome);
    }
}

fn classify_kube_error(error: &kube::Error) -> FailureKind {
    match error {
        kube::Error::Api(response) => match response.code {
            408 | 429 | 500..=599 => FailureKind::RemoteTransient,
            // Call sites handle expected 404 and create-AlreadyExists before this
            // classification reaches lifecycle retry policy.
            409 => FailureKind::RemoteTransient,
            400 | 401 | 403 | 404 | 405 | 406 | 413 | 415 | 422 => FailureKind::Permanent,
            _ => FailureKind::Permanent,
        },
        kube::Error::HyperError(error) => classify_hyper_error(error),
        kube::Error::Service(error) => classify_error_chain(error.as_ref()),
        kube::Error::ReadEvents(error) => classify_io_error(error),
        kube::Error::SerdeError(_) | kube::Error::FromUtf8(_) => FailureKind::Internal,
        kube::Error::BuildRequest(_)
        | kube::Error::HttpError(_)
        | kube::Error::Discovery(_)
        | kube::Error::InferConfig(_)
        | kube::Error::InferKubeconfig(_)
        | kube::Error::ProxyProtocolUnsupported { .. }
        | kube::Error::ProxyProtocolDisabled { .. }
        | kube::Error::TlsRequired
        | kube::Error::Auth(_)
        | kube::Error::LinesCodecMaxLineLengthExceeded => FailureKind::Permanent,
        _ => FailureKind::Permanent,
    }
}

fn request_error_from_kube(operation: &'static str, error: kube::Error) -> anyhow::Error {
    let (status_code, status_reason, status_message, resource_name) = api_context(&error);
    KubernetesRequestError {
        kind: classify_kube_error(&error),
        retry_after: retry_after(&error),
        status_code,
        status_reason,
        status_message,
        resource_name,
        operation,
        source: anyhow::Error::new(error),
    }
    .into()
}

#[cfg(test)]
pub(crate) fn scripted_kubernetes_error(error: kube::Error) -> anyhow::Error {
    request_error_from_kube("scripted", error)
}

fn classify_error_chain(mut error: &(dyn std::error::Error + 'static)) -> FailureKind {
    loop {
        if let Some(error) = error.downcast_ref::<hyper::Error>() {
            return classify_hyper_error(error);
        }
        if let Some(error) = error.downcast_ref::<std::io::Error>() {
            return classify_io_error(error);
        }
        if let Some(error) = error.downcast_ref::<reqwest::Error>() {
            return if error.is_timeout() {
                FailureKind::Timeout
            } else if error.is_connect() || error.is_request() || error.is_body() {
                FailureKind::Transport
            } else {
                FailureKind::Permanent
            };
        }
        let Some(source) = error.source() else {
            return FailureKind::Permanent;
        };
        error = source;
    }
}

fn classify_hyper_error(error: &hyper::Error) -> FailureKind {
    if error.is_timeout() {
        FailureKind::Timeout
    } else if is_broken_transport(error) {
        FailureKind::Transport
    } else if let Some(source) = error.source() {
        classify_error_chain(source)
    } else {
        FailureKind::Permanent
    }
}

fn is_broken_transport(error: &hyper::Error) -> bool {
    [
        error.is_closed(),
        error.is_incomplete_message(),
        error.is_body_write_aborted(),
    ]
    .into_iter()
    .any(std::convert::identity)
}

fn classify_io_error(error: &std::io::Error) -> FailureKind {
    use std::io::ErrorKind;

    match error.kind() {
        ErrorKind::TimedOut => FailureKind::Timeout,
        ErrorKind::Interrupted
        | ErrorKind::WouldBlock
        | ErrorKind::ConnectionRefused
        | ErrorKind::ConnectionReset
        | ErrorKind::ConnectionAborted
        | ErrorKind::NotConnected
        | ErrorKind::BrokenPipe
        | ErrorKind::AddrNotAvailable
        | ErrorKind::UnexpectedEof => FailureKind::Transport,
        _ => FailureKind::Permanent,
    }
}

fn retry_after(error: &kube::Error) -> Option<Duration> {
    let kube::Error::Api(response) = error else {
        return None;
    };
    response
        .details
        .as_ref()
        .map(|details| details.retry_after_seconds)
        .filter(|seconds| *seconds > 0)
        .map(|seconds| Duration::from_secs(u64::from(seconds)))
}

fn api_context(
    error: &kube::Error,
) -> (Option<u16>, Option<String>, Option<String>, Option<String>) {
    let kube::Error::Api(status) = error else {
        return (None, None, None, None);
    };
    (
        Some(status.code),
        Some(status.reason.clone()),
        Some(status.message.clone()),
        status.details.as_ref().map(|details| details.name.clone()),
    )
}

fn request_error(error: &anyhow::Error) -> Option<&KubernetesRequestError> {
    error
        .chain()
        .find_map(|source| source.downcast_ref::<KubernetesRequestError>())
}

fn is_absent_resource(error: &anyhow::Error, name: &str) -> bool {
    request_error(error).is_some_and(|error| {
        error.status_code == Some(404)
            && (error
                .resource_name
                .as_deref()
                .is_some_and(|resource| resource.is_empty() || resource == name)
                || error.status_message.as_deref() == Some("not found")
                || error
                    .status_message
                    .as_deref()
                    .is_some_and(|message| message.contains(&format!("\"{name}\" not found"))))
    })
}

fn is_already_exists(error: &anyhow::Error, name: &str) -> bool {
    request_error(error).is_some_and(|error| {
        error.status_code == Some(409)
            && error.status_reason.as_deref() == Some("AlreadyExists")
            && error
                .resource_name
                .as_deref()
                .is_none_or(|resource| resource.is_empty() || resource == name)
    })
}

fn last_activity(claim: &DynamicObject) -> Option<DateTime<Utc>> {
    claim
        .annotations()
        .get(LAST_ACTIVITY_ANNOTATION)
        .and_then(|time| match parse_time(time) {
            Ok(time) => Some(time),
            Err(error) => {
                let id = claim.name_any();
                log::warn!(
                    "ignoring invalid {LAST_ACTIVITY_ANNOTATION} annotation on SandboxClaim '{id}': {error:#}"
                );
                None
            }
        })
}

#[async_trait]
impl SandboxApi for KubernetesSandboxApi {
    async fn create_claim(&self, request: CreateSandboxClaim) -> Result<String> {
        let operation = "create_claim";
        let result = async {
            let mut claim = DynamicObject::new(&request.name, &self.claim_resource);
            if let Some(description) = request.description.filter(|value| !value.is_empty()) {
                claim
                    .metadata
                    .annotations
                    .get_or_insert_default()
                    .insert("kubernetes.io/description".to_string(), description);
            }
            claim.data = json!({"spec": {
                "sandboxTemplateRef": {"name": request.template},
                "lifecycle": {"shutdownTime": request.shutdown_time.to_rfc3339(), "shutdownPolicy": "Delete"}
            }});
            match self.request(operation, self.claims.create(&PostParams::default(), &claim)).await {
                Ok(created) => Ok(created.name_any()),
                Err(error) if is_already_exists(&error, &request.name) => Ok(request.name.clone()),
                Err(error) => Err(error).with_context(|| format!("create SandboxClaim '{}'", request.name)),
            }
        }.await;
        Self::record_outcome(operation, &result);
        result
    }

    async fn get(&self, id: &str) -> Result<Option<SandboxRecord>> {
        let operation = "get";
        let result = self
            .composite(operation, async {
                match self.claim(id).await? {
                    Some(claim) => self.record(claim).await.map(Some),
                    None => Ok(None),
                }
            })
            .await;
        Self::record_outcome(operation, &result);
        result
    }

    async fn list(&self) -> Result<Vec<SandboxRecord>> {
        let operation = "list";
        let result = self
            .composite(operation, async {
                let claims = self
                    .request(operation, self.claims.list(&ListParams::default()))
                    .await?;
                let mut records = Vec::with_capacity(claims.items.len());
                for claim in claims.items {
                    let id = claim.name_any();
                    match self.record(claim).await {
                        Ok(record) => records.push(record),
                        Err(error) => log::warn!("skip SandboxClaim '{id}' during list: {error:#}"),
                    }
                }
                Ok(records)
            })
            .await;
        Self::record_outcome(operation, &result);
        result
    }

    async fn set_replicas(&self, id: &str, replicas: i64) -> Result<()> {
        let operation = "set_replicas";
        let result = async {
            let name = self.sandbox_name(id).await?;
            self.request(
                operation,
                self.sandboxes.patch(
                    &name,
                    &PatchParams::default(),
                    &Patch::Merge(json!({"spec": {"replicas": replicas}})),
                ),
            )
            .await
            .with_context(|| format!("set Sandbox '{name}' replicas to {replicas}"))?;
            Ok(())
        }
        .await;
        Self::record_outcome(operation, &result);
        result
    }

    async fn update_shutdown_time(&self, id: &str, shutdown_time: DateTime<Utc>) -> Result<()> {
        let operation = "update_shutdown_time";
        let result = self.request(operation, self.claims.patch(
            id,
            &PatchParams::default(),
            &Patch::Merge(json!({"spec": {"lifecycle": {"shutdownTime": shutdown_time.to_rfc3339()}}})),
        )).await.map(|_| ()).with_context(|| format!("update SandboxClaim '{id}' shutdown time"));
        Self::record_outcome(operation, &result);
        result
    }

    async fn bump_activity(&self, id: &str, now: DateTime<Utc>) -> Result<()> {
        let operation = "bump_activity";
        let result = async {
            let claim = self.claim(id).await?.with_context(|| format!("sandbox not found: {id}"))?;
            if claim.annotations().get(LAST_ACTIVITY_ANNOTATION)
                .and_then(|time| parse_time(time).ok())
                .is_some_and(|last| now - last < chrono::Duration::minutes(1)) {
                return Ok(());
            }
            self.request(operation, self.claims.patch(
                id,
                &PatchParams::default(),
                &Patch::Merge(json!({"metadata": {"annotations": {LAST_ACTIVITY_ANNOTATION: now.to_rfc3339()}}})),
            )).await?;
            Ok(())
        }.await;
        Self::record_outcome(operation, &result);
        result
    }

    async fn delete(&self, id: &str) -> Result<()> {
        let operation = "delete";
        let result = match self
            .request(operation, self.claims.delete(id, &DeleteParams::default()))
            .await
        {
            Ok(_) => Ok(()),
            Err(error) if is_absent_resource(&error, id) => Ok(()),
            Err(error) => Err(error).with_context(|| format!("delete SandboxClaim '{id}'")),
        };
        Self::record_outcome(operation, &result);
        result
    }
}

fn pointer_string(value: &Value, pointer: &str) -> Option<String> {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn object_string(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn parse_time(value: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .map(|time| time.with_timezone(&Utc))
        .with_context(|| format!("parse Kubernetes timestamp '{value}'"))
}

#[cfg(test)]
#[path = "kubernetes_tests.rs"]
mod tests;
