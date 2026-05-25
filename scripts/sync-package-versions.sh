#!/usr/bin/env bash
# Sync the `version:` field in each agent .md frontmatter to match the
# package version in its package.yaml. Run by knope's release workflow
# between `PrepareRelease` and the lockfile-refresh commit.
#
# Knope's Command step execs argv directly (no shell), so multi-line
# `for` loops in knope.toml fail with ENOENT — keep the loop here.
set -euo pipefail

for pkg in pantheon coding; do
  dir="packages/$pkg"
  [ -f "$dir/package.yaml" ] || continue
  ver=$(grep -m1 '^version:' "$dir/package.yaml" | sed 's/version: v//')
  [ -n "$ver" ] || continue
  find "$dir/agents" -maxdepth 1 -name '*.md' | while read -r f; do
    sed -i "s/^version: .*/version: '$ver'/" "$f"
  done
done
