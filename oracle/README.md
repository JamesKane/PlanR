# Oracle corpora

Scripts that must behave on Plan R as on Plan 9. The reference is Plan
9's own source (`../plan9`). `cargo xtask oracle` (and `cargo xtask ci`)
runs each `CORPUS/NAME.rc` on Plan R and compares what it writes to
standard output and error, and whether it exits normally, with the
reference:

- `rc/`: the shell. The reference is Plan 9's own rc, built from the
  Plan 9 tree to run on the host (`scripts/build-plan9-rc.sh`; xtask
  builds it when it is missing), with Plan 9's rcmain.
- `cmd/`: the commands, where plan9port's commands do as Plan 9's do.
  The reference is the same rc running plan9port's commands.
- `expect/`: what the host cannot answer, or answers differently from
  Plan 9: the commands plan9port lacks (`cp`, `mv`, `pwd`, `chmod`),
  what depends on Plan 9's kernel or ramfs (`ls -l`, `stat`'s names, a
  file's times), Plan 9's error strings, and each case where
  plan9port's code differs from Plan 9's (`print`'s `%g`, `cmp`'s and
  `du`'s output, `touch -t`, …). The reference is written down beside
  each script: `NAME.out` and `NAME.err` (empty if missing), and
  `NAME.fail` if it should fail. Check each line against Plan 9's
  source before writing it down.

On the host a program's status is a Unix exit code, so a status string
is compared by echoing it. Both sides' results are kept in
`build/oracle/{host,amd64,arm64}/CORPUS/NAME.{stdout,stderr,status}`.

Each script runs as `rc NAME.rc`, alone in an empty directory (it may
make files there), with standard input from `/dev/null`:

- on the host, under Plan 9's rc with a fresh `/env` of its own (kept
  under `build/oracle/host/CORPUS/NAME.root`), whose `path` is `(.
  NAME.root/bin $PLAN9/bin)`: `rc` is Plan 9's, the other commands
  plan9port's. plan9port is found through `$PLAN9`, else `~/plan9port`
  (`scripts/install-plan9port.sh` builds it);
- on Plan R, by `toracle` (`cmd/utest/src/bin/toracle.rs`), booted with
  `test=oracle`; the corpora are in the boot archive at
  `/sys/lib/oracle`. The directories are in `/tmp`, a ramfs.

Write scripts with the commands Plan R has, and only those Plan 9 has
(there is no `true` or `false`: use `~ a a` and `~ a b`). An error in
rc ends a script, so a case that makes one goes in a subshell, `@{…} ||
echo failed`, or an `eval`, for the lines after it to run; a command
that fails does not end it. Keep out of `rc/` and `cmd/` what differs by
system rather than by program:

- paths (`whatis echo`, `<{}`'s file names), the system's error strings
  (send a program's stderr to `/dev/null`, or put the case in
  `expect/`), and the statuses of programs and subshells, which on Plan
  R are wait messages with pids;
- rc's own messages go to its own standard error whatever the
  redirection (as Plan 9's: only programs get redirections), so a
  builtin's error with a system error string cannot be sent away;
- the local time zone, the time now, owners, `ls -l`'s times, qids, and
  the order of directory entries where nothing sorts them (`ls -n`, `du`
  of a directory holding more than one directory);
- a file's times other than by writing it: Plan 9's ramfs, where Plan R
  runs these, ignores `wstat`'s times, so `touch` of a file that exists
  leaves it be; make files of different ages by writing them a second
  apart;
- the name `stat` gives `.` (Plan 9's is the directory's own name);
- `seq -f` formats with more than one conversion (plan9port prints
  garbage for the second).

A bare `=` as an argument is a syntax error in Plan 9's rc: quote it,
`test a '=' b`.

Each corpus's `allow` lists its known differences: a script, a stream
(`stdout`, `stderr` or `status`) and why. A listed difference that no
longer shows fails the run, so the list stays true; keep each one to a
script that needs it, so the rest of the script's streams are still
checked.
