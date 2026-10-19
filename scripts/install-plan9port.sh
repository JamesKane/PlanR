#!/bin/bash
# Builds plan9port, the oracle for Plan R's userland tests (rc, sed, awk,
# grep, sort, mk, diff, …), into a directory of your own.
#
#     bash scripts/install-plan9port.sh [dir]
#
# Run as yourself, not root: it writes only under dir (default
# $PLAN9, else ~/plan9port). plan9port records where it was built, so the
# tree must stay there. It checks out a fixed revision so every developer
# compares against the same tools; set PLAN9PORT_REV to try another.
# `cargo xtask oracle` finds the tree through $PLAN9, else ~/plan9port.
#
# The build needs a C compiler and the X11 and font headers. If any are
# missing it prints the dnf command to install them (that part needs root)
# and stops.
set -euo pipefail

die() { echo "install-plan9port: $*" >&2; exit 1; }

url=https://github.com/9fans/plan9port
rev=${PLAN9PORT_REV:-b6564bd96ca189c69e28797738dad56f91eb5967}
dir=${1:-${PLAN9:-$HOME/plan9port}}

[ "$(id -u)" -ne 0 ] || die "run as yourself, not root"
command -v git >/dev/null || die "git is not installed"

# Headers the build needs, by the Fedora package that has them.
missing=()
command -v gcc >/dev/null || missing+=(gcc)
for want in X11/Xlib.h:libX11-devel X11/Intrinsic.h:libXt-devel X11/extensions/Xext.h:libXext-devel \
	fontconfig/fontconfig.h:fontconfig-devel freetype2/ft2build.h:freetype-devel; do
	[ -e "/usr/include/${want%%:*}" ] || missing+=("${want#*:}")
done
if [ ${#missing[@]} -gt 0 ]; then
	echo "install-plan9port: missing build dependencies; install them with:" >&2
	echo "    sudo dnf install ${missing[*]}" >&2
	exit 1
fi

if [ -e "$dir" ]; then
	[ -d "$dir/.git" ] && git -C "$dir" remote get-url origin 2>/dev/null | grep -q '9fans/plan9port' ||
		die "$dir exists and is not a plan9port checkout; name another directory"
	git -C "$dir" fetch --quiet origin
else
	git clone --quiet "$url" "$dir"
fi
git -C "$dir" checkout --quiet "$rev"

cd "$dir"
./INSTALL >install.log 2>&1 || die "the build failed; see $dir/install.log"

for tool in rc sed awk grep sort mk diff; do
	[ -x "$dir/bin/$tool" ] || die "the build left no $dir/bin/$tool; see $dir/install.log"
done
echo "install-plan9port: built $rev in $dir"
echo "install-plan9port: add 'export PLAN9=$dir' to your shell profile if it is not ~/plan9port"
