# SPDX-FileCopyrightText: 2026 Juan Carlos Isaza
# SPDX-License-Identifier: LicenseRef-Xiliux-Propietario
# E2E de ENLACES en el montaje real de Windows (D28): crea enlaces con winext4util, monta con
# winext4.exe --escritura en W:, los usa desde Windows (leer, escribir, borrar, y el BORRADO
# RECURSIVO de una carpeta con un enlace dentro, que NO debe tocar el destino), desmonta y pasa
# e2fsck y el kernel (WSL Debian). Uso: .\e2e-enlaces.ps1 [-bin <carpeta con los .exe>]
# Sale con «RESULTADO: 0 fallos». W: debe estar libre.
param([string]$bin = "D:\winext4\target\x86_64-pc-windows-gnu\release")
$ErrorActionPreference = "Continue"
$s = Join-Path $env:TEMP "winext4-e2e-enlaces"; New-Item -ItemType Directory -Force $s | Out-Null
$img = "$s\e2e-enlaces.img"
$U = "$bin\winext4util.exe"; $W = "$bin\winext4.exe"
$mal = 0
function ok($cond, $que) { if ($cond) { "  ok   $que" } else { "  MAL  $que"; $script:mal++ } }
Remove-Item $img -ErrorAction SilentlyContinue
& $U formatear $img --tamano 64M | Out-Null
$wp = wsl -d Debian -e wslpath -a ($img -replace '\\','/')
# Contenido base con debugfs (lector/escritor independiente)
"contenido del destino" | Set-Content "$s\dest.txt" -NoNewline
"dentro" | Set-Content "$s\f.txt" -NoNewline
wsl -d Debian -e env PATH=/usr/sbin:/usr/bin debugfs -w -R "write $(wsl -d Debian -e wslpath -a ($s -replace '\\','/'))/dest.txt destino.txt" $wp 2>$null | Out-Null
wsl -d Debian -e env PATH=/usr/sbin:/usr/bin debugfs -w -R "mkdir carpeta" $wp 2>$null | Out-Null
wsl -d Debian -e env PATH=/usr/sbin:/usr/bin debugfs -w -R "write $(wsl -d Debian -e wslpath -a ($s -replace '\\','/'))/f.txt carpeta/f" $wp 2>$null | Out-Null
"== winext4util enlazar"
& $U enlazar $img destino.txt /rapido --simbolico
& $U enlazar $img carpeta /a-carpeta --simbolico
& $U enlazar $img /etc/passwd /absoluto --simbolico
& $U enlazar $img /destino.txt /duro.txt
wsl -d Debian -e env PATH=/usr/sbin:/usr/bin debugfs -w -R "mkdir padre" $wp 2>$null | Out-Null
wsl -d Debian -e env PATH=/usr/sbin:/usr/bin debugfs -w -R "mkdir padre2" $wp 2>$null | Out-Null
& $U enlazar $img ../carpeta /padre/enlace --simbolico
& $U enlazar $img ../carpeta /padre2/enlace --simbolico
$o = & $U enlazar $img /carpeta /dir-duro 2>&1; $c = $LASTEXITCODE; $o | Select-Object -First 1
ok ($c -ne 0) "enlace duro a un directorio se niega"
$o = & $U enlazar \\.\PhysicalDrive2 x /y --simbolico 2>&1; $c = $LASTEXITCODE; $o | Select-Object -First 1
ok ($c -ne 0) "enlazar en disco físico se niega"
"== montar con escritura"
$p = Start-Process $W -ArgumentList $img, 'W:\', '--escritura' -PassThru -WindowStyle Hidden -RedirectStandardError "$s\montaje.err"
$fin = (Get-Date).AddSeconds(15); while (-not (Test-Path W:\) -and (Get-Date) -lt $fin) { Start-Sleep -Milliseconds 300 }
ok ((Test-Path W:\) -and -not $p.HasExited) "montado (proceso $($p.Id), nuevo)"
ok ((Get-Content W:\rapido -Raw) -eq "contenido del destino") "leer por el enlace rápido = el destino"
ok (-not (Test-Path W:\a-carpeta -PathType Container)) "el enlace a carpeta NO se ve como carpeta (no se sigue en Windows)"
ok ((Get-Content W:\a-carpeta -Raw) -eq "carpeta") "...se ve como archivo con su destino"
ok (-not (Test-Path W:\a-carpeta\f)) "...y no se puede entrar por él"
ok ((Get-Content W:\absoluto -Raw) -eq "/etc/passwd") "enlace absoluto (no seguible) = su destino como texto"
ok (((Get-Item W:\absoluto).Attributes -band [IO.FileAttributes]::ReadOnly) -eq 0) "enlace no seguible: SIN marca de solo lectura (se puede borrar; la escritura se niega al abrir)"
$antes = (Get-Item W:\absoluto).Length
try { Add-Content W:\absoluto "x" -ErrorAction Stop; ok $false "escribir en un enlace no seguible se niega" } catch { ok $true "escribir en un enlace no seguible se niega" }
ok ((Get-Item W:\absoluto).Length -eq $antes) "el enlace no seguible sigue intacto"
Add-Content W:\duro.txt "|por el duro" -NoNewline
ok ((Get-Content W:\destino.txt -Raw) -eq "contenido del destino|por el duro") "escribir por el enlace duro cambia el destino"
"  dir: " + ((cmd /c dir W:\ | Select-String "a-carpeta|absoluto") -join " / ")
Remove-Item W:\padre -Recurse -Force
ok (-not (Test-Path W:\padre)) "Remove-Item -Recurse de una carpeta con un enlace dentro"
ok ((Get-Content W:\carpeta\f -Raw) -eq "dentro") "...NO vació la carpeta destino (no entró en el enlace)"
cmd /c "rmdir /s /q W:\padre2" 2>&1
ok (-not (Test-Path W:\padre2)) "rmdir /s de una carpeta con un enlace dentro"
ok ((Get-Content W:\carpeta\f -Raw) -eq "dentro") "...tampoco vació la carpeta destino"
Remove-Item W:\a-carpeta
ok (-not (Test-Path W:\a-carpeta)) "borrar el enlace a carpeta"
ok ((Get-Content W:\carpeta\f -Raw) -eq "dentro") "...y la carpeta sigue intacta"
Remove-Item W:\rapido
ok ((-not (Test-Path W:\rapido)) -and (Test-Path W:\destino.txt)) "borrar el enlace rápido deja el destino"
Remove-Item W:\duro.txt
ok ((Get-Content W:\destino.txt -Raw) -eq "contenido del destino|por el duro") "borrar un enlace duro deja el otro nombre"
& $W --desmontar W:\ | Out-Null
$fin = (Get-Date).AddSeconds(15); while ((Test-Path W:\) -and (Get-Date) -lt $fin) { Start-Sleep -Milliseconds 300 }
"== e2fsck -fn"
$f = wsl -d Debian -e env PATH=/usr/sbin:/usr/bin e2fsck -fn $wp 2>&1; $c = $LASTEXITCODE
ok ($c -eq 0) "e2fsck limpio (exit $c)"; if ($c -ne 0) { $f | Select-Object -Last 8 }
"== kernel"
$k = wsl -d Debian -u root -e sh -c "M=`$(mktemp -d); mount -o loop -r '$wp' `$M && readlink `$M/absoluto && stat -c %h `$M/destino.txt && cat `$M/destino.txt; echo; ls `$M; umount `$M"
$k
ok (($k -join "|") -match '/etc/passwd\|1\|contenido del destino\|por el duro') "el kernel ve lo mismo (absoluto, 1 enlace, contenido)"
if ($mal) { "--- registro del montaje:"; Get-Content "$s\montaje.err" | Select-String "ERROR|error" }
"RESULTADO: $mal fallos"
