---
harnx: patch
---
Fix Gemini requests failing with a 400 "Role 'function' is not supported" error. Tool-result turns are now sent with the `user` role, which is the only valid container for `functionResponse` parts (Gemini accepts only `user`/`model` roles). Newer Gemini endpoints reject the previously-tolerated `function` role.
