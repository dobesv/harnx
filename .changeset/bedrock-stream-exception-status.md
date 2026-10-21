---
harnx: patch
---
Classify Bedrock streaming validation and access-denied exceptions as non-retryable errors instead of transient server failures. Preserve the parsed exception payload when no string message is supplied, including legacy base64 frames. Unknown stream exceptions keep their existing transient handling.
