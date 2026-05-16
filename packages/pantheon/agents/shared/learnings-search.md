## Searching Past Solutions for Learnings

Before planning or when investigating a problem, search the `docs/solutions/` directory for past solutions that might be relevant to your current task. This helps you avoid repeating work and leverage institutional knowledge.

### When to Search

- **Before planning**: Extract key technical terms from the task and search for related solutions
- **When investigating a problem**: Search for error patterns, component names, or similar issues
- **When uncertain about approach**: Look for precedent in how similar problems were solved

### Search Strategy

Search the `docs/solutions/` directory efficiently using whatever search tools are available:

1. **Extract keywords from the current task**
   - Identify module names, error patterns, component names, and technical terms
   - Example: "Add authentication to API" → keywords: `auth`, `api`, `authentication`, `jwt`

2. **Search YAML frontmatter first** (efficient pre-filtering)
   - Search by tags: `rg -l 'tags:.*<keyword>' docs/solutions/`
   - Search by component: `rg -l 'component:.*<keyword>' docs/solutions/`
   - Search by problem type: `rg -l 'problem_type:.*<type>' docs/solutions/`
   - Example: `rg -l 'tags:.*kubernetes' docs/solutions/` finds all solutions tagged with kubernetes

3. **Refine based on result count**
   - If >10 candidates: narrow with more specific patterns or combine multiple keywords
   - If 3-10 candidates: proceed to relevance scoring
   - If <3 results: broaden to full-text content search with `rg '<keyword>' docs/solutions/`

4. **Score relevance efficiently**
   - Read only the first 30 lines of each candidate (frontmatter + Problem section)
   - Look for direct relevance to your current task
   - Skip solutions that don't match your specific context

### Output Format

When relevant past solutions are found, present them as a "Prior Art" section in your response:

```
## Prior Art (from docs/solutions/)
- [filename.md] — Brief summary of how it relates to this task
- [filename.md] — Brief summary of the solution approach
```

Include the filename and a one-line summary of relevance. This helps the team understand what precedent exists.

### Graceful Fallback

If `docs/solutions/` doesn't exist or is empty, note "No past solutions found" and continue. This is **not** a blocking failure — proceed with your analysis and planning. The directory may not be populated yet, or your search terms may not match existing solutions.

### Example Search Flow

Task: "Fix intermittent database connection timeouts in the API"

1. Keywords: `database`, `timeout`, `connection`, `api`
2. Search: `rg -l 'tags:.*database' docs/solutions/` → 5 results
3. Refine: `rg -l 'tags:.*timeout' docs/solutions/` → 2 results
4. Read first 30 lines of each → 1 is highly relevant
5. Output: "## Prior Art: [connection-pooling-fix.md] — Addresses similar timeout issues by tuning pool size"
