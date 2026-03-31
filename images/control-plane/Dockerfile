# Control plane image for Nemo
# Produces a minimal image with the nemo-server binary.
# Same binary runs as API server or loop engine based on args.

FROM rust:1.88-bookworm@sha256:8aa70d1416cf5b1cff4b95ec6c57f1c5e4e649a3b53d616a26695cda6fbb46bc AS builder

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

RUN cargo build --release --package nemo-control-plane 2>/dev/null || true

# Copy real source and rebuild
COPY control-plane/ control-plane/
COPY cli/ cli/
RUN touch control-plane/src/main.rs control-plane/src/lib.rs && \
    cargo build --release --package nemo-control-plane

# Runtime image: minimal Debian with TLS certs
FROM debian:bookworm-slim@sha256:ee5473f786ff8a4e03409277c276b459b29afa55a84268bef55342c4f705b7ad

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

COPY --from=builder /build/target/release/nemo-server /usr/local/bin/nemo-server

# Run as non-root
RUN groupadd -g 1000 nemo && \
    useradd -u 1000 -g nemo -m nemo
USER nemo

ENTRYPOINT ["tini", "--", "/usr/local/bin/nemo-server"]
CMD ["api-server"]
