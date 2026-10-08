//! CID ownership checks run before blob reads and conditional responses.

use anyhow::Result;
use harnx_blob_store::{
    media::{ensure_attachments_bucket, put_media},
    plans::{create_document, ensure_plans_bucket, serialize_plan, PlanDocument, PlanFrontMatter},
};
use harnx_core::cid_url::{CidUrl, PlanItem, SessionRef};
use harnx_runtime::nats_session_metadata::{
    SessionInitializer, SessionMetadata, SessionMetadataStore,
};
use reqwest::{header, RequestBuilder, Response, StatusCode};

use crate::{access_agents::Fixture as HttpFixture, common};

const HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const RULES: &str = "rules:\n  - agents: [allowed]\n    scopes: [prompt]\n    users: [alice, bob]\n  - agents: [allowed]\n    scopes: [admin]\n    users: [admin]\n  - agents: ['allowed@default']\n    scopes: [admin]\n    users: [suffix-admin]\n  - agents: ['*']\n    scopes: [admin]\n    users: [superadmin]\n";

struct Blobs {
    media: CidUrl,
    plan: CidUrl,
    plan_etag: String,
}

struct Fixture {
    http: HttpFixture,
    owned: Blobs,
    legacy: Blobs,
    orphan: Blobs,
    hidden: Blobs,
    inline: Blobs,
    _nats: common::NatsServerHandle,
}

async fn seed(
    jetstream: &async_nats::jetstream::Context,
    store: &SessionMetadataStore,
    session: SessionRef,
    initializer: Option<SessionInitializer>,
) -> Result<Blobs> {
    if let Some(initializer) = initializer {
        store
            .create(&SessionMetadata::new(&session.session_id, initializer))
            .await?;
    }
    let media = CidUrl::Media {
        session: session.clone(),
        hash: HASH.into(),
    };
    let media_store = ensure_attachments_bucket(jetstream, 1).await?;
    put_media(&media_store, &media, b"png bytes", "image/png").await?;
    let plan = CidUrl::Plan {
        session,
        slug: "project-plan".into(),
        item: PlanItem::Index,
    };
    let plan_store = ensure_plans_bucket(jetstream, 1).await?;
    let document = PlanDocument {
        front: PlanFrontMatter {
            id: plan.to_string(),
            title: Some("Private Plan".into()),
            created_at: "2026-09-30T00:00:00Z".into(),
            ..Default::default()
        },
        body: "Private plan body.".into(),
    };
    let revision = create_document(&plan_store, &plan, &serialize_plan(&document)?).await?;
    Ok(Blobs {
        media,
        plan,
        plan_etag: format!("\"{revision}\""),
    })
}

async fn fixture(rules_on: bool) -> Result<Option<Fixture>> {
    harnx_core::require_nextest();
    let Some(nats) = common::spawn_nats_server().await? else {
        return Ok(None);
    };
    let client = async_nats::connect(nats.url()).await?;
    let jetstream = async_nats::jetstream::new(client);
    let store = SessionMetadataStore::ensure(&jetstream, 1).await?;
    let http = HttpFixture::start_with_nats(rules_on.then_some(RULES), nats.url()).await?;
    let owned = seed(
        &jetstream,
        &store,
        SessionRef::new(Some("allowed".into()), "cidown".into())?,
        Some(SessionInitializer::named("allowed", Default::default()).with_user_id("alice")),
    )
    .await?;
    let legacy = seed(
        &jetstream,
        &store,
        SessionRef::new(Some("allowed".into()), "legacy".into())?,
        Some(SessionInitializer::named("allowed", Default::default())),
    )
    .await?;
    let orphan = seed(
        &jetstream,
        &store,
        SessionRef::new(Some("allowed".into()), "orphan".into())?,
        None,
    )
    .await?;
    let hidden = seed(
        &jetstream,
        &store,
        SessionRef::new(Some("blocked".into()), "hidden".into())?,
        Some(SessionInitializer::named("blocked", Default::default()).with_user_id("alice")),
    )
    .await?;
    let inline = seed(
        &jetstream,
        &store,
        SessionRef::new(None, "inline".into())?,
        Some(
            SessionInitializer::inline("Inline agent", Default::default(), Default::default())
                .with_user_id("alice"),
        ),
    )
    .await?;
    Ok(Some(Fixture {
        http,
        owned,
        legacy,
        orphan,
        hidden,
        inline,
        _nats: nats,
    }))
}

impl Fixture {
    fn get(&self, url: &CidUrl, user: &str) -> RequestBuilder {
        let encoded = percent_encoding::utf8_percent_encode(
            &url.to_string(),
            percent_encoding::NON_ALPHANUMERIC,
        )
        .to_string();
        self.http
            .client
            .get(format!("{}/v1/cid/{encoded}", self.http.base))
            .header("x-user", user)
    }
}

fn assert_security_headers(response: &Response) {
    assert_eq!(response.headers()["x-content-type-options"], "nosniff");
    assert_eq!(
        response.headers()["content-security-policy"],
        "default-src 'none'; img-src 'self'; style-src 'unsafe-inline'; sandbox"
    );
    assert_eq!(response.headers()["access-control-allow-origin"], "*");
}

async fn assert_hidden(response: Response) -> Result<()> {
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        response.headers()[header::CACHE_CONTROL],
        "private, no-store"
    );
    assert!(!response.headers().contains_key(header::ETAG));
    assert_security_headers(&response);
    assert_eq!(response.text().await?, "cid blob not found");
    Ok(())
}

#[tokio::test]
async fn access_nats_cid_owner_and_admin_fetch_media_and_plan_other_prompt_user_is_hidden(
) -> Result<()> {
    let Some(f) = fixture(true).await? else {
        return Ok(());
    };
    for url in [&f.owned.media, &f.owned.plan] {
        for user in ["alice", "admin"] {
            let response = f.get(url, user).send().await?;
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(
                response.headers()[header::CACHE_CONTROL],
                "private, no-store"
            );
            assert_security_headers(&response);
            if matches!(url, CidUrl::Media { .. }) {
                assert_eq!(response.bytes().await?.as_ref(), b"png bytes");
            } else {
                assert!(response.text().await?.contains("Private plan body."));
            }
        }
        assert_hidden(f.get(url, "bob").send().await?).await?;
        // CID refs are bare, even though this server's default cluster is named.
        assert_hidden(f.get(url, "suffix-admin").send().await?).await?;
    }
    Ok(())
}

#[tokio::test]
async fn access_nats_cid_legacy_requires_admin_orphan_hidden_agent_and_temp_are_denied(
) -> Result<()> {
    let Some(f) = fixture(true).await? else {
        return Ok(());
    };
    for url in [&f.legacy.media, &f.legacy.plan] {
        assert_hidden(f.get(url, "alice").send().await?).await?;
        let admin = f.get(url, "admin").send().await?;
        assert_eq!(admin.status(), StatusCode::OK);
        assert_eq!(admin.headers()[header::CACHE_CONTROL], "private, no-store");
    }
    for blobs in [&f.orphan, &f.hidden, &f.inline] {
        for url in [&blobs.media, &blobs.plan] {
            for user in ["alice", "admin"] {
                assert_hidden(f.get(url, user).send().await?).await?;
            }
        }
    }
    for url in [
        &f.inline.media,
        &f.inline.plan,
        &f.orphan.media,
        &f.orphan.plan,
    ] {
        assert_hidden(f.get(url, "superadmin").send().await?).await?;
    }
    Ok(())
}

#[tokio::test]
async fn access_nats_cid_conditional_responses_authorize_before_etag_and_use_private_cache(
) -> Result<()> {
    let Some(f) = fixture(true).await? else {
        return Ok(());
    };
    for user in ["alice", "admin"] {
        let response = f
            .get(&f.owned.plan, user)
            .header(header::IF_NONE_MATCH, &f.owned.plan_etag)
            .send()
            .await?;
        assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(
            response.headers()[header::CACHE_CONTROL],
            "private, no-store"
        );
        assert_eq!(response.headers()[header::ETAG], f.owned.plan_etag);
        assert_security_headers(&response);
        assert!(response.bytes().await?.is_empty());
    }
    for user in ["bob", "suffix-admin"] {
        assert_hidden(
            f.get(&f.owned.plan, user)
                .header(header::IF_NONE_MATCH, &f.owned.plan_etag)
                .send()
                .await?,
        )
        .await?;
        assert_hidden(
            f.get(&f.owned.media, user)
                .header(header::IF_NONE_MATCH, format!("\"{HASH}\""))
                .send()
                .await?,
        )
        .await?;
    }
    for blobs in [&f.orphan, &f.hidden, &f.inline] {
        assert_hidden(
            f.get(&blobs.plan, "admin")
                .header(header::IF_NONE_MATCH, &blobs.plan_etag)
                .send()
                .await?,
        )
        .await?;
    }
    Ok(())
}

#[tokio::test]
async fn access_nats_cid_rules_off_preserve_public_media_plan_revalidation_and_bearer_access(
) -> Result<()> {
    let Some(f) = fixture(false).await? else {
        return Ok(());
    };
    for blobs in [&f.owned, &f.legacy, &f.orphan, &f.hidden, &f.inline] {
        let media = f
            .get(&blobs.media, "bob")
            .header(header::IF_NONE_MATCH, format!("\"{HASH}\""))
            .send()
            .await?;
        assert_eq!(media.status(), StatusCode::OK);
        assert_eq!(
            media.headers()[header::CACHE_CONTROL],
            "public, max-age=31536000, immutable"
        );
        let plan = f.get(&blobs.plan, "bob").send().await?;
        assert_eq!(plan.status(), StatusCode::OK);
        assert_eq!(plan.headers()[header::CACHE_CONTROL], "no-cache");
        let conditional = f
            .get(&blobs.plan, "bob")
            .header(header::IF_NONE_MATCH, &blobs.plan_etag)
            .send()
            .await?;
        assert_eq!(conditional.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(conditional.headers()[header::CACHE_CONTROL], "no-cache");
    }
    Ok(())
}
