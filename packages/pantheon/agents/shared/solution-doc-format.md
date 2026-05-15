## Solution Document Format

Solution documents capture resolved problems, their root causes, and the solutions applied. They serve as a knowledge base for the Mnemosyne compounding agent and future developers encountering similar issues.

### YAML Frontmatter Schema

Every solution document must begin with YAML frontmatter that provides structured metadata for categorization, search, and filtering:

```yaml
---
title: "<descriptive title of the solution>"
date: YYYY-MM-DD
category: "<category-directory-name>"
problem_type: <enum>
component: "<affected component or module>"
root_cause: "<brief root cause classification>"
resolution_type: <enum>
severity: <enum>
tags:
  - tag1
  - tag2
plan_ref: "<plan name, if applicable>"
---
```

**Field Definitions:**

- **title**: A clear, descriptive title that summarizes the problem and solution (e.g., "Fixed race condition in cache invalidation during concurrent updates")
- **date**: ISO 8601 date when the solution was documented (YYYY-MM-DD)
- **category**: The directory name where this document is stored, determined by `problem_type` (see mapping below)
- **problem_type**: Enum classifying the type of problem. Must be one of:
  - `build_error` — Compilation, build system, or dependency resolution failures
  - `test_failure` — Test suite failures, flaky tests, or test infrastructure issues
  - `runtime_error` — Crashes, exceptions, or runtime failures in production or development
  - `performance_issue` — Slow queries, memory leaks, CPU spikes, or latency problems
  - `database_issue` — Database connection, migration, or data consistency problems
  - `security_issue` — Vulnerabilities, authentication, authorization, or data protection issues
  - `integration_issue` — Third-party API, service integration, or cross-system communication failures
  - `logic_error` — Incorrect business logic, algorithmic bugs, or state management issues
  - `workflow_issue` — CI/CD pipeline, deployment, or operational workflow problems
- **component**: The affected component, module, service, or subsystem (e.g., "cache-layer", "auth-service", "prometheus-scraper")
- **root_cause**: A brief classification of the underlying cause (e.g., "missing synchronization", "incorrect configuration", "API contract mismatch")
- **resolution_type**: Enum classifying how the problem was resolved. Must be one of:
  - `code_fix` — Code changes to fix logic or implementation
  - `migration` — Data migration or schema changes
  - `config_change` — Configuration or environment variable adjustments
  - `test_fix` — Test suite fixes or additions
  - `dependency_update` — Dependency version updates or replacements
  - `workflow_improvement` — Process or workflow improvements
- **severity**: Enum indicating the impact of the problem. Must be one of:
  - `critical` — Blocks production, causes data loss, or affects all users
  - `high` — Significant impact on functionality or performance
  - `medium` — Moderate impact, affects some users or features
  - `low` — Minor impact, cosmetic or edge-case issues
- **tags**: Array of searchable tags for cross-referencing (e.g., `["concurrency", "caching", "race-condition"]`)
- **plan_ref**: Optional reference to a plan name if this solution was part of a larger initiative

### Category Directory Mapping

The `category` field must match the directory where the document is stored. Use this mapping:

| problem_type | category directory |
|---|---|
| build_error | build-errors/ |
| test_failure | test-failures/ |
| runtime_error | runtime-errors/ |
| performance_issue | performance-issues/ |
| database_issue | database-issues/ |
| security_issue | security-issues/ |
| integration_issue | integration-issues/ |
| logic_error | logic-errors/ |
| workflow_issue | workflow-issues/ |

### Document Sections

After the YAML frontmatter, structure the solution document with these sections in order:

#### 1. Problem

A concise 1-2 sentence description of the issue. This should be understandable to someone unfamiliar with the codebase.

Example:
> The cache invalidation mechanism was not thread-safe, causing stale data to be served to concurrent requests when multiple threads updated the same cache entry simultaneously.

#### 2. Symptoms

Observable symptoms, error messages, or behaviors that indicated the problem. Include:
- Error messages or stack traces (if applicable)
- Observed behavior (e.g., "requests timeout after 30 seconds")
- When the issue occurs (e.g., "only under high load", "intermittently on Mondays")
- Impact on users or systems

Example:
```
- Error: `java.util.ConcurrentModificationException` in cache update loop
- Behavior: Stale user profiles served to 5-10% of requests during peak traffic
- Frequency: Intermittent, reproducible under load testing with 100+ concurrent users
```

#### 3. Investigation Steps

A narrative of what was tried and what was discovered. This helps future developers understand the debugging process and avoid dead ends.

Example:
> Started by reviewing cache hit/miss ratios in Prometheus, which showed a 15% miss rate during peak hours. Enabled debug logging in the cache layer and found that invalidation events were being lost. Traced the issue to the `CacheManager.invalidate()` method, which was iterating over a HashMap without synchronization. Reproduced the issue in a unit test with 50 concurrent threads updating the same cache key.

#### 4. Root Cause

A technical explanation of why the issue occurred. This should be detailed enough for someone to understand the underlying mechanism.

Example:
> The `CacheManager` class used an unsynchronized `HashMap` to store cache entries. When multiple threads called `invalidate()` simultaneously, the HashMap's internal state became corrupted due to concurrent modification. The iteration in the invalidation loop would skip entries or throw `ConcurrentModificationException`, leaving stale data in the cache.

#### 5. Solution

The actual fix with code examples. Use before/after comparisons when applicable. Include:
- Code changes (snippets or diffs)
- Configuration changes (if applicable)
- Migration steps (if applicable)
- Deployment considerations

Example:
```
Changed the HashMap to a ConcurrentHashMap and wrapped the invalidation loop with proper synchronization:

**Before:**
```java
private HashMap<String, CacheEntry> cache = new HashMap<>();

public void invalidate(String key) {
  for (String k : cache.keySet()) {
    if (k.startsWith(key)) {
      cache.remove(k);
    }
  }
}
```

**After:**
```java
private ConcurrentHashMap<String, CacheEntry> cache = new ConcurrentHashMap<>();

public void invalidate(String key) {
  cache.entrySet().removeIf(entry -> entry.getKey().startsWith(key));
}
```

#### 6. Why This Works

An explanation of why the solution addresses the root cause. This helps developers understand the fix and apply similar patterns elsewhere.

Example:
> `ConcurrentHashMap` is designed for thread-safe concurrent access without requiring external synchronization. The `removeIf()` method atomically removes entries that match the predicate, preventing the concurrent modification issues that occurred with the manual iteration. This ensures that invalidation events are never lost, even under high concurrency.

#### 7. Prevention Strategies

How to avoid recurrence, including:
- Test cases that would catch this issue
- Best practices or patterns to follow
- Code review checklist items
- Monitoring or alerting strategies

Example:
> **Test Cases:**
> - Add a stress test with 100+ concurrent threads updating and invalidating the same cache key
> - Verify that no entries remain after invalidation
> - Assert that no `ConcurrentModificationException` is thrown
>
> **Best Practices:**
> - Always use thread-safe collections (ConcurrentHashMap, CopyOnWriteArrayList) when shared across threads
> - Avoid manual iteration over collections that may be modified concurrently
> - Use `removeIf()` or `forEach()` for safe concurrent modification
>
> **Code Review Checklist:**
> - [ ] Are shared collections thread-safe?
> - [ ] Is iteration safe under concurrent modification?
> - [ ] Are there unit tests for concurrent access patterns?

#### 8. Related Issues

Links to related issue tracker tickets, pull requests, or other solution documents. This helps connect related problems and solutions.

Example:
> - **Issue:** [PROJ-1234](https://jira.example.com/browse/PROJ-1234) — Cache invalidation performance improvements
> - **Issue:** [#456](https://github.com/example/repo/issues/456) — Cache invalidation tracking
> - **PR:** [#5678](https://github.com/example/repo/pull/5678) — Implement ConcurrentHashMap migration
> - **Related Solution:** [logic-errors/concurrent-map-iteration.md](../logic-errors/concurrent-map-iteration.md)

### Writing Guidelines

- **Be specific:** Use concrete examples, error messages, and code snippets
- **Be complete:** Include enough detail for someone to understand and apply the solution
- **Be clear:** Write for developers unfamiliar with the specific issue
- **Be actionable:** Provide steps, code, and patterns that can be directly applied
- **Be honest:** Acknowledge limitations, trade-offs, or incomplete solutions
- **Be searchable:** Use descriptive titles and tags that match how developers would search for this issue

### File Naming

Solution documents should be named descriptively and placed in the appropriate category directory:

- Format: `<kebab-case-description>.md`
- Example: `cache-invalidation-race-condition.md` in `logic-errors/`
- Avoid generic names like `fix.md` or `solution.md`
