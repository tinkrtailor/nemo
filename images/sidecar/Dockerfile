# Auth sidecar image for Nautiloop agent jobs.
#
# Built from the Rust `sidecar/` workspace member. Final image is
# `FROM scratch` with only the compiled binary and a CA certs bundle
# (the bundle is present for historical consistency with the Go image
# while the Go sources remain in tree during the phase 5 bake period;
# it is NOT consulted by the rustls TLS layer at runtime, which uses
# `webpki-roots` compiled into the binary). Delete this bundle copy in
# spec phase 6 once the Go sources are removed.
#
# NOTE: `NAUTILOOP_EXTRA_CA_BUNDLE` is test-only per SR-10. Production
# images MUST NOT set it and this Dockerfile MUST NOT reference it.

FROM rust:1.88-alpine AS builder

RUN apk add --no-cache musl-dev

WORKDIR /build

# Copy the full workspace so cargo can resolve the workspace package
# references. The build only compiles `-p nautiloop-sidecar`, so the
# other members do not contribute to the final binary.
COPY Cargo.toml Cargo.lock ./
COPY control-plane/ control-plane/
COPY cli/ cli/
COPY sidecar/ sidecar/

RUN cargo build \
    -p nautiloop-sidecar \
    --release \
    --locked

FROM scratch

# Historical CA bundle copy — see header for rationale. Delete in
# spec phase 6.
COPY --from=builder /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/
COPY --from=builder /build/target/release/nautiloop-sidecar /auth-sidecar

ENTRYPOINT ["/auth-sidecar"]
