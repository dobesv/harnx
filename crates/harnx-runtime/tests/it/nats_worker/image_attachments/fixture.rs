use super::image::ImageAttachment;
use super::*;
use harnx_runtime::agent_loop::OnToolRoundFn;

#[derive(Clone, Copy)]
pub(super) enum RemoteImage {
    Original,
    CorruptBeforeInference,
}

pub(super) struct ImageFixture {
    pub bridge: McpBridge,
    pub provider: CapturedProvider,
    pub session: ProviderSession,
    pub image: Arc<ImageAttachment>,
    pub env: ResultEnvironment,
}

impl ImageFixture {
    pub async fn start() -> Result<Self> {
        Self::start_with_calls(&[("large", "image-call")]).await
    }

    pub async fn start_with_calls(calls: &'static [(&'static str, &'static str)]) -> Result<Self> {
        let env = ResultEnvironment::start().await?;
        let image = Arc::new(ImageAttachment::new()?);
        image.assert_cache_missing();
        let bridge = McpBridge::start(&env, image.script()).await?;
        let provider = CapturedProvider::start(calls, "image received").await?;
        let session = ProviderSession::create(
            &env,
            SessionSpec {
                id: "img123",
                api_base: &provider.base,
                supports_vision: true,
            },
        )
        .await?;
        Ok(Self {
            bridge,
            provider,
            session,
            image,
            env,
        })
    }

    pub async fn run_observed_turn(&self, remote: RemoteImage) -> Result<()> {
        let observed = Arc::new(AtomicUsize::new(0));
        let on_round: OnToolRoundFn = {
            let image = self.image.clone();
            let js = self.env.js.clone();
            let observed = observed.clone();
            Arc::new(move |_, results| {
                let image = image.clone();
                let js = js.clone();
                let observed = observed.clone();
                let output = results[0].output.clone();
                Box::pin(async move {
                    // After real bridge offload, before the next model boundary.
                    image.assert_bridge_offload(&js, &output).await?;
                    if matches!(remote, RemoteImage::CorruptBeforeInference) {
                        image.corrupt_remote(&js).await?;
                    }
                    observed.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                })
            })
        };
        let result = self.session.run_turn(Some(on_round)).await;
        assert_eq!(observed.load(Ordering::SeqCst), 1);
        result
    }
}
