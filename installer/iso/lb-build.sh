#!/usr/bin/env bash
# Runs inside the privileged debian:trixie container started by build.sh.
set -euo pipefail

. /iso/snapshot-apt.sh
snapshot=$(cat /iso/snapshot)
main=http://snapshot.debian.org/archive/debian/$snapshot/
security=http://snapshot.debian.org/archive/debian-security/$snapshot/

apt-get update -qq
apt-get install -y -qq live-build debian-archive-keyring ca-certificates >/dev/null

rm -rf /work/lb
mkdir -p /work/lb
cd /work/lb

lb config \
	--mode debian \
	--distribution trixie \
	--architectures amd64 \
	--archive-areas "main contrib non-free-firmware" \
	--parent-archive-areas "main contrib non-free-firmware" \
	--mirror-bootstrap "$main" \
	--mirror-chroot "$main" \
	--mirror-chroot-security "$security" \
	--parent-mirror-bootstrap "$main" \
	--parent-mirror-chroot "$main" \
	--parent-mirror-chroot-security "$security" \
	--apt-options "--yes -o Acquire::Check-Valid-Until=false" \
	--binary-image iso-hybrid \
	--bootloaders "grub-efi syslinux" \
	--uefi-secure-boot enable \
	--debian-installer none \
	--memtest none \
	--apt-recommends false \
	--firmware-chroot true \
	--firmware-binary false \
	--linux-flavours amd64 \
	--security true \
	--updates true \
	--iso-volume FORMALSPEC \
	--iso-application "FormalSpec installer" \
	--iso-publisher "FormalSpec" \
	--iso-preparer "FormalSpec installer build" \
	--image-name formalspec-installer \
	--bootappend-live "boot=live components username=root hostname=formalspec locales=en_US.UTF-8 keyboard-layouts=us"

cp -R /work/config/. config/

lb build

mv /work/lb/formalspec-installer-amd64.hybrid.iso /work/formalspec-installer.iso
chown "$OUT_UID:$OUT_GID" /work/formalspec-installer.iso
