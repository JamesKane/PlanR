# rc oracle corpus

Scripts that must behave the same under Plan R's `rc` and plan9port's.
`cargo xtask oracle` (and `cargo xtask ci`) runs each `NAME.rc` on both,
and compares what it writes to standard output and error, and whether it
exits normally (plan9port's rc gives only a Unix exit code, so a status
string is compared by echoing it). Both sides' results are kept in
`build/oracle/{host,amd64,arm64}/NAME.{stdout,stderr,status}`.

Each script runs as `rc NAME.rc`, alone in an empty directory (it may
make files there), with standard input from `/dev/null`:

- on the host, with only `PLAN9` and `PATH=.:$PLAN9/bin` in the
  environment, so that `$path` is like Plan 9's `(. /bin)`. plan9port is
  found through `$PLAN9`, else `~/plan9port`
  (`scripts/install-plan9port.sh` builds it);
- on Plan R, by `toracle` (`cmd/utest/src/bin/toracle.rs`), booted with
  `test=oracle`; the scripts are in the boot archive at
  `/sys/lib/oracle/rc`.

Write scripts with the commands Plan R has (`echo`, `cat` and rc for
now; more as P3 brings them). An error ends a script, so a failing case
goes in a subshell, `@{…} || echo failed`, for the lines after it to run.
Keep out what differs by system rather than by shell: paths
(`whatis echo`, `<{}`'s file names), the system's error strings (they go
in `oserrors.rc`), and here documents inside functions (plan9port's rc
loses the rest of the script).

`allow` lists the known differences: a script, a stream (`stdout`,
`stderr` or `status`) and why. A listed difference that no longer shows
fails the run, so the list stays true; keep each one to a script that
needs it, so the rest of the script's streams are still checked.
