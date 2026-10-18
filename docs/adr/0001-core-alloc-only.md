# ADR 0001: Rust code depends only on `core` and `alloc`

- Status: Accepted
- Date: 2026-10-17

## Context

Plan 9 was self-contained: one tree, one set of libraries, one person could
understand all of it. Modern Rust projects usually pull in hundreds of
crates, along with their churn, supply-chain risk and design choices.

## Decision

Every crate that ships in Plan R (loader, kernel, libraries, commands,
APE2) depends only on `core`, `alloc` and other in-tree crates. No `std`
inside Plan R, and no third-party crates. We write our own runtime
(`libr`), UEFI and ACPI support, network stack, crypto, TLS, compression,
graphics and fonts.

Host-only tools and quarantined foreign code are covered separately by
OPEN-QUESTIONS Q1 and Q2.

## Consequences

- Full control and auditability; the system stays Plan 9-sized and
  coherent.
- Much more to write. Crypto and TLS in particular need strong testing
  (ADR 0004) and eventually external review.
- `xtask deps` enforces the rule in CI by inspecting `cargo metadata`.
- Host unit tests need a small shim so `no_std` crates can run under
  `cargo test`.
