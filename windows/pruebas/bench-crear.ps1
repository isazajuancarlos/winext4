# SPDX-FileCopyrightText: 2026 Juan Carlos Isaza
# SPDX-License-Identifier: LicenseRef-Xiliux-Propietario
# Banco de VELOCIDAD al CREAR muchos archivos en una carpeta (desde Windows, por el montaje), en
# tandas, para ver si el coste por archivo CRECE con el tamaño de la carpeta (cuadrático).
# Referencia: lo mismo en NTFS. Uso: .\bench-crear.ps1 [-exe <winext4.exe>] [-total 3000] [-tanda 500]
param([string]$exe = "D:\winext4\target\x86_64-pc-windows-gnu\release\winext4.exe", [int]$total = 3000, [int]$tanda = 500)
$s = Join-Path $env:TEMP "winext4-bench-crear"; New-Item -ItemType Directory -Force $s | Out-Null
function medir($dir) {
    New-Item -ItemType Directory -Force $dir | Out-Null
    for ($t0 = 0; $t0 -lt $total; $t0 += $tanda) {
        $sw = [Diagnostics.Stopwatch]::StartNew()
        for ($i = $t0; $i -lt $t0 + $tanda; $i++) { [IO.File]::WriteAllText("$dir\archivo-$i.txt", "x") }
        $sw.Stop()
        "  {0,5}..{1,5}: {2,7:N0} ms  ({3:N2} ms/archivo)" -f $t0, ($t0 + $tanda), $sw.ElapsedMilliseconds, ($sw.ElapsedMilliseconds / $tanda)
    }
    $sw = [Diagnostics.Stopwatch]::StartNew(); $n = @(Get-ChildItem $dir).Count; $sw.Stop()
    "  listar $n archivos: $($sw.ElapsedMilliseconds) ms"
}
"== NTFS (referencia)"
$ref = "$s\ntfs"; Remove-Item $ref -Recurse -Force -ErrorAction SilentlyContinue
medir $ref
Remove-Item $ref -Recurse -Force -ErrorAction SilentlyContinue
"== winext4 (imagen en W:, con escritura)"
$img = "$s\c.img"; Remove-Item $img -ErrorAction SilentlyContinue
& (Join-Path (Split-Path $exe) "winext4util.exe") formatear $img --tamano 256M | Out-Null
$p = Start-Process $exe -ArgumentList $img, 'W:\', '--escritura' -PassThru -WindowStyle Hidden
$fin = (Get-Date).AddSeconds(15); while (-not (Test-Path W:\) -and (Get-Date) -lt $fin) { Start-Sleep -Milliseconds 300 }
try { medir "W:\carpeta" } finally {
    & $exe --desmontar W:\ | Out-Null
    $fin = (Get-Date).AddSeconds(30); while ((Test-Path W:\) -and (Get-Date) -lt $fin) { Start-Sleep -Milliseconds 300 }
}
