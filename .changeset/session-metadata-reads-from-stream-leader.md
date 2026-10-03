---
harnx: patch
---
Fix sub-agent calls failing with `prompt admission missing after reservation` or `prompt has no admission` on a replicated NATS cluster. A frontend wrote a prompt's admission to the session metadata bucket and read it straight back with a direct get, which a follower answers from whatever it has applied so far. The worker it then activated read the same records the same way, so it could also terminate the activation as metadata-less or refuse the prompt for having no durable run admission. Every read of a session metadata key, including the confirmation after an ambiguous write and the session activity that attachments update, now goes to the stream leader. Existing buckets with direct get enabled are covered without changing their configuration.
