# Control plane image for Nautiloop
# Produces a minimal image with the nautiloop-server binary.
# Same binary runs as API server or loop engine based on args.

FROM rust:1.88-bookworm AS builder

WORKDIR /build

# Cache dependency layer: copy manifests first
COPY Cargo.toml Cargo.lock ./
COPY control-plane/Cargo.toml control-plane/Cargo.toml
COPY cli/Cargo.toml cli/Cargo.toml

# Create stub sources so cargo can resolve the workspace
RUN mkdir -p control-plane/src cli/src && \
    echo "fn main() {}" > control-plane/src/main.rs && \
    echo "" > control-plane/src/lib.rs && \
    echo "fn main() {}" > cli/src/main.rs

RUN cargo build --release --package nautiloop-control-plane 2>/dev/null || true

# Copy real source and rebuild
COPY control-plane/ control-plane/
COPY cli/ cli/
RUN touch control-plane/src/main.rs control-plane/src/lib.rs && \
    cargo build --release --package nautiloop-control-plane

# Runtime image: minimal Debian with TLS certs
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    tini \
    git \
    openssh-client \
    curl \
    && rm -rf /var/lib/apt/lists/* \
    && curl -fsSL https://cli.github.com/packages/githubcli-archive-keyring.gpg \
       -o /usr/share/keyrings/githubcli-archive-keyring.gpg \
    && echo "deb [arch=$(dpkg --print-architecture) signed-by=/usr/share/keyrings/githubcli-archive-keyring.gpg] https://cli.github.com/packages stable main" \
       > /etc/apt/sources.list.d/github-cli.list \
    && apt-get update && apt-get install -y --no-install-recommends gh \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/target/release/nautiloop-server /usr/local/bin/nautiloop-server

# Run as non-root
RUN groupadd -g 1000 nautiloop && \
    useradd -u 1000 -g nautiloop -m nautiloop
USER nautiloop

ENTRYPOINT ["tini", "--", "/usr/local/bin/nautiloop-server"]
CMD ["api-server"]
