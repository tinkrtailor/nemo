#!/usr/bin/env bash
set -euo pipefail

REGISTRY="localhost:5001"
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
        -t "${REGISTRY}/nautiloop-control-plane:dev" \
        "${CONTEXT}"
    docker push "${REGISTRY}/nautiloop-control-plane:dev"
    echo "    Pushed ${REGISTRY}/nautiloop-control-plane:dev"
fi

if "$BUILD_SIDECAR"; then
    echo "==> Building sidecar image..."
    docker build \
        -f "${CONTEXT}/sidecar/Dockerfile" \
        -t "${REGISTRY}/nautiloop-sidecar:dev" \
        "${CONTEXT}"
    docker push "${REGISTRY}/nautiloop-sidecar:dev"
    echo "    Pushed ${REGISTRY}/nautiloop-sidecar:dev"
fi

if "$BUILD_AGENT_BASE"; then
    echo "==> Building agent-base image..."
    docker build \
        -f "${CONTEXT}/images/base/Dockerfile" \
        -t "${REGISTRY}/nautiloop-agent-base:dev" \
        "${CONTEXT}"
    docker push "${REGISTRY}/nautiloop-agent-base:dev"
    echo "    Pushed ${REGISTRY}/nautiloop-agent-base:dev"
fi

echo "==> Build complete."
