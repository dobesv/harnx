---
harnx: patch
---
Fix tool servers failing new tool calls with `tool invocation recovery: replay has no durable invocation` or `key already exists: wrong last revision` on a replicated NATS cluster. The server read the tool invocation journal with direct gets, which a follower answers from whatever it has applied so far, so a read could miss the row the worker wrote just before dispatching the call. Tool servers and workers now read journal rows, and list a session's rows, from the stream leader, and refuse a listing while the stream has no leader. Existing journal buckets with direct get enabled are covered without changing their configuration.
