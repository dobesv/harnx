"""Gemini catalog ingestion tests: provider mapping, refresh persistence, chat compatibility.

The Gemini provider was missing from the LiteLLM mapping until #2638, so its
pricing silently went stale and audio-only entries slipped in as chat models
once ingestion resumed. This module covers the mapping, the refresh pipeline,
and the compatibility gate, using explicit payloads — no network access.
"""

from __future__ import annotations

import unittest
from typing import Any

import update_models as um


class TestGeminiProviderMapping(unittest.TestCase):
    def test_gemini_maps_to_gemini(self) -> None:
        # LiteLLM's registry keys Gemini models `gemini/...` with
        # `litellm_provider: "gemini"`; nothing upstream uses `google_ai`.
        self.assertEqual(um.LITELLM_TO_HARNX_PROVIDER["gemini"], "gemini")
        self.assertIn("gemini", um.CHAT_PROVIDER_DEFAULTS)

    def test_google_ai_maps_to_gemini(self) -> None:
        # Legacy alias from older registry snapshots; harmless to keep.
        self.assertEqual(um.LITELLM_TO_HARNX_PROVIDER["google_ai"], "gemini")


class TestGeminiRefreshPersistence(unittest.TestCase):
    """Regression coverage for #2638: a refresh must keep Gemini pricing
    current instead of silently retaining stale catalog entries that lack
    `cache_read_price`, and must keep curated harnx-only fields.
    """

    def _litellm_gemini_payload(self) -> dict[str, Any]:
        # Real generator data shape from LiteLLM's registry: prices per token.
        return {
            "litellm_provider": "gemini",
            "mode": "chat",
            "max_input_tokens": 1048576,
            "max_output_tokens": 65536,
            "input_cost_per_token": 7.5e-07,
            "output_cost_per_token": 3.75e-06,
            "cache_read_input_token_cost": 7.5e-08,
            "supports_vision": True,
            "supports_function_calling": True,
        }

    def _refresh_gemini_models(
        self,
        old_models: dict[str, dict[str, Any]],
        model_name: str = "gemini-3.8-flash",
    ) -> tuple[str, list[dict[str, Any]]]:
        payload = self._litellm_gemini_payload()
        prefix, model_name = um.provider_prefix_and_model_name(
            f"gemini/{model_name}", payload
        )
        provider = um.LITELLM_TO_HARNX_PROVIDER[prefix]
        mode = um.infer_mode(prefix, payload)
        model = um.build_model_from_litellm(model_name, payload, mode)
        model = um.merge_old_fields(model, old_models.get(model_name))
        fetched = {model_name: model}

        warnings: list[str] = []
        refreshed = um.regenerate_provider_models("gemini", old_models, fetched, warnings)
        return provider, refreshed

    def test_refresh_picks_up_current_gemini_provider_key(self) -> None:
        # `gemini/...` keys must resolve to the harnx `gemini` provider;
        # before the fix they were discarded before any other check ran.
        provider, _ = self._refresh_gemini_models({})
        self.assertEqual(provider, "gemini")

    def test_refresh_replaces_missing_cache_read_price(self) -> None:
        # The stale shipped entry: input/output prices but no cache_read_price.
        stale = {
            "name": "gemini-3.8-flash",
            "max_input_tokens": 1048576,
            "max_output_tokens": 65536,
            "input_price": 0.75,
            "output_price": 3.75,
            "supports_vision": True,
            "supports_tool_use": True,
        }
        _, models = self._refresh_gemini_models({"gemini-3.8-flash": stale})
        refreshed = {model["name"]: model for model in models}["gemini-3.8-flash"]
        self.assertEqual(refreshed["input_price"], 0.75)
        self.assertEqual(refreshed["output_price"], 3.75)
        # $7.5e-08 per token → $0.075 per million.
        self.assertEqual(refreshed["cache_read_price"], 0.075)

    def test_refresh_preserves_curated_patches(self) -> None:
        # Curated harnx-only fields survive a refresh. (gemini-3.8-flash's
        # sampling patch is generator-owned and intentionally replaced.)
        stale = {
            "name": "gemini-3.5-flash",
            "patches": [".body.generationConfig = ((.body.generationConfig // {}) | del(.temperature))"],
            "require_max_tokens": True,
        }
        _, models = self._refresh_gemini_models(
            {"gemini-3.5-flash": stale}, model_name="gemini-3.5-flash"
        )
        refreshed = {model["name"]: model for model in models}["gemini-3.5-flash"]
        self.assertEqual(refreshed["patches"], stale["patches"])
        self.assertTrue(refreshed["require_max_tokens"])


class TestChatOutputCompatibility(unittest.TestCase):
    """Audio-only entries LiteLLM tags mode `chat` must not become catalog
    chat models: the shipped chat-completions transport parses text output
    only (#2638 review follow-up).
    """

    def _text_chat_payload(self) -> dict[str, Any]:
        return {
            "litellm_provider": "gemini",
            "mode": "chat",
            "supported_output_modalities": ["text"],
            "max_input_tokens": 1048576,
            "input_cost_per_token": 7.5e-07,
            "output_cost_per_token": 3.75e-06,
        }

    def _audio_only_chat_payload(self) -> dict[str, Any]:
        # Shape of the real registry entries: lyria-3.5 and
        # gemini-2.5-pro-preview-tts both say mode "chat" with audio output.
        payload = self._text_chat_payload()
        payload["supported_output_modalities"] = ["audio"]
        return payload

    def test_audio_only_chat_entry_is_gated_out_of_collection(self) -> None:
        litellm = {
            "gemini/lyria-3.5": self._audio_only_chat_payload(),
            "gemini/gemini-2.5-pro-preview-tts": self._audio_only_chat_payload(),
            "gemini/gemini-3.8-flash": self._text_chat_payload(),
        }
        collected, _warnings = um.collect_provider_models(litellm, {})
        self.assertEqual(
            sorted(collected.get("gemini", {})),
            ["gemini-3.8-flash"],
            "explicit chat mode must not override the audio-only output gate",
        )

    def test_unspecified_output_modalities_stay(self) -> None:
        # Absent modalities make no claim; zero-price text models are kept.
        payload = self._text_chat_payload()
        del payload["supported_output_modalities"]
        collected, _warnings = um.collect_provider_models(
            {"gemini/gemini-exp-1114": payload}, {}
        )
        self.assertIn("gemini-exp-1114", collected.get("gemini", {}))

    def test_chat_output_is_supported_predicate(self) -> None:
        self.assertTrue(um.chat_output_is_supported(self._text_chat_payload()))
        self.assertTrue(
            um.chat_output_is_supported({"supported_output_modalities": ["text", "audio"]})
        )
        self.assertFalse(um.chat_output_is_supported(self._audio_only_chat_payload()))
        self.assertTrue(um.chat_output_is_supported({}))


if __name__ == "__main__":
    unittest.main()
