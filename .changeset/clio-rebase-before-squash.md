---
pantheon: patch
---
Clio now rebases a branch onto its base before squashing it, so a base that moved on since the branch was cut no longer has its new changes undone inside the squashed commit.
