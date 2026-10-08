use anyhow::Result;
use harnx_runtime::config::GlobalConfig;
use hyper::Request;

use crate::{
    ag_ui_error_to_anyhow, agent_resolve::agent_not_found, percent_decode, resolve_agent_target,
    session_actor::ResolvedAgentTarget, Server,
};

#[derive(Clone)]
struct RequestAgentTarget {
    agent_ref: String,
    target: ResolvedAgentTarget,
    scoped: GlobalConfig,
}

impl Server {
    /// Run before dispatch so attachments and metadata can't bypass agent checks.
    /// Session ownership checks belong here too, before any route reads its body.
    pub(crate) async fn guard_agent_request<B>(&self, req: &mut Request<B>) -> Result<()> {
        let Some((rules, identities)) = self.access(req) else {
            return Ok(());
        };
        let Some(path) = req.uri().path().strip_prefix("/v1/agents/") else {
            return Ok(());
        };
        let Some(agent) = path.split('/').find(|segment| !segment.is_empty()) else {
            return Ok(());
        };
        let agent_ref = percent_decode(agent);
        let (target, scoped) = resolve_agent_target(&self.config, &agent_ref)
            .await
            .map_err(ag_ui_error_to_anyhow)?;
        let ids: Vec<&str> = identities.iter().map(String::as_str).collect();
        let display_ref =
            target.display_ref_with_default_cluster(self.default_cluster_for_display());
        if !rules.can_see_agent(&display_ref, &ids) {
            return Err(ag_ui_error_to_anyhow(agent_not_found(
                &target.display_ref(),
            )));
        }
        self.guard_session_request(req, &target, (rules, &ids))
            .await?;
        req.extensions_mut().insert(RequestAgentTarget {
            agent_ref,
            target,
            scoped,
        });
        Ok(())
    }

    pub(crate) async fn resolve_request_agent<B>(
        &self,
        req: &Request<B>,
        agent_ref: &str,
    ) -> Result<(ResolvedAgentTarget, GlobalConfig)> {
        if let Some(resolved) = req.extensions().get::<RequestAgentTarget>() {
            if resolved.agent_ref == agent_ref {
                return Ok((resolved.target.clone(), resolved.scoped.clone()));
            }
        }
        resolve_agent_target(&self.config, agent_ref)
            .await
            .map_err(ag_ui_error_to_anyhow)
    }
}
