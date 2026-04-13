# Sidecar image for Nautiloop job pods.
# Produces a minimal image with the nautiloop-sidecar binary.
# The sidecar runs as a native sidecar container (initContainer +
# restartPolicy: Always) alongside each agent job pod, providing:
#   - Model API proxy on :9090 (OpenAI + Anthropic credential injection)
#   - Git SSH proxy on :9091 (scoped to the job's repo)
#   - Egress logger on :9092 (HTTP CONNECT proxy with JSON audit log)
#   - Health endpoint on :9093 (gates agent container startup)
#
# Build context must be the workspace root (context: .) so that
# Cargo.toml / Cargo.lock and all workspace member manifests are available.

FROM rust:1.88-bookworm AS builder

WORKDIR /build

# Cache dependency layer: copy manifests first, stub sources so the
# workspace resolver can run without pulling in real source files.
COPY Cargo.toml Cargo.lock ./
COPY control-plane/Cargo.toml control-plane/Cargo.toml
COPY cli/Cargo.toml cli/Cargo.toml
COPY sidecar/Cargo.toml sidecar/Cargo.toml
COPY sidecar/tests/parity/Cargo.toml sidecar/tests/parity/Cargo.toml

RUN mkdir -p control-plane/src cli/src sidecar/src sidecar/tests/parity/src && \
    echo "fn main() {}" > control-plane/src/main.rs && \
    echo "" > control-plane/src/lib.rs && \
    echo "fn main() {}" > cli/src/main.rs && \
    echo "" > sidecar/src/lib.rs && \
    echo "fn main() {}" > sidecar/src/main.rs && \
    echo "fn main() {}" > sidecar/tests/parity/src/main.rs

RUN cargo build --release --package nautiloop-sidecar 2>/dev/null || true

# Copy real sidecar source and rebuild.
COPY sidecar/ sidecar/
RUN touch sidecar/src/main.rs sidecar/src/lib.rs && \
    cargo build --release --package nautiloop-sidecar

# Runtime image: minimal Debian with TLS certs only.
# No git / openssh-client needed — the sidecar handles SSH internally
# via russh and proxies git operations without shelling out.
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    tini \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/target/release/nautiloop-sidecar /usr/local/bin/nautiloop-sidecar

RUN groupadd -g 1000 nautiloop && \
    useradd -u 1000 -g nautiloop -m nautiloop
USER nautiloop

ENTRYPOINT ["tini", "--", "/usr/local/bin/nautiloop-sidecar"]
