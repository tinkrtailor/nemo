#!/usr/bin/env bash
# sidecar/scripts/lint-no-test-utils-in-prod.sh
#
# Fails if any CI workflow references the sidecar's internal
# `__test_utils` feature (or its old `test-utils` spelling) on a
# build/publish step. The feature re-enables the SSH SSRF bypass
# path that integration tests rely on, and MUST NEVER be enabled
# in a release build.
#
# This script is intentionally not wired into CI by the spec that
# introduced it; see issue #71 for the CI wiring followup. It lives
# in-repo as a drop-in so the wiring PR can plug it in without
# re-authoring the check.
set -euo pipefail

WORKFLOWS=".github/workflows"
if [ ! -d "$WORKFLOWS" ]; then
  echo "No $WORKFLOWS directory; nothing to check"
  exit 0
fi

# Match both the new `__test_utils` name and the old `test-utils`
# spelling, in `--features` arguments targeting the sidecar crate
# explicitly or via an unqualified `--features` on a cargo invocation
# that builds the sidecar.
BAD_PATTERN='--features[[:space:]]*[^[:space:]]*(__)?test[-_]utils'

if grep -rEn "$BAD_PATTERN" "$WORKFLOWS" 2>/dev/null; then
  echo "ERROR: CI workflows reference the internal __test_utils feature."
  echo "This feature is test-only and must NOT be enabled in release builds."
  echo "See sidecar/Cargo.toml [features] for the rationale."
  exit 1
fi

echo "OK: no __test_utils feature references in CI workflows"
