---
harnx: patch
---

Coordinate A2A admission across replicas with create-only first-message reservations, scoped context ownership and fixed runtime tickets. Retries retain the same context/task/prompt IDs, and foreground recovery closes or stops abandoned admission without replay. Task writes now use owner-checked context CAS, with durable terminal projections before context reuse. Background supervision, remote cancellation and intermediate event streaming remain separate work; drain legacy active turns and exclude mixed writers when upgrading.
