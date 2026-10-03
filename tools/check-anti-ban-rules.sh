#!/usr/bin/env bash
# Fails if docs/anti-ban-rules.md is not an exact copy of PLAN.md section 2.
set -euo pipefail
cd "$(dirname "$0")/.."

plan=$(awk '/^## 2\. Hard rules/ {on=1; next} on && /^---$/ {exit} on' PLAN.md | sed '/^$/d')
doc=$(awk 'on; /^Copy of / {on=1}' docs/anti-ban-rules.md | sed '/^$/d')

if [ -z "$plan" ] || [ "$plan" != "$doc" ]; then
    echo "docs/anti-ban-rules.md differs from PLAN.md section 2:" >&2
    diff <(echo "$plan") <(echo "$doc") >&2 || true
    exit 1
fi
echo "docs/anti-ban-rules.md matches PLAN.md section 2"
