"""Tests for the Claude thinking aliases generated from registry flags."""

from __future__ import annotations

import unittest

import update_models as um
from update_models_variant_tests import ADAPTIVE_ONLY, BUDGET_OR_ADAPTIVE, with_thinking


class TestThinkingVariant(unittest.TestCase):
    """`:thinking` aliases for models that still accept a fixed thinking
    budget (Opus 4.6 and earlier, Sonnet 4.6 and earlier, Haiku 4.5)."""

    def _base_claude_model(self) -> dict:
        return with_thinking({
            "name": "claude-opus-4-6",
            "max_input_tokens": 1000000,
            "max_output_tokens": 128000,
            "input_price": 5,
            "output_price": 25,
            "supports_vision": True,
            "supports_tool_use": True,
        }, BUDGET_OR_ADAPTIVE)

    def test_claude_thinking_variant_created(self) -> None:
        variants = um.thinking_variants(self._base_claude_model(), "claude")
        self.assertEqual(
            [variant["name"] for variant in variants],
            ["claude-opus-4-6:thinking", "claude-opus-4-6:max"],
        )
        self.assertEqual(variants[0]["real_name"], "claude-opus-4-6")

    def test_thinking_variant_has_correct_patch(self) -> None:
        variant = um.thinking_variants(self._base_claude_model(), "claude")[0]
        self.assertEqual(len(variant["patches"]), 1)
        self.assertIn('budget_tokens":16000', variant["patches"][0])

    def test_real_name_alias_inherits_base_cache_prices(self) -> None:
        base = self._base_claude_model()
        base["cache_read_price"] = 0.5
        base["cache_write_price"] = 6.25

        alias = um.thinking_variants(base, "claude")[0]

        self.assertEqual(alias["real_name"], base["name"])
        self.assertEqual(alias["cache_read_price"], base["cache_read_price"])
        self.assertEqual(alias["cache_write_price"], base["cache_write_price"])

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
        for name in ("claude-opus-4-6:thinking", "claude-opus-4-8:xhigh"):
            alias = with_thinking({"name": name}, ADAPTIVE_ONLY)
            self.assertEqual(um.thinking_variants(alias, "claude"), [])

    def test_non_claude_model_no_variant_for_claude_provider(self) -> None:
        model = with_thinking({"name": "gpt-4o"}, BUDGET_OR_ADAPTIVE)
        self.assertEqual(um.thinking_variants(model, "claude"), [])

    def test_model_without_registry_thinking_gets_no_variant(self) -> None:
        self.assertEqual(um.thinking_variants({"name": "claude-opus-4-6"}, "claude"), [])

    def test_bedrock_claude_gets_bedrock_patch(self) -> None:
        base = with_thinking({
            "name": "us.anthropic.claude-opus-4-6",
            "max_input_tokens": 200000,
            "supports_vision": True,
            "supports_tool_use": True,
        }, BUDGET_OR_ADAPTIVE)
        variants = um.thinking_variants(base, "bedrock")
        self.assertEqual(len(variants), 1)
        self.assertEqual(variants[0]["patches"], [um.BEDROCK_THINKING_PATCH])

    def test_non_us_bedrock_model_no_variant(self) -> None:
        base = with_thinking({"name": "ap-northeast-1/anthropic.claude-v2"}, BUDGET_OR_ADAPTIVE)
        self.assertEqual(um.thinking_variants(base, "bedrock"), [])

    def test_thinking_variant_real_name_uses_base_real_name_for_aliases(self) -> None:
        base = with_thinking({
            "name": "claude-opus-latest",
            "real_name": "claude-opus-4-6",  # alias that points to a versioned model
            "max_input_tokens": 1000000,
            "supports_tool_use": True,
        }, BUDGET_OR_ADAPTIVE)
        variant = um.thinking_variants(base, "claude")[0]
        self.assertEqual(variant["real_name"], "claude-opus-4-6")
        self.assertEqual(variant["name"], "claude-opus-latest:thinking")

    def test_thinking_variant_real_name_defaults_to_name_when_no_alias(self) -> None:
        base = with_thinking({"name": "claude-opus-4-6", "max_input_tokens": 1000000}, BUDGET_OR_ADAPTIVE)
        variant = um.thinking_variants(base, "claude")[0]
        self.assertEqual(variant["real_name"], "claude-opus-4-6")


class TestAdaptiveEffortVariants(unittest.TestCase):
    """Models that only accept adaptive thinking get it on the base model, an
    adaptive `:thinking`, and the effort aliases the registry flags."""

    def _base(self, name: str = "claude-opus-4-8") -> dict:
        return with_thinking({
            "name": name,
            "max_input_tokens": 1000000,
            "max_output_tokens": 128000,
            "input_price": 5,
            "output_price": 25,
            "supports_vision": True,
            "supports_tool_use": True,
        }, ADAPTIVE_ONLY)

    def _variant(self, base: dict, provider: str, suffix: str) -> dict:
        variants = um.thinking_variants(base, provider)
        return next(v for v in variants if v["name"].endswith(f":{suffix}"))

    def test_thinking_and_effort_aliases(self) -> None:
        for provider in ("claude", "vertexai"):
            names = [v["name"] for v in um.thinking_variants(self._base(), provider)]
            self.assertEqual(
                names,
                ["claude-opus-4-8:thinking", "claude-opus-4-8:xhigh", "claude-opus-4-8:max"],
            )

    def test_thinking_alias_is_adaptive_at_the_default_effort(self) -> None:
        variant = self._variant(self._base(), "claude", "thinking")
        patch = variant["patches"][0]
        self.assertIn('"type":"adaptive"', patch)
        self.assertNotIn("output_config", patch)
        self.assertNotIn("budget_tokens", patch)
        self.assertEqual(variant["max_output_tokens"], 24000)

    def test_effort_variant_patch_is_adaptive_with_effort(self) -> None:
        patch = self._variant(self._base(), "claude", "xhigh")["patches"][0]
        self.assertIn('"type":"adaptive"', patch)
        # Whole-object assignment: jaq won't create the intermediate object.
        self.assertIn('.body.output_config = {"effort":"xhigh"}', patch)
        self.assertIn("del(.body.temperature)", patch)
        self.assertNotIn("budget_tokens", patch)

    def _generated_patches(self, provider: str) -> list[str]:
        prefix = "us.anthropic." if provider == "bedrock" else ""
        patches = []
        for name, flags in (("claude-opus-4-6", BUDGET_OR_ADAPTIVE), ("claude-opus-4-8", ADAPTIVE_ONLY)):
            base = with_thinking({"name": prefix + name}, flags)
            um.apply_base_thinking(base, provider)
            variants = um.thinking_variants(base, provider)
            patches += [*base.get("patches", []), *(v["patches"][0] for v in variants)]
        return patches

    def test_no_generated_patch_assigns_into_a_nested_path(self) -> None:
        # jaq does not create missing intermediate objects, so an assignment
        # into `.body.<obj>.<field>` errors at request time and aborts the rest
        # of the pipeline. Every generated patch must assign whole containers.
        patches = [
            um.claude_adaptive_patch(),
            um.claude_adaptive_patch("max"),
            um.BEDROCK_THINKING_PATCH,
            um.BEDROCK_ADAPTIVE_THINKING_PATCH,
            *self._generated_patches("claude"),
            *self._generated_patches("vertexai"),
            *self._generated_patches("bedrock"),
        ]
        for patch in patches:
            for assignment in patch.split("|"):
                lhs = assignment.split("=")[0].strip()
                if not lhs.startswith("."):
                    continue
                self.assertLessEqual(
                    lhs.count("."),
                    2,
                    f"patch assigns into a nested path: {assignment.strip()!r}",
                )

    def test_effort_variant_requires_max_tokens(self) -> None:
        # Adaptive-only Opus rejects requests without an explicit max_tokens.
        variant = self._variant(self._base(), "claude", "xhigh")
        self.assertTrue(variant.get("require_max_tokens"))
        self.assertEqual(variant["max_output_tokens"], 128000)

    def test_effort_variant_routes_via_real_name(self) -> None:
        base = self._base("claude-opus-4-8@default")
        base["real_name"] = "claude-opus-4-8@default"
        variant = self._variant(base, "vertexai", "xhigh")
        self.assertEqual(variant["name"], "claude-opus-4-8@default:xhigh")
        self.assertEqual(variant["real_name"], "claude-opus-4-8@default")

    def test_bedrock_adaptive_only_gets_an_adaptive_thinking_alias(self) -> None:
        base = self._base("us.anthropic.claude-opus-4-8")
        variants = um.thinking_variants(base, "bedrock")
        self.assertEqual([v["name"] for v in variants], ["us.anthropic.claude-opus-4-8:thinking"])
        self.assertEqual(variants[0]["patches"], [um.BEDROCK_ADAPTIVE_THINKING_PATCH])

    def test_apply_base_thinking_patches_adaptive_only(self) -> None:
        for provider in ("claude", "vertexai"):
            model = self._base()
            um.apply_base_thinking(model, provider)
            self.assertEqual(len(model["patches"]), 1)
            self.assertIn('"type":"adaptive"', model["patches"][0])
            self.assertNotIn("output_config", model["patches"][0])
            self.assertTrue(model.get("require_max_tokens"))

    def test_apply_base_thinking_keeps_curated_patches(self) -> None:
        header = '.headers["x-custom"] = "kept"'
        model = self._base()
        model["patches"] = [header, um.CLAUDE_THINKING_PATCH]
        for _ in range(2):
            um.apply_base_thinking(model, "claude")
        self.assertEqual(len(model["patches"]), 2)
        self.assertEqual(model["patches"][0], header)
        self.assertIn('"type":"adaptive"', model["patches"][1])

    def test_apply_base_thinking_skips_models_that_accept_a_budget(self) -> None:
        model = with_thinking({"name": "claude-opus-4-6", "max_input_tokens": 1000}, BUDGET_OR_ADAPTIVE)
        um.apply_base_thinking(model, "claude")
        self.assertNotIn("patches", model)

    def test_apply_base_thinking_skips_bedrock(self) -> None:
        model = self._base("us.anthropic.claude-opus-4-8")
        um.apply_base_thinking(model, "bedrock")
        self.assertNotIn("patches", model)
