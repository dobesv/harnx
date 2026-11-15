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
from update_models_bedrock_tests import (
    TestBedrockCardCorrections,
    TestCuratedCapabilityFlags,
    TestDropBedrockInRegionDuplicates,
    TestIsValidBedrockModelName,
)
from update_models_client_support_tests import TestClientSupport
from update_models_gemini_tests import (
    TestChatOutputCompatibility,
    TestGeminiProviderMapping,
    TestGeminiRefreshPersistence,
)
from update_models_openai_tests import (
    TestOpenAIEndpointDefault,
    TestOpenAIReasoningSummaryDefault,
)
from update_models_thinking_tests import (
    TestAdaptiveEffortVariants,
    TestThinkingVariant,
)
from update_models_variant_tests import (
    TestDirectProviderRules,
    TestOpenAIEffortVariants,
    TestProviderModelRegeneration,
)


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

    def test_vertex_ai_maps_to_vertexai(self) -> None:
        self.assertEqual(um.LITELLM_TO_HARNX_PROVIDER["vertex_ai"], "vertexai")

    def test_every_registry_provider_name_is_mapped(self) -> None:
        # Only names the registry uses: a stale one silently freezes the
        # provider, which is how Gemini (`google_ai`) and xAI (`x_ai`) went stale.
        expected = {
            "openai", "anthropic", "gemini", "vertex_ai", "bedrock",
            "bedrock_converse", "mistral", "cohere", "groq", "perplexity",
            "deepseek", "voyage", "cloudflare", "ai21", "xai", "zai",
            "tencent", "minimax", "moonshot", "deepinfra", "jina_ai",
            "cohere_chat", "vertex_ai-language-models", "vertex_ai-embedding-models",
        }
        self.assertEqual(set(um.LITELLM_TO_HARNX_PROVIDER.keys()), expected)

    def test_providers_without_a_matching_registry_source_stay_unmapped(self) -> None:
        mapped = set(um.LITELLM_TO_HARNX_PROVIDER.values())
        for provider in ("qianwen", "zhipuai", "ernie", "openrouter", "github"):
            self.assertNotIn(provider, mapped)

    def test_mapped_provider_with_no_registry_entries_is_reported(self) -> None:
        registry = {
            "xai/grok-4.20-0309-reasoning": {
                "litellm_provider": "xai", "mode": "chat",
                "max_input_tokens": 1000000, "input_cost_per_token": 1.25e-06,
            },
        }
        fetched, warnings = um.collect_provider_models(registry, {})
        self.assertIn("grok-4.20-0309-reasoning", fetched["xai"])
        self.assertEqual(len(warnings), 1)
        self.assertIn("anthropic", warnings[0])
        self.assertNotIn("xai", warnings[0].split(":", 1)[1])

    def test_bare_key_sources_reach_their_provider(self) -> None:
        # Entries LiteLLM lists without a `provider/` key prefix, under a
        # `litellm_provider` that names a model family. A covered provider
        # drops whatever the registry doesn't list, so missing one of these
        # deletes live models rather than freezing them.
        registry = {
            "command-a-03-2025": {
                "litellm_provider": "cohere_chat", "mode": "chat",
                "max_input_tokens": 256000, "input_cost_per_token": 2.5e-06,
            },
            "gemini-2.5-pro": {
                "litellm_provider": "vertex_ai-language-models", "mode": "chat",
                "max_input_tokens": 1048576, "input_cost_per_token": 1.25e-06,
            },
            "text-embedding-005": {
                "litellm_provider": "vertex_ai-embedding-models", "mode": "embedding",
                "max_input_tokens": 2048, "input_cost_per_token": 2.5e-08,
            },
            "rerank-v3.5": {
                "litellm_provider": "cohere", "mode": "rerank",
                "max_input_tokens": 4096, "input_cost_per_token": 0.0,
            },
        }
        fetched, _ = um.collect_provider_models(registry, {})
        self.assertIn("command-a-03-2025", fetched["cohere"])
        self.assertIn("gemini-2.5-pro", fetched["vertexai"])
        self.assertEqual(fetched["vertexai"]["text-embedding-005"]["type"], "embedding")
        # LiteLLM's mode is `rerank`; the catalog's type is `reranker`.
        self.assertEqual(fetched["cohere"]["rerank-v3.5"]["type"], "reranker")

    def test_both_bedrock_tags_map_to_bedrock(self) -> None:
        # LiteLLM tags most of the live Bedrock catalog `bedrock_converse`,
        # so treating only `bedrock` as Bedrock loses nearly all of it.
        self.assertEqual(um.LITELLM_TO_HARNX_PROVIDER["bedrock"], "bedrock")
        self.assertEqual(um.LITELLM_TO_HARNX_PROVIDER["bedrock_converse"], "bedrock")

    def test_bedrock_mantle_is_not_mapped(self) -> None:
        # A separate endpoint the Bedrock client does not call.
        self.assertNotIn("bedrock_mantle", um.LITELLM_TO_HARNX_PROVIDER)


class TestClaudeModelNames(unittest.TestCase):
    def test_required_output_limit_distinguishes_minor_from_date(self) -> None:
        self.assertTrue(um.claude_requires_max_tokens("claude-opus-5-5"))
        self.assertTrue(um.claude_requires_max_tokens("claude-opus-5-5-20260926"))
        self.assertFalse(um.claude_requires_max_tokens("claude-opus-5-20260926"))
        self.assertFalse(um.claude_requires_max_tokens("claude-opus-4-1-20250805"))

    def test_sonnet_5_family_requires_max_tokens(self) -> None:
        self.assertTrue(um.claude_requires_max_tokens("claude-sonnet-5"))
        self.assertTrue(um.claude_requires_max_tokens("claude-sonnet-5-5"))
        self.assertTrue(um.claude_requires_max_tokens("us.anthropic.claude-sonnet-5-5"))

    def test_is_variant_name(self) -> None:
        self.assertTrue(um.is_variant_name("claude-opus-4-6:thinking"))
        self.assertTrue(um.is_variant_name("gpt-5.6-sol:high"))
        self.assertTrue(um.is_variant_name("claude-opus-4-8:xhigh"))
        self.assertTrue(um.is_variant_name("claude-opus-4-8@default:max"))
        self.assertFalse(um.is_variant_name("claude-opus-4-8"))
        self.assertFalse(um.is_variant_name("claude-opus-4-8@default"))

    def test_generated_variant_ownership_is_provider_specific(self) -> None:
        self.assertTrue(um.is_generated_variant_name("openai", "gpt-5.6-sol:max"))
        self.assertTrue(um.is_generated_variant_name("claude", "claude-opus-4-8:max"))
        self.assertTrue(
            um.is_generated_variant_name(
                "bedrock", "us.anthropic.claude-opus-4-6-v1:thinking"
            )
        )
        self.assertFalse(um.is_generated_variant_name("openai", "custom:max"))
        self.assertFalse(um.is_generated_variant_name("qianwen", "qwen3-max:thinking"))


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

    def test_model_with_only_cache_pricing_not_skipped(self) -> None:
        for field in (
            "cache_read_input_token_cost",
            "cache_creation_input_token_cost",
        ):
            with self.subTest(field=field):
                self.assertFalse(
                    um.should_skip_model(self.TODAY, {field: 0.0000003})
                )

    def test_model_with_only_zero_cache_prices_not_skipped(self) -> None:
        payload = {
            "cache_read_input_token_cost": 0.0,
            "cache_creation_input_token_cost": 0.0,
        }

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

    def test_cache_pricing_uses_scaled_base_rates(self) -> None:
        m = um.build_model_from_litellm(
            "test",
            self._chat_payload(
                cache_read_input_token_cost=0.0000003,
                cache_creation_input_token_cost=0.00000375,
                cache_read_input_token_cost_above_200k_tokens=0.0000006,
                cache_creation_input_token_cost_above_1hr=0.000006,
            ),
            "chat",
        )
        self.assertAlmostEqual(m["cache_read_price"], 0.3, places=4)
        self.assertAlmostEqual(m["cache_write_price"], 3.75, places=4)

    def test_cache_pricing_omitted_for_non_chat_models(self) -> None:
        payload = {
            "input_cost_per_token": 0.00000002,
            "cache_read_input_token_cost": 0.00000001,
            "cache_creation_input_token_cost": 0.00000003,
        }
        m = um.build_model_from_litellm("embed-test", payload, "embedding")
        self.assertNotIn("cache_read_price", m)
        self.assertNotIn("cache_write_price", m)

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
        self.assertNotIn("cache_read_price", m)
        self.assertNotIn("cache_write_price", m)

    def test_cache_prices_follow_output_price_in_field_order(self) -> None:
        ordered = um.ordered_model(
            {
                "name": "test",
                "supports_vision": True,
                "cache_write_price": 3.75,
                "output_price": 15,
                "cache_read_price": 0.3,
            }
        )
        self.assertEqual(
            list(ordered),
            [
                "name",
                "output_price",
                "cache_read_price",
                "cache_write_price",
                "supports_vision",
            ],
        )


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


class TestBuildDiffSummary(unittest.TestCase):
    """A regenerated patch differing from the shipped one has to show up in the
    summary — that summary is the PR body for an auto-merge-labelled PR."""

    def _summary(self, old_patches: list[str], new_patches: list[str]) -> str:
        old = {"claude": {"m": {"name": "m", "patches": old_patches}}}
        new = OrderedDict(claude=[{"name": "m", "patches": new_patches}])
        return um.build_diff_summary(old, new)

    def test_patch_change_reported(self) -> None:
        summary = self._summary([".body.a = 1"], [".body.a = 2"])
        self.assertIn("request patch changes", summary)
        self.assertIn(".body.a = 1", summary)
        self.assertIn(".body.a = 2", summary)

    def test_identical_patches_report_no_changes(self) -> None:
        summary = self._summary([".body.a = 1"], [".body.a = 1"])
        self.assertIn("No model additions", summary)
        self.assertNotIn("- request patch changes:", summary)

    def test_provider_missing_from_new_catalog_is_reported_as_removed(self) -> None:
        # Dropping a whole provider block must not read as "no changes".
        old = {"someprovider": {"m": {"name": "m"}}}
        summary = um.build_diff_summary(old, OrderedDict())
        self.assertIn("someprovider:", summary)
        self.assertIn("removed", summary)
        self.assertNotIn("No model additions", summary)


class TestOrderedProviders(unittest.TestCase):
    """A provider already in models.yaml but in neither PROVIDER_ORDER nor the
    LiteLLM response used to be skipped entirely, silently deleting its models."""

    def test_provider_only_in_existing_yaml_is_included(self) -> None:
        order = um.ordered_providers({}, {"handrolled": {"m": {"name": "m"}}})
        self.assertIn("handrolled", order)

    def test_provider_only_from_litellm_is_included(self) -> None:
        order = um.ordered_providers({"fresh": {"m": {"name": "m"}}}, {})
        self.assertIn("fresh", order)

    def test_known_providers_keep_their_order_and_appear_once(self) -> None:
        order = um.ordered_providers({"claude": {}}, {"openai": {}})
        self.assertEqual(order[: len(um.PROVIDER_ORDER)], um.PROVIDER_ORDER)
        self.assertEqual(len(order), len(set(order)))


if __name__ == "__main__":
    unittest.main(verbosity=2)
