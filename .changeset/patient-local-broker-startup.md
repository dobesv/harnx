---
harnx: patch
---
A slow local `nats-server` startup no longer gets killed after 15 seconds. Waiting for the broker to report its port, accept connections, or recover its JetStream store is now unbounded while the process stays healthy: status notices appear on stderr after 5 seconds and every 10 seconds after that, and Ctrl-C stops the wait. Cancelling one waiting command no longer kills a broker startup that other callers still need — only exiting harnx itself stops and reaps the broker it owns. New commands, `--session` configuration, session listing, and tool routing wait for broker readiness outside their operation timeouts instead of failing through them, and a TUI whose initial history load failed repairs its transcript once the broker is back, without a restart.
