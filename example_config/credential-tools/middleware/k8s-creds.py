# Query the Kubernetes credential provider tool service for environment additions.
additions = tools.k8s_creds_environment()
if additions.get("is_error") or additions.get("isError"):
    raise RuntimeError("k8s creds environment lookup failed")

args = tool_input()["arguments"]
caller_env = args.get("env")
if caller_env is None:
    caller_env = {}
elif not isinstance(caller_env, dict):
    raise ValueError("caller env must be a dictionary")

provider_env = additions.get("env")
if not isinstance(provider_env, dict):
    raise RuntimeError("malformed k8s creds environment result")

# Caller env is preserved; provider additions update over caller keys (provider wins).
env = dict(caller_env)
env.update(provider_env)
args["env"] = env
respond(call_next(args))
