# SPDX-FileCopyrightText: 2026 Juan Carlos Isaza
# SPDX-License-Identifier: LicenseRef-Xiliux-Propietario
# E2E: un PÁNICO del motor con el candado tomado (como el del 2026-11-06 en Z:) no puede dejar el
# volumen inservible hasta reiniciar. El montaje tiene que DESMONTARSE SOLO (sin marcar limpio),
# y al volver a montar el journal se recupera y lo confirmado antes sigue ahí.
# Usa el binario de DEPURACIÓN: solo él trae el disparador (crear «__winext4_panico_de_prueba__»).
# Uso: .\e2e-panico.ps1 [-dbg <carpeta debug>] [-rel <carpeta release>]
param([string]$dbg = 'D:\winext4\target\x86_64-pc-windows-gnu\debug', [string]$rel = 'D:\winext4\target\x86_64-pc-windows-gnu\release')
$mal = 0
function ok($c, $que) { if ($c) { "  ok   $que" } else { "  MAL  $que"; $script:mal++ } }
$s = "$env:TEMP\winext4-e2e-panico"; New-Item -ItemType Directory -Force $s | Out-Null
$img = "$s\p.img"; Remove-Item $img -ErrorAction SilentlyContinue
& "$rel\winext4util.exe" formatear $img --tamano 128M | Out-Null
$p = Start-Process "$dbg\winext4.exe" -ArgumentList $img, 'W:\', '--escritura' -PassThru -WindowStyle Hidden -RedirectStandardError "$s\m1.err"
$fin = (Get-Date).AddSeconds(30); while (-not (Test-Path W:\) -and (Get-Date) -lt $fin) { Start-Sleep -Milliseconds 200 }
ok (Test-Path W:\) "montado W: (binario de depuración)"
New-Item -ItemType Directory W:\antes | Out-Null
1..20 | ForEach-Object { Set-Content "W:\antes\f$_.txt" "antes $_" }
Start-Sleep 3   # el agrupado confirma (ventana de 1 s)
try { Set-Content 'W:\__winext4_panico_de_prueba__' 'x' -ErrorAction Stop; $crear = 'sin error' } catch { $crear = 'error' }
ok ($crear -eq 'error') "crear el archivo disparador falla (pánico contestado como error)"
# La siguiente operación encuentra el motor envenenado → desmontaje solo (tope 30 s, D39).
$null = Test-Path W:\antes\f1.txt
$fin = (Get-Date).AddSeconds(30); while (((Test-Path W:\) -or -not $p.HasExited) -and (Get-Date) -lt $fin) { Start-Sleep -Milliseconds 300; $null = Test-Path W:\antes }
ok (-not (Test-Path W:\)) "W: se desmontó solo"
ok ($p.HasExited) "el proceso terminó (código $(if ($p.HasExited) { $p.ExitCode } else { 'vivo' }))"
if ($p.HasExited) { ok ($p.ExitCode -eq 2) "sale con 2: volumen NO marcado limpio" } else { Stop-Process -Id $p.Id -Force }
$err1 = Get-Content "$s\m1.err" -Raw -Encoding utf8
ok ($err1 -match 'pánico de PRUEBA' -and $err1 -match 'envenenado') "el registro dice el pánico y el desmontaje por envenenamiento"
# Volver a montar (release): el journal se recupera y lo de antes está.
$p2 = Start-Process "$rel\winext4.exe" -ArgumentList $img, 'W:\', '--escritura' -PassThru -WindowStyle Hidden -RedirectStandardError "$s\m2.err"
$fin = (Get-Date).AddSeconds(30); while (-not (Test-Path W:\) -and (Get-Date) -lt $fin) { Start-Sleep -Milliseconds 200 }
ok (Test-Path W:\) "vuelve a montar"
ok ((Get-Content W:\antes\f20.txt -ErrorAction SilentlyContinue) -eq 'antes 20') "lo confirmado antes del pánico sigue ahí"
ok (-not (Test-Path 'W:\__winext4_panico_de_prueba__')) "el archivo del pánico no existe (su transacción no se confirmó)"
& "$rel\winext4.exe" --desmontar 'W:\' | Out-Null; $p2.WaitForExit(30000) | Out-Null
$v = & "$rel\winext4util.exe" verificar $img 2>&1; $vc = $LASTEXITCODE
ok ($vc -eq 0) "verificar tras todo: 0 errores (exit $vc)"
"RESULTADO: $mal fallos"
