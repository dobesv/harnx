---
harnx: minor
---
Let a client name the model catalog it inherits, instead of deriving it from
the filename.

Client configs gain `model_catalog:`, naming a provider block in the shared
`models.yaml`:

```yaml
type: openai-compatible
model_catalog: bedrock
api_base: https://bedrock-runtime.us-east-1.amazonaws.com/openai/v1
```

Until now the filename decided, and for `openai-compatible` clients it did so
by prefix. That made the file's name load-bearing in ways that were easy to
trip over: `deepseek-proxy.yaml` inherited the DeepSeek catalog whether or not
that was intended, and a sensibly-named `aws-prod.yaml` inherited nothing at
all, leaving its models with no context limits, prices or capabilities. The
field also works for native client types, so a `claude` client can borrow a
different block without being renamed.

The filename fallback is unchanged and still applies when `model_catalog` is
absent, so existing configurations keep working. Naming a catalog that does
not exist logs a warning and inherits nothing rather than quietly falling back
to the filename, so a typo surfaces instead of substituting models nobody
asked for; explicitly listed `models:` entries still apply.

The shipped package clients, the example configs and the provider and package
docs now name their catalog.
