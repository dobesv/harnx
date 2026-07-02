---
harnx: minor
---

Wire the remote control surface for NATS-backed `agent@cluster` sessions into the TUI, mirroring existing local-session operations.

Remote sessions can now be resumed from the session picker (the picked session id is threaded into the thin-client turn instead of always starting a new session), cancelled with Ctrl+C (publishes `ControlCommand::Cancel` to the session's NATS control subject, fire-and-forget), and retracted/edited with the existing `d`/`e` keybindings (routed to the thin-client `retract_user_message`/`edit_user_message`, converting the displayed index to the JetStream user-message sequence). Local-agent execution paths are unchanged. CI now installs `nats-server` on Linux so the NATS integration tests run.
