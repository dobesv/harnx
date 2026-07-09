---
harnx: patch
---
Fix server-mode log filter default (`harnx::serve` → `harnx`) so logs from `harnx_*` crates are captured. Correct `.env` precedence to standard dotenv semantics: the ambient/inherited environment always wins and the `.env` file only fills in variables that are not already set (previously `.env` unconditionally overrode inherited variables, silently clobbering operator-set values like `HARNX_LOG_LEVEL`). Fixes #989.
