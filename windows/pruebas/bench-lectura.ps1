# SPDX-FileCopyrightText: 2026 Juan Carlos Isaza
# SPDX-License-Identifier: LicenseRef-Xiliux-Propietario
# Banco de VELOCIDAD de lectura a través del montaje: lee entero un archivo de la imagen (o del
# disco) montado en W: y da MB/s. Uso: .\bench-lectura.ps1 -exe <winext4.exe> [-dispositivo X] [-ruta /x]
# Sin -dispositivo crea una imagen con un archivo de 512 MiB (escrito por debugfs, en WSL).
param([string]$exe = "D:\winext4\target\x86_64-pc-windows-gnu\release\winext4.exe", [string]$dispositivo = "", [string]$ruta = "/grande.bin", [string]$letra = "W", [int]$vueltas = 2)
$s = Join-Path $env:TEMP "winext4-bench"; New-Item -ItemType Directory -Force $s | Out-Null
if (-not $dispositivo) {
    $dispositivo = "$s\bench.img"
    if (-not (Test-Path $dispositivo)) {
        & (Join-Path (Split-Path $exe) "winext4util.exe") formatear $dispositivo --tamano 700M | Out-Null
        $wp = wsl -d Debian -e wslpath -a ($dispositivo -replace '\\', '/')
        wsl -d Debian -e sh -c "head -c 536870912 /dev/urandom > /tmp/grande.bin && PATH=/usr/sbin:/usr/bin debugfs -w -R 'write /tmp/grande.bin grande.bin' '$wp' >/dev/null 2>&1; rm /tmp/grande.bin"
    }
}
$p = Start-Process $exe -ArgumentList $dispositivo, "${letra}:\" -PassThru -WindowStyle Hidden
$fin = (Get-Date).AddSeconds(20); while (-not (Test-Path "${letra}:\") -and (Get-Date) -lt $fin) { Start-Sleep -Milliseconds 300 }
$archivo = "${letra}:" + ($ruta -replace '/', '\')
try {
    for ($v = 1; $v -le $vueltas; $v++) {
        $b = New-Object byte[] (4MB); $f = [IO.File]::Open($archivo, 'Open', 'Read', 'ReadWrite')
        $sw = [Diagnostics.Stopwatch]::StartNew(); $t = 0L
        try { while (($n = $f.Read($b, 0, $b.Length)) -gt 0) { $t += $n } } finally { $f.Close() }
        $sw.Stop()
        "vuelta {0}: {1:N0} MB en {2:N2} s = {3:N0} MB/s" -f $v, ($t / 1MB), $sw.Elapsed.TotalSeconds, ($t / 1MB / $sw.Elapsed.TotalSeconds)
    }
} finally {
    & $exe --desmontar "${letra}:\" | Out-Null
    $fin = (Get-Date).AddSeconds(20); while ((Test-Path "${letra}:\") -and (Get-Date) -lt $fin) { Start-Sleep -Milliseconds 300 }
}
