---
harnx: minor
---
Add `harnx-k8s-creds` — a persistent PreToolUse hook that gives sandboxed bash processes
access to Kubernetes clusters without exposing the host kubeconfig or long-lived credentials.
Supports multiple contexts and exec credential plugins (e.g. aws-iam-authenticator).
