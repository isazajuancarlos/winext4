# winext4

**Native ext4 for Windows** — mount and operate an ext4 volume as a Windows drive
letter, with NTFS-level parity (Explorer, format, check), **no WSL and no virtual
machine**. Own code.

**English** · [Español](#español)

---

## What it is

`winext4` reads and writes ext4 at the **block level** on Windows — not by
orchestrating a Linux kernel, but with an on-disk ext4 engine written from
scratch. The engine is the hard 90% (where filesystem corruption would live), and
it is validated against `e2fsck` / `debugfs` as oracles on Linux before it ever
touches Windows.

**Status: in active development.** The engine reads ext4 end to end (extents,
classic indirect blocks, htree directories, symlinks) and performs transactional
writes via a jbd2-style journal, validated against thousands of injected
power-cut scenarios. The Windows side mounts a real volume as a drive letter.

## Why it's hard, and why it's done from scratch

Mature ext4 engines are GPL; a native Windows filesystem driver has a signing
gate of its own. Writing the engine ourselves resolves the first (nothing GPL is
linked) and lets the on-disk format be verified against the reference tools at
every step.

## This repository

Public home of the project. `winext4` is **proprietary**; its source is not
published here. Follow progress or get in touch at
**[xiliux.com](https://xiliux.com)**.

---

## Español

**ext4 nativo para Windows** — montar y operar un volumen ext4 como una letra de
unidad, con paridad NTFS (Explorador, formatear, comprobar), **sin WSL y sin
máquina virtual**. Código propio.

### Qué es

`winext4` lee y escribe ext4 a **nivel de bloque** en Windows —no orquestando un
kernel de Linux, sino con un motor ext4 en-disco escrito desde cero—. El motor es
el 90% difícil (donde viviría la corrupción del sistema de archivos), y se valida
contra `e2fsck` / `debugfs` como oráculos en Linux antes de tocar Windows.

**Estado: en desarrollo activo.** El motor lee ext4 de punta a punta (extents,
indirectos clásicos, directorios htree, symlinks) y escribe de forma
transaccional por un journal estilo jbd2, validado contra miles de cortes de luz
inyectados. El lado Windows monta un volumen real como letra de unidad.

### Por qué es difícil, y por qué se hace desde cero

Los motores ext4 maduros son GPL; un driver de sistema de archivos nativo de
Windows tiene además su propia puerta de firma. Escribir el motor nosotros
resuelve lo primero (no se enlaza nada GPL) y permite verificar el formato
en-disco contra las herramientas de referencia en cada paso.

### Este repositorio

Casa pública del proyecto. `winext4` es **propietario**; su código no se publica
aquí. Sigue el avance o escríbenos en **[xiliux.com](https://xiliux.com)**.

---

© 2026 Juan Carlos Isaza. Proprietary — all rights reserved.
