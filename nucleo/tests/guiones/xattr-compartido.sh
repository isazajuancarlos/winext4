#!/bin/sh
# SPDX-FileCopyrightText: 2026 Juan Carlos Isaza
# SPDX-License-Identifier: LicenseRef-Xiliux-Propietario
# Fabrica con el KERNEL un ext4 donde tres archivos COMPARTEN un bloque de atributos
# extendidos (el kernel deduplica atributos idénticos: h_refcount = 3).
# Uso: xattr-compartido.sh <salida.img>
set -e
SAL="$1"
command -v setfattr >/dev/null || apt-get install -y -qq attr >/dev/null 2>&1
T=$(mktemp -d)
truncate -s 32M "$T/x.img"
mkfs.ext4 -F -q -b 4096 "$T/x.img"
mkdir -p "$T/m"
mount -o loop "$T/x.img" "$T/m"
V=$(head -c 2000 /dev/zero | tr '\0' 'z')
for f in uno dos tres; do
  echo "$f" > "$T/m/$f"
  setfattr -n user.grande -v "$V" "$T/m/$f"
done
sync
umount "$T/m"
cp "$T/x.img" "$SAL"
rm -rf "$T"
