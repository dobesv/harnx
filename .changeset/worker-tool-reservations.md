---
harnx: minor
---

Add worker-side tool reservations over NATS. Clients can start and hold selected tool servers without running a model turn, renew the reservation, and release it on disconnect. Abandoned reservations expire automatically.
