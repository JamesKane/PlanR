#!/bin/bash
# Writes a Plan R disk image to the designated USB boot medium.
#
#     sudo scripts/write-medium.sh build/arm64/planr.img <serial>
#
# The storage safety rule (docs/HARDWARE.md): this refuses every disk
# except one whose serial number is listed in scripts/planr-media.conf. It
# never takes a device path. It also refuses unless the disk is on USB,
# has nothing mounted, and nothing (RAID, LVM, swap) holds it.
set -euo pipefail

die() { echo "write-medium: $*" >&2; exit 1; }

[ $# -eq 2 ] || die "usage: sudo $0 <image> <serial>"
image=$1 serial=$2
[ "$(id -u)" -eq 0 ] || die "must run as root"
[ -f "$image" ] || die "no image $image"

conf="$(dirname "$0")/planr-media.conf"
# Designated serials: the first word of each non-comment line.
grep -v '^\s*#' "$conf" | awk '{print $1}' | grep -qxF -- "$serial" ||
	die "serial $serial is not designated in $conf"

# Exactly one whole disk with that serial.
mapfile -t matches < <(lsblk -dnr -o NAME,SERIAL,TYPE | awk -v s="$serial" '$2 == s && $3 == "disk" {print $1}')
[ ${#matches[@]} -eq 1 ] || die "${#matches[@]} disks have serial $serial; want exactly one"
name=${matches[0]}
dev=/dev/$name

tran=$(lsblk -dnr -o TRAN "$dev")
[ "$tran" = usb ] || die "$dev ($serial) is on '$tran', not USB"
if lsblk -nr -o MOUNTPOINTS "$dev" | grep -q .; then
	die "$dev has mounted filesystems; unmount them first"
fi
for h in /sys/block/"$name"/holders/* /sys/block/"$name"/"$name"*/holders/*; do
	[ -e "$h" ] && die "$dev is in use by $(basename "$h") (RAID, LVM or similar)"
done
if swapon --show=NAME --noheadings | grep -q "^$dev"; then
	die "$dev holds active swap"
fi

size=$(stat -c %s "$image")
disk=$(blockdev --getsize64 "$dev")
[ "$size" -le "$disk" ] || die "image ($size bytes) is larger than $dev ($disk bytes)"

echo "write-medium: $dev: $(lsblk -dnr -o VENDOR,MODEL,SIZE "$dev" | tr -s ' ') serial $serial"
echo "write-medium: writing $image ($size bytes)"
dd if="$image" of="$dev" bs=4M conv=fsync status=progress
sync
cmp -n "$size" "$image" "$dev" || die "read-back of $dev differs from $image"
blockdev --rereadpt "$dev" 2>/dev/null || true
echo "write-medium: wrote and verified $dev"
