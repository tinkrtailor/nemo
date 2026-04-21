#!/usr/bin/env bash
# sidecar/scripts/lint-no-test-utils-in-prod.sh
#
# Two hard-failing checks in CI:
#
# 1. No CI workflow step may reference the sidecar's internal
#    `__test_utils` feature (or its old `test-utils` spelling) on a
#    RELEASE / publish step. The feature re-enables the SSH SSRF
#    bypass path that integration tests rely on and MUST NEVER be
#    enabled in a release build.
#
#    The `rust-checks-with-test-utils` job in ci.yml legitimately
#    uses the feature — it runs `cargo test` / `cargo clippy`, not
#    `cargo build --release`. To distinguish, we match only the
#    feature combined with cargo build / install / release run.
#
# 2. (FR-28) The "extra CA bundle" escape-hatch env var used by the
#    Rust sidecar's TLS layer must not appear in runtime-propagation
#    files at all. The old parity harness allowlist is gone.
#
# 3. The parity-only bind override env var follows the same rule: it
#    must not appear in runtime-propagation files.
#
#    The env var names are intentionally built from fragments below so
#    this script itself does not contain the literal tokens as a
#    single contiguous byte run. `git grep` will not match this file
#    even though it describes the patterns.
#
#    File-type scoping (runtime-propagation shapes only) keeps
#    documentation and reader code like `sidecar/src/tls.rs` out of
#    the check: a `.rs` file cannot set an env var inside a
#    container, so mere textual mention is not a security risk.

set -euo pipefail

WORKFLOWS=".github/workflows"

# ---- Check 1: __test_utils never referenced in release/build paths ----

if [ -d "$WORKFLOWS" ]; then
  BAD_PATTERN='cargo[[:space:]]+(build|install|run[[:space:]]+--release)([^#\n]*?)--features[[:space:]]*[^[:space:]]*(__)?test[-_]utils'

  if grep -rPzln --include='*.yml' --include='*.yaml' "$BAD_PATTERN" "$WORKFLOWS" 2>/dev/null; then
    echo "ERROR: CI workflows build/install/release-run with the internal __test_utils feature."
    echo "This feature is test-only and must NOT be enabled in release builds."
    echo "See sidecar/Cargo.toml [features] for the rationale."
    exit 1
  fi
else
  echo "No $WORKFLOWS directory; skipping __test_utils check"
fi

# ---- Check 2/3: parity-only env-var allowlist -----------------------

CA_BUNDLE_ENV_PREFIX="NAUTILOOP_EXTRA"
CA_BUNDLE_ENV_SUFFIX="CA_BUNDLE"
CA_BUNDLE_ENV="${CA_BUNDLE_ENV_PREFIX}_${CA_BUNDLE_ENV_SUFFIX}"

BIND_ALL_ENV_A="NAUTILOOP_BIND"
BIND_ALL_ENV_B="ALL_INTERFACES"
BIND_ALL_ENV="${BIND_ALL_ENV_A}_${BIND_ALL_ENV_B}"

# File-type scope: only files that could actually propagate an env
# var into a container at runtime. Rust source and markdown are
# intentionally NOT in scope because they cannot set env vars inside
# a running container (the Rust reader in sidecar/src/tls.rs only
# READS the var, which is fine).
SCOPED_PATHSPECS=(
  'Dockerfile'
  '*.Dockerfile'
  'Dockerfile.*'
  '*.yml'
  '*.yaml'
  '*.sh'
  '.env'
  '.env.*'
  '*.tf'
  '*.hcl'
)

check_allowed_env_references() {
  local env_name="$1"
  local label="$2"
  local matches pattern

  pattern="^(?![[:space:]]*#).*${env_name}"
  matches=$(
    git grep -nP "$pattern" \
      -- "${SCOPED_PATHSPECS[@]}" \
      2>/dev/null || true
  )

  if [ -n "$matches" ]; then
    echo "ERROR: ${env_name} referenced outside the parity allowlist (${label}):"
    echo "$matches"
    echo ""
    echo "These env vars are no longer allowed in runtime-propagation files."
    echo "Full-line comments in YAML / Dockerfile / shell are allowed, but"
    echo "any non-comment reference inside Dockerfile / YAML / shell / .env /"
    echo "terraform / HCL is a violation. Rust source and markdown docs are"
    echo "out of scope because they cannot set env vars in a container."
    exit 1
  fi
}

if git rev-parse --git-dir > /dev/null 2>&1; then
  check_allowed_env_references "$CA_BUNDLE_ENV" "extra CA bundle"
  check_allowed_env_references "$BIND_ALL_ENV" "parity bind override"
else
  echo "Not inside a git repo; skipping ${CA_BUNDLE_ENV} reference check"
fi

echo "OK: no __test_utils feature references in release CI workflows"
echo "OK: ${CA_BUNDLE_ENV} references are within the FR-28 allowlist"
echo "OK: ${BIND_ALL_ENV} references are within the parity allowlist"
