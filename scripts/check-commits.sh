#!/bin/sh
# Validates that every commit subject in a range follows
# Conventional Commits: type(scope): subject
# Usage: scripts/check-commits.sh <git-range>   (e.g. origin/main..HEAD)
set -eu
range="${1:?usage: check-commits.sh <git-range>}"
pattern='^(feat|fix|docs|style|refactor|perf|test|build|ci|chore)(\([a-z0-9._/-]+\))?!?: .+[^.]$'
bad=0
count=0
hashes=$(git rev-list "$range") || { echo "error: invalid range: $range" >&2; exit 2; }
for hash in $hashes; do
    subject=$(git log -1 --format=%s "$hash")
    count=$((count + 1))
    if ! printf '%s' "$subject" | grep -Eq "$pattern"; then
        echo "non-conventional: $hash $subject"
        bad=$((bad + 1))
    fi
done
echo "checked $count commit(s), $bad non-conventional"
[ "$bad" -eq 0 ]
