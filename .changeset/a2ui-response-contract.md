---
harnx: minor
---
Define the shared response contract for A2UI `interact` surfaces: how bound inputs and action context map into `InteractionResponse.data`, one durable acceptance check validating action and data shape against the requested surface (new `invalid_response` outcome), and frontend-agnostic plain-text render/answer-parse helpers for A2A, ACP and CLI fallbacks. The serve `session/interaction_response` RPC now forwards any action and data to that check instead of rejecting non-confirm replies up front.
