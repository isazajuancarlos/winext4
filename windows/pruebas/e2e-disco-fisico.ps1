# SPDX-FileCopyrightText: 2026 Juan Carlos Isaza
# SPDX-License-Identifier: LicenseRef-Xiliux-Propietario
# E2E de ESCRITURA EN DISCO FÍSICO, sobre un VHDX conectado como disco (\\.\PhysicalDriveN):
# nunca sobre un disco real. Corre ELEVADO (diskpart y el disco crudo lo exigen):
#   Start-Process pwsh -Verb RunAs -Wait -ArgumentList '-File', '<este>', '-salida', '<carpeta>'
# Deja en <salida>: e2e.log (resultado) y particion.img (la partición volcada, para e2fsck).
# GUARDA DURA: antes de escribir, exige que el disco N sea EL VHDX recién creado (BusType
# «File Backed Virtual» y su ruta); si no, aborta sin tocar nada.
param([string]$salida = (Join-Path $env:TEMP "winext4-e2e-fisico"), [string]$bin = "D:\winext4\target\x86_64-pc-windows-gnu\release")
$ErrorActionPreference = "Continue"
New-Item -ItemType Directory -Force $salida | Out-Null
$log = Join-Path $salida "e2e.log"
[Console]::OutputEncoding = [Text.Encoding]::UTF8
Start-Transcript -Path $log -Force | Out-Null
$U = "$bin\winext4util.exe"; $W = "$bin\winext4.exe"
$vhd = Join-Path $salida "prueba.vhdx"
$mal = 0
function ok($cond, $que) { if ($cond) { "  ok   $que" } else { "  MAL  $que"; $script:mal++ } }
function diskpart_($lineas) { $f = Join-Path $salida "dp.txt"; Set-Content $f ($lineas -join "`r`n"); diskpart /s $f | Out-Null }
function esperar_montado($letra, $p) { $fin = (Get-Date).AddSeconds(20); while (-not (Test-Path "${letra}:\") -and (Get-Date) -lt $fin -and -not $p.HasExited) { Start-Sleep -Milliseconds 300 }; Test-Path "${letra}:\" }
function desmontar($letra) { & $W --desmontar "${letra}:\" | Out-Null; $fin = (Get-Date).AddSeconds(20); while ((Test-Path "${letra}:\") -and (Get-Date) -lt $fin) { Start-Sleep -Milliseconds 300 } }

try {
    if (-not ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) { throw "hay que correrlo ELEVADO" }
    Remove-Item $vhd -ErrorAction SilentlyContinue
    "== VHDX de 512 MiB, GPT, una partición tipo Linux"
    diskpart_ @("create vdisk file=`"$vhd`" maximum=512 type=expandable", "select vdisk file=`"$vhd`"", "attach vdisk")
    Start-Sleep 2
    $d = Get-Disk | Where-Object { $_.Location -eq $vhd }
    # GUARDA DURA (antes de escribir nada en ningún disco).
    if (-not $d -or @($d).Count -ne 1 -or $d.BusType -ne 'File Backed Virtual' -or $d.Size -gt 600MB) { throw "GUARDA: el disco del VHDX no se identificó sin ambigüedad (BusType=$($d.BusType), tamaño=$($d.Size)); no se toca nada" }
    $N = $d.Number
    "disco del VHDX: PhysicalDrive$N ($($d.BusType), $([math]::Round($d.Size/1MB)) MiB, $($d.Location))"
    Initialize-Disk -Number $N -PartitionStyle GPT -ErrorAction Stop
    $part = New-Partition -DiskNumber $N -UseMaximumSize -GptType '{0FC63DAF-8483-4772-8E79-3D69D8477DE4}' -ErrorAction Stop
    $disco = "\\.\PhysicalDrive$N"
    "partición Linux en el offset $($part.Offset), $([math]::Round($part.Size/1MB)) MiB"
    # Repetir la guarda justo antes de la primera escritura.
    if ((Get-Disk -Number $N).Location -ne $vhd) { throw "GUARDA: el disco $N ya no es el VHDX" }

    "== formatear (winext4util, disco físico autorizado)"
    & $U formatear $disco --autorizo-disco-fisico --etiqueta PRUEBA
    ok ($LASTEXITCODE -eq 0) "formatear la partición Linux del VHDX"

    "== puerta 1: --escritura sin --autorizo-disco-fisico"
    $o = & $W $disco 'X:\' --escritura 2>&1; $c = $LASTEXITCODE
    ok (($c -ne 0) -and (($o -join ' ') -match 'autorizo-disco-fisico')) "se niega y dice por qué"

    "== puerta 2: exclusividad (otro montaje abierto)"
    $ro = Start-Process $W -ArgumentList $disco, 'Y:\' -PassThru -WindowStyle Hidden
    ok (esperar_montado 'Y' $ro) "montaje de solo lectura en Y: (sostiene el disco)"
    $o = & $W $disco 'X:\' --escritura --autorizo-disco-fisico 2>&1; $c = $LASTEXITCODE
    ok (($c -ne 0) -and (($o -join ' ') -match 'otro programa')) "con otro montaje abierto, la escritura se NIEGA"
    desmontar 'Y'

    "== montaje con escritura autorizado"
    $rw = Start-Process $W -ArgumentList $disco, 'X:\', '--escritura', '--autorizo-disco-fisico' -PassThru -WindowStyle Hidden -RedirectStandardError (Join-Path $salida "rw.err")
    ok (esperar_montado 'X' $rw) "montado X: con escritura"
    New-Item -ItemType Directory X:\docs | Out-Null
    1..50 | ForEach-Object { Set-Content "X:\docs\nota-$_.txt" "nota $_" }
    $grande = New-Object byte[] (40MB); (New-Object Random 7).NextBytes($grande); [IO.File]::WriteAllBytes("X:\grande.bin", $grande)
    $sha = (Get-FileHash X:\grande.bin).Hash
    Rename-Item X:\docs\nota-1.txt renombrada.txt
    Remove-Item X:\docs\nota-2.txt
    ok ((Get-ChildItem X:\docs).Count -eq 49) "49 notas tras renombrar y borrar"
    ok ((Get-Content X:\docs\renombrada.txt -Raw).Trim() -eq "nota 1") "renombrar conserva el contenido"
    desmontar 'X'
    ok ($rw.WaitForExit(30000)) "desmontado (el proceso terminó)"
    $err1 = Get-Content (Join-Path $salida "rw.err") -Raw -Encoding utf8
    ok ($err1 -match "comprobación rápida: 0 errores" -and $err1 -cnotmatch "verificación COMPLETA") "primer montaje (volumen recién formateado, con fecha de verificación): SOLO la rápida"
    $v = & $U verificar $disco 2>&1; $c = $LASTEXITCODE
    ok ($c -eq 0) "winext4util verificar tras escribir: 0 errores (exit $c)"

    "== releer en solo lectura"
    $ro = Start-Process $W -ArgumentList $disco, 'Y:\' -PassThru -WindowStyle Hidden
    ok (esperar_montado 'Y' $ro) "remontado en solo lectura"
    ok ((Get-FileHash Y:\grande.bin).Hash -eq $sha) "los 40 MiB se leen idénticos"
    desmontar 'Y'

    "== puerta 3: verificación previa (descriptor del grupo 0 dañado)"
    $fs = [IO.File]::Open($disco, 'Open', 'ReadWrite', 'ReadWrite')
    try {
        $gdt = $part.Offset + 4096   # bloque 1 (bloques de 4 KiB): la tabla de descriptores
        $buf = New-Object byte[] 4096
        $fs.Position = $gdt; [void]$fs.Read($buf, 0, 4096)
        $original = $buf.Clone()
        $buf[0x0C] = $buf[0x0C] -bxor 0x01   # bg_free_blocks_count, sin rehacer la suma
        $fs.Position = $gdt; $fs.Write($buf, 0, 4096); $fs.Flush()
    } finally { $fs.Close() }
    $v = & $U verificar $disco 2>&1; $c = $LASTEXITCODE
    ok ($c -ne 0) "control: verificar VE el daño (exit $c)"
    $o = & $W $disco 'X:\' --escritura --autorizo-disco-fisico 2>&1; $c = $LASTEXITCODE
    ok (($c -ne 0) -and (($o -join ' ') -match 'errores')) "con errores, el montaje con escritura se NIEGA"
    ok ((($o -join ' ') -match 'rápida encontró') -and (($o -join ' ') -cmatch 'verificación COMPLETA')) "...porque la RÁPIDA vio el daño y la COMPLETA lo confirmó"
    # Restaurar el byte (la REPARACIÓN de una suma de descriptor no es de winext4: resellarlo a
    # ciegas bendeciría un descriptor dañado; reparar se niega y manda a e2fsck, a propósito).
    $fs = [IO.File]::Open($disco, 'Open', 'ReadWrite', 'ReadWrite')
    try { $fs.Position = $gdt; $fs.Write($original, 0, 4096); $fs.Flush() } finally { $fs.Close() }
    $v = & $U verificar $disco 2>&1; $c = $LASTEXITCODE
    ok ($c -eq 0) "restaurado: verificar 0 errores (exit $c)"
    $rw = Start-Process $W -ArgumentList $disco, 'X:\', '--escritura', '--autorizo-disco-fisico' -PassThru -WindowStyle Hidden -RedirectStandardError (Join-Path $salida "rw2.err")
    ok (esperar_montado 'X' $rw) "sin errores, vuelve a montar con escritura"
    Set-Content X:\despues.txt "tras restaurar"
    desmontar 'X'
    ok ($rw.WaitForExit(30000)) "desmontado"
    $err2 = Get-Content (Join-Path $salida "rw2.err") -Raw -Encoding utf8   # leído con el proceso YA terminado
    ok ($err2 -match "comprobación rápida: 0 errores" -and $err2 -cnotmatch "verificación COMPLETA") "tras restaurar el daño: SOLO la rápida"

    "== camino del nvme: motivo → COMPLETA limpia → se anota → el siguiente montaje es rápido"
    # Fecha de verificación muy vieja (s_lastcheck = 1), con la suma del superbloque rehecha.
    function crc32c([byte[]]$d) { $c = [long]4294967295; $pol = [long]2197175160; foreach ($b in $d) { $c = $c -bxor [long]$b; for ($k = 0; $k -lt 8; $k++) { if ($c -band 1) { $c = ($c -shr 1) -bxor $pol } else { $c = $c -shr 1 } } }; [uint32]$c }   # crc32c sin inversión final (como ext4)
    $fs = [IO.File]::Open($disco, 'Open', 'ReadWrite', 'ReadWrite')
    try {
        $buf = New-Object byte[] 4096
        $fs.Position = $part.Offset; [void]$fs.Read($buf, 0, 4096)   # bloque 0: el superbloque va en 1024..2047
        $sb = New-Object byte[] 1024; [Array]::Copy($buf, 1024, $sb, 0, 1024)
        $lastcheck_antes = [BitConverter]::ToUInt32($sb, 0x40)
        [BitConverter]::GetBytes([uint32]1).CopyTo($sb, 0x40)
        $cs = crc32c ($sb[0..0x3FB]); [BitConverter]::GetBytes($cs).CopyTo($sb, 0x3FC)
        [Array]::Copy($sb, 0, $buf, 1024, 1024)
        $fs.Position = $part.Offset; $fs.Write($buf, 0, 4096); $fs.Flush()
    } finally { $fs.Close() }
    $v = & $U verificar $disco 2>&1; $c = $LASTEXITCODE
    ok ($c -eq 0) "control: con la fecha vieja (y la suma rehecha) el volumen sigue SANO (exit $c)"
    $rw = Start-Process $W -ArgumentList $disco, 'X:\', '--escritura', '--autorizo-disco-fisico' -PassThru -WindowStyle Hidden -RedirectStandardError (Join-Path $salida "rw3.err")
    ok (esperar_montado 'X' $rw) "monta con escritura"
    Set-Content X:\tras-completa.txt "ok"
    desmontar 'X'; [void]$rw.WaitForExit(30000)
    $err3 = Get-Content (Join-Path $salida "rw3.err") -Raw -Encoding utf8
    ok ($err3 -cmatch "verificación COMPLETA" -and $err3 -match "más de 30 días" -and $err3 -match "verificado: 0 errores") "con la fecha vieja: hizo la COMPLETA por ese motivo y salió limpia"
    $fs = [IO.File]::Open($disco, 'Open', 'Read', 'ReadWrite')
    try { $buf = New-Object byte[] 4096; $fs.Position = $part.Offset; [void]$fs.Read($buf, 0, 4096) } finally { $fs.Close() }
    $lastcheck_despues = [BitConverter]::ToUInt32($buf, 1024 + 0x40)
    $hoy = [uint32]([DateTimeOffset]::UtcNow.ToUnixTimeSeconds())
    ok ([math]::Abs([int64]$hoy - [int64]$lastcheck_despues) -lt 600) "la verificación quedó ANOTADA en el disco (s_lastcheck = ahora: $lastcheck_despues)"
    $rw = Start-Process $W -ArgumentList $disco, 'X:\', '--escritura', '--autorizo-disco-fisico' -PassThru -WindowStyle Hidden -RedirectStandardError (Join-Path $salida "rw4.err")
    ok (esperar_montado 'X' $rw) "vuelve a montar"
    desmontar 'X'; [void]$rw.WaitForExit(30000)
    $err4 = Get-Content (Join-Path $salida "rw4.err") -Raw -Encoding utf8
    ok ($err4 -match "comprobación rápida: 0 errores" -and $err4 -cnotmatch "verificación COMPLETA") "el montaje siguiente: SOLO la rápida"
    "== volcar la partición para e2fsck"
    $img = Join-Path $salida "particion.img"
    $in = [IO.File]::Open($disco, 'Open', 'Read', 'ReadWrite')
    try {
        $out = [IO.File]::Create($img)
        try {
            $in.Position = $part.Offset; $resto = [long]$part.Size; $b = New-Object byte[] (1MB)
            while ($resto -gt 0) { $n = $in.Read($b, 0, [int][math]::Min($b.Length, $resto)); if ($n -le 0) { break }; $out.Write($b, 0, $n); $resto -= $n }
        } finally { $out.Close() }
    } finally { $in.Close() }
    ok ((Get-Item $img).Length -eq $part.Size) "volcada ($([math]::Round($part.Size/1MB)) MiB)"
}
catch { "  MAL  ABORTADO: $_"; $mal++ }
finally {
    foreach ($l in 'X', 'Y') { if (Test-Path "${l}:\") { desmontar $l } }
    if (Test-Path $vhd) { diskpart_ @("select vdisk file=`"$vhd`"", "detach vdisk"); Remove-Item $vhd -ErrorAction SilentlyContinue }
    "VHDX desconectado y borrado: $(-not (Test-Path $vhd))"
    "RESULTADO: $mal fallos"
    Stop-Transcript | Out-Null
}
