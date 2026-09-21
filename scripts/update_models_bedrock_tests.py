"""Bedrock catalog ingestion tests: id screening, de-duplication, corrections.

The Bedrock block is the part of the registry harnx trusts least, so its
filtering and its overrides get their own regression module.
"""

from __future__ import annotations

import unittest

import update_models as um


class TestIsValidBedrockModelName(unittest.TestCase):
    def test_direct_open_weight_model_ids_accepted(self) -> None:
        for name in ("zai.glm-5", "zai.glm-4.7-flash", "minimax.minimax-m2.5"):
            with self.subTest(name=name):
                self.assertTrue(um.is_valid_bedrock_model_name(name))

    def test_canonical_us_format_accepted(self) -> None:
        self.assertTrue(um.is_valid_bedrock_model_name("us.anthropic.claude-opus-4-7"))

    def test_region_prefixed_rejected(self) -> None:
        self.assertFalse(um.is_valid_bedrock_model_name("ap-northeast-1/anthropic.claude-v2"))

    def test_slash_in_name_rejected(self) -> None:
        self.assertFalse(um.is_valid_bedrock_model_name("us.anthropic/claude-opus-4-7"))

    def test_non_us_rejected(self) -> None:
        self.assertFalse(um.is_valid_bedrock_model_name("eu.anthropic.claude-opus-4-7"))

    def test_every_non_us_geography_rejected(self) -> None:
        for name in (
            "eu.anthropic.claude-opus-5",
            "apac.anthropic.claude-3-haiku-20240307-v1:0",
            "au.anthropic.claude-sonnet-5",
            "jp.anthropic.claude-opus-4-8",
            "global.cohere.embed-v4:0",
            "us-gov.nvidia.nemotron-nano-9b-v2",
        ):
            with self.subTest(name=name):
                self.assertFalse(um.is_valid_bedrock_model_name(name))

    def test_vendors_outside_the_old_allowlist_accepted(self) -> None:
        # The previous allowlist admitted only `us.`, `zai.` and `minimax.`,
        # so each vendor AWS added was dropped until someone edited this file.
        for name in (
            "qwen.qwen3-coder-next",
            "moonshotai.kimi-k2.5",
            "deepseek.v3.2",
            "nvidia.nemotron-super-3-120b",
            "openai.gpt-oss-120b-1:0",
            "mistral.devstral-2-123b",
        ):
            with self.subTest(name=name):
                self.assertTrue(um.is_valid_bedrock_model_name(name))

    def test_unqualified_alias_rejected(self) -> None:
        self.assertFalse(um.is_valid_bedrock_model_name("claude-sonnet-4-5-20250929-v1:0"))

    def test_vertex_style_version_pin_rejected(self) -> None:
        self.assertFalse(um.is_valid_bedrock_model_name("anthropic.claude-haiku-4-5@20251001"))

    def test_commitment_tier_rejected(self) -> None:
        self.assertFalse(
            um.is_valid_bedrock_model_name("us-east-1/1-month-commitment/anthropic.claude-v1")
        )


class TestDropBedrockInRegionDuplicates(unittest.TestCase):
    def test_bare_id_dropped_when_us_twin_present(self) -> None:
        models = {
            "anthropic.claude-opus-5": {"name": "anthropic.claude-opus-5"},
            "us.anthropic.claude-opus-5": {"name": "us.anthropic.claude-opus-5"},
        }
        um.drop_bedrock_in_region_duplicates(models)
        self.assertEqual(list(models), ["us.anthropic.claude-opus-5"])

    def test_bare_id_kept_when_it_is_the_only_form(self) -> None:
        # Z.AI and Qwen publish no cross-Region profile, so the bare id is
        # the only way to reach them.
        models = {
            "zai.glm-5": {"name": "zai.glm-5"},
            "qwen.qwen3-coder-next": {"name": "qwen.qwen3-coder-next"},
        }
        um.drop_bedrock_in_region_duplicates(models)
        self.assertEqual(sorted(models), ["qwen.qwen3-coder-next", "zai.glm-5"])

    def test_us_only_id_kept(self) -> None:
        models = {"us.moonshotai.kimi-k3": {"name": "us.moonshotai.kimi-k3"}}
        um.drop_bedrock_in_region_duplicates(models)
        self.assertEqual(list(models), ["us.moonshotai.kimi-k3"])


class TestBedrockCardCorrections(unittest.TestCase):
    def test_glm_flash_output_ceiling_restored(self) -> None:
        # LiteLLM reports 128K; the AWS card says 4K. Trusting the registry
        # here makes harnx ask for thirty times what the model will return.
        models = {"zai.glm-4.7-flash": {"name": "zai.glm-4.7-flash", "max_output_tokens": 128000}}
        um.apply_bedrock_card_corrections(models, [])
        self.assertEqual(models["zai.glm-4.7-flash"]["max_output_tokens"], 4096)
        self.assertEqual(models["zai.glm-4.7-flash"]["max_input_tokens"], 203000)

    def test_uncorrected_model_untouched(self) -> None:
        models = {"zai.glm-5": {"name": "zai.glm-5", "max_output_tokens": 128000}}
        um.apply_bedrock_card_corrections(models, [])
        self.assertEqual(models["zai.glm-5"]["max_output_tokens"], 128000)

    def test_correction_for_absent_model_warns(self) -> None:
        # A pin for a model that stopped arriving is dead config, and saying
        # nothing about it is the staleness these corrections exist to stop.
        warnings: list[str] = []
        models: dict[str, dict] = {}
        um.apply_bedrock_card_corrections(models, warnings)
        self.assertEqual(models, {})
        self.assertEqual(len(warnings), len(um.BEDROCK_CARD_CORRECTIONS))
        self.assertTrue(all("absent model" in w for w in warnings))


class TestCuratedCapabilityFlags(unittest.TestCase):
    def test_known_vision_survives_a_silent_refresh(self) -> None:
        # LiteLLM lists no `supports_vision` for Llama 4 on Bedrock, which is
        # silence rather than a denial; dropping the flag would start refusing
        # image attachments for a multimodal model.
        merged = um.merge_old_fields(
            {"name": "us.meta.llama4-scout-17b-instruct-v1:0"},
            {"name": "us.meta.llama4-scout-17b-instruct-v1:0", "supports_vision": True},
        )
        self.assertTrue(merged["supports_vision"])

    def test_refresh_may_still_turn_a_capability_on(self) -> None:
        merged = um.merge_old_fields(
            {"name": "m", "supports_vision": True},
            {"name": "m"},
        )
        self.assertTrue(merged["supports_vision"])

    def test_absent_in_both_stays_absent(self) -> None:
        merged = um.merge_old_fields({"name": "m"}, {"name": "m"})
        self.assertNotIn("supports_vision", merged)

    def test_stale_false_is_not_resurrected(self) -> None:
        merged = um.merge_old_fields({"name": "m"}, {"name": "m", "supports_vision": False})
        self.assertNotIn("supports_vision", merged)
