#!/usr/bin/env bash
#
# Every `unsafe` block in NOLX must carry a `// SAFETY:` comment naming the invariant it
# depends on. The kernel's memory-safety budget is spent in a handful of places (the HAL,
# page tables, raw object pointers); if a new unsafe block appears without a stated
# invariant, that budget is being spent without review.
#
# Exits non-zero if an unsafe block lacks one.

set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO"

bad=0
total=0

# Find `unsafe` occurrences that open a block (not `unsafe fn`/`unsafe impl` declarations,
# which carry their own contract at the item level) and check the 3 preceding lines for a
# SAFETY comment.
while IFS=: read -r file line _; do
    total=$((total + 1))
    start=$((line > 4 ? line - 4 : 1))
    if ! sed -n "${start},$((line + 1))p" "$file" | grep -q 'SAFETY:'; then
        echo "  missing SAFETY comment: $file:$line" >&2
        bad=$((bad + 1))
    fi
done < <(grep -rn --include='*.rs' -E '^[[:space:]]*(let[[:space:]]+[^=]*=[[:space:]]*)?unsafe([[:space:]]*\{|[[:space:]]*$)' \
            crates kernel tools 2>/dev/null | cut -d: -f1,2 || true)

echo "unsafe blocks checked: $total, missing SAFETY comment: $bad"
if [ "$bad" -ne 0 ]; then
    echo "FAIL: add a // SAFETY: comment above each unsafe block" >&2
    exit 1
fi
echo "OK: every unsafe block states its invariant"
