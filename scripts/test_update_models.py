#!/usr/bin/env python3
"""Lightweight tests for update_models.py pure functions.

Run with: python scripts/test_update_models.py
No network access required.
"""

from __future__ import annotations

import sys
import unittest
from collections import OrderedDict

# Import the module under test
sys.path.insert(0, str(__import__("pathlib").Path(__file__).parent))
import update_models as um


class TestScalePricing(unittest.TestCase):
    def test_per_token_to_per_million(self) -> None:
        # GPT-4o input: $0.0000025/token → $2.50/M
        self.assertAlmostEqual(um.scale_price(0.0000025), 2.5, places=6)

    def test_zero_price(self) -> None:
        self.assertEqual(um.scale_price(0), 0)

    def test_none_price(self) -> None:
        self.assertIsNone(um.scale_price(None))

    def test_integer_result(self) -> None:
        # $0.000005/token → $5.0/M → stored as int 5
        result = um.scale_price(0.000005)
        self.assertEqual(result, 5)
        self.assertIsInstance(result, int)

    def test_float_result(self) -> None:
        # $0.0000025/token → $2.5/M → stored as float
        result = um.scale_price(0.0000025)
        self.assertIsInstance(result, float)
        self.assertAlmostEqual(result, 2.5, places=6)


class TestProviderMapping(unittest.TestCase):
    def test_anthropic_maps_to_claude(self) -> None:
        self.assertEqual(um.LITELLM_TO_HARNX_PROVIDER["anthropic"], "claude")

    def test_google_ai_maps_to_gemini(self) -> None:
        self.assertEqual(um.LITELLM_TO_HARNX_PROVIDER["google_ai"], "gemini")

    def test_vertex_ai_maps_to_vertexai(self) -> None:
        self.assertEqual(um.LITELLM_TO_HARNX_PROVIDER["vertex_ai"], "vertexai")

    def test_all_24_providers_present(self) -> None:
        expected = {
            "openai", "anthropic", "google_ai", "vertex_ai", "bedrock",
            "mistral", "cohere", "groq", "perplexity", "deepseek", "voyage",
            "cloudflare", "openrouter", "ai21", "x_ai", "zhipu",
            "alibaba_cloud", "baidu", "tencent", "minimax", "moonshot",
            "deepinfra", "github", "jina_ai",
        }
        self.assertEqual(set(um.LITELLM_TO_HARNX_PROVIDER.keys()), expected)


class TestOpusVersionDetection(unittest.TestCase):
    def test_opus_minor_version_parsed(self) -> None:
        self.assertEqual(um.opus_minor_version("claude-opus-4-8"), 8)
        self.assertEqual(um.opus_minor_version("claude-opus-4-7-20260416"), 7)
        self.assertEqual(um.opus_minor_version("claude-opus-4-1"), 1)

    def test_opus_minor_version_none_for_non_opus(self) -> None:
        self.assertIsNone(um.opus_minor_version("claude-sonnet-4-6"))
        # Old `claude-4-opus` ordering is not matched (it predates adaptive-only).
        self.assertIsNone(um.opus_minor_version("claude-4-opus-20250514"))

    def test_adaptive_only_threshold(self) -> None:
        self.assertTrue(um.is_adaptive_only_opus("claude-opus-4-7"))
        self.assertTrue(um.is_adaptive_only_opus("claude-opus-4-8"))
        self.assertTrue(um.is_adaptive_only_opus("claude-opus-4-8@default"))
        self.assertFalse(um.is_adaptive_only_opus("claude-opus-4-6"))
        self.assertFalse(um.is_adaptive_only_opus("claude-opus-4-5"))
        self.assertFalse(um.is_adaptive_only_opus("claude-sonnet-4-6"))

    def test_is_variant_name(self) -> None:
        self.assertTrue(um.is_variant_name("claude-opus-4-6:thinking"))
        self.assertTrue(um.is_variant_name("claude-opus-4-8:xhigh"))
        self.assertTrue(um.is_variant_name("claude-opus-4-8@default:max"))
        self.assertFalse(um.is_variant_name("claude-opus-4-8"))
        self.assertFalse(um.is_variant_name("claude-opus-4-8@default"))


class TestThinkingVariant(unittest.TestCase):
    """Manual `:thinking` variants for models that still support manual extended
    thinking (Opus 4.6 and earlier, Sonnet, Haiku)."""

    def _base_claude_model(self) -> dict:
        return {
            "name": "claude-opus-4-6",
            "max_input_tokens": 1000000,
            "max_output_tokens": 128000,
            "input_price": 5,
            "output_price": 25,
            "supports_vision": True,
            "supports_tool_use": True,
        }

    def test_claude_thinking_variant_created(self) -> None:
        variants = um.thinking_variants(self._base_claude_model(), "claude")
        self.assertEqual(len(variants), 1)
        self.assertEqual(variants[0]["name"], "claude-opus-4-6:thinking")
        self.assertEqual(variants[0]["real_name"], "claude-opus-4-6")

    def test_thinking_variant_has_correct_patch(self) -> None:
        variant = um.thinking_variants(self._base_claude_model(), "claude")[0]
        self.assertEqual(len(variant["patches"]), 1)
        self.assertIn('budget_tokens":16000', variant["patches"][0])

    def test_thinking_variant_preserves_supports_tool_use(self) -> None:
        variant = um.thinking_variants(self._base_claude_model(), "claude")[0]
        self.assertTrue(variant.get("supports_tool_use"))

    def test_thinking_variant_preserves_supports_vision(self) -> None:
        variant = um.thinking_variants(self._base_claude_model(), "claude")[0]
        self.assertTrue(variant.get("supports_vision"))

    def test_thinking_variant_has_require_max_tokens(self) -> None:
        variant = um.thinking_variants(self._base_claude_model(), "claude")[0]
        self.assertTrue(variant.get("require_max_tokens"))

    def test_already_variant_returns_empty(self) -> None:
        self.assertEqual(um.thinking_variants({"name": "claude-opus-4-6:thinking"}, "claude"), [])
        self.assertEqual(um.thinking_variants({"name": "claude-opus-4-8:xhigh"}, "claude"), [])

    def test_non_claude_model_no_variant_for_claude_provider(self) -> None:
        self.assertEqual(um.thinking_variants({"name": "gpt-4o"}, "claude"), [])

    def test_bedrock_claude_gets_bedrock_patch(self) -> None:
        base = {
            "name": "us.anthropic.claude-opus-4-6",
            "max_input_tokens": 200000,
            "supports_vision": True,
            "supports_tool_use": True,
        }
        variants = um.thinking_variants(base, "bedrock")
        self.assertEqual(len(variants), 1)
        self.assertIn("additionalModelRequestFields", variants[0]["patches"][0])

    def test_non_us_bedrock_model_no_variant(self) -> None:
        self.assertEqual(um.thinking_variants({"name": "ap-northeast-1/anthropic.claude-v2"}, "bedrock"), [])

    def test_thinking_variant_real_name_uses_base_real_name_for_aliases(self) -> None:
        base = {
            "name": "claude-opus-latest",
            "real_name": "claude-opus-4-6",  # alias that points to a versioned model
            "max_input_tokens": 1000000,
            "supports_tool_use": True,
        }
        variant = um.thinking_variants(base, "claude")[0]
        self.assertEqual(variant["real_name"], "claude-opus-4-6")
        self.assertEqual(variant["name"], "claude-opus-latest:thinking")

    def test_thinking_variant_real_name_defaults_to_name_when_no_alias(self) -> None:
        base = {"name": "claude-opus-4-6", "max_input_tokens": 1000000}
        variant = um.thinking_variants(base, "claude")[0]
        self.assertEqual(variant["real_name"], "claude-opus-4-6")


class TestAdaptiveEffortVariants(unittest.TestCase):
    """Adaptive-only Opus (4.7+) gets effort-level variants instead of
    `:thinking`, and adaptive thinking baked into the base model."""

    def _base(self, name: str = "claude-opus-4-8") -> dict:
        return {
            "name": name,
            "max_input_tokens": 1000000,
            "max_output_tokens": 128000,
            "input_price": 5,
            "output_price": 25,
            "supports_vision": True,
            "supports_tool_use": True,
        }

    def test_effort_variants_instead_of_thinking(self) -> None:
        for provider in ("claude", "vertexai"):
            variants = um.thinking_variants(self._base(), provider)
            names = [v["name"] for v in variants]
            self.assertEqual(names, ["claude-opus-4-8:xhigh", "claude-opus-4-8:max"])

    def test_no_manual_thinking_variant(self) -> None:
        names = [v["name"] for v in um.thinking_variants(self._base(), "claude")]
        self.assertNotIn("claude-opus-4-8:thinking", names)

    def test_effort_variant_patch_is_adaptive_with_effort(self) -> None:
        variants = um.thinking_variants(self._base(), "claude")
        xhigh = next(v for v in variants if v["name"].endswith(":xhigh"))
        patch = xhigh["patches"][0]
        self.assertIn('"type":"adaptive"', patch)
        self.assertIn('.body.output_config.effort = "xhigh"', patch)
        self.assertIn("del(.body.temperature)", patch)
        self.assertNotIn("budget_tokens", patch)

    def test_effort_variant_requires_max_tokens(self) -> None:
        # Adaptive-only Opus rejects requests without an explicit max_tokens.
        variant = um.thinking_variants(self._base(), "claude")[0]
        self.assertTrue(variant.get("require_max_tokens"))
        self.assertEqual(variant["max_output_tokens"], 128000)

    def test_effort_variant_routes_via_real_name(self) -> None:
        base = self._base("claude-opus-4-8@default")
        base["real_name"] = "claude-opus-4-8@default"
        variants = um.thinking_variants(base, "vertexai")
        self.assertEqual(variants[0]["name"], "claude-opus-4-8@default:xhigh")
        self.assertEqual(variants[0]["real_name"], "claude-opus-4-8@default")

    def test_bedrock_adaptive_only_gets_no_variant(self) -> None:
        # Manual thinking 400s and adaptive+effort on Bedrock is unconfirmed.
        base = self._base("us.anthropic.claude-opus-4-8")
        self.assertEqual(um.thinking_variants(base, "bedrock"), [])

    def test_apply_base_thinking_patches_adaptive_only(self) -> None:
        for provider in ("claude", "vertexai"):
            model = self._base()
            um.apply_base_thinking(model, provider)
            self.assertEqual(len(model["patches"]), 1)
            self.assertIn('"type":"adaptive"', model["patches"][0])
            self.assertIn('.body.output_config.effort = "high"', model["patches"][0])
            self.assertTrue(model.get("require_max_tokens"))

    def test_apply_base_thinking_skips_manual_models(self) -> None:
        model = {"name": "claude-opus-4-6", "max_input_tokens": 1000}
        um.apply_base_thinking(model, "claude")
        self.assertNotIn("patches", model)

    def test_apply_base_thinking_skips_bedrock(self) -> None:
        model = self._base("us.anthropic.claude-opus-4-8")
        um.apply_base_thinking(model, "bedrock")
        self.assertNotIn("patches", model)


class TestIsValidBedrockModelName(unittest.TestCase):
    def test_canonical_us_format_accepted(self) -> None:
        self.assertTrue(um.is_valid_bedrock_model_name("us.anthropic.claude-opus-4-7"))

    def test_region_prefixed_rejected(self) -> None:
        self.assertFalse(um.is_valid_bedrock_model_name("ap-northeast-1/anthropic.claude-v2"))

    def test_slash_in_name_rejected(self) -> None:
        self.assertFalse(um.is_valid_bedrock_model_name("us.anthropic/claude-opus-4-7"))

    def test_non_us_rejected(self) -> None:
        self.assertFalse(um.is_valid_bedrock_model_name("eu.anthropic.claude-opus-4-7"))


class TestShouldSkipModel(unittest.TestCase):
    import datetime as _dt
    TODAY = __import__("datetime").date.today()

    def test_past_deprecation_skipped(self) -> None:
        payload = {"deprecation_date": "2020-01-01", "max_input_tokens": 4096}
        self.assertTrue(um.should_skip_model(self.TODAY, payload))

    def test_future_deprecation_not_skipped(self) -> None:
        payload = {"deprecation_date": "2099-01-01", "max_input_tokens": 4096}
        self.assertFalse(um.should_skip_model(self.TODAY, payload))

    def test_no_useful_fields_skipped(self) -> None:
        # The "container" case
        payload = {"mode": "chat"}
        self.assertTrue(um.should_skip_model(self.TODAY, payload))

    def test_model_with_tokens_not_skipped(self) -> None:
        payload = {"max_input_tokens": 128000, "mode": "chat"}
        self.assertFalse(um.should_skip_model(self.TODAY, payload))


class TestProviderPrefixParsing(unittest.TestCase):
    def test_standard_format_parsed(self) -> None:
        result = um.provider_prefix_and_model_name(
            "anthropic/claude-3-5-sonnet-20241022", {}
        )
        self.assertEqual(result, ("anthropic", "claude-3-5-sonnet-20241022"))

    def test_sample_spec_returns_none(self) -> None:
        self.assertIsNone(um.provider_prefix_and_model_name("sample_spec", {}))

    def test_sample_spec_returns_none_even_with_provider(self) -> None:
        self.assertIsNone(
            um.provider_prefix_and_model_name(
                "sample_spec", {"litellm_provider": "openai"}
            )
        )

    def test_bare_key_uses_litellm_provider(self) -> None:
        # Anthropic's first-party models are keyed bare (no `provider/` prefix);
        # the provider must be recovered from the litellm_provider field.
        result = um.provider_prefix_and_model_name(
            "claude-opus-4-8", {"litellm_provider": "anthropic"}
        )
        self.assertEqual(result, ("anthropic", "claude-opus-4-8"))

    def test_bare_openai_key_uses_litellm_provider(self) -> None:
        result = um.provider_prefix_and_model_name(
            "gpt-4o", {"litellm_provider": "openai"}
        )
        self.assertEqual(result, ("openai", "gpt-4o"))

    def test_explicit_prefix_wins_over_litellm_provider(self) -> None:
        # When a `provider/` prefix is present it is authoritative, even if the
        # litellm_provider field names a different (sub-)provider.
        result = um.provider_prefix_and_model_name(
            "vertex_ai/claude-opus-4-8",
            {"litellm_provider": "vertex_ai-anthropic_models"},
        )
        self.assertEqual(result, ("vertex_ai", "claude-opus-4-8"))

    def test_bare_key_without_litellm_provider_returns_none(self) -> None:
        self.assertIsNone(um.provider_prefix_and_model_name("gpt-4o", {}))


class TestBuildModelFromLiteLLM(unittest.TestCase):
    """Tests for the core LiteLLM payload → harnx model dict transformation."""

    def _chat_payload(self, **overrides: object) -> dict:
        base = {
            "max_input_tokens": 128000,
            "max_tokens": 4096,
            "input_cost_per_token": 0.0000025,
            "output_cost_per_token": 0.00001,
            "supports_vision": True,
            "supports_function_calling": True,
        }
        base.update(overrides)
        return base

    def test_name_set(self) -> None:
        m = um.build_model_from_litellm("gpt-4o", self._chat_payload(), "chat")
        self.assertEqual(m["name"], "gpt-4o")

    def test_pricing_scaled_to_per_million(self) -> None:
        m = um.build_model_from_litellm("test", self._chat_payload(
            input_cost_per_token=0.0000025,
            output_cost_per_token=0.00001,
        ), "chat")
        self.assertAlmostEqual(m["input_price"], 2.5, places=4)
        self.assertAlmostEqual(m["output_price"], 10.0, places=4)

    def test_supports_vision_mapped(self) -> None:
        m = um.build_model_from_litellm("test", self._chat_payload(supports_vision=True), "chat")
        self.assertTrue(m["supports_vision"])

    def test_supports_tool_use_mapped_from_function_calling(self) -> None:
        m = um.build_model_from_litellm("test", self._chat_payload(supports_function_calling=True), "chat")
        self.assertTrue(m["supports_tool_use"])

    def test_type_field_omitted_for_chat(self) -> None:
        m = um.build_model_from_litellm("test", self._chat_payload(), "chat")
        self.assertNotIn("type", m)

    def test_type_field_set_for_embedding(self) -> None:
        payload = {"max_input_tokens": 8192, "input_cost_per_token": 0.00000002}
        m = um.build_model_from_litellm("embed-test", payload, "embedding")
        self.assertEqual(m["type"], "embedding")

    def test_max_output_tokens_from_payload(self) -> None:
        payload = {"max_input_tokens": 128000, "max_output_tokens": 4096}
        m = um.build_model_from_litellm("test", payload, "chat")
        self.assertEqual(m["max_output_tokens"], 4096)

    def test_max_input_tokens_falls_back_to_max_tokens(self) -> None:
        # When max_input_tokens is absent, max_tokens is used for input
        payload = {"max_tokens": 4096}
        m = um.build_model_from_litellm("test", payload, "chat")
        self.assertEqual(m["max_input_tokens"], 4096)

    def test_no_pricing_when_absent(self) -> None:
        payload = {"max_input_tokens": 128000}
        m = um.build_model_from_litellm("test", payload, "chat")
        self.assertNotIn("input_price", m)
        self.assertNotIn("output_price", m)


class TestMergeOldFields(unittest.TestCase):
    """Tests for harnx-only field preservation during merge."""

    def test_patches_preserved_if_absent_in_new(self) -> None:
        old = {"name": "m", "patches": ["del(.body.temperature)"]}
        new = {"name": "m", "max_input_tokens": 128000}
        merged = um.merge_old_fields(new, old)
        self.assertEqual(merged["patches"], ["del(.body.temperature)"])

    def test_patches_not_overwritten_if_present_in_new(self) -> None:
        old = {"name": "m", "patches": ["old-patch"]}
        new = {"name": "m", "patches": ["new-patch"]}
        merged = um.merge_old_fields(new, old)
        self.assertEqual(merged["patches"], ["new-patch"])

    def test_require_max_tokens_preserved(self) -> None:
        old = {"name": "m", "require_max_tokens": True}
        new = {"name": "m"}
        merged = um.merge_old_fields(new, old)
        self.assertTrue(merged["require_max_tokens"])

    def test_embedding_fields_preserved(self) -> None:
        old = {"name": "m", "max_tokens_per_chunk": 512, "default_chunk_size": 1000, "max_batch_size": 96}
        new = {"name": "m", "type": "embedding"}
        merged = um.merge_old_fields(new, old)
        self.assertEqual(merged["max_tokens_per_chunk"], 512)
        self.assertEqual(merged["default_chunk_size"], 1000)
        self.assertEqual(merged["max_batch_size"], 96)

    def test_real_name_preserved(self) -> None:
        old = {"name": "claude-alias", "real_name": "claude-opus-4-7"}
        new = {"name": "claude-alias", "max_input_tokens": 200000}
        merged = um.merge_old_fields(new, old)
        self.assertEqual(merged["real_name"], "claude-opus-4-7")

    def test_litellm_fields_not_clobbered_by_old(self) -> None:
        # LiteLLM-sourced fields in new should win over old
        old = {"name": "m", "max_input_tokens": 100000, "input_price": 1.0}
        new = {"name": "m", "max_input_tokens": 200000, "input_price": 5.0}
        merged = um.merge_old_fields(new, old)
        self.assertEqual(merged["max_input_tokens"], 200000)
        self.assertEqual(merged["input_price"], 5.0)

    def test_none_old_model_returns_new_unchanged(self) -> None:
        new = {"name": "m", "max_input_tokens": 128000}
        merged = um.merge_old_fields(new, None)
        self.assertEqual(merged, new)

    def test_endpoint_preserved(self) -> None:
        # GPT models pinned to the responses endpoint must keep the field when
        # regenerated from the LiteLLM registry (which does not carry it).
        old = {"name": "gpt-5", "endpoint": "responses"}
        new = {"name": "gpt-5", "max_input_tokens": 400000}
        merged = um.merge_old_fields(new, old)
        self.assertEqual(merged["endpoint"], "responses")

    def test_endpoint_survives_ordered_model(self) -> None:
        # ordered_model only emits fields in FIELD_ORDER; endpoint must be there
        # so the preserved value is not dropped during serialization.
        ordered = um.ordered_model({"name": "gpt-5", "endpoint": "responses"})
        self.assertEqual(ordered["endpoint"], "responses")


class TestOpenAIEndpointDefault(unittest.TestCase):
    """OpenAI chat models are routed to the responses endpoint when the LiteLLM
    registry's supported_endpoints signals it."""

    _RESP = ["/v1/chat/completions", "/v1/batch", "/v1/responses"]

    def test_responses_capable_gets_endpoint(self) -> None:
        model = {"name": "gpt-5.7"}
        um.apply_openai_endpoint_default(model, "openai", {"supported_endpoints": self._RESP})
        self.assertEqual(model["endpoint"], "responses")

    def test_no_signal_leaves_chat_completions(self) -> None:
        # Legacy models (gpt-4o, gpt-4, ...) have no supported_endpoints field.
        model = {"name": "gpt-4o"}
        um.apply_openai_endpoint_default(model, "openai", {})
        self.assertNotIn("endpoint", model)

    def test_endpoints_without_responses_left_alone(self) -> None:
        model = {"name": "weird"}
        um.apply_openai_endpoint_default(
            model, "openai", {"supported_endpoints": ["/v1/chat/completions"]}
        )
        self.assertNotIn("endpoint", model)

    def test_human_override_not_clobbered(self) -> None:
        # A value already on the (merged) model wins over the registry signal.
        model = {"name": "gpt-4o", "endpoint": "chat/completions"}
        um.apply_openai_endpoint_default(model, "openai", {"supported_endpoints": self._RESP})
        self.assertEqual(model["endpoint"], "chat/completions")

    def test_non_openai_provider_unaffected(self) -> None:
        model = {"name": "claude-x"}
        um.apply_openai_endpoint_default(model, "claude", {"supported_endpoints": self._RESP})
        self.assertNotIn("endpoint", model)

    def test_embedding_model_unaffected(self) -> None:
        model = {"name": "text-embedding-3-large", "type": "embedding"}
        um.apply_openai_endpoint_default(model, "openai", {"supported_endpoints": self._RESP})
        self.assertNotIn("endpoint", model)


class TestRenderProviderBlock(unittest.TestCase):
    """Tests for YAML output formatting."""

    def test_comment_headers_present(self) -> None:
        models = [{"name": "gpt-4o", "max_input_tokens": 128000}]
        block = um.render_provider_block("openai", models)
        self.assertIn("# Links:", block)
        self.assertIn("platform.openai.com", block)

    def test_provider_line_present(self) -> None:
        models = [{"name": "test", "max_input_tokens": 4096}]
        block = um.render_provider_block("openai", models)
        self.assertIn("- provider: openai", block)
        self.assertIn("  models:", block)

    def test_model_items_indented_four_spaces(self) -> None:
        models = [{"name": "gpt-4o", "max_input_tokens": 128000}]
        block = um.render_provider_block("openai", models)
        # Each model item should start with 4 spaces then a dash
        self.assertIn("    - name: gpt-4o", block)

    def test_empty_models_list(self) -> None:
        block = um.render_provider_block("openai", [])
        self.assertIn("[]", block)


class TestOrderedModel(unittest.TestCase):
    def test_field_order_respected(self) -> None:
        model = {
            "supports_tool_use": True,
            "name": "test-model",
            "patches": ["patch"],
            "input_price": 1.0,
        }
        ordered = um.ordered_model(model)
        keys = list(ordered.keys())
        # name must come before input_price which must come before patches
        self.assertLess(keys.index("name"), keys.index("input_price"))
        self.assertLess(keys.index("input_price"), keys.index("patches"))


if __name__ == "__main__":
    unittest.main(verbosity=2)
