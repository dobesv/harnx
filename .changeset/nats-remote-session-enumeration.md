---
harnx: minor
---

Enables remote session enumeration for NATS-backed agents in the TUI session picker, CLI `--list-sessions`, and shell completion. 

Previously, remote (`agent@cluster`) sessions were invisible to enumeration tools unless they existed in the local session directory. This change introduces a NATS KV-backed session index (`harnx_sessions`) that workers populate upon session activation and refresh during lease renewal. Clients now automatically route enumeration requests to this remote index when a remote agent is in context, with graceful degradation and timeouts to ensure local operations remain responsive even during NATS connectivity issues.
