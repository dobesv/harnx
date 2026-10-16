---
harnx: patch
---

Tool and hook servers now retain registration on shutdown by default, letting the 90-second bucket TTL handle cleanup. This fixes a bug where a terminating replica would delete a shared registration key that surviving replicas still need, causing discovery gaps of up to 30 seconds. The default `RegistrationShutdown::Expire` policy is safe for both singleton and HA deployments. The explicit `DeleteIfCurrent` policy is preserved for backward compatibility but is now documented as unsafe for shared keys. Hook servers no longer delete their registration on shutdown. Registrations now correctly expire up to 90 seconds after the final refresh rather than being immediately removed. Issue #2409.
