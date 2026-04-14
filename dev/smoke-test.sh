#!/usr/bin/env bash
set -euo pipefail

NAUTILOOP_SERVER="${NAUTILOOP_SERVER:-http://localhost:18080}"
ENGINEER="${NAUTILOOP_ENGINEER:-dev}"
# SPEC is a repo-relative path read by the server from origin/main.
# Default to the sidecar-followups spec which exists on main.
SPEC="${1:-specs/sidecar-followups.md}"

echo "==> Submitting harden job for: ${SPEC}"
echo "    Server:   ${NAUTILOOP_SERVER}"
echo "    Engineer: ${ENGINEER}"
echo ""

# nemo harden prints:
#   Started loop <uuid>
#     Branch: <branch>
#     State:  PENDING
#
# Capture the loop_id from the first line.
OUTPUT="$(nemo harden "$SPEC" --server "${NAUTILOOP_SERVER}")"
echo "$OUTPUT"
echo ""

LOOP_ID="$(echo "$OUTPUT" | grep '^Started loop ' | awk '{print $3}')"
if [ -z "$LOOP_ID" ]; then
    echo "ERROR: Could not parse loop_id from nemo harden output."
    exit 1
fi

echo "==> Loop ID: ${LOOP_ID}"
echo "==> Streaming logs (Ctrl-C to stop)..."
echo ""

nemo logs "$LOOP_ID" --server "${NAUTILOOP_SERVER}"
