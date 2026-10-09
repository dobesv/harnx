---
harnx: patch
---
Persist exact A2A task cancellation across replicas and recover abandoned tasks from a durable background registry without client traffic. Recovery preserves durable completion, otherwise closes or interrupts the original invocation without replay. Broker uncertainty retains unresolved work; worker lease renewal doesn't prevent A2A takeover. Cross-replica intermediate streaming and coordinated global retention remain separate work.
