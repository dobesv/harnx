use crate::lifecycle::{
    CreateSandboxClaim, SandboxApi, SandboxCondition, SandboxRecord, LAST_ACTIVITY_ANNOTATION,
};
use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use kube::api::{
    ApiResource, DeleteParams, DynamicObject, GroupVersionKind, ListParams, Patch, PatchParams,
    PostParams,
};
use kube::{Api, Client, ResourceExt};
use serde_json::{json, Value};

#[derive(Clone)]
pub struct KubernetesSandboxApi {
    claims: Api<DynamicObject>,
    sandboxes: Api<DynamicObject>,
    claim_resource: ApiResource,
}

impl KubernetesSandboxApi {
    pub fn new(client: Client, namespace: &str) -> Self {
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
        }
    }

    async fn claim(&self, id: &str) -> Result<Option<DynamicObject>> {
        match self.claims.get(id).await {
            Ok(claim) => Ok(Some(claim)),
            Err(kube::Error::Api(response)) if response.code == 404 => Ok(None),
            Err(error) => {
                Err(anyhow::Error::from(error)).with_context(|| format!("get SandboxClaim '{id}'"))
            }
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
            match self.sandboxes.get(name).await {
                Ok(sandbox) => sandbox
                    .data
                    .pointer("/spec/replicas")
                    .and_then(Value::as_i64),
                Err(kube::Error::Api(response)) if response.code == 404 => None,
                Err(error) => {
                    return Err(anyhow::Error::from(error))
                        .with_context(|| format!("get Sandbox '{name}'"));
                }
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
}

fn last_activity(claim: &DynamicObject) -> Option<DateTime<Utc>> {
    claim
        .annotations()
        .get(LAST_ACTIVITY_ANNOTATION)
        .and_then(|time| match parse_time(time) {
            Ok(time) => Some(time),
            Err(error) => {
                let id = claim.name_any();
                // Preserve Tartarus's behavior: an invalid activity annotation
                // falls back to creation time instead of disabling idle cleanup.
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
        let mut claim = DynamicObject::new(&request.name, &self.claim_resource);
        if let Some(description) = request.description.filter(|value| !value.is_empty()) {
            claim
                .metadata
                .annotations
                .get_or_insert_default()
                .insert("kubernetes.io/description".to_string(), description);
        }
        claim.data = json!({
            "spec": {
                "sandboxTemplateRef": {"name": request.template},
                "lifecycle": {
                    "shutdownTime": request.shutdown_time.to_rfc3339(),
                    "shutdownPolicy": "Delete"
                }
            }
        });
        match self.claims.create(&PostParams::default(), &claim).await {
            Ok(created) => Ok(created.name_any()),
            Err(kube::Error::Api(response)) if response.code == 409 => Ok(request.name.clone()),
            Err(error) => Err(anyhow::Error::from(error))
                .with_context(|| format!("create SandboxClaim '{}'", request.name)),
        }
    }

    async fn get(&self, id: &str) -> Result<Option<SandboxRecord>> {
        match self.claim(id).await? {
            Some(claim) => self.record(claim).await.map(Some),
            None => Ok(None),
        }
    }

    async fn list(&self) -> Result<Vec<SandboxRecord>> {
        let claims = self.claims.list(&ListParams::default()).await?;
        let mut records = Vec::with_capacity(claims.items.len());
        for claim in claims.items {
            let id = claim.name_any();
            match self.record(claim).await {
                Ok(record) => records.push(record),
                Err(error) => {
                    // A watcher scan must isolate failures to one claim. A
                    // transient backing-Sandbox error should not prevent idle
                    // cleanup for every other sandbox in the namespace.
                    log::warn!("skip SandboxClaim '{id}' during list: {error:#}");
                }
            }
        }
        Ok(records)
    }

    async fn set_replicas(&self, id: &str, replicas: i64) -> Result<()> {
        let name = self.sandbox_name(id).await?;
        self.sandboxes
            .patch(
                &name,
                &PatchParams::default(),
                &Patch::Merge(json!({"spec": {"replicas": replicas}})),
            )
            .await
            .with_context(|| format!("set Sandbox '{name}' replicas to {replicas}"))?;
        Ok(())
    }

    async fn update_shutdown_time(&self, id: &str, shutdown_time: DateTime<Utc>) -> Result<()> {
        self.claims
            .patch(
                id,
                &PatchParams::default(),
                &Patch::Merge(json!({
                    "spec": {"lifecycle": {"shutdownTime": shutdown_time.to_rfc3339()}}
                })),
            )
            .await
            .with_context(|| format!("update SandboxClaim '{id}' shutdown time"))?;
        Ok(())
    }

    async fn bump_activity(&self, id: &str, now: DateTime<Utc>) -> Result<()> {
        let claim = self
            .claim(id)
            .await?
            .with_context(|| format!("sandbox not found: {id}"))?;
        if let Some(last) = claim
            .annotations()
            .get(LAST_ACTIVITY_ANNOTATION)
            .and_then(|time| parse_time(time).ok())
        {
            if now - last < chrono::Duration::minutes(1) {
                return Ok(());
            }
        }
        self.claims
            .patch(
                id,
                &PatchParams::default(),
                &Patch::Merge(json!({
                    "metadata": {"annotations": {LAST_ACTIVITY_ANNOTATION: now.to_rfc3339()}}
                })),
            )
            .await
            .with_context(|| format!("update SandboxClaim '{id}' activity"))?;
        Ok(())
    }

    async fn delete(&self, id: &str) -> Result<()> {
        match self.claims.delete(id, &DeleteParams::default()).await {
            Ok(_) => Ok(()),
            Err(kube::Error::Api(status)) if status.is_not_found() => Ok(()),
            Err(error) => Err(anyhow::Error::from(error))
                .with_context(|| format!("delete SandboxClaim '{id}'")),
        }
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
