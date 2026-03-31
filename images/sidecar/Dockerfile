# Auth sidecar image for Nemo agent jobs
# FR-14: Single static binary (~10 MB)
# NFR-2: Under 15 MB

FROM golang:1.22-alpine@sha256:7030196c59858ca8942e85aa2f68599240d58282ee35eb795609738a1e242c1f AS builder

WORKDIR /build
COPY go.mod go.sum* ./
RUN go mod download 2>/dev/null || true
COPY . .
RUN CGO_ENABLED=0 GOOS=linux go build -ldflags="-s -w" -o /auth-sidecar .

FROM scratch

COPY --from=builder /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/
COPY --from=builder /auth-sidecar /auth-sidecar

# Run as nobody (UID 65534) to avoid UID-based iptables rules
USER 65534

ENTRYPOINT ["/auth-sidecar"]
