---
title: "TUI custom .tmTheme regression — plumb resolved theme through render path"
date: 2026-06-13
category: logic-errors
problem_type: logic_error
component: "harnx-tui"
root_cause: "hardcoded theme constant in newly-added code path diverged from shared resolver"
resolution_type: code_fix
severity: medium
tags:
  - tui
  - syntect
  - theme
  - config
  - regression
  - testing-pattern
plan_ref: "tui-custom-theme-fix"
---

## Problem

The ratatui TUI hardcoded syntax-highlight theme to builtin `monokai-extended`, ignoring user-configured `dark.tmTheme`/`light.tmTheme` files that `Config::render_options()` already resolved for the non-TUI streaming path. Two render paths sharing a resolver (`render_options()`) but one hardcoding a value caused inconsistent behavior.

## Symptoms

- Custom `.tmTheme` files placed in config directory were ignored in TUI mode
- Light mode (`theme: light` or auto-detected) still applied dark theme colors in code blocks
- Non-TUI streaming path correctly honored custom themes, creating user-visible inconsistency
- No diagnostic mechanism existed to verify which theme was active (tmux/headless tests cannot observe colors)

## Investigation Steps

1. Traced the divergence in `crates/harnx-tui/src/markdown_render.rs:20`:
   - `const THEME_BYTES` hardcoded `monokai-extended` theme asset
   - `static THEME: LazyLock<Option<Theme>>` loaded once at first use
   - `render_markdown()` used static `THEME` with no theme parameter

2. Verified the working non-TUI path:
   - `main.rs:215` calls `render_options()` on non-TUI branch
   - `cli_event_sink.rs:130` passes `RenderOptions { theme, ... }` to `MarkdownRender::init()`

3. Identified why `harnx-render` needed to stay untouched:
   - `get_code_color()` is private to `harnx-render`
   - Pattern needed reimplementing inline in `harnx-runtime`

4. Tested theme extraction with syntect:
   - `Theme.name` is `Option<String>` — may be `None` for custom themes missing `<key>name</key>`
   - `Theme.settings.foreground`/`.background` are optional
   - Scope iteration needed substring matching — syntect may split string literals into multiple spans

5. Discovered workspace syntect lacks `ThemeSet::load_defaults()`:
   - Build config disables default theme-loading feature
   - Tests decode builtin `.theme.bin` assets from `harnx-render/assets/` instead

## Root Cause

During the ratatui rewrite, the TUI markdown renderer hardcoded the theme constant while the non-TUI streaming path correctly used `Config::render_options()`. The resolver existed but was not called from the TUI initialization path.

Additionally, the `append_code_block_text` helper was already at argument limit. Adding `theme: Option<&Theme>` triggered clippy `too_many_arguments`, requiring a narrow `#[allow]` annotation.

## Solution

**1. Thread resolved theme through render path:**

```rust
// crates/harnx-tui/src/markdown_render.rs
pub fn render_markdown(
    text: &str,
    base_style: Style,
    width: u16,
    theme: Option<&Theme>,  // NEW: resolved theme parameter
) -> RenderedEntry { ... }

#[allow(clippy::too_many_arguments)]  // Added after theme param
fn append_code_block_text(
    // ... existing params
    theme: Option<&Theme>,
) { ... }
```

- Removed `const THEME_BYTES` and `static THEME` entirely
- `None` preserves prior no-highlight fallback: dim `Color::DarkGray` via `append_text`
- Production call sites pass resolved theme; tests pass `None`

**2. Resolve theme once at TUI init:**

```rust
// crates/harnx-tui/src/lifecycle.rs:90
impl Tui {
    pub fn init(config: GlobalConfig, ...) -> Result<Self> {
        let code_theme = config.read().render_options()?.theme;
        Ok(Self {
            config,
            code_theme,  // Owned theme stored on Tui struct
            // ...
        })
    }
}
```

```rust
// crates/harnx-tui/src/types.rs:24
pub struct Tui {
    pub(super) config: GlobalConfig,
    pub(super) code_theme: Option<Theme>,  // NEW: resolved at init
    // ...
}
```

- Theme resolved ONCE at startup via `config.read().render_options()?.theme`
- Owned `Theme` stored on `Tui.code_theme` — theme fixed per session
- Deliberately NO cache-key change, NO theme-generation counter, NO runtime theme-switch command

**3. Thread into all production render call sites:**

```rust
// crates/harnx-tui/src/render.rs
fn render_entry(&self, entry: &TranscriptItem, ..., theme: Option<&Theme>) -> RenderedEntry {
    match entry {
        TranscriptItem::AssistantText { text, .. } => {
            crate::markdown_render::render_markdown(text, Style::default(), width, theme)
        }
        // ... other arms
    }
}

// In Tui::render:
let entry = Self::render_entry(
    entry,
    use_utc,
    width,
    Some(i) == streaming_idx,
    self.code_theme.as_ref(),  // Pass resolved theme
);
```

**4. Add `.info theme` diagnostic command:**

```rust
// crates/harnx-runtime/src/commands.rs
pub static COMMANDS: LazyLock<[Command; 47]> = LazyLock::new(|| {
    [
        // ...
        Command::new(".info theme", "Show active syntax-highlight theme"),
        // ...
    ]
});

// Handler at line 224+
Some("theme") => {
    let config = config.read();
    let mode = if config.light_theme() { "light" } else { "dark" };
    writeln!(output, "mode: {mode}")?;

    let render_options = config.render_options()?;
    if let Some(theme) = render_options.theme.as_ref() {
        let theme_path = Config::local_path(&format!("{mode}.tmTheme"));
        let fallback_name = if theme_path.exists() {
            "(custom theme)"
        } else if config.light_theme() {
            "(builtin monokai-extended-light)"
        } else {
            "(builtin monokai-extended)"
        };
        let theme_name = theme.name.as_deref().unwrap_or(fallback_name);
        writeln!(output, "theme: {theme_name}")?;
        if theme_path.exists() {
            writeln!(output, "source: {}", theme_path.display())?;
        } else {
            writeln!(output, "source: builtin")?;
        }
        writeln!(output, "foreground: {}", color_to_hex(theme.settings.foreground.as_ref()))?;
        writeln!(output, "background: {}", color_to_hex(theme.settings.background.as_ref()))?;
        writeln!(output, "string: {}", scope_color(theme, "string"))?;
        writeln!(output, "keyword: {}", scope_color(theme, "keyword"))?;
        writeln!(output, "comment: {}", scope_color(theme, "comment"))?;
    } else {
        writeln!(output, "highlighting: disabled")?;
    }
}

fn scope_color(theme: &Theme, scope_name: &str) -> String {
    theme.scopes.iter()
        .find(|item| {
            item.scope.selectors.iter().any(|selector| {
                selector.path.scopes.iter().any(|scope| scope.to_string() == scope_name)
            })
        })
        .and_then(|item| item.style.foreground.as_ref())
        .map_or_else(|| "default".to_string(), |color| color_to_hex(Some(color)))
}
```

- Outputs: `mode`, `theme` (name or fallback), `source` (path or builtin), foreground/background hex, scope colors
- Testability mechanism: oracle is mode/name/source (stable) — sampled colors can coincide between themes
- Reuses `get_code_color()` selector-iteration pattern from `harnx-render/src/markdown.rs:527` (reimplemented inline)

**5. Tests use decoded theme assets:**

```rust
// Tests decode builtin themes from assets (ThemeSet::load_defaults unavailable)
fn load_builtin_theme(name: &str) -> Theme {
    let bytes = include_bytes!(concat!("../../harnx-render/assets/", name, ".theme.bin"));
    syntect::dumps::from_binary(bytes)
}

#[test]
fn render_markdown_uses_provided_theme() {
    let theme = load_builtin_theme("monokai-extended");
    let result = render_markdown("```rust\nfn hi() {}\n```", Style::default(), 80, Some(&theme));
    // Avoid matching full quoted token — syntect may split quoted strings into multiple spans
    assert!(result.blocks.iter().any(|b| matches!(b, MarkdownBlockData::Paragraph { .. })));
}
```

## Why This Works

**Plumbing pattern**: When two render paths share a resolver, plumb the resolved value through rather than duplicating resolution. `None` semantics preserved across both paths maintains parity.

**Single resolution point**: Theme resolved once at `Tui::init` matches config/env/auto-detect behavior. No runtime switching matches existing architecture — theme is a session-level fixed property.

**Diagnostic for headless testing**: Plain-text `.info theme` command provides stable oracle (mode/name/source) rather than sampled colors, which could coincide between themes. Enables verification in tmux/non-color test contexts.

**Minimal scope**: No new abstractions (`ThemeManager`), no cache-key changes, no config over-validation. Exactly the plumbing needed to fix the regression.

## Prevention Strategies

**For similar shared-resolver divergences:**

- When adding a new code path that uses a resolver, check all existing call sites of that resolver
- Thread `Option<&T>` and preserve `None` semantics for parity
- Prefer plumbing resolved values over duplicating resolution logic

**For testing color/visual features in headless environments:**

- Add plain-text diagnostic commands whose oracle is mode/name/source (stable), not sampled visual attributes
- Test config changes with `EnvGuard` pattern for `HARNX_CONFIG_DIR`
- Write custom `.tmTheme` files to temp dirs for custom-theme tests

**Specific syntect gotchas:**

- `Theme.name` may be `None` for valid `.tmTheme` files missing `<key>name</key>` — provide fallback labels
- Match substring scope selectors, not full tokens — syntect splits quoted strings into multiple spans
- Workspace syntect may lack `ThemeSet::load_defaults()` — decode builtin `.theme.bin` assets in tests instead

**Code review checklist:**

- [ ] Do both render paths call the same resolver?
- [ ] Is `Option<&T>` threaded with preserved `None` semantics?
- [ ] Is there a diagnostic command for headless verification?
- [ ] Does theme-/color-related code handle `name: None` gracefully?
- [ ] Are scope selectors matched by substring, not full token?
- [ ] If adding param to already-wide helper, is `#[allow(clippy::too_many_arguments)]` scoped narrowly?

## Related Issues

- **Issue:** [#542](https://github.com/dobesv/harnx/issues/542) — TUI ignoring custom .tmTheme files
- **Related Solution:** [performance-issues/tui-markdown-widget-architecture-2026-05-06.md](../performance-issues/tui-markdown-widget-architecture-2026-05-06.md) — MarkdownBlockData caching architecture
- **Related Solution:** [logic-errors/tui-session-breakdown-on-exit-2026-05-18.md](../logic-errors/tui-session-breakdown-on-exit-2026-05-18.md) — MarkdownRender without theme returns raw markdown (same root cause: `RenderOptions::default()` vs config-derived)
