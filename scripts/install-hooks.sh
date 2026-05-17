#!/usr/bin/env bash
#
# scripts/install-hooks.sh — make the versioned git hooks active.
#
# We keep hooks in .githooks/ (tracked in the repo) instead of the
# untracked .git/hooks/. One `git config` makes them live; run this once
# per clone. After it, every `git commit` auto-refreshes the README
# numbers via .githooks/pre-commit.

set -euo pipefail
ROOT="$(git rev-parse --show-toplevel)"
cd "$ROOT"

chmod +x .githooks/* scripts/*.sh 2>/dev/null || true
git config core.hooksPath .githooks

echo "Hooks installed: core.hooksPath -> .githooks"
echo "  • pre-commit: regenerates README stats and re-stages README.md"
echo "Bypass once with: DRDROS_SKIP_STATS=1 git commit ..."
