# ADR 0004: Test crypto against the host's libraries

- Status: Accepted
- Date: 2026-10-17

## Context

We write our own cryptography (ADR 0001). The Plan 9 `libsec` dates from
2002, lacks modern primitives and is not a trustworthy reference.

## Decision

The test oracles for cryptographic primitives and protocols are the host's
current libraries (OpenSSL, libsodium), plus NIST and Wycheproof vectors.
They are used only in the host-side `oracle/` harness and are never linked
into Plan R.

## Consequences

- Strong differential coverage from day one, before the kernel exists.
- The host oracle environment must be pinned and documented, so results
  are reproducible.
