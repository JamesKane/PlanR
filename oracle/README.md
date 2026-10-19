# Oracle corpora

Scripts that must behave on Plan R as on Plan 9. The reference is Plan
9's own source (`../plan9`); plan9port is a runnable stand-in for it
where the two agree. `cargo xtask oracle` (and `cargo xtask ci`) runs
each `CORPUS/NAME.rc` on Plan R and compares what it writes to standard
output and error, and whether it exits normally, with the reference:

- `rc/`: the shell. The reference is plan9port's `rc`.
- `cmd/`: the commands, where plan9port's and Plan 9's agree. The
  reference is plan9port, running its own commands.
- `expect/`: what plan9port cannot answer, or answers differently from
  Plan 9: the commands it lacks (`cp`, `mv`, `pwd`, `chmod`), what
  depends on Plan 9's kernel or ramfs (`ls -l`, `stat`'s names), Plan
  9's error strings, and each case where plan9port's code differs from
  Plan 9's (`print`'s `%g`, `cmp`'s and `du`'s output, …). The
  reference is written down beside each script: `NAME.out` and
  `NAME.err` (empty if missing), and `NAME.fail` if it should fail.
  Check each line against Plan 9's source before writing it down.

plan9port's rc gives only a Unix exit code, so a status string is
compared by echoing it. Both sides' results are kept in
`build/oracle/{host,amd64,arm64}/CORPUS/NAME.{stdout,stderr,status}`.

Each script runs as `rc NAME.rc`, alone in an empty directory (it may
make files there), with standard input from `/dev/null`:

- on the host, with only `PLAN9` and `PATH=.:$PLAN9/bin` in the
  environment, so that `$path` is like Plan 9's `(. /bin)` and the
  commands are plan9port's. plan9port is found through `$PLAN9`, else
  `~/plan9port` (`scripts/install-plan9port.sh` builds it);
- on Plan R, by `toracle` (`cmd/utest/src/bin/toracle.rs`), booted with
  `test=oracle`; the corpora are in the boot archive at
  `/sys/lib/oracle`.

Write scripts with the commands Plan R has. An error in rc ends a
script, so a case that makes one goes in a subshell, `@{…} || echo
failed`, for the lines after it to run; a command that fails does not
end it. Keep out of `rc/` and `cmd/` what differs by system rather than
by program:

- paths (`whatis echo`, `<{}`'s file names) and the system's error
  strings (send stderr to `/dev/null`, or put the case in `expect/`);
- the local time zone, the time now, owners, `ls -l`, qids, and the
  order of directory entries where nothing sorts them (`ls -n`, `du`
  of a directory holding more than one directory);
- the name `stat` gives `.` (Plan 9's is the directory's own name);
- here documents inside functions (plan9port's rc loses the rest of
  the script);
- a bare `=` as an argument: plan9port's rc takes it, Plan 9's is a
  syntax error, so quote it, `test a '=' b`;
- `seq -f` formats with more than one conversion (plan9port prints
  garbage for the second).

Each corpus's `allow` lists the known differences: a script, a stream
(`stdout`, `stderr` or `status`) and why. A listed difference that no
longer shows fails the run, so the list stays true; keep each one to a
script that needs it, so the rest of the script's streams are still
checked.
