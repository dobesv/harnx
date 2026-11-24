"""Tests that the catalog lists only models harnx's clients can call, and
that registry coverage decides which hand-kept models a refresh drops."""

from __future__ import annotations

import unittest

import update_models as um
from update_models_variant_tests import registry_entry


def with_mode(entry: dict, mode: str, **extra) -> dict:
    return {**entry, "mode": mode, **extra}


class TestClientSupport(unittest.TestCase):
    def test_entries_the_client_cannot_call_are_not_ingested(self) -> None:
        registry = {
            "vertex_ai/gemini-4-carbon": registry_entry("vertex_ai", {}),
            "vertex_ai/claude-opus-5": registry_entry("vertex_ai", {}),
            # Vertex's client dispatches chat by model family.
            "medlm-large": registry_entry("vertex_ai-language-models", {}),
            "vertex_ai/meta/llama-4-scout-17b-16e-instruct-maas": registry_entry("vertex_ai", {}),
            # Its embedding requests use the text-embedding schema.
            "text-embedding-005": with_mode(
                registry_entry("vertex_ai-embedding-models", {}), "embedding"
            ),
            "multimodalembedding": with_mode(
                registry_entry("vertex_ai-embedding-models", {}), "embedding"
            ),
            # Bedrock's client has no rerank operation; Cohere's has one.
            "cohere.rerank-v3-5:0": with_mode(registry_entry("bedrock", {}), "rerank"),
            "rerank-v3.5": with_mode(registry_entry("cohere", {}), "rerank"),
        }
        fetched, _ = um.collect_provider_models(registry, {})
        self.assertEqual(
            set(fetched["vertexai"]),
            {"gemini-4-carbon", "claude-opus-5", "text-embedding-005"},
        )
        self.assertNotIn("bedrock", fetched)
        self.assertEqual(fetched["cohere"]["rerank-v3.5"]["type"], "reranker")
        # The registry still lists those kinds, so a refresh drops old entries
        # of them instead of keeping them by hand.
        kinds = um.registry_kinds(registry)
        self.assertEqual(kinds["bedrock"], {"reranker"})
        self.assertEqual(kinds["vertexai"], {None, "embedding"})

    def test_a_kind_listed_only_by_deprecated_entries_is_still_covered(self) -> None:
        registry = {
            "command-a-03-2025": registry_entry("cohere_chat", {}),
            "rerank-english-v2.0": with_mode(
                registry_entry("cohere", {}), "rerank", deprecation_date="2020-01-01"
            ),
        }
        old = {
            "command-a-03-2025": {"name": "command-a-03-2025"},
            "rerank-english-v2.0": {"name": "rerank-english-v2.0", "type": "reranker"},
        }
        fetched, _ = um.collect_provider_models(registry, {"cohere": old})
        kinds = um.registry_kinds(registry)
        self.assertEqual(kinds["cohere"], {None, "reranker"})
        refreshed = um.regenerate_provider_models("cohere", old, fetched["cohere"], kinds["cohere"])
        self.assertEqual({model["name"] for model in refreshed}, {"command-a-03-2025"})

    def test_a_provider_whose_entries_are_all_deprecated_is_still_covered(self) -> None:
        registry = {
            "jamba-large": registry_entry("ai21", {"deprecation_date": "2020-01-01"}),
        }
        old = {
            "jamba-large": {"name": "jamba-large"},
            "jamba-embed": {"name": "jamba-embed", "type": "embedding"},
        }
        fetched, _ = um.collect_provider_models(registry, {"ai21": old})
        self.assertNotIn("ai21", fetched)
        refreshed = um.regenerate_provider_models(
            "ai21", old, fetched.get("ai21"), um.registry_kinds(registry)["ai21"]
        )
        # The chat model is dropped; the embedding kind the registry doesn't
        # list stays hand-maintained.
        self.assertEqual([model["name"] for model in refreshed], ["jamba-embed"])

    def test_a_provider_the_registry_doesnt_list_is_kept_as_it_is(self) -> None:
        old = {"jamba-large": {"name": "jamba-large"}}
        kinds = um.registry_kinds({})
        refreshed = um.regenerate_provider_models("ai21", old, None, kinds.get("ai21"))
        self.assertEqual(refreshed, [old["jamba-large"]])
