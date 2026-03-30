#!/bin/bash
set -euo pipefail

# =============================================================================
# Nemo — Docker Image Builder
# =============================================================================
# Builds production images and optionally pushes to GHCR.
# Uses 1Password CLI (op) for GHCR authentication.
#
# Usage:
#   ./build-images.sh --tag 0.1.0                        # Build + push all
#   ./build-images.sh --tag 0.1.0 --no-push              # Build only (local)
#   ./build-images.sh --tag 0.1.0 --only control-plane   # Build one image
#   ./build-images.sh --tag 0.1.0 --platform linux/amd64 # Override platform
# =============================================================================

# -- Configuration ------------------------------------------------------------

REGISTRY="ghcr.io/tinkrtailor"
OP_ENV_FILE="op.env"
IMAGE_TAG=""
PUSH=true
ONLY=""
PLATFORM_OVERRIDE=""

# -- Colors -------------------------------------------------------------------

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

info()    { echo -e "${BLUE}[info]${NC} $1"; }
success() { echo -e "${GREEN}[ok]${NC} $1"; }
warn()    { echo -e "${YELLOW}[warn]${NC} $1"; }
error()   { echo -e "${RED}[error]${NC} $1"; exit 1; }

# -- Parse args ---------------------------------------------------------------

while [[ $# -gt 0 ]]; do
    case "$1" in
        --tag|-t)
            IMAGE_TAG="$2"
            shift 2
            ;;
        --no-push)
            PUSH=false
            shift
            ;;
        --only)
            ONLY="$2"
            shift 2
            ;;
        --platform)
            PLATFORM_OVERRIDE="$2"
            shift 2
            ;;
        -h|--help)
            echo "Usage: ./build-images.sh --tag <version> [options]"
            echo ""
            echo "Options:"
            echo "  --tag, -t <version>   Image tag (required)"
            echo "  --no-push             Build locally, don't push to GHCR"
            echo "  --only <image>        Build only: control-plane, agent-base, or sidecar"
            echo "  --platform <plat>     Override platform (default: linux/amd64)"
            echo "  -h, --help            Show this help"
            exit 0
            ;;
        *)
            error "Unknown argument: $1. Use --help for usage."
            ;;
    esac
done

# -- Validation ---------------------------------------------------------------

if [ -z "$IMAGE_TAG" ]; then
    error "Tag is required. Usage: ./build-images.sh --tag 0.1.0"
fi

if [ -n "$ONLY" ] && [[ ! "$ONLY" =~ ^(control-plane|agent-base|sidecar)$ ]]; then
    error "Invalid --only value '$ONLY'. Must be: control-plane, agent-base, or sidecar"
fi

if ! command -v docker &>/dev/null; then
    error "Docker not found. Install Docker first."
fi

if ! docker buildx version &>/dev/null; then
    error "Docker buildx not available. Install buildx plugin."
fi

should_build() { [ -z "$ONLY" ] || [ "$ONLY" = "$1" ]; }

# -- GHCR auth via 1Password --------------------------------------------------

if [ "$PUSH" = true ]; then
    if ! docker login ghcr.io --get-login &>/dev/null 2>&1; then
        warn "Not logged in to ghcr.io. Attempting login via 1Password..."
        if ! command -v op &>/dev/null; then
            error "Not logged in to ghcr.io and op CLI not available. Install: https://developer.1password.com/docs/cli"
        fi
        if ! op account list &>/dev/null; then
            error "Not signed in to 1Password. Run: op signin"
        fi
        GHCR_USER=$(op read "op://Nemo/github-registry/username" 2>/dev/null) || error "Failed to read GHCR username from 1Password"
        GHCR_TOKEN=$(op read "op://Nemo/github-registry/pat" 2>/dev/null) || error "Failed to read GHCR token from 1Password"
        echo "$GHCR_TOKEN" | docker login ghcr.io -u "$GHCR_USER" --password-stdin || error "GHCR login failed"
        success "Logged in to ghcr.io"
    fi
fi

# -- Buildx setup -------------------------------------------------------------

BUILDER_NAME="nemo-multiarch"
if ! docker buildx inspect "$BUILDER_NAME" &>/dev/null; then
    info "Creating buildx builder '$BUILDER_NAME'..."
    docker buildx create --name "$BUILDER_NAME" --use --bootstrap
else
    docker buildx use "$BUILDER_NAME"
fi

# Hetzner runs amd64
PLATFORM="${PLATFORM_OVERRIDE:-linux/amd64}"

info "Building images with tag: $IMAGE_TAG"
info "Platform: $PLATFORM"
info "Push: $PUSH"
if [ -n "$ONLY" ]; then
    info "Only: $ONLY"
fi
echo ""

# -- Build helpers ------------------------------------------------------------

BUILT_IMAGES=()

build_image() {
    local name="$1"
    local dockerfile="$2"
    local context="$3"
    shift 3

    local full_image="$REGISTRY/nemo-$name:$IMAGE_TAG"

    info "Building nemo-$name ($PLATFORM)..."

    local build_cmd=(
        docker buildx build
        --platform "$PLATFORM"
        -f "$dockerfile"
        -t "$full_image"
        -t "$REGISTRY/nemo-$name:latest"
    )

    if [ "$PUSH" = true ]; then
        build_cmd+=(--push)
    else
        build_cmd+=(--load)
    fi

    build_cmd+=("$context")

    "${build_cmd[@]}" || error "Failed to build nemo-$name"

    BUILT_IMAGES+=("$full_image")
    success "nemo-$name:$IMAGE_TAG built"
}

# -- Build: control-plane -----------------------------------------------------

if should_build "control-plane"; then
    build_image "control-plane" "images/control-plane/Dockerfile" "."
fi

# -- Build: agent-base --------------------------------------------------------

if should_build "agent-base"; then
    build_image "agent-base" "images/base/Dockerfile" "."
fi

# -- Build: sidecar -----------------------------------------------------------

if should_build "sidecar"; then
    build_image "sidecar" "images/sidecar/Dockerfile" "images/sidecar"
fi

# -- Summary ------------------------------------------------------------------

echo ""
success "All images built successfully!"
echo ""
for img in "${BUILT_IMAGES[@]}"; do
    echo "  $img"
done
echo ""

if [ "$PUSH" = true ]; then
    info "Images pushed to $REGISTRY"
    echo ""
    info "Update terraform vars to use tag '$IMAGE_TAG':"
    echo "  TF_VAR_control_plane_image=$REGISTRY/nemo-control-plane:$IMAGE_TAG"
    echo "  TF_VAR_agent_base_image=$REGISTRY/nemo-agent-base:$IMAGE_TAG"
fi
