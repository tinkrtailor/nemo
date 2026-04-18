#!/usr/bin/env bash
set -euo pipefail

CONTEXT="$(cd "$(dirname "$0")/.." && pwd)"

BUILD_CONTROL_PLANE=false
BUILD_SIDECAR=false
BUILD_AGENT_BASE=false

# Parse flags
if [ $# -eq 0 ]; then
    BUILD_CONTROL_PLANE=true
    BUILD_SIDECAR=true
    BUILD_AGENT_BASE=true
fi

for arg in "$@"; do
    case "$arg" in
        --control-plane) BUILD_CONTROL_PLANE=true ;;
        --sidecar)       BUILD_SIDECAR=true ;;
        --agent-base)    BUILD_AGENT_BASE=true ;;
        *) echo "Unknown flag: $arg (valid: --control-plane, --sidecar, --agent-base)" >&2; exit 1 ;;
    esac
done

if "$BUILD_CONTROL_PLANE"; then
    echo "==> Building control-plane image..."
    docker build \
        -f "${CONTEXT}/images/control-plane/Dockerfile" \
        -t "nautiloop-control-plane:dev" \
        "${CONTEXT}"
    echo "==> Importing nautiloop-control-plane:dev into k3d cluster..."
    k3d image import nautiloop-control-plane:dev -c nautiloop-dev
    echo "    Done."
fi

if "$BUILD_SIDECAR"; then
    echo "==> Building sidecar image..."
    docker build \
        -f "${CONTEXT}/sidecar/Dockerfile" \
        -t "nautiloop-sidecar:dev" \
        "${CONTEXT}"
    echo "==> Importing nautiloop-sidecar:dev into k3d cluster..."
    k3d image import nautiloop-sidecar:dev -c nautiloop-dev
    echo "    Done."
fi

if "$BUILD_AGENT_BASE"; then
    echo "==> Building agent-base image (dev: includes Rust toolchain for dogfooding)..."
    docker build \
        --build-arg INCLUDE_RUST=true \
        -f "${CONTEXT}/images/base/Dockerfile" \
        -t "nautiloop-agent-base:dev" \
        "${CONTEXT}"
    echo "==> Importing nautiloop-agent-base:dev into k3d cluster..."
    k3d image import nautiloop-agent-base:dev -c nautiloop-dev
    echo "    Done."
fi

echo "==> Build complete."
