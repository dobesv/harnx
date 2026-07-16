---
"harnx": patch
---

fix(tui): only convert large pastes into attachments

Previously any multi-line paste was turned into a text attachment, which was
annoying for small pastes of just a few lines. A paste now becomes an
attachment only when it is large — more than 8 lines or more than 512
characters. Smaller multi-line pastes are inserted inline into the input.

- Line counting uses `str::lines()` so a single trailing newline does not
  inflate the count.
- Character counting uses `chars().count()`, so the limit is measured in
  characters rather than bytes (multibyte text is counted correctly).
