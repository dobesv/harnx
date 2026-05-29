---
harnx: minor
---
Add `scripts/update_models.py` and `.github/workflows/update-models.yml` to automate weekly regeneration of `crates/harnx/models.yaml` from the LiteLLM model registry. Opens a PR automatically when changes are detected.
