---
harnx: minor
---
Add persistent input prompt during LLM processing: users can type their next message while the LLM is running. Pressing Enter enqueues the message (shown with ⏳ indicator) to be sent when the LLM finishes. Editing a queued message un-queues it until Enter is pressed again. At most one message can be queued at a time.
