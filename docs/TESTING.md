# Testing

## Layers

| Layer | Runs on | Examples |
|---|---|---|
| Unit tests | host (`cargo test` on `no_std` crates via a host test shim) | 9P codec, namespace union logic, allocators, AML interpreter, fs format, crypto, TCP state machine, memdraw |
| Property tests & fuzzing | host | 9P decode/encode round-trip, ELF parser, ACPI/AML, IP packets, fs images, TLS records. Fuzz harness ours (coverage via `-C instrument-coverage` / sancov); libFuzzer may be used in the host zone (Q1) |
| Kernel self-tests | QEMU | In-kernel runner (`kernel/src/port/ktest.rs`, `test=1`): one `ktest:` line per test over serial, then `isa-debug-exit` / PSCI. Covers traps, fixups, kernel stack guard pages, the framebuffer console and log ring (read back), the clock tick, a self-IPI, SMP (every CPU online and ticking, cross-CPU calls, a contended spin lock) and (amd64) HPET delivery through the I/O APIC; tests a machine can't run report `skipped`. Negative tests (`test=trap`) check that a fatal trap prints its report and backtrace. `legacy-apic` reruns amd64 with `apic=xapic apictimer=oneshot`; `cons` (`test=cons`) runs `init -i`, whose `tcons` prompts on the console while the harness types its answers on QEMU's serial line, then an interactive `rc` the harness types a session to (`cargo xtask test --only cons` runs it alone); `uniprocessor` reruns both with one CPU; `log-ring` (`test=reset`) resets warm and checks that 9boot saved the first boot's log ring |
| User integration tests | QEMU | `rc` scripts under `/sys/test`, run by `init` in test mode; TAP-style output over serial |
| Oracle tests | host + QEMU | Same input to Plan R and a reference; byte-compare (below). `cargo xtask oracle` runs `oracle/rc`'s scripts on plan9port and on both arches (`test=oracle`), in CI |
| Multi-node tests | 2+ QEMU instances | `cpu`, `import`, `exportfs`, auth, TLS |
| Hardware tests | Ryzen, Orange Pi | Netbooted nightly runs with serial capture (P7+) |
| Visual tests | QEMU | Screendumps (QMP `screendump`) compared to reference PNGs with a tolerance |

## Oracles

### Crypto: the host's libraries ([ADR 0004](adr/0004-host-crypto-oracles.md))

Modern crypto is tested against **the host's** maintained libraries, not
the 2002 Plan 9 `libsec`:

- **OpenSSL** (and/or libsodium) on the development host: the reference
  for hashes, AEADs, X25519/Ed25519, P-256, RSA verify, HKDF and TLS 1.3.
- **Differential harness** (`oracle/crypto`, host zone): generates random
  inputs, runs them through `lib/sec` (built for the host) and through
  the host library (via the `openssl` CLI or a small C shim linked only
  into the oracle), and compares the outputs.
- **Known-answer vectors:** NIST CAVP/ACVP and Project Wycheproof, imported
  as data files.
- **Protocol interop:** Plan R's TLS 1.3 against host `openssl s_server`
  / `s_client`, inside the multi-node QEMU setup.
- **Constant-time checks:** dudect-style timing tests on host for
  secret-dependent primitives.

### Userland behaviour: plan9port and 9front

"Familiar day-to-day use" is checked against working implementations:

- **plan9port** on the host: reference for `rc`, `sed`, `awk`, `grep`,
  `sort`, `mk`, `tr`, `diff`, … Corpus tests run the same script on
  both and compare stdout, stderr and exit status. Known, intentional
  differences are listed in an allow-file. For `rc` this is
  `cargo xtask oracle` and `oracle/rc` (see its README); the text tools
  join it as they arrive.
- **9front or Bell Labs Plan 9 in QEMU:** reference for namespace,
  `/proc`, `/net`, `rio` and 9P server behaviour where plan9port has
  nothing comparable. The Bell Labs release here is 386-only, so in
  practice 9front amd64 is the useful live reference.
- **9P conformance:** a 9P test client run against Plan R servers and
  against plan9port/9front servers. The same suite checks both our client
  and our servers.

### Firmware/ACPI corpus

ACPI dumps from QEMU (both architectures), the Ryzen board and the Orange
Pi go into `oracle/acpi-corpus` (see its README: `cargo xtask acpidump`
for QEMU, `capture.sh` for the boards). `lib/acpi`'s corpus test parses
every set and compares the result with the set's `summary.txt`. The AML
interpreter must parse them all without errors. iASL disassembly (host
tool) is the oracle for the namespace structure.

## CI

- Every commit: `xtask deps` (dependency rule), `fmt`, `clippy`, host unit
  tests, QEMU boot + kernel self-tests on amd64 and arm64, and the
  `oracle/rc` corpus against plan9port.
- Nightly: full user integration tests, oracle suites, fuzzing (time-boxed),
  multi-node tests and, from P7 on, hardware runs.
- Every QEMU run is headless, with a hard timeout and a serial log kept as
  an artifact.
