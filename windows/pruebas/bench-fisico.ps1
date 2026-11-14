# SPDX-FileCopyrightText: 2026 Juan Carlos Isaza
# SPDX-License-Identifier: LicenseRef-Xiliux-Propietario
# BANCO EN DISCO FÍSICO (el camino donde Windows NO pone caché): un VHDX conectado como
# \\.\PhysicalDriveN, la MISMA carga con varios binarios/configuraciones. Corre ELEVADO:
#   Start-Process pwsh -Verb RunAs -Wait -ArgumentList '-File', '<este>', '-salida', '<carpeta>'
# GUARDA DURA (la de e2e-disco-fisico): solo escribe en el disco que sea EL VHDX recién creado.
# Configuraciones: "nombre=ruta\winext4.exe=MiB de caché" (MiB vacío = no se fija la variable).
param(
    [string]$salida = (Join-Path $env:TEMP "winext4-bench-fisico"),
    [string]$util = "D:\winext4\target\x86_64-pc-windows-gnu\release\winext4util.exe",
    [string[]]$configs = @(
        "A-instalado=C:\Program Files\winext4\winext4.exe=",
        "C-nuevo-cache-64=D:\winext4\target\x86_64-pc-windows-gnu\release\winext4.exe=64",
        "D-nuevo-cache-256=D:\winext4\target\x86_64-pc-windows-gnu\release\winext4.exe=256",
        "E-cache-64-lotes-ipc=D:\winext4\target\x86_64-pc-windows-gnu\release\winext4.exe=64"
    ),
    [int]$crear = 2000, [int]$arbol = 1500
)
$ErrorActionPreference = "Continue"
New-Item -ItemType Directory -Force $salida | Out-Null
$log = Join-Path $salida "bench.log"
[Console]::OutputEncoding = [Text.Encoding]::UTF8
Start-Transcript -Path $log -Force | Out-Null
$vhd = Join-Path $salida "bench.vhdx"
function diskpart_($lineas) { $f = Join-Path $salida "dp.txt"; Set-Content $f ($lineas -join "`r`n"); diskpart /s $f | Out-Null }
# Escribe la línea (Write-Host: va al registro) y DEVUELVE solo los ms (si no, la tabla recibe
# la línea y el número juntos).
function medir($que, [scriptblock]$b) {
    $sw = [Diagnostics.Stopwatch]::StartNew()
    $r = (& $b) -join ' '
    $ms = [int]$sw.Elapsed.TotalMilliseconds
    Write-Host ("   {0,-34} {1,8} ms  {2}" -f $que, $ms, $r)
    return $ms
}

try {
    if (-not ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) { throw "hay que correrlo ELEVADO" }
    # Árbol de origen en NTFS (fuera del volumen medido): $arbol archivos pequeños en 50 carpetas.
    $origen = Join-Path $salida "origen"
    if (-not (Test-Path $origen)) {
        for ($i = 0; $i -lt $arbol; $i++) {
            $d = Join-Path $origen ("d{0:D2}" -f ($i % 50)); if (-not (Test-Path $d)) { New-Item -ItemType Directory $d | Out-Null }
            [IO.File]::WriteAllText((Join-Path $d "a$i.rs"), ("// archivo $i`n" * (1 + $i % 40)))
        }
    }
    # Un VHDX NUESTRO (misma ruta exacta) que quedó conectado de una corrida cortada: se desconecta.
    if (Get-Disk | Where-Object { $_.Location -eq $vhd }) { diskpart_ @("select vdisk file=`"$vhd`"", "detach vdisk"); Start-Sleep 2 }
    Remove-Item $vhd -ErrorAction SilentlyContinue
    if (Test-Path $vhd) { throw "no se pudo quitar el VHDX anterior ($vhd)" }
    diskpart_ @("create vdisk file=`"$vhd`" maximum=2048 type=expandable", "select vdisk file=`"$vhd`"", "attach vdisk")
    Start-Sleep 2
    $d = Get-Disk | Where-Object { $_.Location -eq $vhd }
    if (-not $d -or @($d).Count -ne 1 -or $d.BusType -ne 'File Backed Virtual' -or $d.Size -gt 2100MB) { throw "GUARDA: el disco del VHDX no se identificó sin ambigüedad; no se toca nada" }
    $N = $d.Number
    Initialize-Disk -Number $N -PartitionStyle GPT -ErrorAction Stop
    New-Partition -DiskNumber $N -UseMaximumSize -GptType '{0FC63DAF-8483-4772-8E79-3D69D8477DE4}' -ErrorAction Stop | Out-Null
    $disco = "\\.\PhysicalDrive$N"
    "disco del VHDX: $disco"
    $resumen = @()
    foreach ($c in $configs) { try {
        $nombre, $exe, $mb = $c -split '=', 3
        if ((Get-Disk -Number $N).Location -ne $vhd) { throw "GUARDA: el disco $N ya no es el VHDX" }
        & $util formatear $disco --autorizo-disco-fisico --etiqueta BENCH | Out-Null
        if ($mb -ne '') { $env:WINEXT4_CACHE_MB = $mb } else { Remove-Item Env:WINEXT4_CACHE_MB -ErrorAction SilentlyContinue }
        if ($nombre -match 'lotes-ipc') { $env:WINEXT4_LOTES_IPC = '1' } else { Remove-Item Env:WINEXT4_LOTES_IPC -ErrorAction SilentlyContinue }
        $env:RUSTKAN_PERFIL = '1'
        $err = Join-Path $salida "$nombre.err"
        $p = Start-Process $exe -ArgumentList $disco, 'X:\', '--escritura', '--autorizo-disco-fisico' -PassThru -WindowStyle Hidden -RedirectStandardError $err
        $fin = (Get-Date).AddSeconds(30); while (-not (Test-Path X:\) -and (Get-Date) -lt $fin) { Start-Sleep -Milliseconds 200 }
        if (-not (Test-Path X:\)) { throw "$nombre no montó" }
        "== $nombre ($exe, caché=$(if ($mb -eq '') {'por defecto'} else {"$mb MiB"}))"
        New-Item -ItemType Directory X:\c | Out-Null
        $t1 = medir "crear $crear archivos" { for ($i = 0; $i -lt $crear; $i++) { [IO.File]::WriteAllText("X:\c\f$i.txt", "x$i") }; '' }
        $t2 = medir "listar (Get-ChildItem)" { "$(@(Get-ChildItem X:\c).Count) entradas" }
        $t3 = medir "copiar árbol de $arbol desde NTFS" { Copy-Item $origen X:\arbol -Recurse; '' }
        $t4 = medir "listar árbol recursivo" { "$(@(Get-ChildItem X:\arbol -Recurse -File).Count) archivos" }
        $t5 = medir "leer el árbol entero" { $n = 0; foreach ($f in Get-ChildItem X:\arbol -Recurse -File) { $n += [IO.File]::ReadAllBytes($f.FullName).Length }; "$n bytes" }
        $archivos = @(Get-ChildItem X:\arbol -Recurse -File | ForEach-Object FullName)
        $t5p = medir "leer el árbol con 4 lectores a la vez" { $n = ($archivos | ForEach-Object -ThrottleLimit 4 -Parallel { [IO.File]::ReadAllBytes($_).Length } | Measure-Object -Sum).Sum; "$n bytes" }
        $t5s = medir "...y con 1 lector (misma lista)" { $n = 0; foreach ($f in $archivos) { $n += [IO.File]::ReadAllBytes($f).Length }; "$n bytes" }
        $t5b = medir "releer el árbol (¿caché de Windows?)" { $n = 0; foreach ($f in Get-ChildItem X:\arbol -Recurse -File) { $n += [IO.File]::ReadAllBytes($f.FullName).Length }; "$n bytes" }
        $grande = New-Object byte[] (256MB); (New-Object Random 3).NextBytes($grande)
        # Una sola llamada WriteFile de 256 MiB puede fallar en Dokan («recursos insuficientes»: el
        # driver copia el búfer ENTERO a memoria del kernel, sys/write.c). Se anota y se sigue; la
        # escritura en trozos de 1 MiB es lo que hacen Copy-Item, el Explorador y casi todos.
        $tw1 = medir "escribir 256 MiB de UNA vez" { try { [IO.File]::WriteAllBytes("X:\grande1.bin", $grande); '' } catch { "FALLÓ: $($_.Exception.InnerException.Message)" } }
        Remove-Item X:\grande1.bin -ErrorAction SilentlyContinue
        $tw = medir "escribir 256 MiB en trozos de 1 MiB" { $fs = [IO.File]::Create("X:\grande.bin"); for ($o = 0; $o -lt $grande.Length; $o += 1MB) { $fs.Write($grande, $o, 1MB) }; $fs.Dispose(); '' }
        $tr1 = medir "leer 256 MiB" { [IO.File]::ReadAllBytes("X:\grande.bin").Length }
        $tr2 = medir "releer 256 MiB" { [IO.File]::ReadAllBytes("X:\grande.bin").Length }
        $mbs = { param($ms) if ($ms -gt 0) { [int](256000 / $ms) } else { 0 } }
        Write-Host ("   secuencial: escribir {0} MB/s, leer {1} MB/s, releer {2} MB/s" -f (& $mbs $tw), (& $mbs $tr1), (& $mbs $tr2))
        $t6 = medir "borrar todo" { Remove-Item X:\c, X:\arbol, X:\grande.bin -Recurse -Force; '' }
        & $exe --desmontar 'X:\' | Out-Null
        $p.WaitForExit(60000) | Out-Null
        $cache = (Get-Content $err -Encoding utf8 | Select-String 'aciertos') -join ''
        if ($cache) { "   $cache" }
        $perfil = Get-Content $err -Encoding utf8 | Select-String -Pattern 'rustkan perfil' -Context 0,40
        if ($perfil) { $perfil.Line; $perfil.Context.PostContext | Where-Object { $_ -match '^\s' } }
        $v = & $util verificar $disco 2>&1; $vc = $LASTEXITCODE
        "   verificar tras la carga: exit $vc $(if ($vc -eq 0) {'(0 errores)'} else {'<<< ERRORES'})"
        $resumen += "{0,-22} crear {1,6} listar {2,5} copiar {3,6} listar-rec {4,5} leer {5,6} releer {6,6} borrar {7,6} (ms) | 256MiB esc {8} lee {9} relee {10} MB/s | verificar={11}" -f $nombre, $t1, $t2, $t3, $t4, $t5, $t5b, $t6, (& $mbs $tw), (& $mbs $tr1), (& $mbs $tr2), $vc
    } catch { "   ERROR en ${nombre}: $_"; & $exe --desmontar 'X:\' 2>$null | Out-Null; Start-Sleep 3 }
    }
    "== RESUMEN"
    $resumen
} catch {
    "ERROR: $_"
} finally {
    Remove-Item Env:WINEXT4_CACHE_MB -ErrorAction SilentlyContinue
    diskpart_ @("select vdisk file=`"$vhd`"", "detach vdisk")
    Stop-Transcript | Out-Null
}
