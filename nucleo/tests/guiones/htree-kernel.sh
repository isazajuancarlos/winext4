#!/bin/sh
# SPDX-FileCopyrightText: 2026 Juan Carlos Isaza
# SPDX-License-Identifier: LicenseRef-Xiliux-Propietario
# El KERNEL de Linux busca por el ÍNDICE htree cada nombre de una lista (una búsqueda por
# nombre en un directorio indexado va por dx_probe, no por un barrido lineal), y además crea
# y borra unos cuantos (el kernel escribiendo sobre el índice que dejamos nosotros).
# Uso: htree-kernel.sh <imagen> <dir-en-la-imagen> <lista-de-nombres>
# Sale != 0 si falta alguno o el kernel registra un error de ext4.
set -e
IMG="$1"; DIR="$2"; LISTA="$3"
M=$(mktemp -d)
dmesg -C 2>/dev/null || true
mount -o loop "$IMG" "$M"
faltan=0; vistos=0
while IFS= read -r n; do
  if [ -e "$M$DIR/$n" ]; then vistos=$((vistos+1)); else faltan=$((faltan+1)); echo "FALTA: $n"; fi
done < "$LISTA"
# El kernel inserta y borra en el índice que escribimos.
i=0; while [ $i -lt 200 ]; do touch "$M$DIR/kernel-$i-$(printf '%0150d' $i)"; i=$((i+1)); done
i=0; while [ $i -lt 200 ]; do rm "$M$DIR/kernel-$i-$(printf '%0150d' $i)"; i=$((i+2)); done
umount "$M"; rmdir "$M"
err=$(dmesg 2>/dev/null | grep -ci 'EXT4-fs error' || true)
echo "vistos=$vistos faltan=$faltan errores_kernel=$err"
[ "$faltan" -eq 0 ] && [ "$err" -eq 0 ] && [ "$vistos" -gt 0 ]
