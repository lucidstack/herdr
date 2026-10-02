#!/usr/bin/env bash
# Opens one issue per stable herdr release published since this fork's merge base, as a
# reminder to merge it. Runs daily from .github/workflows/upstream-releases.yml;
# DRY_RUN=1 only prints what it would open.
#
# REPO      repository to open issues in (default: the current one)
# UPSTREAM  repository to watch (default: herdrdev/herdr)
# SINCE     ignore releases published before this time (default: the fork point)
set -euo pipefail

upstream="${UPSTREAM:-herdrdev/herdr}"
since="${SINCE:-2026-09-25T00:00:00Z}"
repo="${REPO:-$(gh repo view --json nameWithOwner --jq .nameWithOwner)}"
dry_run="${DRY_RUN:-}"
label="upstream"

if [ -z "$dry_run" ]; then
    gh label create "$label" --repo "$repo" --color 5319e7 \
        --description "A herdr release to consider porting" 2>/dev/null || true
fi

# Oldest first, so the issues read in release order.
tags="$(gh release list --repo "$upstream" --exclude-drafts --exclude-pre-releases --limit 50 \
    --json tagName,publishedAt --jq ".[] | select(.publishedAt > \"$since\") | .tagName" | tac)"

while IFS= read -r tag; do
    [ -n "$tag" ] || continue
    title="Upstream herdr $tag"
    if gh issue list --repo "$repo" --label "$label" --state all --search "\"$title\" in:title" \
        --json title --jq '.[].title' | grep -Fxq "$title"; then
        continue
    fi
    body="$(mktemp)"
    {
        gh release view "$tag" --repo "$upstream" --json url,publishedAt \
            --jq '"herdr \(.url | split("/") | last) was released on \(.publishedAt[:10]): \(.url)"'
        echo
        echo "Merge \`upstream/master\` into the fork, check its notes below against the fork's changes, and close this issue once it is merged."
        echo
        echo "---"
        echo
        # GitHub caps issue bodies at 65536 characters.
        gh release view "$tag" --repo "$upstream" --json body --jq .body | head -c 60000
    } >"$body"
    if [ -n "$dry_run" ]; then
        echo "would open: $title ($(wc -c <"$body" | tr -d ' ') bytes)"
    else
        gh issue create --repo "$repo" --title "$title" --label "$label" --body-file "$body"
    fi
    rm -f "$body"
done <<<"$tags"
