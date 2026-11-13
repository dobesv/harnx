---
harnx: minor
pantheon: patch
coding: patch
---
Allow bash command templates to opt in to new-message interruption with `on_user_message: terminate`. Interrupted templates terminate their process group and return partial output with `stopped_by: new_message`. Enable this for Pantheon's PR stability waiter so agents can read new messages before restarting the wait.
