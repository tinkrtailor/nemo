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

# FR-3b: Install Bun + pnpm system-wide so audit/implement/test stages
# can run JS monorepos that have migrated off npm. Node + npm come from
# the base image; Bun ships a standalone binary with its own runtime,
# so pinning by version + verifying via the release's SHASUMS256.txt
# gives the same supply-chain guarantee as the opencode/sccache fetches
# above without us having to hardcode per-arch digests that drift with
# every bump.
#
# pnpm is installed via corepack (ships with node:22) so the version
# pinned in each repo's `packageManager` field is honored at runtime
# without us having to track per-monorepo pnpm releases here.
ARG BUN_VERSION=1.1.43
RUN set -eux; \
    case "$(uname -m)" in \
      x86_64)  asset=bun-linux-x64.zip ;; \
      aarch64) asset=bun-linux-aarch64.zip ;; \
      *)       echo "Unsupported arch for bun: $(uname -m)" >&2; exit 1 ;; \
    esac; \
    apt-get update && apt-get install -y --no-install-recommends unzip && rm -rf /var/lib/apt/lists/*; \
    base_url="https://github.com/oven-sh/bun/releases/download/bun-v${BUN_VERSION}"; \
    # Download with the original asset filename so `sha256sum -c` can
    # find it: SHASUMS256.txt references `bun-linux-<arch>.zip`, and
    # `sha256sum -c` resolves the filename in that line against cwd.
    cd /tmp; \
    curl -fsSL -O "${base_url}/${asset}"; \
    curl -fsSL -o /tmp/SHASUMS256.txt "${base_url}/SHASUMS256.txt"; \
    grep -E "  ${asset}\$" SHASUMS256.txt | sha256sum -c -; \
    unzip -q "/tmp/${asset}" -d /tmp; \
    dir="$(find /tmp -maxdepth 1 -type d -name 'bun-linux-*' | head -n1)"; \
    mv "${dir}/bun" /usr/local/bin/bun; \
    chmod +x /usr/local/bin/bun; \
    ln -sf /usr/local/bin/bun /usr/local/bin/bunx; \
    rm -rf "/tmp/${asset}" /tmp/SHASUMS256.txt /tmp/bun-linux-*; \
    /usr/local/bin/bun --version
# Enable corepack (ships with node:22) so `pnpm` / `yarn` invocations
# resolve to the version pinned in each repo's package.json.
RUN corepack enable && corepack prepare pnpm@latest --activate && pnpm --version

# Optional: Rust toolchain for Rust-targeted repos (e.g. nautiloop itself).
# Off by default to keep the base image lean (~200MB saved). Enable per-build
# with `--build-arg INCLUDE_RUST=true`. Production release workflow leaves it off
# and publishes slim; dev builds enable it so Claude can verify + commit Rust code.
ARG INCLUDE_RUST=false
ENV CARGO_HOME=/usr/local/cargo
ENV RUSTUP_HOME=/usr/local/rustup
ENV PATH=/usr/local/cargo/bin:$PATH
RUN if [ "$INCLUDE_RUST" = "true" ]; then \
        set -eux; \
        curl -fsSL https://sh.rustup.rs | sh -s -- -y --default-toolchain stable --profile minimal --no-modify-path --component clippy --component rustfmt; \
        chmod -R a+rX "$CARGO_HOME" "$RUSTUP_HOME"; \
        rustc --version; cargo --version; cargo clippy --version; \
    fi

# Spec #130: Install sccache (Rust compilation cache) alongside the Rust toolchain.
# sccache is compiled independently by upstream as a musl binary, so it's portable.
# With RUSTC_WRAPPER=sccache + SCCACHE_DIR=/cache/sccache, rustc outputs are cached
# across agent pods via a shared PVC, killing the cold-compile wall (~25min → ~2min).
ARG SCCACHE_VERSION=v0.10.0
ARG SCCACHE_SHA256_AMD64=cf3370c249a717164a4f4e6a2b3e0da7fa41e40770cba2d4cf95f2fde6aeef4b
ARG SCCACHE_SHA256_ARM64=f1d62a17ea091d0f34e7ee0cfed2b5d67d2c29ccd33acbad80a0af83e7b27e8f
RUN if [ "$INCLUDE_RUST" = "true" ]; then \
        set -eux; \
        case "$(uname -m)" in \
          x86_64)  arch=x86_64-unknown-linux-musl;  expected="$SCCACHE_SHA256_AMD64" ;; \
          aarch64) arch=aarch64-unknown-linux-musl; expected="$SCCACHE_SHA256_ARM64" ;; \
          *)       echo "Unsupported arch for sccache: $(uname -m)" >&2; exit 1 ;; \
        esac; \
        curl -fsSL -o /tmp/sccache.tar.gz \
          "https://github.com/mozilla/sccache/releases/download/${SCCACHE_VERSION}/sccache-${SCCACHE_VERSION}-${arch}.tar.gz"; \
        tar -xzf /tmp/sccache.tar.gz -C /tmp; \
        mv "/tmp/sccache-${SCCACHE_VERSION}-${arch}/sccache" /usr/local/bin/sccache; \
        chmod +x /usr/local/bin/sccache; \
        rm -rf /tmp/sccache.tar.gz "/tmp/sccache-${SCCACHE_VERSION}-${arch}"; \
        /usr/local/bin/sccache --version; \
    fi

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
