# SPDX-FileCopyrightText: 2026 Juan Carlos Isaza
# SPDX-License-Identifier: LicenseRef-Xiliux-Propietario
# E2E de BORRADO en el montaje: escribe, lee y borra N archivos, y exige que TODOS desaparezcan
# al instante y sigan borrados tras desmontar (debugfs). Caza el fallo del 2026-11-05: Dokan no
# siempre repite la marca de borrado en el cleanup y hasta 25 de 25 borrados se perdían en
# silencio. Uso: .\e2e-borrado.ps1 [-b <carpeta de binarios>] [-n 40]
param([string]$b = 'D:\winext4\target\x86_64-pc-windows-gnu\release', [int]$n = 40)
$mal = 0
$s = "$env:TEMP\winext4-diag-borrado"; New-Item -ItemType Directory -Force $s | Out-Null
$img = "$s\d.img"; Remove-Item $img -ErrorAction SilentlyContinue
& $b\winext4util.exe formatear $img --tamano 64M | Out-Null
$p = Start-Process $b\winext4.exe -ArgumentList $img, 'W:\', '--escritura' -PassThru -WindowStyle Hidden -RedirectStandardError "$s\err.txt"
$fin = (Get-Date).AddSeconds(15); while (-not (Test-Path W:\) -and (Get-Date) -lt $fin) { Start-Sleep -Milliseconds 300 }
New-Item -ItemType Directory W:\c | Out-Null
$inmediato = 0; $tarde = 0; $nunca = 0; $esperas = @()
for ($i = 1; $i -le $n; $i++) {
    $f = "W:\c\f$i.txt"
    Set-Content $f "contenido $i"
    $null = Get-Content $f
    Remove-Item $f
    if (Test-Path $f) {
        $inmediato++
        $sw = [Diagnostics.Stopwatch]::StartNew()
        while ((Test-Path $f) -and $sw.Elapsed.TotalSeconds -lt 8) { Start-Sleep -Milliseconds 50 }
        if (Test-Path $f) { $nunca++ } else { $tarde++; $esperas += [int]$sw.Elapsed.TotalMilliseconds }
    }
}
"$n borrados: visibles justo después=$inmediato (desaparecieron luego=$tarde, en ms: $($esperas -join ','); nunca en 8 s=$nunca)"
$quedan = @(Get-ChildItem W:\c -Name)
"en W:\c antes de desmontar: $($quedan.Count) ($($quedan -join ', '))"
& $b\winext4.exe --desmontar W:\ | Out-Null; Start-Sleep 2
$wp = wsl -d Debian -e wslpath -a ($img -replace '\\', '/')
$k = wsl -d Debian -e env PATH=/usr/sbin:/usr/bin debugfs -R "ls /c" $wp 2>$null
"tras desmontar, debugfs ve en /c: $((($k -join ' ') -split '\s+' | Where-Object { $_ -like 'f*.txt' }) -join ', ')"
Get-Content "$s\err.txt" | Select-String 'ERROR'

if ($inmediato -gt 0) { "  MAL  $inmediato borrados seguían visibles"; $mal++ }
$quedan_disco = @((($k -join ' ') -split '\s+' | Where-Object { $_ -like 'f*.txt' }))
if ($quedan_disco.Count -gt 0) { "  MAL  $($quedan_disco.Count) archivos borrados siguen en el disco"; $mal++ }
$control = wsl -d Debian -e env PATH=/usr/sbin:/usr/bin debugfs -R "ls /" $wp 2>$null
if (-not (($control -join ' ') -match 'lost\+found')) { "  MAL  control: debugfs no listó la raíz (el negativo no vale)"; $mal++ }
"RESULTADO: $mal fallos"