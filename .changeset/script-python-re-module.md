---
harnx: patch
---
Scripts can now `import re` in their embedded `python3`, and the script VM's memory limit is enforced through BashKit 0.21's metering allocator, which `harnx` and `harnx-worker` install.
