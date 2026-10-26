"""OpenAI request settings a refresh derives from the LiteLLM registry.

The registry's per-model capability fields decide which OpenAI models use the
Responses API and which of those ask it for reasoning summaries.
"""

from __future__ import annotations

import unittest

import update_models as um


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


class TestOpenAIReasoningSummaryDefault(unittest.TestCase):
    """Responses models the registry marks as reasoning ask for summaries."""

    _ON_RESPONSES = {"supported_endpoints": ["/v1/chat/completions", "/v1/responses"]}
    _REASONING = {**_ON_RESPONSES, "supports_reasoning": True}

    def _refresh(self, model: dict, payload: dict) -> dict:
        # main() applies the registry defaults in this order.
        um.apply_openai_endpoint_default(model, "openai", payload)
        um.apply_openai_reasoning_summary(model, "openai", payload)
        return model

    def test_reasoning_model_requests_summaries(self) -> None:
        model = self._refresh({"name": "gpt-6.1-terra"}, self._REASONING)
        self.assertEqual(model["patches"], ['.body.reasoning = {"summary":"auto"}'])

    def test_model_without_the_flag_gets_no_patch(self) -> None:
        # The registry leaves the flag out for gpt-4.1 and sets it false for
        # gpt-audio; neither counts as reasoning.
        for payload in (self._ON_RESPONSES, {**self._ON_RESPONSES, "supports_reasoning": False}):
            with self.subTest(payload=payload):
                model = self._refresh({"name": "gpt-4o"}, payload)
                self.assertEqual(model["endpoint"], "responses")
                self.assertNotIn("patches", model)

    def test_chat_completions_model_gets_no_patch(self) -> None:
        # The patch writes the Responses API's reasoning object.
        model = self._refresh({"name": "gpt-5.5-cyber-preview"}, {"supports_reasoning": True})
        self.assertNotIn("patches", model)

    def test_curated_patch_wins(self) -> None:
        curated = ['.body.reasoning = {"effort":"low","summary":"concise"}']
        model = self._refresh({"name": "gpt-5.5", "patches": curated}, self._REASONING)
        self.assertEqual(model["patches"], curated)

    def test_non_openai_provider_unaffected(self) -> None:
        model = {"name": "gpt-6.1-terra", "endpoint": "responses"}
        um.apply_openai_reasoning_summary(model, "openrouter", self._REASONING)
        self.assertNotIn("patches", model)
