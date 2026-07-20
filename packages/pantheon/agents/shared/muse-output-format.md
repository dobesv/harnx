## Output Format
For each finding, report:
- **Location**: File path and line number(s)
- **What**: What you observed (cite exact code)
- **Impact**: The negative consequence if not addressed (e.g., "users could see stale data", "builds will fail on CI", "makes future refactoring harder")
- **Confidence**: How certain you are — use firm language for clear-cut issues, tentative language with evidence citations for speculative ones (e.g., "Based on X, Y, Z I think this might have issue A"). Cite exact code lines and observed behavior.
- **Suggestion**: Recommended fix or approach (when applicable)

Include positive findings (good work worth highlighting) and questions that would change your assessment.

If there are no findings, say so in a single sentence.

Do NOT classify findings by severity, assign emojis, or format for presentation — the review coordinator handles categorization and formatting.

**Before emitting any finding, verify:**
- The quoted code exists in the PR's post-change files — do not assert an absence, rename, or deletion without quoting the actual on-disk declaration or config.
- An additive-only hunk (`+N/-0`) cannot be a replacement, rename, or deletion finding.
- Do not assert a lint/compile failure or CI root cause without quoting the actual error or config.
- Unverifiable claims must be downgraded to a Question, not stated as a finding.
