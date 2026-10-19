#!/bin/bash
# Builds Plan 9's own rc, from the Plan 9 tree's sys/src/cmd/rc, to run
# on this Linux host: the oracle `cargo xtask oracle` runs the rc and
# cmd corpora under.
#
#     bash scripts/build-plan9-rc.sh [out]
#
# It compiles Plan 9's sources unchanged but for two lines of plan9.c
# (lib9 makes SIGCHLD a note, which is turned off; `<{}` names /dev/fd/N,
# which Linux programs can open) with plan9port's 9c and its lib9, bio
# and libString; scripts/plan9rc has the rest: a header included first
# and shim.c, which keeps Plan 9's /env and /rc/lib/rcmain under
# $RC9ROOT. The Plan 9 tree is $PLAN9SRC, else ../plan9 beside this one;
# plan9port is $PLAN9, else ~/plan9port. out defaults to build/plan9rc.
set -euo pipefail

die() { echo "build-plan9-rc: $*" >&2; exit 1; }

here=$(cd "$(dirname "$0")/.." && pwd)
src=${PLAN9SRC:-$here/../plan9}/sys/src/cmd/rc
export PLAN9=${PLAN9:-$HOME/plan9port}
out=${1:-$here/build/plan9rc}
[ -f "$src/plan9.c" ] || die "no Plan 9 rc sources in $src (set PLAN9SRC)"
[ -x "$PLAN9/bin/9c" ] || die "no plan9port in $PLAN9 (scripts/install-plan9port.sh)"
command -v bison >/dev/null || die "bison is not installed (dnf install bison)"
# plan9port's own tools last, so that sed and the rest are the system's.
export PATH=$PATH:$PLAN9/bin

work=$out/src
rm -rf "$work"
mkdir -p "$work"
cp "$src"/*.[chy] "$here/scripts/plan9rc/pre.h" "$here/scripts/plan9rc/shim.c" "$work"
cd "$work"
sed -i 's|^\tnotify(notifyf);$|&\n\tnotifyoff("sys: child");|' plan9.c
sed -i 's|^char \*Fdprefix = "/fd/";|char *Fdprefix = "/dev/fd/";|' plan9.c
sed -i 's|#include <String.h>|#include <libString.h>|' havefork.c
grep -q 'notifyoff("sys: child")' plan9.c || die "plan9.c's Trapinit is not as expected"
grep -q 'Fdprefix = "/dev/fd/"' plan9.c || die "plan9.c's Fdprefix is not as expected"
bison -y -d syn.y 2>/dev/null
cp y.tab.h x.tab.h
for f in code exec getflags glob here io lex pcmd pfnc simple subr trap tree var havefork plan9 y.tab; do
	9c -w -fcommon -include pre.h $f.c
done
9c shim.c
9l -o "$out/rc" ./*.o -lString -lbio -l9
cp "${PLAN9SRC:-$here/../plan9}/rc/lib/rcmain" "$out/rcmain"
echo "build-plan9-rc: $out/rc"
