# Files are provisioned by the long-lived proxy on the bash host, not in this VFS.
additions = call_tool("proxy_auth_environment", {})
if additions.get("is_error") or additions.get("isError"):
    raise RuntimeError("proxy auth environment lookup failed")

args = tool_input()["arguments"]
caller_env = args.get("env")
if not isinstance(caller_env, dict):
    caller_env = {}
env = additions["env"]
# Existing caller keys win, including explicit proxy/CA overrides.
env.update(caller_env)
args["env"] = env
respond(call_next(args))
