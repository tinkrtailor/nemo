# Base Agent Image for Nautiloop
# FR-1 through FR-3: Tools, runtimes, and agent CLIs
# NFR-1: Target < 2 GB compressed via multi-stage build

# Stage 1: Install tools that need compilation
FROM python:3.12-slim AS python-tools
RUN pip install --no-cache-dir yq

# Stage 2: Fetch opencode binary from the official sst/opencode releases.
#
# We deliberately do NOT use ghcr.io/anomalyco/opencode:latest — that image
# ships the musl (Alpine) build of opencode, which cannot run on the
# node:22-bookworm-slim (Debian/glibc) base: the binary's PT_INTERP points
# at /lib/ld-musl-x86_64.so.1 and its C++ symbols come from a musl-built
# libstdc++, so bash reports "cannot execute: required file not found" and
# the audit/review/implement stages all fail with exit 127 before the
# agent does any work.
#
# Pulling the glibc build directly from the upstream release tarball
# sidesteps this and gets us reproducible, pinned versions.
FROM debian:bookworm-slim AS opencode-src
# TARGETARCH is auto-populated by BuildKit (both `docker build` with the
# default BuildKit builder AND `docker buildx build`) to the target
# platform. Do NOT set a default here — BuildKit only auto-populates
# when the ARG has no default value. With a default, the arg becomes a
# static build arg and BuildKit stops overriding it, which causes
# multi-platform builds to fetch the wrong binary.
# For legacy non-BuildKit builds (DOCKER_BUILDKIT=0), pass explicitly:
#   docker build --build-arg TARGETARCH=amd64 ...
ARG TARGETARCH
ARG OPENCODE_VERSION=v1.3.17
# Pinned SHA256 digests for the two supported arches. If upstream replaces
# a release asset (or a MITM tampers with it), the shasum check below fails
# the build BEFORE we execute anything from the tarball. This is the whole
# reason we verify before running `opencode --version`.
ARG OPENCODE_SHA256_AMD64=19e540840cfd04afebed92cfe67dce0cd6c425ed5f729ef946e8071f776205a9
ARG OPENCODE_SHA256_ARM64=345293ed29c703cf3c4b0e696db50c3586c973a4d4ff90f1db5b6cbbbb2cee4b
RUN apt-get update && apt-get install -y --no-install-recommends \
    curl ca-certificates \
    && rm -rf /var/lib/apt/lists/*
RUN set -eux; \
    case "$TARGETARCH" in \
      amd64) asset="opencode-linux-x64.tar.gz";   expected_sha="$OPENCODE_SHA256_AMD64" ;; \
      arm64) asset="opencode-linux-arm64.tar.gz"; expected_sha="$OPENCODE_SHA256_ARM64" ;; \
      *)     echo "Unsupported TARGETARCH: $TARGETARCH" >&2; exit 1 ;; \
    esac; \
    curl -fsSL -o /tmp/opencode.tar.gz \
      "https://github.com/sst/opencode/releases/download/${OPENCODE_VERSION}/${asset}"; \
    echo "${expected_sha}  /tmp/opencode.tar.gz" | sha256sum -c -; \
    mkdir -p /opt/opencode; \
    tar -xzf /tmp/opencode.tar.gz -C /opt/opencode opencode; \
    chmod +x /opt/opencode/opencode; \
    /opt/opencode/opencode --version

# Stage 3: Final image
FROM node:22-bookworm-slim

# FR-1: System packages
RUN apt-get update && apt-get install -y --no-install-recommends \
    git \
    curl \
    jq \
    build-essential \
    python3 \
    python3-pip \
    openssh-client \
    ca-certificates \
    tini \
    iptables \
    procps \
    && rm -rf /var/lib/apt/lists/*

# FR-1: tomlq from yq package (provides TOML-to-JSON conversion)
COPY --from=python-tools /usr/local/bin/tomlq /usr/local/bin/tomlq
COPY --from=python-tools /usr/local/bin/yq /usr/local/bin/yq
COPY --from=python-tools /usr/local/lib/python3.12/site-packages /usr/local/lib/python3.12/site-packages

# FR-2: Install claude-code
RUN npm install -g @anthropic-ai/claude-code@2.1.87

# FR-3: Install opencode binary (glibc build, see opencode-src stage)
COPY --from=opencode-src /opt/opencode/opencode /usr/local/bin/opencode
RUN chmod +x /usr/local/bin/opencode && /usr/local/bin/opencode --version

# FR-4: Entrypoint script
COPY images/base/nautiloop-agent-entry /usr/local/bin/nautiloop-agent-entry
RUN chmod +x /usr/local/bin/nautiloop-agent-entry

# FR-2d: Pod introspection script (read-only runtime diagnostics)
COPY images/base/nautiloop-introspect /usr/local/bin/nautiloop-introspect
RUN chmod +x /usr/local/bin/nautiloop-introspect

# Default prompt templates (FR-33)
# Build context must be the repo root: docker build -f images/base/Dockerfile .
# Prompts are embedded in the image as fallback defaults.
COPY .nautiloop/prompts/ /etc/nautiloop/prompts/

# Create agent user (UID 1000) for non-root execution (FR-25).
# node:22-bookworm-slim has UID/GID 1000 as 'node'. Reuse it.
#
# HOME deliberately lives at /home/agent and NOT inside /work. /work is
# mounted from the worktree at runtime (read-only for REVIEW/AUDIT, FR-6),
# and a sub-mount inside a read-only mount fails at pod start because
# containerd cannot create the mountpoint inside the read-only rootfs.
RUN mkdir -p /home/agent && \
    cp -a /home/node/. /home/agent/ 2>/dev/null || true && \
    usermod -l agent -d /home/agent -s /bin/bash node && \
    groupmod -n agent node && \
    chown -R agent:agent /home/agent

# Writable directories that will be emptyDir mounts at runtime.
# These must exist in the image because the agent container runs with
# read_only_root_filesystem: true — containerd cannot create mountpoints
# in a read-only rootfs.
RUN mkdir -p /work /output /sessions /tmp/shared && \
    chown -R agent:agent /work /output /sessions /tmp/shared /home/agent

USER agent
WORKDIR /work

# Use tini as PID 1 for proper signal handling (FR-4)
ENTRYPOINT ["tini", "--", "/usr/local/bin/nautiloop-agent-entry"]
