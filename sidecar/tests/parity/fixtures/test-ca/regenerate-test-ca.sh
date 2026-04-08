#!/usr/bin/env bash
# Regenerate the parity harness test CA + mock service TLS certs.
#
# TEST-ONLY. Run from within sidecar/tests/parity/fixtures/test-ca/.
# Commit the resulting ca.pem, ca.key, and mock cert/key files.
#
# Spec reference: SR-7, SR-8 of specs/sidecar-parity-harness.md.

set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
FIXTURES="$(cd "$HERE/.." && pwd)"

echo "==> regenerating test CA in $HERE"

# 1. Root CA (SR-7 exact invocation)
openssl req -x509 -newkey rsa:2048 -keyout "$HERE/ca.key" -out "$HERE/ca.pem" \
    -sha256 -days 3650 -nodes \
    -subj "/CN=Nautiloop Parity Harness Test CA" \
    -addext "basicConstraints=critical,CA:TRUE"

# 2. mock-openai cert with SAN = api.openai.com
echo "==> signing mock-openai cert"
pushd "$FIXTURES/mock-openai" > /dev/null
openssl req -newkey rsa:2048 -keyout key.pem -out csr.pem -nodes \
    -sha256 \
    -subj "/CN=api.openai.com" \
    -addext "subjectAltName=DNS:api.openai.com"
openssl x509 -req -in csr.pem -CA "$HERE/ca.pem" -CAkey "$HERE/ca.key" \
    -CAcreateserial -out cert.pem -days 3650 -sha256 \
    -copy_extensions copy
rm -f csr.pem
popd > /dev/null

# 3. mock-anthropic cert with SAN = api.anthropic.com
echo "==> signing mock-anthropic cert"
pushd "$FIXTURES/mock-anthropic" > /dev/null
openssl req -newkey rsa:2048 -keyout key.pem -out csr.pem -nodes \
    -sha256 \
    -subj "/CN=api.anthropic.com" \
    -addext "subjectAltName=DNS:api.anthropic.com"
openssl x509 -req -in csr.pem -CA "$HERE/ca.pem" -CAkey "$HERE/ca.key" \
    -CAcreateserial -out cert.pem -days 3650 -sha256 \
    -copy_extensions copy
rm -f csr.pem
popd > /dev/null

# 4. Clean up CA serial file (regenerated automatically).
rm -f "$HERE/ca.srl"

echo "==> done. Commit the new ca.pem, ca.key, and mock cert/key files."
