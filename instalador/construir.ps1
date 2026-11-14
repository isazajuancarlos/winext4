# SPDX-FileCopyrightText: 2026 Juan Carlos Isaza
# SPDX-License-Identifier: LicenseRef-Xiliux-Propietario
# Construye dist\winext4.msi y dist\winext4-setup.exe (WiX 3.14, MS-RL, sin cuota).
# Uso: pwsh instalador\construir.ps1 [-Version 0.1.0]
param([string]$Version = '0.1.0')
$ErrorActionPreference = 'Stop'
$raiz = Split-Path $PSScriptRoot -Parent
$ins = $PSScriptRoot
$dist = Join-Path $raiz 'dist'
$obj = Join-Path $ins 'obj'
New-Item -ItemType Directory -Force $dist, $obj | Out-Null

# 1. Binarios (release, toolchain gnu: la import-lib de Dokan es formato GNU).
$env:CARGO_TARGET_DIR = Join-Path $raiz 'target-instalador'
cargo +stable-x86_64-pc-windows-gnu build --release --target x86_64-pc-windows-gnu --manifest-path (Join-Path $raiz 'Cargo.toml') -p winext4-windows -p winext4-util -p winext4-servicio
if ($LASTEXITCODE -ne 0) { throw "cargo build falló" }
$bin = Join-Path $env:CARGO_TARGET_DIR 'x86_64-pc-windows-gnu\release'
Remove-Item Env:CARGO_TARGET_DIR

# 2. Dokan: el MSI oficial, anclado a su SHA-256 (si cambia, no se construye: D31).
$dokan = Join-Path $ins 'vendido\Dokan_x64.msi'
$esperado = '69FF8CB37BFEC3A75921C85FFD1C6370B50A9EC4ECEF2CF3A009D488DCBF5465'
if (-not (Test-Path $dokan)) {
  gh release download v2.3.1.1000 -R dokan-dev/dokany -p Dokan_x64.msi -D (Split-Path $dokan)
}
$h = (Get-FileHash $dokan -Algorithm SHA256).Hash
if ($h -ne $esperado) { throw "Dokan_x64.msi no coincide con el SHA-256 anclado ($h)" }
if ((Get-AuthenticodeSignature $dokan).Status -ne 'Valid') { throw "Dokan_x64.msi sin firma válida" }
$lic = Join-Path $ins 'vendido\LICENCIA-Dokan-LGPL.txt'
if (-not (Test-Path $lic)) { Copy-Item 'C:\Program Files\Dokan\Dokan Library-2.3.1\license.lgpl.txt' $lic -ErrorAction SilentlyContinue }
if (-not (Test-Path $lic)) { Copy-Item (Join-Path $raiz 'dist\winext4-windows\LICENCIA-Dokan-LGPL.txt') $lic }

# 3. MSI del producto.
Push-Location $ins
try {
  candle -nologo -arch x64 "-dVersion=$Version" "-dBin=$bin" -out "$obj\winext4.wixobj" winext4.wxs
  if ($LASTEXITCODE -ne 0) { throw "candle (msi) falló" }
  # ICE38/43/57: en instalación POR MÁQUINA (ALLUSERS=1) el menú Inicio es el de todos los
  # usuarios, pero esas ICE lo evalúan como perfil de usuario: falso positivo conocido de WiX 3.
  light -nologo -sice:ICE61 -sice:ICE38 -sice:ICE43 -sice:ICE57 -out "$dist\winext4.msi" "$obj\winext4.wixobj" -cultures:es-es
  if ($LASTEXITCODE -ne 0) { throw "light (msi) falló" }
  # 4. Ejecutable único (Burn): Dokan + winext4.
  candle -nologo -arch x64 -ext WixBalExtension "-dVersion=$Version" "-dMsi=$dist\winext4.msi" -out "$obj\bundle.wixobj" bundle.wxs
  if ($LASTEXITCODE -ne 0) { throw "candle (bundle) falló" }
  light -nologo -ext WixBalExtension -out "$dist\winext4-setup.exe" "$obj\bundle.wixobj"
  if ($LASTEXITCODE -ne 0) { throw "light (bundle) falló" }
} finally { Pop-Location }
Get-ChildItem $dist -Filter 'winext4*' | Where-Object { $_.Extension -in '.msi', '.exe' } | Select-Object Name, Length
