"""Focused regression tests for generated and preserved model variants."""

from __future__ import annotations

import unittest

import update_models as um

GEMINI_THOUGHTS_PATCH = (
    ".body.generationConfig.thinkingConfig = "
    '((.body.generationConfig.thinkingConfig // {}) + {"includeThoughts":true})'
)


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
            self.assertIn(f'{{"effort":"{effort}","summary":"auto"}}', patch)

    def test_base_patch_is_programmatic(self) -> None:
        for name in (
            "gpt-5.6-sol",
            "gpt-5.6-terra",
            "gpt-5.6-luna",
            "gpt-6-astra",
            "gpt-6-sol",
            "gpt-6.1-sol",
            "gpt-6-luna",
        ):
            with self.subTest(name=name):
                model = self._base(name)
                model["patches"] = ["stale"]
                um.apply_openai_base_patches(model, "openai")
                self.assertEqual(
                    model["patches"],
                    [f'{um.OPENAI_NO_SAMPLING_PATCH} | .body.reasoning = {{"summary":"auto"}}'],
                )

    def test_astra_exposes_high_and_max_with_refreshed_prices(self) -> None:
        base = self._base("gpt-6-astra")
        base.update(input_price=10, output_price=50)
        variants = um.openai_effort_variants(base, "openai")
        self.assertEqual(
            [v["name"] for v in variants],
            ["gpt-6-astra:high", "gpt-6-astra:max"],
        )
        for variant in variants:
            effort = variant["name"].rsplit(":", 1)[1]
            self.assertEqual(variant["real_name"], "gpt-6-astra")
            self.assertEqual(variant["input_price"], 10)
            self.assertEqual(variant["output_price"], 50)
            self.assertEqual(variant["endpoint"], "responses")
            self.assertIn(f'"effort":"{effort}"', variant["patches"][0])

    def test_terra_only_exposes_the_curated_high_alias(self) -> None:
        variants = um.openai_effort_variants(self._base("gpt-5.6-terra"), "openai")
        self.assertEqual(
            [variant["name"] for variant in variants], ["gpt-5.6-terra:high"]
        )

    def test_gpt_6_sol_family_exposes_high_and_max(self) -> None:
        for name in ("gpt-6-sol", "gpt-6.1-sol"):
            with self.subTest(name=name):
                base = self._base(name)
                base["cache_read_price"] = 0.1
                variants = um.openai_effort_variants(base, "openai")
                self.assertEqual(
                    [variant["name"] for variant in variants],
                    [f"{name}:high", f"{name}:max"],
                )
                for variant in variants:
                    self.assertEqual(variant["real_name"], name)
                    self.assertEqual(variant["cache_read_price"], 0.1)
                    self.assertEqual(variant["endpoint"], "responses")

    def test_other_providers_and_models_are_unchanged(self) -> None:
        self.assertEqual(um.openai_effort_variants(self._base(), "openrouter"), [])
        self.assertEqual(
            um.openai_effort_variants(self._base("gpt-5.6-luna"), "openai"), []
        )


class TestProviderModelRegeneration(unittest.TestCase):
    def _refresh_catalog(self, provider: str, old: dict[str, dict]) -> dict[str, dict]:
        # Simulate registry data without harnx fields or generated aliases, so
        # preservation must go through both the field merge and the generator.
        fetched = {
            name: um.merge_old_fields(
                {key: value for key, value in model.items() if key not in um.HARNX_ONLY_FIELDS},
                model,
            )
            for name, model in old.items()
            if not um.is_generated_variant_name(provider, name)
        }
        return {
            model["name"]: model
            for model in um.regenerate_provider_models(provider, old, fetched, [])
        }

    def _assert_request_settings_preserved(self, old: dict, refreshed: dict) -> None:
        for field in ("patches", "max_output_tokens", "require_max_tokens", "real_name"):
            self.assertEqual(refreshed.get(field), old.get(field), field)

    def test_claude_summary_patches_survive_refresh_and_alias_regeneration(self) -> None:
        _, providers = um.load_existing_data()
        for provider, minimum_summarized in (("claude", 36), ("vertexai", 58)):
            old = providers[provider]
            refreshed = self._refresh_catalog(provider, old)
            checked = 0
            for name, model in old.items():
                if not name.startswith("claude-"):
                    continue
                with self.subTest(provider=provider, name=name):
                    self._assert_request_settings_preserved(model, refreshed[name])
                if any('"display":"summarized"' in patch for patch in model.get("patches", [])):
                    checked += 1
            self.assertGreaterEqual(checked, minimum_summarized, provider)

    def test_bedrock_summary_patches_survive_refresh_without_changing_older_models(self) -> None:
        _, providers = um.load_existing_data()
        old = providers["bedrock"]
        refreshed = self._refresh_catalog("bedrock", old)
        summarized = set()
        older_thinking = 0
        for name, model in old.items():
            if not name.startswith("us.anthropic.claude-"):
                continue
            with self.subTest(name=name):
                self._assert_request_settings_preserved(model, refreshed[name])
            patches = model.get("patches", [])
            if any('"display":"summarized"' in patch for patch in patches):
                summarized.add(name)
            elif name.endswith(":thinking"):
                older_thinking += 1
                with self.subTest(older_alias=name):
                    self.assertNotIn("display", " ".join(refreshed[name]["patches"]))
        self.assertEqual(summarized, {
            "us.anthropic.claude-fable-5:thinking",
            "us.anthropic.claude-fable-5-1:thinking",
            "us.anthropic.claude-opus-5:thinking",
            "us.anthropic.claude-sonnet-5:thinking",
            "us.anthropic.claude-sonnet-5-5:thinking",
        })
        self.assertGreater(older_thinking, 0)

    def test_openai_summary_patches_survive_refresh_and_alias_regeneration(self) -> None:
        _, providers = um.load_existing_data()
        old = providers["openai"]
        fetched = {
            name: um.merge_old_fields(
                {key: value for key, value in model.items() if key not in um.HARNX_ONLY_FIELDS},
                model,
            )
            for name, model in old.items()
            if not um.is_generated_variant_name("openai", name)
        }
        regenerated = {
            model["name"]: model
            for model in um.regenerate_provider_models("openai", old, fetched, [])
        }
        checked = 0
        for name, model in old.items():
            if model.get("endpoint") != "responses":
                continue
            with self.subTest(name=name):
                self.assertEqual(regenerated[name].get("patches"), model.get("patches"))
            if any('"summary":"auto"' in patch for patch in model.get("patches", [])):
                checked += 1
        self.assertGreater(checked, 0)

    def _assert_gemini_provider_patches_survive_refresh(
        self, provider: str, models: dict[str, dict]
    ) -> int:
        old = {
            name: model
            for name, model in models.items()
            if not name.startswith("claude-")
        }
        fetched = {
            name: um.merge_old_fields(
                {key: value for key, value in model.items() if key not in um.HARNX_ONLY_FIELDS},
                model,
            )
            for name, model in old.items()
        }
        regenerated = {
            model["name"]: model
            for model in um.regenerate_provider_models(provider, old, fetched, [])
        }
        checked = 0
        for name, model in old.items():
            with self.subTest(provider=provider, name=name):
                self.assertEqual(regenerated[name].get("patches"), model.get("patches"))
            if any('"includeThoughts":true' in patch for patch in model.get("patches", [])):
                checked += 1
        return checked

    def test_gemini_thought_patches_survive_refresh(self) -> None:
        _, providers = um.load_existing_data()
        checked = sum(
            self._assert_gemini_provider_patches_survive_refresh(provider, providers[provider])
            for provider in ("gemini", "vertexai")
        )
        self.assertGreaterEqual(checked, 25)

    def test_unowned_gemini_alias_keeps_its_own_thinking_patch(self) -> None:
        base_name = "gemini-2.5-pro"
        alias_name = f"{base_name}:high"
        patch = (
            '.body.generationConfig.thinkingConfig = '
            '{"thinkingLevel":"high","includeThoughts":true}'
        )
        for provider in ("gemini", "vertexai"):
            old = {alias_name: {"name": alias_name, "real_name": base_name, "patches": [patch]}}
            regenerated = {
                model["name"]: model
                for model in um.regenerate_provider_models(provider, old, {base_name: {"name": base_name}}, [])
            }
            self.assertEqual(regenerated[alias_name], old[alias_name])
            self.assertEqual(regenerated[base_name].get("patches"), [GEMINI_THOUGHTS_PATCH])

    def _regenerate_new_models(self, provider: str, models: list[dict]) -> dict[str, dict]:
        fetched = {model["name"]: model for model in models}
        return {
            model["name"]: model
            for model in um.regenerate_provider_models(provider, {}, fetched, [])
        }

    def test_new_gemini_thinking_models_request_thoughts(self) -> None:
        # A model the registry adds has no curated patch to carry over.
        models = [
            {"name": "gemini-4-argon", "supports_tool_use": True},
            {"name": "gemini-4-argon-cyber-permissive"},
            {"name": "gemini-2.5-flash"},
            {"name": "argon-latest", "real_name": "gemini-4-argon"},
        ]
        for provider in ("gemini", "vertexai"):
            regenerated = self._regenerate_new_models(provider, models)
            for model in models:
                with self.subTest(provider=provider, name=model["name"]):
                    self.assertEqual(
                        regenerated[model["name"]].get("patches"), [GEMINI_THOUGHTS_PATCH]
                    )

    def test_gemini_models_that_do_not_think_get_no_patch(self) -> None:
        models = [
            {"name": "gemini-1.5-pro"},
            {"name": "gemini-2.0-flash"},
            {"name": "gemini-2.5-flash-image"},
            {"name": "gemini-2.5-flash-preview-tts"},
            {"name": "gemini-2.5-flash-native-audio-latest"},
            {"name": "gemini-omni-1.1-flash"},
            {"name": "gemini-3.5-embedding", "type": "embedding"},
        ]
        for provider in ("gemini", "vertexai"):
            regenerated = self._regenerate_new_models(provider, models)
            for model in models:
                with self.subTest(provider=provider, name=model["name"]):
                    self.assertNotIn("patches", regenerated[model["name"]])
        # Other providers serve Gemini through APIs without generationConfig.
        regenerated = self._regenerate_new_models("github", [{"name": "gemini-4-argon"}])
        self.assertNotIn("patches", regenerated["gemini-4-argon"])

    def test_curated_gemini_patch_is_not_replaced(self) -> None:
        name = "gemini-3.5-flash"
        curated = ['.body.generationConfig.thinkingConfig = {"thinkingBudget":0}']
        for provider in ("gemini", "vertexai"):
            old = {name: {"name": name, "patches": curated}}
            fetched = {name: um.merge_old_fields({"name": name, "input_price": 1}, old[name])}
            regenerated = um.regenerate_provider_models(provider, old, fetched, [])
            with self.subTest(provider=provider):
                self.assertEqual(regenerated[0]["patches"], curated)

    def test_bedrock_opus_5_5_does_not_generate_fixed_budget_variant(self) -> None:
        name = "us.anthropic.claude-opus-5-5"
        self.assertEqual(um.thinking_variants({"name": name}, "bedrock"), [])

    def test_bedrock_sonnet_5_5_adaptive_correction_survives_regeneration(self) -> None:
        _, providers = um.load_existing_data()
        name = "us.anthropic.claude-sonnet-5-5"
        alias = f"{name}:thinking"
        old = {key: providers["bedrock"][key] for key in (name, alias)}
        fetched = {name: dict(old[name], input_price=3)}
        regenerated = {
            model["name"]: model
            for model in um.regenerate_provider_models("bedrock", old, fetched, [])
        }
        variant = regenerated[alias]
        self.assertEqual(variant["patches"], old[alias]["patches"])
        self.assertEqual(variant["real_name"], name)
        self.assertEqual(variant["max_output_tokens"], 24000)
        self.assertTrue(variant["require_max_tokens"])
        self.assertEqual(variant["input_price"], 3)
        patch = variant["patches"][0]
        self.assertIn('"type":"adaptive","display":"summarized"', patch)
        self.assertIn('"output_config":{"effort":"high"}', patch)
        self.assertNotIn("budget_tokens", patch)
        self.assertNotIn("patches", regenerated[name])
        self.assertEqual(regenerated[name]["max_output_tokens"], 128000)

        # Only the probed model changes; don't migrate older aliases here.
        older = um.thinking_variants({"name": "us.anthropic.claude-sonnet-4-6"}, "bedrock")[0]
        self.assertIn('"type":"enabled","budget_tokens":16000', older["patches"][0])

    def test_adaptive_models_regenerate_variants_with_required_limit(self) -> None:
        for name, input_price, base_effort in (
            ("claude-opus-5-5", 4, "medium"),
            ("claude-fable-5-1", 10, "high"),
        ):
            base = {"name": name, "max_output_tokens": 128000, "input_price": input_price}
            old = {f"{name}:thinking": {"name": f"{name}:thinking"}}
            for provider in ("claude", "vertexai"):
                models = um.regenerate_provider_models(provider, old, {name: base}, [])
                by_name = {model["name"]: model for model in models}
                self.assertEqual(set(by_name), {name, f"{name}:xhigh", f"{name}:max"})
                for model in models:
                    self.assertTrue(model["require_max_tokens"])
                    self.assertEqual(model["max_output_tokens"], 128000)
                    self.assertEqual(model["input_price"], input_price)
                    self.assertIn('"type":"adaptive"', model["patches"][0])
                    self.assertNotIn("budget_tokens", model["patches"][0])
                self.assertIn(f'"effort":"{base_effort}"', by_name[name]["patches"][0])
                for effort in ("xhigh", "max"):
                    self.assertIn(
                        f'"effort":"{effort}"', by_name[f"{name}:{effort}"]["patches"][0]
                    )

    def test_fable_5_1_adds_binding_controls(self) -> None:
        name = "claude-fable-5-1"
        base = {"name": name, "max_output_tokens": 128000}
        models = um.regenerate_provider_models("claude", {}, {name: base}, [])
        for model in models:
            self.assertIn("drop_block", model["patches"][0])
            self.assertIn("thinking-binding-controls-2026-08-01", model["patches"][0])

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
            by_name["gpt-5.6-sol"]["patches"],
            [f'{um.OPENAI_NO_SAMPLING_PATCH} | .body.reasoning = {{"summary":"auto"}}'],
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
