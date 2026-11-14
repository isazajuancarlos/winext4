#!/bin/sh
# SPDX-FileCopyrightText: 2026 Juan Carlos Isaza
# SPDX-License-Identifier: LicenseRef-Xiliux-Propietario
# Fabrica una imagen ext4 con transacciones de journal escritas por E2FSPROGS (debugfs), cuyo
# contenido NO está en su sitio, con REVOCACIÓN y con sumas (journal_open -c):
#   T1: bloques 5000,5001 <- 'X' (8 KiB)
#   T2: bloque  5001      <- 'Y' (4 KiB)   y revoca el bloque 5000
# Tras reproducir: 5000 queda COMO ESTABA (revocado por una transacción posterior), 5001 = 'Y'.
# Uso: journal-debugfs.sh <salida.img>
set -e
SAL="$1"
T=$(mktemp -d)
truncate -s 64M "$SAL"
mkfs.ext4 -F -q -b 4096 "$SAL"
head -c 8192 /dev/zero | tr '\0' 'X' > "$T/x"
head -c 4096 /dev/zero | tr '\0' 'Y' > "$T/y"
debugfs -w -f - "$SAL" >/dev/null 2>&1 <<EOF
jo -c
jw -b 5000,5001 $T/x
jw -b 5001 $T/y
jw -r 5000
jc
EOF
rm -rf "$T"
dumpe2fs -h "$SAL" 2>/dev/null | grep -E 'Filesystem features|Journal features'
debugfs -R logdump "$SAL" 2>/dev/null | head -20
