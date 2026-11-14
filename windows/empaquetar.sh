#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2026 Juan Carlos Isaza
# SPDX-License-Identifier: LicenseRef-Xiliux-Propietario
#
# Arma el paquete AUTOCONTENIDO de Windows DESDE LINUX: cross-compila winext4.exe, trae el
# DokanSetup firmado (LGPL, redistribuible) y comprime todo en un zip que el usuario instala en
# un Windows nuevo sin bajar nada mas. Uso: windows/empaquetar.sh
set -euo pipefail

raiz="$(cd "$(dirname "$0")/.." && pwd)"
dist="$raiz/dist/winext4-windows"
ver_dokan="v2.3.1.1000"
url_dokan="https://github.com/dokan-dev/dokany/releases/download/${ver_dokan}/DokanSetup.exe"

echo "==> cross-compilando winext4.exe (release)"
( cd "$raiz" && cargo build -p winext4-windows --release --target x86_64-pc-windows-gnu )

echo "==> armando $dist"
rm -rf "$dist"; mkdir -p "$dist"
cp "$raiz/target/x86_64-pc-windows-gnu/release/winext4.exe" "$dist/"
cp "$raiz/windows/instalar.ps1" "$dist/"
cp "$raiz/windows/LEEME.txt" "$dist/" 2>/dev/null || true

# Licencia LGPL de Dokan (cumplimiento: viaja con el binario), desde el clon de estudio si esta.
if [ -f /mnt/data/analisis/dokany/license.lgpl.txt ]; then
  cp /mnt/data/analisis/dokany/license.lgpl.txt "$dist/LICENCIA-Dokan-LGPL.txt"
fi

echo "==> trayendo DokanSetup firmado ($ver_dokan)"
if curl -fL --retry 2 -o "$dist/DokanSetup.exe" "$url_dokan"; then
  echo "    DokanSetup.exe: $(du -h "$dist/DokanSetup.exe" | cut -f1)"
else
  echo "    AVISO: no se pudo bajar DokanSetup.exe; el paquete queda incompleto."
  echo "    Bajalo a mano de $url_dokan y ponlo en $dist/"
fi

echo "==> comprimiendo"
( cd "$raiz/dist" && zip -qr winext4-windows.zip winext4-windows )
echo "LISTO: $raiz/dist/winext4-windows.zip"
ls -la "$dist"
