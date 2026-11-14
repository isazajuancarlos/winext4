<!-- SPDX-FileCopyrightText: 2026 Juan Carlos Isaza -->
<!-- SPDX-License-Identifier: LicenseRef-Xiliux-Propietario -->
# winext4 para Windows — compilar (desde Linux) y empaquetar

El host de Windows usa **Dokan** (LGPL; enlace **dinámico** al DLL → compatible con producto
propietario). El motor (`nucleo`) sigue 100% propio. **Todo esto se compila en Linux**; lo
único que necesita Windows es *probar* que monta (compilar ≠ funcionar).

## Requisitos (una vez, en Fedora)

```
sudo dnf install -y mingw64-gcc mingw64-winpthreads-static
rustup target add x86_64-pc-windows-gnu
```

## Compilar el `.exe`

```
cargo build -p winext4-windows --release --target x86_64-pc-windows-gnu
```

Sale `target/x86_64-pc-windows-gnu/release/winext4.exe`. Dokan llega por **rustkan** (dependencia
`path = "../../rustkan"`: el repo `D:\rustkan` tiene que estar al lado), que carga `dokan2.dll` en
EJECUCIÓN y solo desde System32 — no hay import-lib, ni `build.rs`, ni `vendor/dokan` (retirados el
2026-11-05). Sin Dokan instalado, `winext4` arranca y lo DICE en vez de no abrir.

## Compilar NATIVO en Windows (gnu) — verificado 2026-11-04

En la máquina Windows (PowerShell), sin Visual Studio: el toolchain **windows-gnu** de rustup trae
su propio `ld` y `gcc`.

```powershell
rustup toolchain install stable-x86_64-pc-windows-gnu
cargo +stable-x86_64-pc-windows-gnu build  -p winext4-windows --release    --target x86_64-pc-windows-gnu
cargo +stable-x86_64-pc-windows-gnu clippy -p winext4-windows --all-targets --target x86_64-pc-windows-gnu -- -D warnings
cargo +stable-x86_64-pc-windows-gnu test   -p winext4-windows               --target x86_64-pc-windows-gnu
```

NO uses el toolchain por defecto `msvc`: en esta máquina no encuentra `link.exe` (las Build Tools de
scoop no están en el PATH). Con rustkan ya no hay import-lib que lo impida: es solo el enlazador.

## Empaquetar (autocontenido, instalación nueva de Windows)

El paquete trae TODO; el usuario no instala nada aparte:

1. `winext4.exe` (nuestro, propietario).
2. **DokanSetup** firmado del release oficial de Dokany (bundlea `dokan2.sys` + `dokan2.dll`,
   LGPL, driver firmado → carga en Windows limpio). Se incluye su licencia LGPL + el enlace a su
   fuente (cumplimiento LGPL: enlace dinámico + DLL reemplazable + oferta de fuente).
3. `instalar.ps1`/`.bat`: corre DokanSetup en silencio y copia `winext4.exe` + registra el
   auto-montaje. Opcional: envolver en un instalador (Inno Setup/WiX) para un solo `.exe`.

## Probar en Windows (el único paso que no es Linux)

En un Windows (VM o real), tras instalar el driver Dokan y **con consola de ADMINISTRADOR**
(abrir `\\.\PhysicalDriveN` exige elevación):

```powershell
# disco físico — auto-detecta la partición ext4 por su tipo GPT (Linux fs)
winext4.exe \\.\PhysicalDrive2 M:\
# si la auto-detección no aplica (p. ej. disco 4Kn), dar el offset a mano:
winext4.exe \\.\PhysicalDrive2 M:\ --offset 17408      # el «Offset» de Get-Partition
# una imagen en archivo (empieza en el byte 0):
winext4.exe C:\ruta\disco.img M:\
```

Aparece la unidad `M:` con el ext4 (solo lectura). Para desmontar: cierra la ventana de
`winext4` o expulsa la unidad. El host incluye un **adaptador de bloque** que (1) traduce el
offset de la partición dentro del disco y (2) alinea las lecturas al sector, porque un handle
`\\.\PhysicalDriveN` exige E/S alineada y el ext4 no empieza en el byte 0 del disco. Si algo del
FFI falla, se corrige y se recompila aquí.
