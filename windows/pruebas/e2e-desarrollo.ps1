# SPDX-FileCopyrightText: 2026 Juan Carlos Isaza
# SPDX-License-Identifier: LicenseRef-Xiliux-Propietario
# E2E de USO REAL (D28): un día de desarrollo sobre un volumen winext4 con escritura — git clone,
# cargo build (con target/ DENTRO del volumen, el caso pesado), git status, editar, commit — y
# después, desde Linux (WSL): e2fsck limpio, el kernel monta el volumen y `git fsck` del clon sano.
# Uso: .\e2e-desarrollo.ps1 [-bin <carpeta>] [-repo D:\winext4]. W: debe estar libre.
param([string]$bin = "D:\winext4\target\x86_64-pc-windows-gnu\release", [string]$repo = "D:\winext4")
$ErrorActionPreference = "Continue"
$s = Join-Path $env:TEMP "winext4-e2e-desarrollo"; New-Item -ItemType Directory -Force $s | Out-Null
$U = "$bin\winext4util.exe"; $W = "$bin\winext4.exe"
$img = "$s\dev.img"
$mal = 0
function ok($cond, $que) { if ($cond) { "  ok   $que" } else { "  MAL  $que"; $script:mal++ } }
# Devuelve SOLO el código de salida (lo demás va a la pantalla con Write-Host: si se devolviera
# junto, PowerShell evaluaría @("texto", 0) -eq 0 → @(0) → FALSO).
function t($que, [scriptblock]$b) { $sw = [Diagnostics.Stopwatch]::StartNew(); $o = & $b 2>&1; $c = $LASTEXITCODE; $sw.Stop(); Write-Host ("  [{0,6:N1} s] {1} (exit {2})" -f $sw.Elapsed.TotalSeconds, $que, $c); if ($c -ne 0) { $o | Select-Object -Last 6 | Out-String | Write-Host }; return [int]$c }
Remove-Item $img -ErrorAction SilentlyContinue
& $U formatear $img --tamano 4G | Out-Null
$p = Start-Process $W -ArgumentList $img, 'W:\', '--escritura' -PassThru -WindowStyle Hidden -RedirectStandardError "$s\montaje.err"
$fin = (Get-Date).AddSeconds(20); while (-not (Test-Path W:\) -and (Get-Date) -lt $fin) { Start-Sleep -Milliseconds 300 }
ok (Test-Path W:\) "montado W: con escritura"
try {
    "== git"
    ok ((t "git clone $repo" { git clone -q $repo W:\clon }) -eq 0) "git clone"
# winext4 depende de rustkan como HERMANO (`path = "../../rustkan"` en windows/Cargo.toml): se
# clona al lado, igual que en el disco (D:\winext4 junto a D:\rustkan). Sin él, cargo no carga
# el workspace (no es un fallo del sistema de archivos: medido el 2026-11-05).
$hermano = Join-Path (Split-Path $repo -Parent) 'rustkan'
ok ((t "git clone $hermano" { git clone -q $hermano W:\rustkan }) -eq 0) "git clone de rustkan (dependencia hermana)"
    $n = (git -C W:\clon ls-files | Measure-Object).Count
    ok ($n -gt 50) "el clon tiene $n archivos versionados"
    ok ((t "git status" { git -C W:\clon status --porcelain }) -eq 0) "git status"
    ok ((git -C W:\clon status --porcelain | Measure-Object).Count -eq 0) "el clon está limpio (sin cambios fantasma por fin de línea, mayúsculas o permisos)"
    "== cargo (target/ dentro del volumen)"
    $env:CARGO_TARGET_DIR = "W:\clon\target"
    ok ((t "cargo build -p winext4-nucleo (release)" { cargo +stable-x86_64-pc-windows-gnu build -q -p winext4-nucleo --target x86_64-pc-windows-gnu --release --manifest-path W:\clon\Cargo.toml }) -eq 0) "cargo build"
    ok ((t "cargo test --lib del núcleo" { cargo +stable-x86_64-pc-windows-gnu test -q -p winext4-nucleo --target x86_64-pc-windows-gnu --release --lib --manifest-path W:\clon\Cargo.toml }) -eq 0) "cargo test (24 pruebas unitarias) compilado y corrido DESDE el volumen"
    Remove-Item Env:CARGO_TARGET_DIR
    $nt = (Get-ChildItem W:\clon\target -Recurse -File -ErrorAction SilentlyContinue | Measure-Object).Count
    "  target/ en el volumen: $nt archivos"
    "== editar y commit"
    Add-Content W:\clon\README.md "`nLínea añadida desde Windows sobre winext4."
    git -C W:\clon -c user.name=prueba -c user.email=prueba@ejemplo.invalid commit -qam "prueba de commit sobre winext4" 2>&1 | Out-Null
    ok ($LASTEXITCODE -eq 0) "git commit"
    ok ((git -C W:\clon log -1 --format=%s) -eq "prueba de commit sobre winext4") "el commit está en la historia"
    ok ((t "git fsck (desde Windows)" { git -C W:\clon fsck --no-progress }) -eq 0) "git fsck desde Windows"
} finally {
    Remove-Item Env:CARGO_TARGET_DIR -ErrorAction SilentlyContinue
    & $W --desmontar W:\ | Out-Null
    [void]$p.WaitForExit(60000)
}
$errs = @(Get-Content "$s\montaje.err" | Select-String 'ERROR')
ok ($errs.Count -eq 0) "el montaje no registró ningún error ($($errs.Count))"; $errs | Select-Object -First 5
"== desde Linux (WSL)"
$wp = wsl -d Debian -e wslpath -a ($img -replace '\\', '/')
$f = wsl -d Debian -e env PATH=/usr/sbin:/usr/bin e2fsck -fn $wp 2>&1; $c = $LASTEXITCODE
ok ($c -eq 0) "e2fsck -fn limpio (exit $c)"; if ($c -ne 0) { $f | Select-Object -Last 10 }
$k = wsl -d Debian -u root -e sh -c "M=`$(mktemp -d); mount -o loop -r '$wp' `$M && git config --global --add safe.directory '*' && git -C `$M/clon fsck --no-progress >/dev/null 2>&1 && echo FSCK-OK; git -C `$M/clon log -1 --format=%s; tail -n1 `$M/clon/README.md; umount `$M; rmdir `$M"
$k
ok (($k -join '|') -match 'FSCK-OK') "el KERNEL monta el volumen y git fsck del clon sale sano"
ok (($k -join '|') -match 'prueba de commit sobre winext4') "Linux ve el commit hecho desde Windows"
"RESULTADO: $mal fallos"
