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


class TestThinkingVariant(unittest.TestCase):
    def _base_claude_model(self) -> dict:
        return {
            "name": "claude-opus-4-7",
            "max_input_tokens": 1000000,
            "max_output_tokens": 128000,
            "input_price": 5,
            "output_price": 25,
            "supports_vision": True,
            "supports_tool_use": True,
        }

    def test_claude_thinking_variant_created(self) -> None:
        base = self._base_claude_model()
        variant = um.thinking_variant(base, "claude")
        self.assertIsNotNone(variant)
        assert variant is not None
        self.assertEqual(variant["name"], "claude-opus-4-7:thinking")
        self.assertEqual(variant["real_name"], "claude-opus-4-7")

    def test_thinking_variant_has_correct_patch(self) -> None:
        base = self._base_claude_model()
        variant = um.thinking_variant(base, "claude")
        assert variant is not None
        self.assertIn("patches", variant)
        self.assertEqual(len(variant["patches"]), 1)
        self.assertIn("budget_tokens\":16000", variant["patches"][0])

    def test_thinking_variant_preserves_supports_tool_use(self) -> None:
        base = self._base_claude_model()
        variant = um.thinking_variant(base, "claude")
        assert variant is not None
        self.assertTrue(variant.get("supports_tool_use"))

    def test_thinking_variant_preserves_supports_vision(self) -> None:
        base = self._base_claude_model()
        variant = um.thinking_variant(base, "claude")
        assert variant is not None
        self.assertTrue(variant.get("supports_vision"))

    def test_thinking_variant_has_require_max_tokens(self) -> None:
        base = self._base_claude_model()
        variant = um.thinking_variant(base, "claude")
        assert variant is not None
        self.assertTrue(variant.get("require_max_tokens"))

    def test_already_thinking_returns_none(self) -> None:
        base = {"name": "claude-opus-4-7:thinking"}
        self.assertIsNone(um.thinking_variant(base, "claude"))

    def test_non_claude_model_no_variant_for_claude_provider(self) -> None:
        base = {"name": "gpt-4o"}
        self.assertIsNone(um.thinking_variant(base, "claude"))

    def test_bedrock_claude_gets_bedrock_patch(self) -> None:
        base = {
            "name": "us.anthropic.claude-opus-4-7",
            "max_input_tokens": 200000,
            "supports_vision": True,
            "supports_tool_use": True,
        }
        variant = um.thinking_variant(base, "bedrock")
        assert variant is not None
        self.assertIn("additionalModelRequestFields", variant["patches"][0])

    def test_non_us_bedrock_model_no_variant(self) -> None:
        base = {"name": "ap-northeast-1/anthropic.claude-v2"}
        self.assertIsNone(um.thinking_variant(base, "bedrock"))

    def test_thinking_variant_real_name_uses_base_real_name_for_aliases(self) -> None:
        # If the base model has a real_name (alias), thinking variant should route to that
        base = {
            "name": "claude-opus-latest",
            "real_name": "claude-opus-4-7",  # alias that points to a versioned model
            "max_input_tokens": 1000000,
            "supports_tool_use": True,
        }
        variant = um.thinking_variant(base, "claude")
        assert variant is not None
        # Should route to the underlying model, not the alias name
        self.assertEqual(variant["real_name"], "claude-opus-4-7")
        self.assertEqual(variant["name"], "claude-opus-latest:thinking")

    def test_thinking_variant_real_name_defaults_to_name_when_no_alias(self) -> None:
        base = {
            "name": "claude-opus-4-7",
            "max_input_tokens": 1000000,
        }
        variant = um.thinking_variant(base, "claude")
        assert variant is not None
        self.assertEqual(variant["real_name"], "claude-opus-4-7")


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
        result = um.provider_prefix_and_model_name("anthropic/claude-3-5-sonnet-20241022")
        self.assertEqual(result, ("anthropic", "claude-3-5-sonnet-20241022"))

    def test_sample_spec_returns_none(self) -> None:
        self.assertIsNone(um.provider_prefix_and_model_name("sample_spec"))

    def test_no_slash_returns_none(self) -> None:
        self.assertIsNone(um.provider_prefix_and_model_name("gpt-4o"))


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
