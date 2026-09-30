---
harnx: patch
---
Keep claimed NATS worker activations alive with progress acknowledgements, admit independent sessions concurrently, preserve immediate durable turn failures, back off busy activations before expensive preflight reads, terminate repeated pre-turn infrastructure failures after recording an error, and make the worker-claim timeout configurable.
