#!/usr/bin/env python3
"""Host-side credential source, never installed in the child environment."""
import datetime
import json
import pathlib
import sys

kind, counter_file = sys.argv[1:]
counter = pathlib.Path(counter_file)
generation = int(counter.read_text()) + 1 if counter.exists() else 1
counter.write_text(str(generation))
# A real expiry forces a later request through the provider again. Subsequent
# credentials stay valid so the test also exercises normal cached operation.
valid_for = 2 if generation == 1 else 3600
expiry = datetime.datetime.now(datetime.timezone.utc) + datetime.timedelta(seconds=valid_for)
expiry = expiry.isoformat(timespec="seconds").replace("+00:00", "Z")
if kind == "aws":
    reply = {
        "Version": 1,
        "AccessKeyId": f"AKIAFIXTURE{generation:09}",
        "SecretAccessKey": f"aws-secret-{generation}",
        "SessionToken": f"aws-session-{generation}",
        "Expiration": expiry,
    }
else:
    reply = {
        "apiVersion": "client.authentication.k8s.io/v1",
        "kind": "ExecCredential",
        "status": {"token": f"k8s-token-{generation}", "expirationTimestamp": expiry},
    }
print(json.dumps(reply))
