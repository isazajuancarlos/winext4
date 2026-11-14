<!-- SPDX-FileCopyrightText: 2026 Juan Carlos Isaza -->
<!-- SPDX-License-Identifier: LicenseRef-Xiliux-Propietario -->
# winext4 — diseño

**Qué es.** Un sistema de archivos **ext4 nativo para Windows**, con paridad de primera
clase con NTFS: Windows monta un volumen ext4 como una letra de unidad y opera sobre él
—Explorador, copiar/pegar, permisos, formatear, comprobar— **como si fuera NTFS**, sin
máquina virtual, sin WSL, sin invitado Linux. Todo el código es **propio** (titularidad
Juan Carlos Isaza), salvo el shim de kernel que se decida reusar (ver «Firma»).

Decidido por Juan el **2026-11-04**. Nombre: `winext4` (Windows + ext4).

## Por qué un driver nativo, y no cuchavira

[`cuchavira`](../cuchavira) ya da ext4 R/W en Windows **orquestando el kernel Linux de
WSL** (una VM ligera; `\\wsl.localhost` es 9P) — maduro y sin riesgo de corrupción, pero
**necesita WSL instalado y no es nativo ni arrancable**. `winext4` es el otro producto: el
acceso **a nivel de bloque**, sin VM, sobre un disco sin SO, en una máquina limpia. Las dos
cosas son legítimas y conviven; `winext4` no reemplaza a cuchavira.

La objeción de octubre (un motor ext4 R/W propio son ~45k líneas críticas; los motores
maduros lwext4/rust-fs-ext4 son GPL y contaminarían lo propietario) se resuelve aquí por la
vía que Juan eligió: **lo escribimos nosotros** (nada de enlazar GPL), aceptando el coste
del motor propio a conciencia, y **validándolo contra `e2fsck` en Linux** para que el riesgo
de corrupción se mida antes de tocar un disco real en Windows.

## Arquitectura — el motor es el 90% y la costura de Windows es delgada

Espejo del reparto de WinBtrfs (referencia estudiada en `/mnt/data/analisis/winbtrfs`,
NO derivada: su código es LGPL, el nuestro es propio):

| Pieza | Papel | Lenguaje | Estado |
|---|---|---|---|
| **`nucleo`** — motor ext4 en-disco (superbloque, grupos, inodos, extents, htree, indirectos, jbd2), agnóstico de plataforma | el 90%, donde vive el riesgo | Rust (D38), sin dependencias | **LECTURA ✅** (F0+F1); escritura = F3 |
| **`fuse`** — host FUSE (Linux): `lookup/getattr/readdir/read/readlink` → el motor | binding de plataforma #1 | Rust + `fuser` | **✅ el kernel lo monta y lee** |
| **`windows`** — host de Windows sobre **Dokan** (el MISMO binding que FUSE) | binding de plataforma #2 | Rust + FFI a `dokan2.dll` | **monta, lista y lee en Windows ✅ (PROBADO 2026-11-04)**: disco físico real, auto-detección GPT + adaptador alineado al sector |
| **herramientas** — `mkwinext4` (formatear), `winext4fsck` (comprobar) | formato/chequeo | Rust, modo usuario | pendiente |
| **`winext4.sys`** — FSD de kernel (solo si se quiere ARRANCAR desde ext4) | opción futura | kernel (C/WDK) | opcional, no en la vía de envío |

**El motor (`nucleo`) es común a cualquier costura** y es donde vive TODO el riesgo de
corrupción, por eso es lo primero y se prueba ENTERO en Linux contra imágenes ext4 reales
con `e2fsck`/`dumpe2fs` de oráculo (D28/D30/D33). Ninguna decisión de Windows lo condiciona.

## La vía a «driver listo»: gratis, propietario, autocontenido

El temor era la **firma de un driver de kernel** (cert EV + Microsoft: dinero + cuenta). La vía
elegida (decidida por Juan, 2026-11-04) la disuelve con **Dokan** como binding de modo usuario:

- **Dokan es LGPL + MIT, no GPL pura.** La LGPL **permite producto propietario** si el DLL se
  enlaza **dinámicamente** y es reemplazable → **winext4 sigue propietario y no cuesta nada**
  (WinFsp exigía US$2.400/3a para no abrir el código; Dokan no). El motor `nucleo` sigue 100%
  propio; Dokan es solo el adaptador al SO, un DLL aparte.
- **El `dokan2.sys` viene firmado** (por ISLOG, en cada release) → carga en un **Windows recién
  instalado**. Lo bundleamos en el instalador (con su licencia LGPL + oferta de fuente): el
  usuario **no instala nada aparte**. Un FSD de kernel propio solo haría falta para ARRANCAR
  desde ext4 (fuera del objetivo), y ahí sí volvería la firma de Microsoft.

**El binding, y toda la cadena de Windows, se COMPILA desde Linux — probado:**

1. El binding ya funciona por FUSE: el **kernel de Linux monta y lee** el motor
   (`fuse/tests/montaje.rs`). Dokan es el mismo modelo de mapeo de operaciones.
2. **Cross-compila aquí**: `x86_64-pc-windows-gnu` + `mingw64-gcc`. El `.exe` de `windows/`
   habla con Dokan a través de **rustkan** (2026-11-05: `../rustkan`, que carga `dokan2.dll` en
   ejecución desde System32; antes era una import-lib generada con `dlltool`). Ver
   `windows/COMPILAR.md`.
3. **Lo ÚNICO atado a Windows** es *probar que monta* (compilar ≠ funcionar, D28): un arranque
   en cualquier Windows con el driver instalado por nuestro propio instalador. No es un entorno
   de build que montar, es un check.

Firma de distribución (aparte, barata, no bloquea): Authenticode OV del instalador para evitar
el aviso de SmartScreen.

## Lenguaje (D38)

El motor y las herramientas de modo usuario nacen en **Rust**. El FSD de kernel es el único
punto donde «¿por qué no Rust?» tiene respuesta real: el WDK de Windows es C/C++ y un FSD de
kernel en Rust es territorio inexplorado. Se decide en su fase, con el motivo escrito, no por
inercia.

## Fases

- **F0 — superbloque + geometría** ✅: superbloque, características, tamaño de bloque, geometría
  de grupos; validado contra ext4 real.
- **F1 — lectura** ✅ **COMPLETA**: descriptores de grupo, inodos, **extents** (ext4, incl.
  profundidad > 0) y **bloques indirectos** (ext2/3, directo+simple+doble), directorios (lineal
  y htree — el bloque de índice se autodescarta por su `rec_len`), **enlaces simbólicos**, leer
  archivo y resolver rutas. Validado con `debugfs` (byte a byte): ext4 real, ext2 real (5 MB,
  indirecto doble), dir multi-bloque (2000), symlink; + unit del autodescarte htree y extents
  de profundidad 1. **Caveats (D30):** htree validado por CONSTRUCCIÓN (debugfs no lo fabrica);
  indirecto triple y extents muy profundos sin probar a escala; faltan archivos especiales y
  xattr/ACL.
- **F-MONTAJE (FUSE, Linux)** ✅: el host `fuse` expone el motor y **el kernel real lo monta y
  lee** (`ls`/`cat`/`cmp`/`readlink`). Es el binding, probado — y el mismo modelo que WinFsp.
- **F2 — journal (jbd2)**: entenderlo y reproducirlo (recuperación), precondición de escribir.
- **F3 — escritura** 🟡 (motor COMPLETO salvo journal y partir hojas htree; aún NO cableada al
  host ni habilitada sobre un disco real):
  - **sumas crc32c** de todos los metadatos (`csum.rs`), recalculadas idénticas a las de
    `mkfs.ext4` (`tests/sumas_reales.rs`);
  - **bitmaps, descriptores y contadores**, asignar/liberar bloques e inodos (`escritura.rs`);
    `abrir_escritura` se NIEGA ante lo que no sabe mantener (journal por recuperar, inline_data,
    encrypt, casefold, meta_bg, bigalloc, gdt_csum crc16, FS sucio, suma rota) y deja el FS
    marcado «no limpio» hasta `cerrar_escritura`;
  - **crecer, huecos dispersos, truncar**; árbol de extents reconstruido hasta profundidad 1
    (1360 fragmentos); orden anti-corrupción (inodo antes de liberar);
  - **directorios** (`directorios.rs`): crear archivo/directorio, borrar, borrar directorio vacío,
    renombrar/mover (con `..` y enlaces); **htree**: hash `half_md4`/`tea` (`hash.rs`) validado
    contra la colocación de `e2fsck -D` en 400 nombres.
  - Banco: `e2fsck -fn` limpio + `debugfs` (lector independiente) tras cada operación; mutantes
    en el código medido (contador de libres, suma de bitmap, hoja htree, enlaces del padre)
    ponen el banco en ROJO. Corre en Windows con e2fsprogs vía WSL (`tests/comun`).
  - **Resta**: partir hojas htree llenas (hoy se niega), profundidad de extents > 1, extents no
    inicializados, symlinks/xattr al escribir, **journal (jbd2, F2)**, y cablearlo en el host
    Dokan detrás de una bandera explícita.
- **F4 — host Windows sobre Dokan** ✅ **(lectura, PROBADO 2026-11-04)**: callbacks de lectura
  (ZwCreateFile, ReadFile, FindFiles, GetFileInformation, GetVolumeInformation, GetDiskFreeSpace)
  mapeados al motor; escrituras rechazadas (volumen RO). Compila cross desde Linux **y nativo en
  Windows** (toolchain gnu de rustup) a un `.exe` que importa `dokan2.dll` (clippy `-D warnings`
  limpio). **Añadido el adaptador de bloque** (`DispositivoBloque`): un handle `\\.\PhysicalDriveN`
  exige E/S alineada al sector y el ext4 empieza en el offset de SU partición, no en 0 — el adaptador
  traduce el offset (auto-detectado de la GPT por el tipo Linux-fs, o `--offset <bytes>`) y alinea
  las lecturas, sin tocar `nucleo`. **Probado montando `\\.\PhysicalDrive2` (offset 17408) en `Z:`**:
  293 entradas listadas (con ocultos) y contenido de archivo leído (`conocimiento/MAPA.md`), por
  auto-detección y por offset. Dokan LGPL → gratis y propietario.
- **F5 — empaque «fácil de instalar»** ✅ (lectura): `windows/empaquetar.sh` arma DESDE LINUX un
  `dist/winext4-windows.zip` AUTOCONTENIDO (nuestro `.exe` + `DokanSetup.exe` firmado + `instalar.ps1`
  + licencia LGPL); en un Windows nuevo = descomprimir y correr `instalar.ps1`. Falta pulir
  (instalador de un solo `.exe` con Inno/WiX, Authenticode OV) y `mkwinext4`/`winext4fsck`.
- **(opcional) FSD de kernel** `winext4.sys`: solo si se quiere ARRANCAR desde ext4; ahí sí
  entra la firma de Microsoft. No está en la vía de envío.

## Hoja de ruta al 100 % (decidida 2026-11-05, «todo, paso a paso, cada pieza probada»)

Orden: cada pieza se apoya en la anterior y no empieza hasta que la anterior pasa su banco
(`e2fsck` + `debugfs` + mutante en el código medido). Referencias de lo que NO hay que repetir:
investigación del 2026-11-05 (Ext2Fsd corrompía 64bit; Ext4Fsd pierde datos por caché, #82,
y da pantallazos; lwext4 resucitaba transacciones viejas al reproducir el journal, #86, y
sobrescribía la raíz htree con un bitmap dañado, #175).

| # | Pieza | Qué la valida |
|---|---|---|
| A1 | **Recuperar el journal** (jbd2: escaneo, revocación, reproducción; csum v2/v3) — monta discos que Linux dejó sucios | diferencial bit a bit contra la reproducción de `e2fsck` sobre journals que escribe el KERNEL (WSL + apagado forzado) |
| B1 | **`winext4fsck` verificar** (solo lectura): sumas, bitmaps contra lo realmente usado, contadores, árbol de directorios, enlaces, htree | pareja: imágenes sanas → 0; corrupciones plantadas → las mismas que ve `e2fsck` |
| A2 | **Escribir A TRAVÉS del journal** (metadatos, modo ordenado) — un corte deja el disco consistente | cortes inyectados en cada paso de cada operación → tras reproducir, `e2fsck` limpio |
| — | **Desbloquear escritura en disco físico** ✅ preparado (2026-11-05, decisión de Juan: «preparar y probar en un VHDX»): doble permiso, verificación previa, apertura exclusiva; servicio con modo `escritura-disco-fisico`. Activarlo en el nvme es otra decisión suya | E2E en VHDX como `\\.\PhysicalDriveN` (guarda dura por BusType y ruta): 17/17 + `e2fsck` de la partición volcada |
| E | Completar la escritura: refcount de bloques xattr ✅, extents de profundidad > 1 ✅, **partir nodos htree y añadir nivel ✅** (2026-11-05: kernel busca los 2571 nombres por el índice y escribe encima; mutantes de corte y de nivel en rojo), **grupos `BLOCK_UNINIT`/`INODE_UNINIT` ✅** (antes se saltaban: un volumen nuevo se quedaba «sin inodos» al llenar el grupo 0; ahora se inicializan como el kernel, con guarda que se niega si el descriptor no cuadra); **symlinks y enlaces duros ✅** (motor + `winext4util enlazar`; el montaje sigue los enlaces a archivo y NO los de carpeta: con Dokan no se pueden anunciar como enlace y un borrado recursivo vaciaba el destino; leer un symlink rápido desde Windows devolvía bloques ajenos o error de E/S, corregido); **permisos ✅** (solo lectura ⇄ bits de escritura, como Samba; E2E con `stat` del kernel y mutante en rojo); `orphan_file` ya convive (mkfs 1.47 lo activa por defecto y todo el banco corre sobre esos volúmenes) | `e2fsck` + `debugfs` + kernel + simulación + E2E en el montaje |
| C | **`mkwinext4`** (formatear desde Windows) | `e2fsck` limpio + Linux lo monta |
| B2 | **`winext4fsck` reparar** lo seguro (contadores, bitmaps, huérfanos); lo grave → recomendar `e2fsck` | imágenes rotas → reparadas → `e2fsck` limpio |
| D | Herramientas: `info` (estilo dumpe2fs), etiqueta/UUID, **rescate** (copiar archivos de un FS dañado ignorando lo roto) | contra imágenes reales y dañadas |
| F | **Servicio de Windows** con montaje automático al arrancar | E2E reinicio |
| G | **Instalador MSI** (WiX) con Dokan firmado, menú Inicio, desinstalador, servicio. **SIN acciones personalizadas VBScript/JScript** (el equipo de Juan tiene VBScript desactivado por protección, y es tendencia en Windows endurecidos): solo tablas estándar (ServiceInstall/ServiceControl) y, si hace falta lógica, un `.exe` nativo nuestro | instalar/desinstalar en limpio, con VBScript desactivado |
| H | Fuzzing de lectura y escritura sobre imágenes dañadas (sin pánicos) | horas de fuzz sin caída |
| V | **Velocidad** ✅ (2026-11-05): lectura en tramos; caché de directorios + `FindFilesWithPattern`; journal de 2 barreras con transacciones acumuladas (vaciado antes de reutilizar para datos un bloque con copia en el registro); AGRUPADO de transacciones (ventana 1 s, punto de guardado por operación con registro para deshacer, lo liberado no se reutiliza hasta confirmar). Crear: 33 → 4,3 ms | `cortes.rs` (cada escritura) + `journal_acumulado.rs` (vueltas del registro, contenido tras reutilizar) + `agrupado.rs` (cortes con grupos, deshacer solo la op fallida, lo liberado intacto tras corte, frontera de durabilidad) + `e2e-durable.ps1` (proceso matado); mutantes M1, M2, G1-G3, D1, D2 en rojo |

**Regla de oro (del aprendizaje de cuchavira):** la escritura sobre un disco real no se
habilita hasta que F3 pase el banco de `e2fsck` sin una sola imagen corrupta. RO primero,
siempre.
