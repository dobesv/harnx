use std::collections::VecDeque;
use std::net::{IpAddr, Ipv4Addr};
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use harnx_fetch_tools::net::{redirect_policy, BoxError, GuardedResolver, Lookup, LookupFuture};
use harnx_fetch_tools::server::{FetchParams, FetchServer};
use reqwest::dns::{Name, Resolve};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

type DnsAnswers = VecDeque<Result<Vec<IpAddr>, String>>;

#[derive(Clone)]
struct FixedLookup {
    answers: Arc<Mutex<DnsAnswers>>,
}

impl FixedLookup {
    fn new(answers: impl IntoIterator<Item = Vec<IpAddr>>) -> Self {
        Self {
            answers: Arc::new(Mutex::new(
                answers.into_iter().map(Ok).collect::<VecDeque<_>>(),
            )),
        }
    }
}

impl Lookup for FixedLookup {
    fn lookup(&self, _host: String) -> LookupFuture {
        let answer = self
            .answers
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Err("no configured DNS answer".to_owned()));
        Box::pin(async move {
            answer.map_err(|message| -> BoxError { std::io::Error::other(message).into() })
        })
    }
}

struct SlowLookup;

impl Lookup for SlowLookup {
    fn lookup(&self, _host: String) -> LookupFuture {
        Box::pin(async {
            tokio::time::sleep(Duration::from_secs(60)).await;
            Ok(vec![IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))])
        })
    }
}

async fn assert_listener_untouched(listener: &tokio::net::TcpListener) {
    assert!(
        tokio::time::timeout(Duration::from_millis(100), listener.accept())
            .await
            .is_err(),
        "blocked address received a TCP connection"
    );
}

async fn fetch(
    server: &FetchServer,
    url: impl Into<String>,
    proxy: Option<&str>,
) -> Result<(), String> {
    server
        .fetch_html_impl(FetchParams {
            url: url.into(),
            proxy: proxy.map(str::to_owned),
            ..FetchParams::default()
        })
        .await
        .map(|_| ())
        .map_err(|error| error.message.to_string())
}

#[tokio::test]
async fn private_dns_answer_is_rejected_before_any_connection() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let lookup = FixedLookup::new([vec!["127.0.0.1".parse().unwrap()]]);
    let server = FetchServer::with_lookup(false, Arc::new(lookup));
    let error = fetch(&server, format!("http://blocked.test:{port}/"), None)
        .await
        .unwrap_err();
    assert!(
        error.contains("blocked address") || error.contains("DNS"),
        "{error}"
    );
    assert_listener_untouched(&listener).await;
}

#[tokio::test]
async fn mixed_public_private_dns_answer_rejects_whole_answer_without_connection() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let lookup = FixedLookup::new([vec![
        "8.8.8.8".parse().unwrap(),
        "127.0.0.1".parse().unwrap(),
        "2606:4700:4700::1111".parse().unwrap(),
    ]]);
    let server = FetchServer::with_lookup(false, Arc::new(lookup));
    assert!(fetch(&server, format!("http://mixed.test:{port}/"), None)
        .await
        .is_err());
    assert_listener_untouched(&listener).await;
}

#[tokio::test]
async fn empty_dns_answer_is_an_explicit_error() {
    let server =
        FetchServer::with_lookup(false, Arc::new(FixedLookup::new([Vec::<IpAddr>::new()])));
    let error = fetch(&server, "http://empty.test/", None)
        .await
        .unwrap_err();
    assert!(
        error.contains("no addresses") || error.contains("DNS"),
        "{error}"
    );
}

#[tokio::test]
async fn resolver_rechecks_dns_on_each_call_and_rejects_changed_answer() {
    let lookup = FixedLookup::new([
        vec!["8.8.8.8".parse().unwrap()],
        vec!["127.0.0.1".parse().unwrap()],
    ]);
    let resolver = GuardedResolver::with_lookup(false, Arc::new(lookup));
    let first = resolver
        .resolve(Name::from_str("changing.test").unwrap())
        .await
        .unwrap()
        .collect::<Vec<_>>();
    assert_eq!(first[0].ip(), "8.8.8.8".parse::<IpAddr>().unwrap());
    assert!(resolver
        .resolve(Name::from_str("changing.test").unwrap())
        .await
        .is_err());
}

#[tokio::test]
async fn dns_lookup_timeout_is_bounded() {
    let resolver = GuardedResolver::with_lookup(false, Arc::new(SlowLookup));
    let started = std::time::Instant::now();
    let error = resolver
        .resolve(Name::from_str("slow.test").unwrap())
        .await
        .err()
        .expect("slow DNS must time out");
    assert!(error.to_string().contains("timed out"));
    assert!(started.elapsed() < Duration::from_secs(7));
}

#[tokio::test]
async fn proxy_argument_and_environment_proxy_cannot_bypass_protection() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = format!("http://{}", listener.local_addr().unwrap());
    let server = FetchServer::with_lookup(
        false,
        Arc::new(FixedLookup::new([vec!["127.0.0.1".parse().unwrap()]])),
    );

    let error = fetch(&server, "http://target.test/", Some(&proxy))
        .await
        .unwrap_err();
    assert!(error.contains("proxy is disabled"));

    // SAFETY: nextest isolates this test in its own process.
    unsafe { std::env::set_var("HTTP_PROXY", &proxy) };
    let error = fetch(&server, "http://target.test/", None)
        .await
        .unwrap_err();
    unsafe { std::env::remove_var("HTTP_PROXY") };
    assert!(
        error.contains("blocked address") || error.contains("DNS"),
        "{error}"
    );
    assert_listener_untouched(&listener).await;
}

fn redirect_client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(redirect_policy(false))
        .build()
        .unwrap()
}

#[tokio::test]
async fn live_redirect_policy_blocks_private_literal_before_dial() {
    let blocked = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let blocked_url = format!("http://{}/private", blocked.local_addr().unwrap());
    let start = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/start"))
        .respond_with(ResponseTemplate::new(302).insert_header("location", blocked_url))
        .expect(1)
        .mount(&start)
        .await;

    let error = redirect_client()
        .get(format!("{}/start", start.uri()))
        .send()
        .await
        .expect_err("production redirect policy must reject the private hop");
    assert!(error.is_redirect(), "unexpected request error: {error}");
    assert_listener_untouched(&blocked).await;
}

#[tokio::test]
async fn live_public_private_public_chain_stops_before_private_connection() {
    use tokio::io::AsyncWriteExt;

    // `public_end` is on loopback, which the guarded resolver would block in
    // production. That is irrelevant here: the object under test is the redirect
    // policy, and `redirect_client()` uses the default resolver, so loopback is
    // reachable. The only thing that can stop the chain is the policy rejecting
    // the private middle hop — so if `public_end` is never hit, the block
    // happened at that hop, not because loopback is unreachable.
    let public_end = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/end"))
        .respond_with(ResponseTemplate::new(200).set_body_string("must not be reached"))
        .mount(&public_end)
        .await;

    let blocked = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let blocked_url = format!("http://{}/private", blocked.local_addr().unwrap());
    let public_end_url = format!("{}/end", public_end.uri());

    let start = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/start"))
        .respond_with(ResponseTemplate::new(302).insert_header("location", blocked_url))
        .expect(1)
        .mount(&start)
        .await;

    // Spawn the private-hop accept loop only after both mocks are mounted, so the
    // timeout budget covers just the request itself and not mock-server setup.
    let blocked_server = tokio::spawn(async move {
        match tokio::time::timeout(Duration::from_millis(200), blocked.accept()).await {
            Ok(Ok((mut stream, _))) => {
                let response = format!(
                    "HTTP/1.1 302 Found\r\nLocation: {public_end_url}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
                stream.write_all(response.as_bytes()).await.unwrap();
                true
            }
            _ => false,
        }
    });

    let error = redirect_client()
        .get(format!("{}/start", start.uri()))
        .send()
        .await
        .expect_err("private middle hop must terminate the redirect chain");
    assert!(error.is_redirect(), "unexpected request error: {error}");
    assert!(
        !blocked_server.await.unwrap(),
        "private redirect hop received a TCP connection"
    );
    assert!(
        public_end.received_requests().await.unwrap().is_empty(),
        "public hop after the blocked target was reached"
    );
}
