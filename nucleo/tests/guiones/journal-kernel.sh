#!/bin/sh
# SPDX-FileCopyrightText: 2026 Juan Carlos Isaza
# SPDX-License-Identifier: LicenseRef-Xiliux-Propietario
# Fabrica una imagen ext4 con TRANSACCIONES DE JOURNAL ESCRITAS POR EL KERNEL y sin aplicar:
# monta, escribe, y apaga a la fuerza (EXT4_IOC_SHUTDOWN con -f = vaciar el journal pero no
# hacer checkpoint), como un corte de luz. Uso: journal-kernel.sh <salida.img>
set -e
SAL="$1"
command -v xfs_io >/dev/null || apt-get install -y -qq xfsprogs >/dev/null 2>&1
T=$(mktemp -d)
truncate -s 64M "$T/j.img"
mkfs.ext4 -F -q -b 4096 "$T/j.img"
mkdir -p "$T/m"
mount -o loop "$T/j.img" "$T/m"
mkdir "$T/m/d"
i=0
while [ $i -lt 40 ]; do echo "hola $i" > "$T/m/d/f$i"; i=$((i+1)); done
sync
echo "escrito tras el sync" > "$T/m/tras-sync"
mkdir "$T/m/nuevo"
i=0
while [ $i -lt 200 ]; do echo "x$i" > "$T/m/nuevo/archivo-con-nombre-$i"; i=$((i+1)); done
rm "$T/m/d/f3" "$T/m/d/f7"
dd if=/dev/urandom of="$T/m/d/grande" bs=64k count=16 2>/dev/null
xfs_io -x -c 'shutdown -f' "$T/m"
umount "$T/m"
cp "$T/j.img" "$SAL"
rm -rf "$T"
dumpe2fs -h "$SAL" 2>/dev/null | grep -E 'Filesystem features|Journal features|Filesystem state'
