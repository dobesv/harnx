---
harnx: minor
---

Updated `llama-server` provider to support per-model GGUF configuration and HuggingFace auto-download.
- Models in `models[]` now specify their own `model_path`, `hf_repo`, and tuning knobs (`ctx_size`, `n_gpu_layers`, `threads`, `extra_args`, `socket_path`).
- Added support for HuggingFace auto-download via the `-hf` flag in `llama-server`.
- Model source resolution precedence: `model_path` (local) -> `hf_repo` (HuggingFace) -> model `name` as the HuggingFace repo spec.
- Multi-model support: one provider config can now serve multiple models, each in its own lazily-spawned `llama-server` subprocess.
