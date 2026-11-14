#!/bin/sh
# SPDX-FileCopyrightText: 2026 Juan Carlos Isaza
# SPDX-License-Identifier: LicenseRef-Xiliux-Propietario
# El KERNEL de Linux monta la imagen, escribe (directorios, archivos, uno grande, xattr,
# enlaces), borra algo, y desmonta limpio. Uso: kernel-escribe.sh <imagen>
set -e
IMG="$1"
command -v setfattr >/dev/null || apt-get install -y -qq attr >/dev/null 2>&1
M=$(mktemp -d)
mount -o loop "$IMG" "$M"
mkdir -p "$M/kernel/sub"
i=0
while [ $i -lt 300 ]; do echo "archivo $i del kernel" > "$M/kernel/sub/f$i"; i=$((i+1)); done
dd if=/dev/urandom of="$M/kernel/grande.bin" bs=1M count=20 2>/dev/null
setfattr -n user.prueba -v "valor" "$M/kernel/grande.bin"
ln -s grande.bin "$M/kernel/enlace"
ln "$M/kernel/sub/f1" "$M/kernel/duro"
rm "$M/kernel/sub/f2" "$M/kernel/sub/f3"
sha256sum "$M/kernel/grande.bin" | cut -d' ' -f1 > "$IMG.sha"
sync
umount "$M"
rmdir "$M"
