---
harnx: patch
---

Recover interrupted middleware rounds from durable attempts and effective results without rerunning completed siblings. Uncertain downstream effects return an interrupted error instead of redispatching. Checkpoint-store failures leave the round pending until recovery can read its records.
