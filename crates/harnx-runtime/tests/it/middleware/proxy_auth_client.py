#!/usr/bin/env python3
"""Run inside actual bash exec/spawn; fixtures only, no external network."""
import base64
import http.client
import json
import os
from pathlib import Path
import ssl
from urllib.parse import urlsplit

assert os.environ["CALLER_VALUE"] == "kept"
assert os.environ["HTTP_PROXY"] == os.environ["HTTPS_PROXY"]
ca = Path(os.environ["SSL_CERT_FILE"])
assert ca.is_file()
for key in ("CURL_CA_BUNDLE", "NODE_EXTRA_CA_CERTS", "REQUESTS_CA_BUNDLE", "GIT_SSL_CAINFO"):
    assert os.environ[key] == str(ca)
config_root = Path(os.environ["ACLI_CONFIG_DIR"])
assert (config_root / "acli/.keep").is_file()
config = (config_root / "acli/jira_config.yaml").read_text()
assert "current_profile: cloud:account" in config
assert "token: !!binary " in config
assert "fixture-jira-token" not in config
assert "fixture-github-token" not in json.dumps(dict(os.environ))
assert "fixture-jira-token" not in json.dumps(dict(os.environ))
proxy = urlsplit(os.environ["HTTPS_PROXY"])
context = ssl.create_default_context()
context.verify_flags |= ssl.VERIFY_X509_STRICT

def headers(host):
    connection = http.client.HTTPSConnection(proxy.hostname, proxy.port, context=context, timeout=10)
    connection.set_tunnel(host, 443)
    connection.request("GET", "/fixture", headers={"Authorization": "Bearer sentinel"})
    response = connection.getresponse()
    assert response.status == 200
    result = json.loads(response.read())
    connection.close()
    return result

def basic(value):
    return "Basic " + base64.b64encode(value.encode()).decode()

assert headers("github.com")["authorization"] == basic("x-access-token:fixture-github-token")
for host in ("api.github.com", "uploads.github.com", "objects.githubusercontent.com"):
    assert headers(host)["authorization"] == "Bearer fixture-github-token"
for host in ("api.atlassian.com", "example.atlassian.net"):
    assert headers(host)["authorization"] == basic("test@example.invalid:fixture-jira-token")
for host in ("as.atlassian.com", "unrelated.invalid"):
    assert headers(host)["authorization"] == "Bearer sentinel"
print("PROXY_MIDDLEWARE_OK")
