#!/usr/bin/env python3
"""Regenerate SSH key fixtures for the parity harness.

**TEST-ONLY.** Generates Ed25519 key pairs in canonical OpenSSH format
(70-char Base64 wrapping, matching what `ssh-keygen` emits) so that
russh's `PrivateKey::from_openssh` parses them cleanly.

Run this from `sidecar/tests/parity/` only when rotating the fixture
set. After running, commit the resulting binary key files.

Dependencies:

    pip install cryptography

The test CA + mock service TLS certs are regenerated via
`test-ca/regenerate-test-ca.sh` instead — this script only touches
SSH keys and the two `known_hosts` files.

Why we build the OpenSSH format by hand instead of using
`cryptography.serialization.PrivateFormat.OpenSSH`:

The `cryptography` library wraps Base64 at 76 chars which the
`ssh-key` crate (russh's key parser) rejects as `InvalidEncoding`.
OpenSSH canonical format wraps at 70 chars. We therefore implement
PROTOCOL.key framing ourselves per openssh-portable:
    <https://github.com/openssh/openssh-portable/blob/master/PROTOCOL.key>

Spec references: SR-2, FR-9 of specs/sidecar-parity-harness.md.
"""
import base64
import os
import struct
import sys

from cryptography.hazmat.primitives import serialization
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey


def _lp(b: bytes) -> bytes:
    """Length-prefix a byte string per SSH wire format (big-endian u32)."""
    return struct.pack(">I", len(b)) + b


def canonical_openssh_private_key(priv: Ed25519PrivateKey, comment: str) -> bytes:
    """Serialize an Ed25519 key as a canonical OpenSSH PEM blob.

    Canonical means 70-char Base64 wrap and the exact PROTOCOL.key
    framing that openssh-portable emits. This is what the ssh-key
    crate (used by russh) expects.
    """
    raw_private = priv.private_bytes(
        encoding=serialization.Encoding.Raw,
        format=serialization.PrivateFormat.Raw,
        encryption_algorithm=serialization.NoEncryption(),
    )
    raw_public = priv.public_key().public_bytes(
        encoding=serialization.Encoding.Raw,
        format=serialization.PublicFormat.Raw,
    )
    pubkey_blob = _lp(b"ssh-ed25519") + _lp(raw_public)
    checkint = os.urandom(4)
    inner = (
        checkint
        + checkint
        + _lp(b"ssh-ed25519")
        + _lp(raw_public)
        + _lp(raw_private + raw_public)
        + _lp(comment.encode())
    )
    # Pad to multiple of 8 (block size for 'none' cipher).
    pad = (-len(inner)) % 8
    for i in range(pad):
        inner += bytes([i + 1])
    body = (
        b"openssh-key-v1\x00"
        + _lp(b"none")
        + _lp(b"none")
        + _lp(b"")
        + struct.pack(">I", 1)
        + _lp(pubkey_blob)
        + _lp(inner)
    )
    b64 = base64.b64encode(body).decode()
    lines = [b64[i : i + 70] for i in range(0, len(b64), 70)]
    return (
        "-----BEGIN OPENSSH PRIVATE KEY-----\n"
        + "\n".join(lines)
        + "\n-----END OPENSSH PRIVATE KEY-----\n"
    ).encode()


def canonical_openssh_public_key(priv: Ed25519PrivateKey, comment: str) -> bytes:
    raw_public = priv.public_key().public_bytes(
        encoding=serialization.Encoding.Raw,
        format=serialization.PublicFormat.Raw,
    )
    blob = _lp(b"ssh-ed25519") + _lp(raw_public)
    b64 = base64.b64encode(blob).decode()
    return f"ssh-ed25519 {b64} {comment}\n".encode()


def write_priv_pub(priv_bytes: bytes, pub_bytes: bytes, priv_path: str, pub_path: str):
    os.makedirs(os.path.dirname(priv_path), exist_ok=True)
    with open(priv_path, "wb") as f:
        f.write(priv_bytes)
    os.chmod(priv_path, 0o600)
    with open(pub_path, "wb") as f:
        f.write(pub_bytes)
    os.chmod(pub_path, 0o644)
    print(f"  wrote {priv_path} / {pub_path}")


def main():
    # Argument: parity harness root (parent of fixtures/). Defaults to
    # the directory this script lives in.
    here = os.path.dirname(os.path.abspath(__file__))
    fixtures = here  # this script lives inside fixtures/
    if len(sys.argv) > 1:
        fixtures = os.path.join(sys.argv[1], "fixtures")

    # 1. Mock-github-ssh host key.
    host_key = Ed25519PrivateKey.generate()
    host_priv_bytes = canonical_openssh_private_key(
        host_key, "nautiloop-parity-harness-mock-github-ssh"
    )
    host_pub_bytes = canonical_openssh_public_key(
        host_key, "nautiloop-parity-harness-mock-github-ssh"
    )
    write_priv_pub(
        host_priv_bytes,
        host_pub_bytes,
        os.path.join(fixtures, "mock-github-ssh/host_key"),
        os.path.join(fixtures, "mock-github-ssh/host_key.pub"),
    )

    # 2. Harness client key — SAME bytes in both go-secrets and
    #    rust-secrets. The harness tests enforce byte-identity.
    client_key = Ed25519PrivateKey.generate()
    priv_bytes = canonical_openssh_private_key(
        client_key, "nautiloop-parity-harness-client"
    )
    pub_bytes = canonical_openssh_public_key(
        client_key, "nautiloop-parity-harness-client"
    )
    for subdir in ("go-secrets", "rust-secrets"):
        write_priv_pub(
            priv_bytes,
            pub_bytes,
            os.path.join(fixtures, subdir, "ssh-key/id_ed25519"),
            os.path.join(fixtures, subdir, "ssh-key/id_ed25519.pub"),
        )

    # 3. Authorized keys — the mock's public trust store trusts the
    #    harness client key.
    ak_path = os.path.join(fixtures, "mock-github-ssh/authorized_keys")
    with open(ak_path, "wb") as f:
        f.write(pub_bytes)
    os.chmod(ak_path, 0o644)
    print(f"  wrote {ak_path}")

    # 4. known_hosts — trusts mock-github-ssh under all hostnames the
    #    sidecars reach it by (see docker-compose extra_hosts).
    host_pub_line = host_pub_bytes.decode().strip().rsplit(" ", 1)[0]
    known_hosts = (
        f"github.com {host_pub_line}\n"
        f"100.64.0.12 {host_pub_line}\n"
    )
    for subdir in ("go-secrets", "rust-secrets"):
        kh_path = os.path.join(fixtures, subdir, "ssh-known-hosts/known_hosts")
        os.makedirs(os.path.dirname(kh_path), exist_ok=True)
        with open(kh_path, "w") as f:
            f.write(known_hosts)
        os.chmod(kh_path, 0o644)
        print(f"  wrote {kh_path}")


if __name__ == "__main__":
    main()
