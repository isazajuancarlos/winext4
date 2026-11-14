// SPDX-FileCopyrightText: 2026 Juan Carlos Isaza
// SPDX-License-Identifier: LicenseRef-Xiliux-Propietario
//! El **descriptor de grupo de bloques** (`ext4_group_desc`): dónde están los bitmaps y la
//! tabla de inodos del grupo, cuánto queda libre, y sus banderas de inicialización perezosa.
//! Con `64BIT` mide 64 bytes y cada cantidad se parte en `_lo` (bytes 0..32) y `_hi`
//! (bytes 32..64); sin `64BIT`, 32 bytes y solo `_lo`.
//!
//! Se guarda CRUDO y se lee/escribe campo a campo: así un campo que el motor no entiende
//! (`bg_exclude_bitmap`, reservados) viaja intacto de la lectura a la escritura.

/// `EXT4_BG_INODE_UNINIT`: bitmap de inodos sin inicializar (todos libres).
pub const BG_INODE_UNINIT: u16 = 0x1;
/// `EXT4_BG_BLOCK_UNINIT`: bitmap de bloques sin inicializar (se calcula, no se lee).
pub const BG_BLOCK_UNINIT: u16 = 0x2;
/// `EXT4_BG_INODE_ZEROED`: la tabla de inodos ya está a cero en disco.
pub const BG_INODE_ZEROED: u16 = 0x4;

/// Un descriptor de grupo crudo, con acceso por campo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Descriptor {
    /// Los `tamano_desc` bytes tal cual están en disco.
    pub crudo: Vec<u8>,
}

impl Descriptor {
    fn ancho(&self) -> bool {
        self.crudo.len() >= 64
    }

    fn u16(&self, off: usize) -> u16 {
        u16::from_le_bytes([self.crudo[off], self.crudo[off + 1]])
    }

    fn u32(&self, off: usize) -> u32 {
        u32::from_le_bytes(self.crudo[off..off + 4].try_into().expect("4 bytes"))
    }

    fn pon16(&mut self, off: usize, v: u16) {
        self.crudo[off..off + 2].copy_from_slice(&v.to_le_bytes());
    }

    /// Un campo de 32+32 bits (`lo` en `off`, `hi` en `off_hi` si el descriptor es ancho).
    /// Solo lectura: la escritura nunca mueve bitmaps ni tablas de inodos.
    fn l64(&self, off: usize, off_hi: usize) -> u64 {
        let hi = if self.ancho() { self.u32(off_hi) as u64 } else { 0 };
        self.u32(off) as u64 | (hi << 32)
    }

    /// Un campo de 16+16 bits.
    fn l32(&self, off: usize, off_hi: usize) -> u32 {
        let hi = if self.ancho() { self.u16(off_hi) as u32 } else { 0 };
        self.u16(off) as u32 | (hi << 16)
    }

    fn pon_l32(&mut self, off: usize, off_hi: usize, v: u32) {
        self.pon16(off, v as u16);
        if self.ancho() {
            self.pon16(off_hi, (v >> 16) as u16);
        }
    }

    /// Bloque del bitmap de bloques.
    #[must_use]
    pub fn bitmap_bloques(&self) -> u64 {
        self.l64(0x00, 0x20)
    }

    /// Bloque del bitmap de inodos.
    #[must_use]
    pub fn bitmap_inodos(&self) -> u64 {
        self.l64(0x04, 0x24)
    }

    /// Primer bloque de la tabla de inodos.
    #[must_use]
    pub fn tabla_inodos(&self) -> u64 {
        self.l64(0x08, 0x28)
    }

    /// Bloques libres del grupo.
    #[must_use]
    pub fn bloques_libres(&self) -> u32 {
        self.l32(0x0C, 0x2C)
    }

    /// Fija los bloques libres del grupo.
    pub fn pon_bloques_libres(&mut self, v: u32) {
        self.pon_l32(0x0C, 0x2C, v);
    }

    /// Inodos libres del grupo.
    #[must_use]
    pub fn inodos_libres(&self) -> u32 {
        self.l32(0x0E, 0x2E)
    }

    /// Fija los inodos libres del grupo.
    pub fn pon_inodos_libres(&mut self, v: u32) {
        self.pon_l32(0x0E, 0x2E, v);
    }

    /// Directorios en uso del grupo.
    #[must_use]
    pub fn dirs_usados(&self) -> u32 {
        self.l32(0x10, 0x30)
    }

    /// Fija los directorios en uso del grupo.
    pub fn pon_dirs_usados(&mut self, v: u32) {
        self.pon_l32(0x10, 0x30, v);
    }

    /// Banderas `bg_flags`.
    #[must_use]
    pub fn banderas(&self) -> u16 {
        self.u16(0x12)
    }

    /// Fija `bg_flags`.
    pub fn pon_banderas(&mut self, v: u16) {
        self.pon16(0x12, v);
    }

    /// Suma del bitmap de bloques (`lo` + `hi` si es ancho).
    #[must_use]
    pub fn csum_bitmap_bloques(&self) -> u32 {
        self.l32(0x18, 0x38)
    }

    /// Fija la suma del bitmap de bloques.
    pub fn pon_csum_bitmap_bloques(&mut self, v: u32) {
        self.pon_l32(0x18, 0x38, v);
    }

    /// Suma del bitmap de inodos.
    #[must_use]
    pub fn csum_bitmap_inodos(&self) -> u32 {
        self.l32(0x1A, 0x3A)
    }

    /// Fija la suma del bitmap de inodos.
    pub fn pon_csum_bitmap_inodos(&mut self, v: u32) {
        self.pon_l32(0x1A, 0x3A, v);
    }

    /// `bg_itable_unused`: inodos sin usar al FINAL de la tabla (nunca asignados).
    #[must_use]
    pub fn itable_sin_usar(&self) -> u32 {
        self.l32(0x1C, 0x32)
    }

    /// Fija `bg_itable_unused`.
    pub fn pon_itable_sin_usar(&mut self, v: u32) {
        self.pon_l32(0x1C, 0x32, v);
    }

    /// Suma del propio descriptor (`bg_checksum`).
    #[must_use]
    pub fn csum(&self) -> u16 {
        self.u16(crate::csum::GD_OFF_CSUM)
    }

    /// Recalcula y escribe la suma del descriptor.
    pub fn sellar(&mut self, semilla: u32, grupo: u32) {
        let c = crate::csum::descriptor(semilla, grupo, &self.crudo);
        self.pon16(crate::csum::GD_OFF_CSUM, c);
    }

    /// Ajusta la suma de un bitmap de BLOQUES a lo que caben en 16 o 32 bits según el ancho
    /// (el kernel guarda solo los 16 bajos si el descriptor es de 32 bytes).
    #[must_use]
    pub fn recorta(&self, v: u32) -> u32 {
        if self.ancho() { v } else { v & 0xFFFF }
    }
}
