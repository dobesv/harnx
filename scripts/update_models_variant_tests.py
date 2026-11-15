"""Focused regression tests for generated and preserved model variants."""

from __future__ import annotations

import unittest

import update_models as um

GEMINI_THOUGHTS_PATCH = (
    ".body.generationConfig.thinkingConfig = "
    '((.body.generationConfig.thinkingConfig // {}) + {"includeThoughts":true})'
)


# Thinking flags as the LiteLLM registry records them for Claude models.
BUDGET_ONLY = {"supports_reasoning": True}  # Haiku 4.5 and earlier
BUDGET_OR_ADAPTIVE = {  # Opus and Sonnet 4.6
    "supports_reasoning": True,
    "supports_adaptive_thinking": True,
    "supports_legacy_thinking": True,
    "supports_max_reasoning_effort": True,
}
ADAPTIVE_ONLY = {  # Opus 4.7 and later, every 5.x model
    "supports_reasoning": True,
    "supports_adaptive_thinking": True,
    "supports_xhigh_reasoning_effort": True,
    "supports_max_reasoning_effort": True,
}


def registry_entry(litellm_provider: str, flags: dict) -> dict:
    """A chat entry shaped like LiteLLM's, with the given thinking flags."""
    return {
        "litellm_provider": litellm_provider,
        "mode": "chat",
        "max_input_tokens": 1000000,
        "max_output_tokens": 128000,
        "input_cost_per_token": 5e-06,
        "output_cost_per_token": 2.5e-05,
        "supports_function_calling": True,
        **flags,
    }


def with_thinking(model: dict, flags: dict) -> dict:
    """`model` as ingestion leaves it for a registry entry with `flags`."""
    return {**model, um.THINKING_SUPPORT_FIELD: um.ThinkingSupport.from_registry(flags)}


class TestDirectProviderRules(unittest.TestCase):
    def test_curated_zai_provider_can_be_preserved_and_rendered(self) -> None:
        old = {"glm-5.3": {"name": "glm-5.3", "max_input_tokens": 868928}}
        models = um.regenerate_provider_models("zai", old, None)
        block = um.render_provider_block("zai", models)
        self.assertIn("provider: zai", block)
        self.assertIn("name: glm-5.3", block)
        self.assertIn("https://docs.z.ai/", block)

    def test_refreshed_direct_models_retain_supported_reasoning(self) -> None:
        for provider, name, effort in (("zai", "glm-5.3", "high"), ("minimax", "MiniMax-M3.1-Flash", "low")):
            with self.subTest(provider=provider):
                old = {name: {"name": name, "patches": ["stale"]}}
                refreshed = {name: {"name": name, "max_input_tokens": 1000000}}
                models = um.regenerate_provider_models(provider, old, refreshed)
                patch = models[0]["patches"][0]
                self.assertIn(f'.body.reasoning_effort = "{effort}"', patch)
                self.assertNotIn('"disabled"', patch)
                if provider == "minimax":
                    self.assertIn(".body.max_completion_tokens", patch)
                    self.assertIn("del(.body.max_tokens)", patch)
                    self.assertEqual((models[0]["max_input_tokens"], models[0]["max_output_tokens"]), (868928, 131072))


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
            "gpt-6.1-terra",
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
    def _registry_catalog(self, registry: dict[str, dict]) -> dict[str, dict[str, dict]]:
        # Run registry entries through the same ingestion and regeneration as
        # the weekly refresh, starting from an empty catalog.
        fetched, _ = um.collect_provider_models(registry, {})
        return {
            provider: {
                model["name"]: model
                for model in um.regenerate_provider_models(provider, {}, models)
            }
            for provider, models in fetched.items()
        }

    def test_registry_flags_decide_claude_thinking_aliases(self) -> None:
        catalog = self._registry_catalog({
            "claude-haiku-4-5": registry_entry("anthropic", BUDGET_ONLY),
            "claude-opus-4-6": registry_entry("anthropic", BUDGET_OR_ADAPTIVE),
            "claude-fable-5-2": registry_entry("anthropic", ADAPTIVE_ONLY),
            "vertex_ai/claude-opus-5@default": registry_entry("vertex_ai", ADAPTIVE_ONLY),
            "vertex_ai/claude-3-5-sonnet": registry_entry("vertex_ai", {}),
            "us.anthropic.claude-sonnet-4-6": registry_entry("bedrock_converse", BUDGET_OR_ADAPTIVE),
            "us.anthropic.claude-mythos-5-2": registry_entry("bedrock_converse", ADAPTIVE_ONLY),
            "us.anthropic.claude-3-5-haiku-20241022-v1:0": registry_entry("bedrock_converse", {}),
        })
        claude, vertex, bedrock = catalog["claude"], catalog["vertexai"], catalog["bedrock"]
        self.assertEqual(set(claude), {
            "claude-haiku-4-5", "claude-haiku-4-5:thinking",
            "claude-opus-4-6", "claude-opus-4-6:thinking", "claude-opus-4-6:max",
            "claude-fable-5-2", "claude-fable-5-2:thinking",
            "claude-fable-5-2:xhigh", "claude-fable-5-2:max",
        })
        self.assertEqual(set(vertex), {
            "claude-opus-5@default", "claude-opus-5@default:thinking",
            "claude-opus-5@default:xhigh", "claude-opus-5@default:max",
            "claude-3-5-sonnet",
        })
        self.assertEqual(set(bedrock), {
            "us.anthropic.claude-sonnet-4-6", "us.anthropic.claude-sonnet-4-6:thinking",
            "us.anthropic.claude-mythos-5-2", "us.anthropic.claude-mythos-5-2:thinking",
            "us.anthropic.claude-3-5-haiku-20241022-v1:0",
        })

        # A model that still accepts a fixed budget keeps it on `:thinking`,
        # and leaves its base model alone.
        for name in ("claude-haiku-4-5", "claude-opus-4-6"):
            self.assertEqual(claude[f"{name}:thinking"]["patches"], [um.CLAUDE_THINKING_PATCH])
            self.assertNotIn("patches", claude[name])
        self.assertEqual(
            bedrock["us.anthropic.claude-sonnet-4-6:thinking"]["patches"],
            [um.BEDROCK_THINKING_PATCH],
        )
        self.assertIn('"effort":"max"', claude["claude-opus-4-6:max"]["patches"][0])

        # Adaptive-only models get adaptive thinking everywhere, at the
        # model's own default effort except on the effort aliases.
        for models, name in ((claude, "claude-fable-5-2"), (vertex, "claude-opus-5@default")):
            base, thinking = models[name], models[f"{name}:thinking"]
            self.assertTrue(base["require_max_tokens"])
            self.assertEqual(base["max_output_tokens"], 128000)
            self.assertEqual(thinking["max_output_tokens"], 24000)
            self.assertEqual(thinking["real_name"], name)
            for model in (base, thinking):
                patch = model["patches"][0]
                self.assertIn('"type":"adaptive","display":"summarized"', patch)
                self.assertNotIn("output_config", patch)
                self.assertNotIn("budget_tokens", patch)
            for effort in ("xhigh", "max"):
                self.assertIn(f'"effort":"{effort}"', models[f"{name}:{effort}"]["patches"][0])
        self.assertEqual(
            bedrock["us.anthropic.claude-mythos-5-2:thinking"]["patches"],
            [um.BEDROCK_ADAPTIVE_THINKING_PATCH],
        )
        self.assertNotIn("patches", bedrock["us.anthropic.claude-mythos-5-2"])

        # Models that don't reason get no thinking alias.
        self.assertNotIn("patches", vertex["claude-3-5-sonnet"])

    def test_thinking_support_reads_registry_flags(self) -> None:
        support = um.ThinkingSupport.from_registry
        self.assertEqual(support(BUDGET_ONLY), um.ThinkingSupport(budget=True))
        self.assertEqual(
            support(BUDGET_OR_ADAPTIVE),
            um.ThinkingSupport(budget=True, adaptive=True, efforts=("max",)),
        )
        self.assertEqual(
            support(ADAPTIVE_ONLY),
            um.ThinkingSupport(adaptive=True, efforts=("xhigh", "max")),
        )
        self.assertTrue(support(ADAPTIVE_ONLY).adaptive_only)
        self.assertEqual(support({}), um.ThinkingSupport())
        # An effort flag without adaptive thinking doesn't make an alias.
        self.assertEqual(
            support({"supports_reasoning": True, "supports_max_reasoning_effort": True}),
            um.ThinkingSupport(budget=True),
        )

    def test_unlisted_models_are_dropped_from_covered_providers(self) -> None:
        old = {
            "claude-opus-4-1": {"name": "claude-opus-4-1"},
            "claude-opus-4-1:thinking": {"name": "claude-opus-4-1:thinking"},
            "claude-custom:max": {"name": "claude-custom:max", "real_name": "claude-opus-4-8"},
        }
        fetched = {"claude-opus-4-8": with_thinking({"name": "claude-opus-4-8"}, ADAPTIVE_ONLY)}
        names = {model["name"] for model in um.regenerate_provider_models("claude", old, fetched)}
        self.assertEqual(names, {
            "claude-opus-4-8", "claude-opus-4-8:thinking",
            "claude-opus-4-8:xhigh", "claude-opus-4-8:max",
        })

    def test_kinds_of_model_the_registry_doesnt_list_stay_hand_maintained(self) -> None:
        # The registry lists Jina's rerankers but none of its embedding models.
        old = {
            "jina-embeddings-v3": {"name": "jina-embeddings-v3", "type": "embedding"},
            "jina-reranker-v1": {"name": "jina-reranker-v1", "type": "reranker"},
        }
        fetched = {"jina-reranker-v3.5": {"name": "jina-reranker-v3.5", "type": "reranker"}}
        names = {model["name"] for model in um.regenerate_provider_models("jina", old, fetched)}
        self.assertEqual(names, {"jina-embeddings-v3", "jina-reranker-v3.5"})

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
            for model in um.regenerate_provider_models("openai", old, fetched)
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
            for model in um.regenerate_provider_models(provider, old, fetched)
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

    def _regenerate_new_models(self, provider: str, models: list[dict]) -> dict[str, dict]:
        fetched = {model["name"]: model for model in models}
        return {
            model["name"]: model
            for model in um.regenerate_provider_models(provider, {}, fetched)
        }

    def test_new_gemini_thinking_models_request_thoughts(self) -> None:
        # A model the registry adds has no curated patch to carry over.
        models = [
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

    def test_argon_refresh_preserves_single_request_context_and_output_limits(self) -> None:
        name = "gemini-4-argon"
        fetched = {
            name: {
                "name": name, "max_input_tokens": 1048576,
                "max_output_tokens": 1048576, "input_price": 2, "output_price": 10,
            }
        }
        curated = '.headers["x-custom"] = "preserved"'
        old = {name: {"name": name, "patches": [curated, GEMINI_THOUGHTS_PATCH]}}
        fetched[name] = um.merge_old_fields(fetched[name], old[name])
        models = um.regenerate_provider_models("gemini", old, fetched)
        model = models[0]
        self.assertEqual(model["max_input_tokens"], 983040)
        self.assertEqual(model["max_output_tokens"], 65536)
        self.assertTrue(model["require_max_tokens"])
        self.assertEqual(model["input_price"], 2)
        self.assertEqual(model["patches"][0], curated)
        self.assertIn("includeThoughts", model["patches"][1])
        self.assertEqual(len(model["patches"]), 2)
        fetched_again = {name: um.merge_old_fields({"name": name}, model)}
        refreshed = um.regenerate_provider_models("gemini", {name: model}, fetched_again)[0]
        self.assertEqual(refreshed["patches"], model["patches"])

    def test_glm_5_3_effort_patch_survives_refresh(self) -> None:
        name = "us.zai.glm-5.3"
        _, providers = um.load_existing_data()
        old = {name: providers["bedrock"][name]}
        refreshed = um.regenerate_provider_models(
            "bedrock", old, {name: {"name": name, "max_input_tokens": 1000000}}
        )[0]
        self.assertEqual(refreshed["patches"], old[name]["patches"])
        self.assertIn('"reasoning_effort":"high"', refreshed["patches"][0])

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
            regenerated = um.regenerate_provider_models(provider, old, fetched)
            with self.subTest(provider=provider):
                self.assertEqual(regenerated[0]["patches"], curated)

    def test_refresh_replaces_a_rejected_budget_alias(self) -> None:
        # Shipped while the generator matched models by name: a fixed budget
        # that an adaptive-only model answers with a 400.
        name = "claude-opus-5"
        alias = f"{name}:thinking"
        old = {
            name: {"name": name},
            alias: {"name": alias, "real_name": name, "patches": [um.CLAUDE_THINKING_PATCH]},
        }
        base = with_thinking(
            {"name": name, "max_output_tokens": 128000, "input_price": 5}, ADAPTIVE_ONLY
        )
        for provider in ("claude", "vertexai"):
            by_name = {
                model["name"]: model
                for model in um.regenerate_provider_models(provider, old, {name: base})
            }
            with self.subTest(provider=provider):
                self.assertEqual(set(by_name), {name, alias, f"{name}:xhigh", f"{name}:max"})
                for model in by_name.values():
                    self.assertTrue(model["require_max_tokens"])
                    self.assertEqual(model["input_price"], 5)
                    self.assertIn('"type":"adaptive"', model["patches"][0])
                    self.assertNotIn("budget_tokens", model["patches"][0])
                self.assertEqual(by_name[alias]["max_output_tokens"], 24000)
                self.assertEqual(by_name[f"{name}:max"]["max_output_tokens"], 128000)

    def test_binding_controls_follow_models_that_check_replayed_thinking(self) -> None:
        for name, binds in (
            ("claude-fable-5-1", True),
            ("claude-mythos-5-1", True),
            ("claude-fable-5-2@default", True),
            ("claude-fable-6", True),
            ("claude-haiku-5-5", True),
            ("claude-fable-5", False),
            ("claude-opus-5-5", False),
        ):
            base = with_thinking({"name": name, "max_output_tokens": 128000}, ADAPTIVE_ONLY)
            for provider in ("claude", "vertexai"):
                models = um.regenerate_provider_models(provider, {}, {name: base})
                with self.subTest(provider=provider, name=name):
                    self.assertEqual(len(models), 4)
                    for model in models:
                        patch = model["patches"][0]
                        self.assertEqual("drop_block" in patch, binds)
                        self.assertEqual(
                            "thinking-binding-controls-2026-08-01" in patch, binds
                        )

    def test_openai_aliases_refresh_while_unlisted_alias_is_dropped(self) -> None:
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
        regenerated = um.regenerate_provider_models("openai", old_models, fetched_models)
        by_name = {model["name"]: model for model in regenerated}

        self.assertEqual(by_name["gpt-5.6-sol:high"]["input_price"], 4)
        self.assertEqual(by_name["gpt-5.6-sol:max"]["output_price"], 20)
        self.assertNotIn("custom:max", by_name)
        self.assertEqual(
            by_name["gpt-5.6-sol"]["patches"],
            [f'{um.OPENAI_NO_SAMPLING_PATCH} | .body.reasoning = {{"summary":"auto"}}'],
        )

    def test_provider_without_registry_data_is_preserved_verbatim(self) -> None:
        old_models = {
            "qwen3-max:thinking": {
                "name": "qwen3-max:thinking",
                "patches": [".body.enable_thinking = true"],
            }
        }
        self.assertEqual(
            um.regenerate_provider_models("qianwen", old_models, None),
            [old_models["qwen3-max:thinking"]],
        )
