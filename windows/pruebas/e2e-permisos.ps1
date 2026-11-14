# SPDX-FileCopyrightText: 2026 Juan Carlos Isaza
# SPDX-License-Identifier: LicenseRef-Xiliux-Propietario
# E2E de PERMISOS en el montaje real de Windows (D28): el atributo «solo lectura» de Windows
# ⇄ los bits de escritura de ext4 (como Samba: +r = a-w, -r = u+w). Comprobado con `stat` del
# KERNEL (WSL Debian) y e2fsck. Uso: .\e2e-permisos.ps1 [-bin <carpeta>]. W: debe estar libre.
param([string]$bin = "D:\winext4\target\x86_64-pc-windows-gnu\release")
$ErrorActionPreference = "Continue"
$s = Join-Path $env:TEMP "winext4-e2e-permisos"; New-Item -ItemType Directory -Force $s | Out-Null
$img = "$s\permisos.img"
$U = "$bin\winext4util.exe"; $W = "$bin\winext4.exe"
$mal = 0
function ok($cond, $que) { if ($cond) { "  ok   $que" } else { "  MAL  $que"; $script:mal++ } }
function ro($p) { ((Get-Item $p).Attributes -band [IO.FileAttributes]::ReadOnly) -ne 0 }
Remove-Item $img -ErrorAction SilentlyContinue
& $U formatear $img --tamano 32M | Out-Null
$wp = wsl -d Debian -e wslpath -a ($img -replace '\\','/')
function kernel($guion) { wsl -d Debian -u root -e sh -c "M=`$(mktemp -d); mount -o loop '$wp' `$M && cd `$M && $guion; cd /; umount `$M; rmdir `$M" }
kernel "echo a > ro444 && chmod 0444 ro444 && echo b > priv600 && chmod 0600 priv600" | Out-Null
"== montar con escritura"
$p = Start-Process $W -ArgumentList $img, 'W:\', '--escritura' -PassThru -WindowStyle Hidden
$fin = (Get-Date).AddSeconds(15); while (-not (Test-Path W:\) -and (Get-Date) -lt $fin) { Start-Sleep -Milliseconds 300 }
ok ((Test-Path W:\) -and -not $p.HasExited) "montado"
ok (ro W:\ro444) "un 0444 de Linux se ve como SOLO LECTURA"
ok (-not (ro W:\priv600)) "un 0600 de Linux se ve escribible"
try { Add-Content W:\ro444 "x" -ErrorAction Stop; ok $false "escribir en un solo lectura se niega" } catch { ok $true "escribir en un solo lectura se niega" }
cmd /c "attrib +r W:\priv600" | Out-Null
ok (ro W:\priv600) "attrib +r se refleja"
Set-Content W:\nuevo.txt "de windows"
cmd /c "attrib +r W:\nuevo.txt" | Out-Null
$antes = Get-Content W:\nuevo.txt -Raw
cmd /c "attrib -r W:\ro444" | Out-Null
ok (-not (ro W:\ro444)) "attrib -r se refleja"
Add-Content W:\ro444 "escrito tras -r"
ok ((Get-Content W:\ro444 -Raw) -match "escrito tras -r") "tras -r se puede escribir"
cmd /c "attrib +h W:\nuevo.txt" | Out-Null
ok ((Get-Content W:\nuevo.txt -Raw) -eq $antes) "otros atributos de DOS (oculto) se aceptan sin efecto"
& $W --desmontar W:\ | Out-Null
$fin = (Get-Date).AddSeconds(15); while ((Test-Path W:\) -and (Get-Date) -lt $fin) { Start-Sleep -Milliseconds 300 }
"== kernel (stat -c %a)"
$k = kernel "stat -c '%n=%a' ro444 priv600 nuevo.txt"
$k
$m = @{}; foreach ($l in $k) { if ($l -match '^(.+)=(\d+)$') { $m[$matches[1]] = $matches[2] } }
ok ($m['priv600'] -eq '400') "attrib +r sobre 0600 → 0400 (a-w)"
ok ($m['ro444'] -eq '644') "attrib -r sobre 0444 → 0644 (u+w)"
ok ($m['nuevo.txt'] -eq '444') "archivo creado en Windows (0644) + attrib +r → 0444"
"== e2fsck -fn"
$f = wsl -d Debian -e env PATH=/usr/sbin:/usr/bin e2fsck -fn $wp 2>&1; $c = $LASTEXITCODE
ok ($c -eq 0) "e2fsck limpio (exit $c)"; if ($c -ne 0) { $f | Select-Object -Last 8 }
"RESULTADO: $mal fallos"
