"""Focused regression tests for generated and preserved model variants."""

from __future__ import annotations

import unittest

import update_models as um


class TestOpenAIEffortVariants(unittest.TestCase):
    def _base(self, name: str = "gpt-5.6-sol") -> dict:
        return {
            "name": name,
            "max_input_tokens": 1050000,
            "max_output_tokens": 128000,
            "input_price": 4,
            "output_price": 20,
            "supports_vision": True,
            "supports_tool_use": True,
            "endpoint": "responses",
        }

    def test_variants_are_generated_from_the_refreshed_base(self) -> None:
        variants = um.openai_effort_variants(self._base(), "openai")
        self.assertEqual(
            [variant["name"] for variant in variants],
            ["gpt-5.6-sol:high", "gpt-5.6-sol:max"],
        )
        for variant in variants:
            self.assertEqual(variant["real_name"], "gpt-5.6-sol")
            self.assertEqual(variant["input_price"], 4)
            self.assertEqual(variant["output_price"], 20)
            self.assertEqual(variant["endpoint"], "responses")
            self.assertNotIn("require_max_tokens", variant)

    def test_variant_patch_sets_effort_without_sampling_parameters(self) -> None:
        variants = um.openai_effort_variants(self._base(), "openai")
        for variant in variants:
            effort = variant["name"].rsplit(":", 1)[1]
            patch = variant["patches"][0]
            self.assertIn("del(.body.temperature)", patch)
            self.assertIn("del(.body.top_p)", patch)
            self.assertIn(f'{{"effort":"{effort}"}}', patch)

    def test_base_patch_is_programmatic(self) -> None:
        model = self._base()
        model["patches"] = ["stale"]
        um.apply_openai_base_patches(model, "openai")
        self.assertEqual(model["patches"], [um.OPENAI_NO_SAMPLING_PATCH])

    def test_terra_only_exposes_the_curated_high_alias(self) -> None:
        variants = um.openai_effort_variants(self._base("gpt-5.6-terra"), "openai")
        self.assertEqual(
            [variant["name"] for variant in variants], ["gpt-5.6-terra:high"]
        )

    def test_other_providers_and_models_are_unchanged(self) -> None:
        self.assertEqual(um.openai_effort_variants(self._base(), "openrouter"), [])
        self.assertEqual(
            um.openai_effort_variants(self._base("gpt-5.6-luna"), "openai"), []
        )


class TestProviderModelRegeneration(unittest.TestCase):
    def test_openai_aliases_refresh_while_unowned_max_alias_survives(self) -> None:
        old_models = {
            "gpt-5.6-sol:high": {"name": "gpt-5.6-sol:high", "input_price": 99},
            "gpt-5.6-sol:max": {"name": "gpt-5.6-sol:max", "input_price": 99},
            "custom:max": {"name": "custom:max", "patches": [".body.custom = true"]},
        }
        fetched_models = {
            "gpt-5.6-sol": {
                "name": "gpt-5.6-sol",
                "max_input_tokens": 1050000,
                "max_output_tokens": 128000,
                "input_price": 4,
                "output_price": 20,
                "endpoint": "responses",
            },
            # A registry entry that collides with a generator-owned alias must
            # not overwrite the alias rebuilt from the base model.
            "gpt-5.6-sol:max": {
                "name": "gpt-5.6-sol:max",
                "input_price": 999,
            },
        }
        warnings: list[str] = []

        regenerated = um.regenerate_provider_models(
            "openai", old_models, fetched_models, warnings
        )
        by_name = {model["name"]: model for model in regenerated}

        self.assertEqual(by_name["gpt-5.6-sol:high"]["input_price"], 4)
        self.assertEqual(by_name["gpt-5.6-sol:max"]["output_price"], 20)
        self.assertEqual(by_name["custom:max"]["patches"], [".body.custom = true"])
        self.assertEqual(
            by_name["gpt-5.6-sol"]["patches"], [um.OPENAI_NO_SAMPLING_PATCH]
        )
        self.assertEqual(len(warnings), 1)
        self.assertIn("custom:max", warnings[0])
        self.assertNotIn("gpt-5.6-sol:max", warnings[0])

    def test_provider_without_registry_data_is_preserved_verbatim(self) -> None:
        old_models = {
            "qwen3-max:thinking": {
                "name": "qwen3-max:thinking",
                "patches": [".body.enable_thinking = true"],
            }
        }
        self.assertEqual(
            um.regenerate_provider_models("qianwen", old_models, None, []),
            [old_models["qwen3-max:thinking"]],
        )

    def test_unowned_thinking_alias_survives_registry_data(self) -> None:
        old_models = {
            "qwen3-max:thinking": {
                "name": "qwen3-max:thinking",
                "patches": [".body.enable_thinking = true"],
            }
        }
        fetched_models = {
            "qwen3-max": {"name": "qwen3-max", "max_input_tokens": 262144}
        }

        regenerated = um.regenerate_provider_models(
            "qianwen", old_models, fetched_models, []
        )

        self.assertIn("qwen3-max:thinking", {model["name"] for model in regenerated})
