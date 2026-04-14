#!/usr/bin/env bash
set -euo pipefail

echo "==> Deleting k3d cluster nautiloop-dev..."
k3d cluster delete nautiloop-dev 2>/dev/null || echo "    (cluster not found, skipping)"

echo "==> Deleting k3d registry nautiloop-registry..."
k3d registry delete nautiloop-registry 2>/dev/null || echo "    (registry not found, skipping)"

echo "==> Teardown complete."
