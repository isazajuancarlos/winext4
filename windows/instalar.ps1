# SPDX-FileCopyrightText: 2026 Juan Carlos Isaza
# SPDX-License-Identifier: LicenseRef-Xiliux-Propietario
#
# Instalador de winext4 para una instalacion NUEVA de Windows. Autocontenido: en la misma
# carpeta vienen winext4.exe y DokanSetup.exe (driver firmado, LGPL) — no se descarga nada.
#
# Uso (clic derecho > Ejecutar con PowerShell, o en consola de admin):
#   powershell -ExecutionPolicy Bypass -File instalar.ps1

#Requires -RunAsAdministrator
$ErrorActionPreference = 'Stop'
$aqui = Split-Path -Parent $MyInvocation.MyCommand.Path

# 1) Driver Dokan (firmado). Solo si no esta ya instalado.
$dokanInstalado = Test-Path "$env:SystemRoot\System32\drivers\dokan2.sys"
if (-not $dokanInstalado) {
    $setup = Join-Path $aqui 'DokanSetup.exe'
    if (-not (Test-Path $setup)) { throw "Falta DokanSetup.exe junto a este script." }
    Write-Host "Instalando el driver Dokan (firmado)..."
    # DokanSetup es un instalador Inno Setup: /VERYSILENT lo corre sin interaccion.
    Start-Process -FilePath $setup -ArgumentList '/VERYSILENT','/SUPPRESSMSGBOXES','/NORESTART' -Wait
} else {
    Write-Host "Dokan ya esta instalado."
}

# 2) Copiar winext4.exe a Archivos de programa y ponerlo en el PATH de la maquina.
$destino = Join-Path $env:ProgramFiles 'winext4'
New-Item -ItemType Directory -Force -Path $destino | Out-Null
Copy-Item (Join-Path $aqui 'winext4.exe') $destino -Force
$ruta = [Environment]::GetEnvironmentVariable('Path','Machine')
if ($ruta -notlike "*$destino*") {
    [Environment]::SetEnvironmentVariable('Path', "$ruta;$destino", 'Machine')
    Write-Host "winext4 agregado al PATH (reinicia la consola para verlo)."
}

Write-Host ""
Write-Host "Listo. Para montar un disco ext4 como la unidad M: (solo lectura):"
Write-Host "   winext4 \\.\PhysicalDrive1 M:\"
Write-Host "o una imagen:  winext4 C:\ruta\disco.img M:\"
