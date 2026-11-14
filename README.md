<!-- SPDX-FileCopyrightText: 2026 Juan Carlos Isaza -->
<!-- SPDX-License-Identifier: LicenseRef-Xiliux-Propietario -->
# winext4

Sistema de archivos **ext4 nativo para Windows** — montar y operar un volumen ext4 como
una letra de unidad, con paridad NTFS, **sin WSL y sin máquina virtual**. Código propio.

El diseño, las fases y las decisiones (incluida la de firma del driver) están en
[`DISENO.md`](DISENO.md).

## Estado (2026-11-05, versión 0.2.1 instalada)

- **Lectura** completa: extents de cualquier profundidad, indirectos clásicos (ext2/3),
  directorios lineales y htree, enlaces simbólicos, xattr. Montado a diario en Windows: el
  servicio monta el nvme real como `Z:` en **solo lectura** al arrancar.
- **Escritura**: toda
  transaccional por el **journal** (jbd2, modo ordenado) — 590 cortes de luz inyectados, todos
  consistentes tras reproducir. Crear/borrar/renombrar archivos y directorios, crecer/truncar con
  árbol de extents hasta profundidad 5 (probado con `e2fsck` hasta 2), xattr compartidos con refcount, directorios **htree**
  completos (partir hojas, partir nodos índice y añadir nivel, hasta el máximo sin `largedir`), y
  grupos sin inicializar (`BLOCK_UNINIT`/`INODE_UNINIT`) inicializados como el kernel.
- **Velocidad** (medida el 2026-11-05): lectura del nvme real 45 → ~200-460 MB/s (tramos
  contiguos en una lectura); crear archivos 36-58 ms crecientes → **~4,3 ms planos** (caché de
  directorios, búsqueda por nombre exacto, journal de 2 barreras y transacciones AGRUPADAS como
  jbd2: se confirman cada 1 s, con `FlushFileBuffers` y al desmontar; tras un corte se pierde
  como mucho ese segundo, el volumen queda siempre consistente). NTFS: ~0,7 ms.
- **Herramientas** (`winext4util`): `info`, `verificar`, `recuperar` (journal), `reparar` (con
  archivo para deshacer obligatorio en disco físico), `deshacer`, `rescatar`, `inodo`, `formatear`.
- **Servicio** de Windows (`winext4svc`) con montaje automático, e **instalador MSI** (WiX, sin
  acciones de script) que trae Dokan firmado.
- **Fuzzing** propio (`nucleo/tests/fuzz.rs`): 50 000 imágenes dañadas sin un pánico ni un cuelgue.
- Oráculo de todo el banco: `e2fsck -fn`, `debugfs` y el **kernel de Linux** (WSL) montando lo
  que escribimos. Mutantes en el código medido ponen el banco en rojo (D33).

- **Enlaces**: el motor crea enlaces simbólicos (rápidos y lentos) y duros, y los resuelve como
  el kernel (`buscar_siguiendo`, tope de 40). Desde Windows se crean con `winext4util enlazar`
  — **el Explorador y `mklink` no pueden**: Dokan 2.3.1 no pasa `FSCTL_SET_REPARSE_POINT` ni
  `FileLinkInformation` al sistema de archivos (medido y leído en su fuente). En el montaje, un
  enlace a ARCHIVO se sigue; uno a CARPETA, absoluto o roto se ve como archivo con su destino de
  contenido. **No se siguen enlaces a carpeta a propósito**: Dokan no deja anunciarlos como
  enlace, y entonces `Remove-Item -Recurse` entraba en ellos y vaciaba el destino (medido).

- **Permisos**: el atributo «solo lectura» de Windows ⇄ los bits de escritura de ext4, como
  Samba (`attrib +r` = `a-w`, `-r` = `u+w`); un archivo sin bits de escritura no se abre para
  escribir. Las ACL de Windows no se traducen a uid/gid: los usuarios de las dos máquinas no son
  los mismos y mapearlos sería inventar.

- **Disco físico con escritura**: detrás de un doble permiso explícito (`--autorizo-disco-fisico`
  o el modo `escritura-disco-fisico` del servicio), con comprobación previa: la RÁPIDA siempre
  (segundos) y la COMPLETA solo con motivo —errores posteriores a la última completa, más de 30
  días, o un fallo de la rápida—; tras una completa limpia se anota `s_lastcheck`, como e2fsck (con
  errores se niega) y apertura en EXCLUSIVA (con otro montaje abierto se niega). Probado SOLO en un
  VHDX conectado como disco (`windows/pruebas/e2e-disco-fisico.ps1`: 17/17 + `e2fsck`); el nvme de
  Juan sigue montado en solo lectura hasta que él cambie `montajes.txt`.

Lo que falta, en [`DISENO.md`](DISENO.md): firma Authenticode (aplazada: la regla ASR de Defender
está en auditoría).

## Estructura

- `nucleo/` — el motor ext4 en-disco: lectura, escritura transaccional, journal, verificar,
  reparar, formatear. Rust puro, sin dependencias.
- `windows/` — host **Dokan** (`winext4.exe`): monta disco o imagen como letra de unidad.
- `util/` — `winext4util`, las herramientas de mantenimiento y recuperación.
- `servicio/` — `winext4svc`, montaje automático al arrancar (`montajes.txt`).
- `instalador/` — MSI y bundle con Dokan (`construir.ps1`).
- `fuse/` — host **FUSE** (Linux): monta el motor como un FS real (no compila en Windows; el
  banco de Windows lo excluye: `--exclude winext4-fuse`).

## Windows: fácil de instalar, gratis y propietario

El binding de Windows usa **Dokan** (LGPL, enlace dinámico → producto propietario sin pagar; su
`dokan2.sys` viene firmado). El host (`winext4.exe`, solo lectura hoy) se compila con el toolchain
`x86_64-pc-windows-gnu` —cross desde Linux o **nativo en Windows** (el toolchain gnu de rustup trae
`ld`+`gcc`)— y habla con Dokan a través de **rustkan** (`D:\rustkan`, nuestra capa Rust sobre Dokan, que
carga `dokan2.dll` en ejecución SOLO desde System32: sin import-lib ni `build.rs`). Monta un **disco físico crudo** detectando la
partición ext4 por su tipo GPT (Linux-fs) y leyendo con un **adaptador alineado al sector**;
`--offset <bytes>` fuerza el arranque si hace falta. `windows/empaquetar.sh` arma un
**`dist/winext4-windows.zip` autocontenido** (`winext4.exe` + `DokanSetup.exe` firmado +
`instalar.ps1`): en un **Windows nuevo** se descomprime y se corre `instalar.ps1`, sin descargar nada.

**PROBADO en Windows 11 el 2026-11-04** (D28): monta el disco físico real (`\\.\PhysicalDrive2`,
partición Linux-fs en el offset 17408) como unidad `Z:`, lista 293 entradas —incluidos ocultos— y
**lee el contenido de archivos** (`conocimiento/MAPA.md`), solo lectura, tanto por auto-detección GPT
como por `--offset`. Ver `windows/COMPILAR.md`. Un driver de kernel propio solo haría falta para
ARRANCAR desde ext4.

## Probar

```bash
# Linux
cargo test --workspace --release
cargo clippy --workspace --all-targets -- -D warnings
# Windows (e2fsprogs y el kernel vía WSL Debian)
cargo +stable-x86_64-pc-windows-gnu test --workspace --exclude winext4-fuse --target x86_64-pc-windows-gnu --release
```

Sin e2fsprogs las pruebas de integración se saltan DICIÉNDOLO («SALTADA»); un banco verde con
saltadas no midió nada (D30). Fuzz largo: `WINEXT4_FUZZ_CASOS=50000 cargo test --release --test fuzz -- --ignored`.
