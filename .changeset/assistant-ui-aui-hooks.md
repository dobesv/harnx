---
harnx: patch
---
Fix the web build against `@assistant-ui/react` 0.15, which removed the `useThread`, `useMessage`, `useComposerRuntime` and `useThreadRuntime` hooks.

State reads move to `useAuiState`, which takes a selector over one combined state object, so `useThread(s => s.isRunning)` becomes `useAuiState(s => s.thread.isRunning)` and `useMessage(s => s.role)` becomes `useAuiState(s => s.message.role)`. The two runtime handles come off `useAui()` instead, as `aui.composer` and `aui.thread`, and keep the same methods.
