#!/bin/sh
# Copies a running Linux system's ACPI tables into a tarball for
# oracle/acpi-corpus. Run on the board as root:
#
#     cd /tmp && sudo sh capture.sh <set-name>    # e.g. opi6p, ryzen-x870e
#
# Only reads: /sys/firmware/acpi/tables (the tables as firmware handed them
# over) and /sys/class/dmi/id (board and firmware versions). Writes one
# tarball, <set-name>.tar, in the current directory, owned by the user who
# ran sudo. Run it from /tmp: on the Ryzen that is tmpfs, and nothing may
# be written to its Fedora disk or data array.
set -eu

name=${1:?usage: sudo sh capture.sh <set-name>}
case $name in
*/* | .* | "") echo "capture.sh: bad set name: $name" >&2; exit 1 ;;
esac
src=/sys/firmware/acpi/tables
[ -r "$src/DSDT" ] || { echo "capture.sh: cannot read $src (run as root?)" >&2; exit 1; }

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
mkdir "$work/$name"

# Top-level files only: dynamic/ holds tables the OS loaded at run time
# and data/ holds non-table blobs.
for t in "$src"/*; do
	[ -f "$t" ] && cat "$t" > "$work/$name/$(basename "$t").dat"
done

{
	echo "# Copied from $src by oracle/acpi-corpus/capture.sh."
	echo "captured: $(date -u +%Y-%m-%d)"
	echo "kernel: $(uname -sr)"
	for f in sys_vendor product_name board_vendor board_name bios_vendor bios_version bios_date; do
		[ -r "/sys/class/dmi/id/$f" ] && echo "$f: $(cat "/sys/class/dmi/id/$f")"
	done
} > "$work/$name/origin.txt"

tar -C "$work" -cf "$name.tar" "$name"
[ -n "${SUDO_USER:-}" ] && chown "$SUDO_USER" "$name.tar"
echo "capture.sh: wrote $name.tar ($(ls "$work/$name" | wc -l) files)"
