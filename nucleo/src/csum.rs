// SPDX-FileCopyrightText: 2026 Juan Carlos Isaza
// SPDX-License-Identifier: LicenseRef-Xiliux-Propietario
//! Sumas de verificación de metadatos de ext4 (`RO_COMPAT_METADATA_CSUM`): **crc32c**
//! (Castagnoli) en la forma CRUDA del kernel —`crc32c_le(crc, datos)` sin inversión inicial
//! ni final—, y la fórmula de cada estructura (superbloque, descriptor de grupo, bitmaps,
//! inodo, bloque de extents, bloque de directorio).
//!
//! Escribir un metadato sin rehacer su suma deja un FS que `e2fsck` rechaza y que el kernel
//! se niega a leer: por eso cada fórmula se valida contra imágenes REALES de `mkfs.ext4`
//! (`tests/sumas_reales.rs`), no contra lo que uno cree que dice la documentación.

/// Tabla de crc32c reflejado (polinomio 0x82F63B78), calculada en compilación.
const TABLA: [u32; 256] = {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { (c >> 1) ^ 0x82F6_3B78 } else { c >> 1 };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
};

/// `crc32c_le` del kernel: continúa `crc` con `datos`, sin invertir al entrar ni al salir.
#[must_use]
pub fn crc32c(mut crc: u32, datos: &[u8]) -> u32 {
    for &b in datos {
        crc = TABLA[((crc ^ b as u32) & 0xFF) as usize] ^ (crc >> 8);
    }
    crc
}

/// Offset de `s_checksum` en el superbloque (los 4 últimos bytes de sus 1024).
pub const SB_OFF_CSUM: usize = 0x3FC;
/// Offset de `s_checksum_seed` (válido con `INCOMPAT_CSUM_SEED`).
pub const SB_OFF_SEMILLA: usize = 0x270;
/// Offset de `bg_checksum` en un descriptor de grupo.
pub const GD_OFF_CSUM: usize = 0x1E;
/// Offset de `l_i_checksum_lo` en un inodo.
pub const INO_OFF_CSUM_LO: usize = 0x7C;
/// Offset de `i_checksum_hi` en un inodo (dentro de la zona «extra»).
pub const INO_OFF_CSUM_HI: usize = 0x82;
/// Offset de `i_extra_isize` en un inodo.
pub const INO_OFF_EXTRA: usize = 0x80;
/// Offset de `i_generation`.
pub const INO_OFF_GEN: usize = 0x64;

/// Suma del superbloque: `crc32c(~0, sb[..0x3FC])`.
#[must_use]
pub fn superbloque(sb: &[u8]) -> u32 {
    crc32c(!0, &sb[..SB_OFF_CSUM])
}

/// Semilla de las sumas del volumen: `s_checksum_seed` si el FS la guarda
/// (`INCOMPAT_CSUM_SEED`), si no `crc32c(~0, uuid)`.
#[must_use]
pub fn semilla(sb: &[u8], guarda_semilla: bool, uuid: &[u8; 16]) -> u32 {
    if guarda_semilla {
        u32::from_le_bytes(sb[SB_OFF_SEMILLA..SB_OFF_SEMILLA + 4].try_into().expect("4 bytes"))
    } else {
        crc32c(!0, uuid)
    }
}

/// Suma (16 bits) de un descriptor de grupo: semilla + número de grupo + el descriptor con
/// su campo de suma a cero.
#[must_use]
pub fn descriptor(semilla: u32, grupo: u32, desc: &[u8]) -> u16 {
    let mut c = crc32c(semilla, &grupo.to_le_bytes());
    c = crc32c(c, &desc[..GD_OFF_CSUM]);
    c = crc32c(c, &[0, 0]);
    if desc.len() > GD_OFF_CSUM + 2 {
        c = crc32c(c, &desc[GD_OFF_CSUM + 2..]);
    }
    (c & 0xFFFF) as u16
}

/// Suma de un bitmap (de bloques o de inodos): `crc32c(semilla, bitmap[..bytes])`, con
/// `bytes` = bloques (o inodos) por grupo / 8.
#[must_use]
pub fn bitmap(semilla: u32, bitmap: &[u8], bytes: usize) -> u32 {
    crc32c(semilla, &bitmap[..bytes])
}

/// Semilla propia de un inodo: `crc32c(crc32c(semilla, ino), generación)`. La usan su propia
/// suma y las de sus bloques de extents y de directorio.
#[must_use]
pub fn semilla_inodo(semilla: u32, ino: u32, generacion: u32) -> u32 {
    crc32c(crc32c(semilla, &ino.to_le_bytes()), &generacion.to_le_bytes())
}

/// ¿El inodo tiene espacio para `i_checksum_hi`? (`i_extra_isize` ≥ 4 en un inodo > 128.)
#[must_use]
pub fn inodo_tiene_hi(inodo: &[u8]) -> bool {
    inodo.len() > 128 && u16::from_le_bytes([inodo[INO_OFF_EXTRA], inodo[INO_OFF_EXTRA + 1]]) >= 4
}

/// Suma de un inodo (32 bits; se guardan los 16 bajos en `lo` y, si cabe, los altos en `hi`):
/// semilla del inodo + el inodo entero con sus dos campos de suma a cero.
#[must_use]
pub fn inodo(semilla: u32, ino: u32, inodo: &[u8]) -> u32 {
    let gen = u32::from_le_bytes(inodo[INO_OFF_GEN..INO_OFF_GEN + 4].try_into().expect("4 bytes"));
    let mut c = semilla_inodo(semilla, ino, gen);
    let mut copia = inodo.to_vec();
    copia[INO_OFF_CSUM_LO..INO_OFF_CSUM_LO + 2].fill(0);
    if inodo_tiene_hi(inodo) {
        copia[INO_OFF_CSUM_HI..INO_OFF_CSUM_HI + 2].fill(0);
    }
    c = crc32c(c, &copia);
    if !inodo_tiene_hi(inodo) {
        c &= 0xFFFF;
    }
    c
}

/// La suma guardada en un inodo (lo + hi si existe).
#[must_use]
pub fn inodo_guardada(inodo: &[u8]) -> u32 {
    let lo = u16::from_le_bytes([inodo[INO_OFF_CSUM_LO], inodo[INO_OFF_CSUM_LO + 1]]) as u32;
    if inodo_tiene_hi(inodo) {
        let hi = u16::from_le_bytes([inodo[INO_OFF_CSUM_HI], inodo[INO_OFF_CSUM_HI + 1]]) as u32;
        lo | (hi << 16)
    } else {
        lo
    }
}

/// Escribe en `inodo` su suma correcta.
pub fn sellar_inodo(semilla: u32, ino: u32, inodo: &mut [u8]) {
    let c = self::inodo(semilla, ino, inodo);
    inodo[INO_OFF_CSUM_LO..INO_OFF_CSUM_LO + 2].copy_from_slice(&((c & 0xFFFF) as u16).to_le_bytes());
    if inodo_tiene_hi(inodo) {
        inodo[INO_OFF_CSUM_HI..INO_OFF_CSUM_HI + 2].copy_from_slice(&((c >> 16) as u16).to_le_bytes());
    }
}

/// Offset de la cola (`ext4_extent_tail`) de un bloque de extents: tras `eh_max` entradas.
#[must_use]
pub fn extent_off_cola(bloque: &[u8]) -> usize {
    let eh_max = u16::from_le_bytes([bloque[4], bloque[5]]) as usize;
    12 + 12 * eh_max
}

/// Suma de un bloque de extents (no la raíz en `i_block`, que va dentro de la del inodo).
#[must_use]
pub fn bloque_extents(semilla_inodo: u32, bloque: &[u8]) -> u32 {
    crc32c(semilla_inodo, &bloque[..extent_off_cola(bloque)])
}

/// Sella un bloque de extents con su suma.
pub fn sellar_bloque_extents(semilla_inodo: u32, bloque: &mut [u8]) {
    let off = extent_off_cola(bloque);
    let c = bloque_extents(semilla_inodo, bloque);
    bloque[off..off + 4].copy_from_slice(&c.to_le_bytes());
}

/// Tamaño de la cola de suma de un bloque hoja de directorio.
pub const DIR_COLA: usize = 12;

/// ¿Termina el bloque de directorio en una cola de suma (`inode`=0, `rec_len`=12,
/// `name_len`=0, `file_type`=0xDE)?
#[must_use]
pub fn dir_tiene_cola(bloque: &[u8]) -> bool {
    let i = bloque.len() - DIR_COLA;
    bloque[i..i + 4] == [0, 0, 0, 0] && bloque[i + 4..i + 6] == [12, 0] && bloque[i + 6] == 0 && bloque[i + 7] == 0xDE
}

/// Suma de un bloque hoja de directorio: todo salvo la cola.
#[must_use]
pub fn bloque_dir(semilla_inodo: u32, bloque: &[u8]) -> u32 {
    crc32c(semilla_inodo, &bloque[..bloque.len() - DIR_COLA])
}

/// Escribe la cola de suma (estructura + suma) al final de un bloque hoja de directorio.
pub fn sellar_bloque_dir(semilla_inodo: u32, bloque: &mut [u8]) {
    let i = bloque.len() - DIR_COLA;
    bloque[i..i + 4].fill(0);
    bloque[i + 4..i + 6].copy_from_slice(&12u16.to_le_bytes());
    bloque[i + 6] = 0;
    bloque[i + 7] = 0xDE;
    let c = bloque_dir(semilla_inodo, bloque);
    bloque[i + 8..i + 12].copy_from_slice(&c.to_le_bytes());
}

/// Suma de un bloque ÍNDICE de directorio htree (`dx_root` o `dx_node`). `base` es el offset
/// de su `dx_countlimit` (`limit`, `count`); la cola `dx_tail` (reservado + suma) va tras
/// `limit` entradas de 8 bytes. Se suma el bloque hasta la última entrada USADA, luego el
/// campo reservado de la cola y luego la propia suma A CERO (4 bytes). Devuelve (offset de la
/// cola, suma).
#[must_use]
pub fn bloque_dx(semilla_inodo: u32, bloque: &[u8], base: usize) -> (usize, u32) {
    let limite = u16::from_le_bytes([bloque[base], bloque[base + 1]]) as usize;
    let cuenta = u16::from_le_bytes([bloque[base + 2], bloque[base + 3]]) as usize;
    let cola = base + limite * 8;
    let c = crc32c(semilla_inodo, &bloque[..base + cuenta * 8]);
    let c = crc32c(c, &bloque[cola..cola + 4]);
    (cola, crc32c(c, &[0, 0, 0, 0]))
}

/// Sella un bloque índice htree con su suma (si el bloque tiene sitio para la cola).
pub fn sellar_bloque_dx(semilla_inodo: u32, bloque: &mut [u8], base: usize) {
    let (cola, c) = bloque_dx(semilla_inodo, bloque, base);
    bloque[cola + 4..cola + 8].copy_from_slice(&c.to_le_bytes());
}

/// Suma de un bloque de atributos extendidos: semilla + nº de bloque (64 bits LE) + el bloque
/// con `h_checksum` (offset 0x10) a cero. Validada contra `debugfs ea_set` (util `inodo`).
#[must_use]
pub fn bloque_xattr(semilla: u32, bloque_nr: u64, blk: &[u8]) -> u32 {
    let c = crc32c(semilla, &bloque_nr.to_le_bytes());
    let c = crc32c(c, &blk[..0x10]);
    let c = crc32c(c, &[0, 0, 0, 0]);
    crc32c(c, &blk[0x14..])
}

#[cfg(test)]
mod pruebas {
    use super::*;

    #[test]
    fn crc32c_vector_conocido() {
        // Vector estándar de crc32c (con las inversiones que el kernel deja al llamante):
        // crc32c("123456789") = 0xE3069283.
        assert_eq!(!crc32c(!0, b"123456789"), 0xE306_9283);
        // Pareja (D33): un byte distinto da otra suma.
        assert_ne!(!crc32c(!0, b"123456788"), 0xE306_9283);
    }
}
