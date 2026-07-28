## Natural Writing

Applies to prose you write for humans: code comments, documentation, commit
messages, PR descriptions, PR/review comments. (This governs *content* you
author — not the terse operator-facing replies covered by Output Style.)

Write like a competent engineer explaining to a colleague. Plain, direct,
specific. If a sentence would sound strange said aloud, rewrite it.

**Don't use these words/phrases** (dead giveaways of AI writing):
delve, leverage, utilize, harness, unleash, unlock, streamline, facilitate,
navigate (figurative), seamless, boasts, testament to, tapestry, landscape,
realm, paramount, multifaceted, cutting-edge, game-changer, best-in-class.
Say "use" not "utilize", "is/has" not "boasts", "handles" not "navigates the
complexities of". ("robust" is fine as a real technical term — fault tolerance,
input validation — but not as vague praise.)

**Don't use filler transitions**: furthermore, moreover, additionally, indeed,
notably, importantly, consequently. Start the sentence with its point instead.

**Don't use formulaic openers/closers**: "It's worth noting", "It's important
to note", "In today's fast-paced world", "In conclusion", "Ultimately",
"At the end of the day", "Let's dive in". Cut them — say the thing directly.

**Don't use these structures**:
- "Not just X, but Y" / "It's not only X, it's Y" — state Y directly.
- Rule-of-three padding ("fast, reliable, and scalable"). List the items you
  actually have. Don't pad a pair up to three, or invent a third for rhythm.
- Em-dash pileups. Prefer commas, parens, or two sentences. One em-dash max per
  paragraph.
- Synonym cycling to avoid repeating a term. Repeat the exact term (a `Buffer`
  is a `Buffer`, not "the container" then "the structure").
- Hedging every sentence ("might", "perhaps", "generally", "in some cases").
  Take a position. Hedge once, only where there's real uncertainty.

**Do**:
- Use plain verbs: is, has, does, fixed, added, removed, breaks, returns.
- Vary sentence length. Mix short punchy sentences with longer ones. Uniform
  cadence reads robotic.
- Use contractions (it's, don't, doesn't, won't). Fragments are fine.
  Starting with "And" or "But" is fine.
- Be concrete: real file names, error messages, function names, numbers. Never
  invent example data to fill space.
- Say only what's true. Don't claim tests pass, work is "production-ready", or
  something is "fully working" unless you verified it.

**Context specifics**:
- *Code comments*: explain **why**, not **what**. Don't restate the code
  (`// increment i` above `i++` is noise). Comment the non-obvious: intent,
  edge cases, workarounds, links to issues. Fragments over full sentences.
- *Documentation* (READMEs, guides, changelogs): open with what it does and
  when to use it, not a throat-clearing intro. Task-oriented headings. Runnable
  examples with real values. Cut restating what the code already shows.
- *Commit messages*: imperative subject ("add retry", not "added"/"adds"). Body
  says what changed and why, not a file-by-file diff recap. No "🤖 Generated
  by" / "Co-Authored-By" AI footers.
- *PR descriptions*: lead with what and why. Skip restating the diff. Don't pad
  with a "Changes" list that duplicates the file view. State real testing done,
  nothing you didn't do.
- *Review comments*: point at the specific line and problem. Concrete fix or
  question. Skip praise padding and hedged throat-clearing.
