#!/bin/sh
# SPDX-FileCopyrightText: 2026 Juan Carlos Isaza
# SPDX-License-Identifier: LicenseRef-Xiliux-Propietario
# El KERNEL lee los enlaces que creó winext4 (pruebas/enlaces.rs) y escribe encima.
# Uso: enlaces-kernel.sh <imagen> <destino-lento-esperado>
set -e
IMG="$1"; LENTO="$2"
M=$(mktemp -d)
dmesg -C 2>/dev/null || true
mount -o loop "$IMG" "$M"
mal=0
c() { if [ "$1" != "$2" ]; then echo "MAL $3: «$1» != «$2»"; mal=1; fi; }
c "$(readlink "$M/rapido")" "destino.txt" "readlink rapido"
c "$(readlink "$M/lento")" "$LENTO" "readlink lento"
c "$(readlink "$M/absoluto")" "/etc/passwd" "readlink absoluto"
c "$(cat "$M/rapido")" "contenido del destino" "cat a través de rapido"
c "$(cat "$M/a-carpeta/f")" "dentro" "cat a través de a-carpeta"
c "$(cat "$M/duro.txt")" "contenido del destino" "cat duro.txt"
c "$(stat -c %h "$M/destino.txt")" "2" "enlaces de destino.txt"
c "$(stat -c %i "$M/destino.txt")" "$(stat -c %i "$M/duro.txt")" "mismo inodo"
[ -L "$M/roto" ] || { echo "MAL roto no es symlink"; mal=1; }
# El kernel escribe A TRAVÉS del enlace duro y crea/borra enlaces sobre lo nuestro.
echo "kernel" >> "$M/duro.txt"
c "$(tail -n1 "$M/destino.txt")" "kernel" "escritura por el enlace duro"
ln -s destino.txt "$M/del-kernel"; ln "$M/destino.txt" "$M/tercero"; rm "$M/tercero" "$M/del-kernel"
umount "$M"; rmdir "$M"
err=$(dmesg 2>/dev/null | grep -ci 'EXT4-fs error' || true)
echo "mal=$mal errores_kernel=$err"
[ "$mal" -eq 0 ] && [ "$err" -eq 0 ]
