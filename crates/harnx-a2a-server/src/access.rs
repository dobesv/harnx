//! Export visibility runs before protocol dispatch and any task lookup.

use crate::identity::{self, RequestIdentity};
use a2a_lf::A2AError;
use axum::{
    extract::{Request, State},
    http::{Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use harnx_core::access_rules::AccessRules;
use std::sync::Arc;

#[derive(Clone)]
pub(crate) struct ExportAccess {
    pub agent_ref: String,
    pub rules: Arc<AccessRules>,
}

pub(crate) async fn require_visible_export(
    State(access): State<ExportAccess>,
    request: Request,
    next: Next,
) -> Response {
    let Some(principal) = request.extensions().get::<RequestIdentity>() else {
        return identity::rpc_error_response(
            request,
            StatusCode::UNAUTHORIZED,
            identity::missing_identity(),
        )
        .await;
    };
    let caller = principal.caller();
    if access.rules.can_see_agent(&access.agent_ref, caller.view()) {
        return next.run(request).await;
    }
    if request.method() == Method::POST && request.uri().path() == "/" {
        return identity::rpc_error_response(
            request,
            StatusCode::OK,
            A2AError::new(-32001, "task not found"),
        )
        .await;
    }
    StatusCode::NOT_FOUND.into_response()
}

#[cfg(test)]
mod tests {
    use crate::{
        cli::Args,
        identity::Identity,
        test_support::{EnvGuard, TestConfigSandbox},
    };
    use clap::Parser;

    #[tokio::test]
    async fn access_startup_requires_identity_before_export_or_worker_setup() {
        let sandbox = TestConfigSandbox::new();
        let _env = EnvGuard::set("HARNX_ACCESS_RULES", None);
        let path = sandbox.config_dir().join("access.yaml");
        std::fs::write(&path, "rules: []\n").unwrap();
        for explicit in [false, true] {
            let mut args =
                Args::try_parse_from(["harnx-a2a-server", "--agent", "missing-agent"]).unwrap();
            args.config_dir = Some(sandbox.config_dir().to_owned());
            args.access_rules = explicit.then(|| path.clone());
            let error = crate::run(args).await.unwrap_err().to_string();
            assert_eq!(
                error,
                "access rules require a trusted identity source; configure --user-id-header"
            );
        }
        for source in ["X-User-ID", "header:X-User-ID", "cookie:user"] {
            Identity::new(&[source.into()])
                .unwrap()
                .validate_access_rules(true)
                .unwrap();
        }
    }

    #[tokio::test]
    async fn access_startup_explicit_missing_rules_file_fails() {
        let sandbox = TestConfigSandbox::new();
        let _env = EnvGuard::set("HARNX_ACCESS_RULES", None);
        let path = sandbox.config_dir().join("missing-access.yaml");
        let mut args =
            Args::try_parse_from(["harnx-a2a-server", "--agent", "missing-agent"]).unwrap();
        args.config_dir = Some(sandbox.config_dir().to_owned());
        args.access_rules = Some(path.clone());
        args.user_id_header = vec!["X-User-ID".into()];
        let error = crate::run(args).await.unwrap_err().to_string();
        assert!(error.contains(path.to_str().unwrap()), "{error}");
    }
}
