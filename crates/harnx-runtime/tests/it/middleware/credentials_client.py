#!/usr/bin/env python3
import json
import os
import pathlib
import re
import subprocess
import urllib.error
import urllib.request

assert os.environ["CALLER_VALUE"] == "kept"
for secret in ("AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY", "AWS_SESSION_TOKEN", "AWS_CONFIG_FILE", "AWS_SHARED_CREDENTIALS_FILE"):
    assert secret not in os.environ, secret
opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
if os.environ["CREDENTIAL_KIND"] == "aws":
    endpoint = os.environ["AWS_CONTAINER_CREDENTIALS_FULL_URI"]
    authorization = os.environ["AWS_CONTAINER_AUTHORIZATION_TOKEN"]
    assert endpoint.startswith("http://127.0.0.1:") and endpoint.endswith("/creds")
    assert authorization != "caller-token"
    assert os.environ["AWS_REGION"] == "us-west-2"
    request = urllib.request.Request(endpoint, headers={"Authorization": authorization})
    with opener.open(request, timeout=10) as response:
        credential = json.load(response)
    generation = int(credential["SecretAccessKey"].rsplit("-", 1)[1])
    assert credential["Token"] == f"aws-session-{generation}"
    assert credential["AccessKeyId"] == f"AKIAFIXTURE{generation:09}"
    expiration = credential["Expiration"]
    path = None
else:
    path = os.environ["KUBECONFIG"]
    assert path != "caller-kubeconfig"
    source = pathlib.Path(path).read_text()
    assert "k8s-token-" not in source and "credential_process.py" not in source
    assert "my-cluster" in source and "https://127.0.0.1:6443" in source
    assert "command: curl" in source
    endpoint = re.search(r"http://127\.0\.0\.1:\d+/token/my-cluster", source).group()
    authorization = re.search(r"Authorization: (Bearer [0-9a-f-]+)", source).group(1)
    # Execute the generated curl credential command, rather than a host plugin.
    credential = json.loads(subprocess.check_output([
        "curl", "--silent", "--fail", "--header", f"Authorization: {authorization}", endpoint,
    ], timeout=10))
    generation = int(credential["status"]["token"].rsplit("-", 1)[1])
    assert credential["kind"] == "ExecCredential"
    expiration = credential["status"]["expirationTimestamp"]

try:
    opener.open(endpoint, timeout=10)
    raise AssertionError("credential endpoint allowed missing capability")
except urllib.error.HTTPError as error:
    assert error.code == 401
print(json.dumps({"generation": generation, "expiration": expiration, "path": path}))
