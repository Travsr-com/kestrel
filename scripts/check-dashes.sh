#!/usr/bin/env bash
# Fails if any tracked file contains an em-dash or en-dash.
# The characters are built from bytes so this script does not contain them itself.
set -euo pipefail
pattern="$(printf '\xe2\x80\x93|\xe2\x80\x94')"
if git grep -nE "$pattern" -- . ':!LICENSE'; then
  echo "error: em-dash or en-dash found (use a comma, period or parentheses)" >&2
  exit 1
fi
echo "ok: no em-dashes or en-dashes"
