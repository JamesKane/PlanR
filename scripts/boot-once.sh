#!/bin/bash
# Makes the next boot, and only the next, start Plan R from its USB stick.
#
#     sudo bash boot-once.sh
#
# Run on the board under its own OS. It finds the stick by the GPT disk
# GUID `cargo xtask image` gives Plan R media (never by device path), adds
# a "Plan R" boot entry for it if there is none, and sets BootNext to that
# entry. It creates the entry with --create-only, so BootOrder and every
# other entry (Fedora's, Ubuntu's, FreeBSD's) stay as they are: the reset
# after Plan R boots the usual OS.
set -euo pipefail

die() { echo "boot-once: $*" >&2; exit 1; }

[ "$(id -u)" -eq 0 ] || die "must run as root"
command -v efibootmgr >/dev/null || die "efibootmgr is not installed"

case $(uname -m) in
x86_64) guid=4bcb9d0c-7869-4731-a911-b885abf8b21a loader='\EFI\BOOT\BOOTX64.EFI' ;;
aarch64) guid=abbd20ac-16ac-4905-b3c3-0d0c50139892 loader='\EFI\BOOT\BOOTAA64.EFI' ;;
*) die "unknown architecture $(uname -m)" ;;
esac

mapfile -t disks < <(lsblk -dnr -o NAME,PTUUID,TRAN | awk -v g="$guid" 'tolower($2) == g {print $1 " " $3}')
[ ${#disks[@]} -eq 1 ] || die "${#disks[@]} disks carry the Plan R disk GUID $guid; want exactly one (is the stick in?)"
read -r name tran <<<"${disks[0]}"
[ "$tran" = usb ] || die "/dev/$name has the Plan R GUID but is on '$tran', not USB"

# The ESP's partition GUID, as the boot entry's device path records it.
part=$(lsblk -nr -o PARTUUID "/dev/${name}1" 2>/dev/null || true)
[ -n "$part" ] || die "/dev/$name has no first partition"

# Reuse a "Plan R" entry that points at this partition; else create one.
# (Plain POSIX awk: Ubuntu's is mawk.)
entry=$(efibootmgr -v | awk -v p="$part" '/Plan R/ && index(tolower($0), tolower(p)) {sub(/^Boot/, "", $1); sub(/\*?$/, "", $1); print $1; exit}')
if [ -z "$entry" ]; then
	before=$(efibootmgr | awk '/^Boot[0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f]/ {print substr($1, 5, 4)}' | sort)
	efibootmgr --create-only --disk "/dev/$name" --part 1 --label "Plan R" --loader "$loader" >/dev/null
	after=$(efibootmgr | awk '/^Boot[0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f]/ {print substr($1, 5, 4)}' | sort)
	entry=$(comm -13 <(echo "$before") <(echo "$after") | head -1)
	[ -n "$entry" ] || die "efibootmgr did not create an entry"
	echo "boot-once: created Boot$entry \"Plan R\" for /dev/${name}1 ($part)"
fi

efibootmgr --bootnext "$entry" >/dev/null
echo "boot-once: BootNext = $entry (Plan R on /dev/$name). Reboot when ready; the reset after returns to the usual OS."
efibootmgr | grep -E '^(BootNext|BootOrder)'
