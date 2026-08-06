---
harnx: minor
---
`harnx worker --cluster __local__ --diagnose` starts this configuration's tool servers, reports which ones registered and how many tools each advertises, and exits without serving sessions. It applies the same selection and startup the worker uses, so it shows what a real run would do — including servers pulled in from packages the active agent cannot use — without racing a front-end that exits after its turn.
