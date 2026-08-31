#!/usr/bin/env bash

set -euo pipefail

poll_seconds="${HARNX_WAIT_PR_POLL_SECONDS:-60}"
stall_seconds="${HARNX_WAIT_PR_STALL_SECONDS:-7200}"
settle_seconds="${HARNX_WAIT_PR_SETTLE_SECONDS:-300}"
max_errors="${HARNX_WAIT_PR_MAX_ERRORS:-3}"

pr_url="${PR_URL:-}"
repo="${REPO:-}"
branch="${BRANCH:-}"
head_owner="${HEAD_OWNER:-}"

started_at=$SECONDS
consecutive_errors=0
previous_status=""
previous_activity=""
status_stable_since=0
candidate_reason=""
candidate_activity=""
candidate_since=0
candidate_observations=0

retry_or_fail() {
  local operation="$1"
  local status="$2"
  local details="$3"
  local first_line="${details%%$'\n'*}"

  consecutive_errors=$((consecutive_errors + 1))
  printf 'GitHub query failed (%s, attempt %s/%s, exit %s): %.240s\n' \
    "$operation" "$consecutive_errors" "$max_errors" "$status" "$first_line" >&2
  if ((consecutive_errors >= max_errors)); then
    printf 'Stopping after %s consecutive GitHub query failures.\n' "$consecutive_errors" >&2
    exit 1
  fi
}

sleep_until_next_poll() {
  sleep "$poll_seconds"
}

count_check_bucket() {
  local bucket="$1"
  awk -F '\t' -v expected="$bucket" '$1 == expected { count += 1 } END { print count + 0 }' \
    <<<"$check_rows"
}

print_result() {
  local reason="$1"
  local state="$2"
  local elapsed=$((SECONDS - started_at))

  printf 'PR_STABLE\n'
  printf 'url=%s\n' "$pr_url"
  printf 'reason=%s\n' "$reason"
  printf 'state=%s\n' "$state"
  printf 'head=%s\n' "$head_oid"
  printf 'checks_total=%s\n' "$checks_total"
  printf 'checks_pass=%s\n' "$checks_pass"
  printf 'checks_fail=%s\n' "$checks_fail"
  printf 'checks_pending=%s\n' "$checks_pending"
  printf 'checks_skipping=%s\n' "$checks_skipping"
  printf 'checks_cancel=%s\n' "$checks_cancel"
  printf 'elapsed_secs=%s\n' "$elapsed"
}

if ! command -v gh >/dev/null 2>&1; then
  printf 'The GitHub CLI (gh) is required to wait for pull request stability.\n' >&2
  exit 1
fi

if [[ -z "$pr_url" ]]; then
  if [[ -z "$repo" ]]; then
    if repo_output=$(gh repo view --json nameWithOwner --jq '.nameWithOwner' 2>&1); then
      :
    else
      status=$?
      printf 'Could not determine the GitHub repository (exit %s): %.240s\n' \
        "$status" "${repo_output%%$'\n'*}" >&2
      exit 1
    fi
    repo="$repo_output"
  fi

  if [[ -z "$branch" ]]; then
    branch=$(git branch --show-current)
  fi
  if [[ -z "$branch" ]]; then
    printf 'A branch is required when no pull request URL is supplied.\n' >&2
    exit 1
  fi

  printf 'Waiting for an open pull request for %s:%s...\n' "$repo" "$branch"
fi

while true; do
  if [[ -z "$pr_url" ]]; then
    if matches=$(gh pr list --repo "$repo" --head "$branch" --state open --limit 20 \
      --json url,headRepositoryOwner \
      --jq '.[] | [(.headRepositoryOwner.login // ""), .url] | @tsv' 2>&1); then
      consecutive_errors=0
    else
      status=$?
      retry_or_fail "find pull request" "$status" "$matches"
      sleep_until_next_poll
      continue
    fi

    matching_urls=()
    while IFS=$'\t' read -r owner candidate_url; do
      if [[ -n "$candidate_url" && (-z "$head_owner" || "$owner" == "$head_owner") ]]; then
        matching_urls+=("$candidate_url")
      fi
    done <<<"$matches"

    if ((${#matching_urls[@]} == 0)); then
      sleep_until_next_poll
      continue
    fi
    if ((${#matching_urls[@]} > 1)); then
      printf 'Multiple open pull requests match %s:%s; supply pr_url or head_owner.\n' \
        "$repo" "$branch" >&2
      exit 1
    fi

    pr_url="${matching_urls[0]}"
    printf 'Monitoring %s\n' "$pr_url"
  fi

  if pr_info=$(gh pr view "$pr_url" \
    --json state,url,headRefOid,updatedAt,comments,reviews \
    --jq '[.state, .url, .headRefOid, .updatedAt, ((.comments // []) | map({id, createdAt, updatedAt, body}) | sort_by(.id) | tojson | @base64), ((.reviews // []) | map({id, submittedAt, state, body}) | sort_by(.id) | tojson | @base64)] | @tsv' \
    2>&1); then
    :
  else
    status=$?
    retry_or_fail "read pull request" "$status" "$pr_info"
    sleep_until_next_poll
    continue
  fi

  IFS=$'\t' read -r pr_state canonical_url head_oid updated_at comments_fingerprint \
    reviews_fingerprint <<<"$pr_info"
  if [[ -z "$pr_state" || -z "$canonical_url" || -z "$head_oid" ]]; then
    retry_or_fail "parse pull request" 1 "GitHub returned incomplete pull request data"
    sleep_until_next_poll
    continue
  fi
  pr_url="$canonical_url"

  check_rows=""
  if [[ "$pr_state" == "OPEN" ]]; then
    if check_rows=$(gh pr checks "$pr_url" --json bucket,name,state,workflow \
      --jq 'sort_by(.workflow, .name, .state, .bucket) | .[] | [.bucket, .workflow, .name, .state] | @tsv' \
      2>&1); then
      consecutive_errors=0
    else
      status=$?
      retry_or_fail "read status checks" "$status" "$check_rows"
      sleep_until_next_poll
      continue
    fi
  fi
  consecutive_errors=0

  checks_total=$(awk -F '\t' 'NF >= 4 { count += 1 } END { print count + 0 }' <<<"$check_rows")
  checks_pass=$(count_check_bucket pass)
  checks_fail=$(count_check_bucket fail)
  checks_pending=$(count_check_bucket pending)
  checks_skipping=$(count_check_bucket skipping)
  checks_cancel=$(count_check_bucket cancel)

  case "$pr_state" in
    CLOSED)
      print_result pr_closed "$pr_state"
      exit 0
      ;;
    MERGED)
      print_result pr_merged "$pr_state"
      exit 0
      ;;
    OPEN) ;;
    *)
      printf 'Unexpected pull request state: %s\n' "$pr_state" >&2
      exit 1
      ;;
  esac

  now=$SECONDS
  status_fingerprint="${head_oid}"$'\n'"${check_rows}"
  activity_fingerprint="${status_fingerprint}"$'\n'"${updated_at}"$'\n'\
"${comments_fingerprint}"$'\n'"${reviews_fingerprint}"

  if [[ -z "$previous_status" || "$status_fingerprint" != "$previous_status" ]]; then
    status_stable_since=$now
    candidate_reason=""
    candidate_activity=""
    candidate_observations=0
  elif [[ "$activity_fingerprint" != "$previous_activity" ]]; then
    candidate_reason=""
    candidate_activity=""
    candidate_observations=0
  fi

  reason=""
  if ((checks_total > 0 && checks_pending == 0)); then
    reason="checks_terminal"
  elif ((now - status_stable_since >= stall_seconds)); then
    reason="activity_stalled"
  fi

  if [[ -n "$reason" ]]; then
    if [[ "$candidate_reason" == "$reason" && "$candidate_activity" == "$activity_fingerprint" ]]; then
      candidate_observations=$((candidate_observations + 1))
      if ((candidate_observations >= 2 && now - candidate_since >= settle_seconds)); then
        print_result "$reason" "$pr_state"
        exit 0
      fi
    else
      candidate_reason="$reason"
      candidate_activity="$activity_fingerprint"
      candidate_since=$now
      candidate_observations=1
      if [[ "$reason" == "checks_terminal" ]]; then
        printf 'All %s status checks are terminal; waiting %ss for quiet activity.\n' \
          "$checks_total" "$settle_seconds"
      else
        printf 'Head and checks are unchanged; waiting %ss for quiet activity.\n' \
          "$settle_seconds"
      fi
    fi
  fi

  previous_status="$status_fingerprint"
  previous_activity="$activity_fingerprint"
  sleep_until_next_poll
done
