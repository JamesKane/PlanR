# ADR 0007: AMD64 baseline is x86-64-v2

- Status: Accepted
- Date: 2026-10-17
- Resolves: OPEN-QUESTIONS Q10

## Context

The baseline ISA decides which instructions the compiler may emit
everywhere, and which machines can run Plan R at all.

## Decision

AMD64 code is compiled for **x86-64-v2** (SSE4.2, POPCNT, CMPXCHG16B,
SSSE3). `.cargo/config.toml` sets `-C target-cpu=x86-64-v2` for the
`x86_64-unknown-uefi` and `x86_64-unknown-none` targets. Newer features
(AVX2, AVX-512, AES-NI, SHA, VAES) are detected at runtime and dispatched
explicitly, mainly in `lib/sec`.

## Consequences

- Runs on essentially every 64-bit x86 machine of the last 15 years,
  including the reference Ryzen 9 7900X and the Alder Lake build server.
- Hot paths that want AVX-class instructions need explicit runtime
  dispatch.
