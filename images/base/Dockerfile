# Base Agent Image for Nemo
# FR-1 through FR-3: Tools, runtimes, and agent CLIs
# NFR-1: Target < 2 GB compressed via multi-stage build

# Stage 1: Install tools that need compilation
FROM python:3.12-slim@sha256:6294405dc2a6612366092848c41c2ec7f9ba7368b618ee707dcf7f75c1d07830 AS python-tools
RUN pip install --no-cache-dir yq

# Stage 2: Get opencode binary from published image
FROM ghcr.io/anomalyco/opencode:latest@sha256:f19f2c5902ae1b20667b707d495dfc80d4c30761b52f4d8a66ed4f7778693d74 AS opencode-src

# Stage 3: Final image
FROM node:22-bookworm-slim@sha256:263e93fe2e7f5edc1e0f6647fcc679aa88e7cdd25d93bccf65f97ab68eacb93b

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

# FR-3: Install opencode binary
COPY --from=opencode-src /usr/local/bin/opencode /usr/local/bin/opencode
RUN chmod +x /usr/local/bin/opencode

# FR-4: Entrypoint script
COPY images/base/nemo-agent-entry /usr/local/bin/nemo-agent-entry
RUN chmod +x /usr/local/bin/nemo-agent-entry

# Default prompt templates (FR-33)
# Build context must be the repo root: docker build -f images/base/Dockerfile .
# Prompts are embedded in the image as fallback defaults.
COPY .nemo/prompts/ /etc/nemo/prompts/

# Create agent user (UID 1000) for non-root execution (FR-25)
# node:22-bookworm-slim has UID/GID 1000 as 'node'. Reuse it.
RUN mkdir -p /work/home && \
    cp -a /home/node/. /work/home/ 2>/dev/null || true && \
    usermod -l agent -d /work/home -s /bin/bash node && \
    groupmod -n agent node && \
    chown -R agent:agent /work/home

# Writable directories that will be emptyDir mounts at runtime
RUN mkdir -p /work /work/home /output /sessions /tmp/shared && \
    chown -R agent:agent /work /output /sessions /tmp/shared

USER agent
WORKDIR /work

# Use tini as PID 1 for proper signal handling (FR-4)
ENTRYPOINT ["tini", "--", "/usr/local/bin/nemo-agent-entry"]
