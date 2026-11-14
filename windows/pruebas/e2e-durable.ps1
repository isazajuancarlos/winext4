# SPDX-FileCopyrightText: 2026 Juan Carlos Isaza
# SPDX-License-Identifier: LicenseRef-Xiliux-Propietario
# E2E de DURABILIDAD con agrupado de transacciones: se escribe por el montaje y se MATA el
# proceso de golpe (como un cuelgue o un apagón, sin desmontar). Tras recuperar el journal:
#   (1) lo escrito con FlushFileBuffers (Flush(true)) está;
#   (2) lo escrito sin pedirlo, pasada la ventana (1 s), también (lo confirma el temporizador);
#   (3) e2fsck limpio siempre.
# Uso: .\e2e-durable.ps1 [-bin <carpeta>]. W: debe estar libre.
param([string]$bin = "D:\winext4\target\x86_64-pc-windows-gnu\release")
$ErrorActionPreference = "Continue"
$s = Join-Path $env:TEMP "winext4-e2e-durable"; New-Item -ItemType Directory -Force $s | Out-Null
$U = "$bin\winext4util.exe"; $W = "$bin\winext4.exe"
$mal = 0
function ok($cond, $que) { if ($cond) { "  ok   $que" } else { "  MAL  $que"; $script:mal++ } }
function caso($nombre, [scriptblock]$escribir, $archivo, $esperado) {
    "== $nombre"
    $img = "$s\$nombre.img"; Remove-Item $img -ErrorAction SilentlyContinue
    & $U formatear $img --tamano 32M | Out-Null
    $p = Start-Process $W -ArgumentList $img, 'W:\', '--escritura' -PassThru -WindowStyle Hidden
    $fin = (Get-Date).AddSeconds(15); while (-not (Test-Path W:\) -and (Get-Date) -lt $fin) { Start-Sleep -Milliseconds 200 }
    & $escribir
    Stop-Process -Id $p.Id -Force   # «se va la luz»: sin desmontar, sin cerrar_escritura
    $p.WaitForExit(10000) | Out-Null
    $fin = (Get-Date).AddSeconds(15); while ((Test-Path W:\) -and (Get-Date) -lt $fin) { Start-Sleep -Milliseconds 200 }
    $wp = wsl -d Debian -e wslpath -a ($img -replace '\\', '/')
    $antes = wsl -d Debian -e env PATH=/usr/sbin:/usr/bin dumpe2fs -h $wp 2>$null | Select-String 'needs_recovery'
    ok ($null -ne $antes) "control: tras matar el proceso el volumen pide recuperación (no se cerró limpio)"
    $r = & $U recuperar $img 2>&1 | Select-Object -Last 1
    "  $r"
    $f = wsl -d Debian -e env PATH=/usr/sbin:/usr/bin e2fsck -fn $wp 2>&1; $c = $LASTEXITCODE
    ok ($c -eq 0) "e2fsck limpio tras recuperar (exit $c)"
    $tmp = "$s\leido.bin"; Remove-Item $tmp -ErrorAction SilentlyContinue
    wsl -d Debian -e env PATH=/usr/sbin:/usr/bin debugfs -R "dump $archivo $(wsl -d Debian -e wslpath -a ($tmp -replace '\\','/'))" $wp 2>$null | Out-Null
    $leido = if (Test-Path $tmp) { [IO.File]::ReadAllText($tmp) } else { $null }
    ok ($leido -eq $esperado) "$archivo está con su contenido tras el «apagón» ($([string]$leido))"
}
caso "flush" { $fs = [IO.File]::Create('W:\con-flush.txt'); $b = [Text.Encoding]::ASCII.GetBytes('guardado con flush'); $fs.Write($b, 0, $b.Length); $fs.Flush($true); $fs.Close() } '/con-flush.txt' 'guardado con flush'
caso "ventana" { [IO.File]::WriteAllText('W:\sin-flush.txt', 'confirmado por el temporizador'); Start-Sleep -Seconds 2 } '/sin-flush.txt' 'confirmado por el temporizador'
"RESULTADO: $mal fallos"
