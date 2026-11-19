//! Environment defaults shared by the tool service and temporary legacy adapter.
use serde_json::{Map, Value};

pub(crate) fn defaults(
    proxy_port: u16,
    ca_cert_path: &str,
    extra_env: &Map<String, Value>,
) -> Map<String, Value> {
    let mut env = extra_env.clone();
    for (key, value) in [
        ("HTTP_PROXY", format!("http://127.0.0.1:{proxy_port}")),
        ("HTTPS_PROXY", format!("http://127.0.0.1:{proxy_port}")),
        ("SSL_CERT_FILE", ca_cert_path.to_owned()),
        ("NODE_EXTRA_CA_CERTS", ca_cert_path.to_owned()),
        ("CURL_CA_BUNDLE", ca_cert_path.to_owned()),
        ("REQUESTS_CA_BUNDLE", ca_cert_path.to_owned()),
        ("GIT_SSL_CAINFO", ca_cert_path.to_owned()),
    ] {
        env.entry(key.to_owned()).or_insert(Value::String(value));
    }
    env
}
