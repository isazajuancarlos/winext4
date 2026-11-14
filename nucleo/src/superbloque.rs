// SPDX-FileCopyrightText: 2026 Juan Carlos Isaza
// SPDX-License-Identifier: LicenseRef-Xiliux-Propietario
//! El **superbloque** ext4: el primer bloque de metadatos, en el byte 1024 del volumen,
//! 1024 bytes de largo, con el mismo sitio y endianness en todas las variantes de ext2/3/4.
//!
//! Es la puerta de entrada del motor: dice la magia (`0xEF53`), el tamaño de bloque, cuántos
//! inodos/bloques hay, y —lo decisivo para saber QUÉ tenemos que implementar— las tres
//! palabras de banderas de características (`compat`/`incompat`/`ro_compat`). Un ext4 moderno
//! trae `EXTENTS`, `64BIT` y `METADATA_CSUM`: esas tres marcan el alcance real del driver.
//!
//! Fuente del formato: diseño en-disco de ext4 (kernel.org), campos verificados contra
//! `dumpe2fs` de `e2fsprogs` sobre imágenes reales (ver `tests/imagen_real.rs`).

/// Desplazamiento del superbloque desde el inicio del volumen, en bytes. Constante en ext2/3/4
/// e independiente del tamaño de bloque (con bloques de 1 KiB, el bloque 0 es el de arranque).
pub const DESPLAZAMIENTO: usize = 1024;

/// Tamaño del superbloque en bytes.
pub const TAMANO: usize = 1024;

/// La firma de un superbloque ext2/3/4, en `s_magic` (byte 0x38).
pub const MAGIA: u16 = 0xEF53;

/// Por qué un bloque de bytes NO es un superbloque ext4 legible. Nunca se sustituye por un
/// defecto (D20): el llamante decide qué hacer, pero el motor no inventa.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ErrorSuperbloque {
    /// El búfer es más corto que un superbloque (1024 bytes). No se lee de más.
    DemasiadoCorto { tenia: usize },
    /// `s_magic` no es `0xEF53`: no es un volumen ext2/3/4 (o el desplazamiento está mal).
    MagiaInvalida { vista: u16 },
    /// `s_log_block_size` codifica un tamaño de bloque fuera del rango legal (1 KiB–64 KiB).
    /// Un valor absurdo aquí descarrila todo cálculo de desplazamiento posterior.
    TamanoBloqueInvalido { s_log_block_size: u32 },
    /// Geometría imposible (las comprobaciones de cordura de e2fsck): sin ellas, un byte
    /// dañado dispara bucles de millones de grupos o reservas de memoria gigantes.
    GeometriaImposible { que: &'static str },
}

impl core::fmt::Display for ErrorSuperbloque {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::DemasiadoCorto { tenia } => {
                write!(f, "búfer de {tenia} bytes; el superbloque exige {TAMANO}")
            }
            Self::MagiaInvalida { vista } => {
                write!(f, "magia {vista:#06x}, no es ext4 (se esperaba {MAGIA:#06x})")
            }
            Self::TamanoBloqueInvalido { s_log_block_size } => {
                write!(f, "s_log_block_size={s_log_block_size} da un tamaño de bloque ilegal")
            }
            Self::GeometriaImposible { que } => write!(f, "geometría imposible: {que}"),
        }
    }
}

impl std::error::Error for ErrorSuperbloque {}

/// El superbloque leído y validado. Solo los campos que el motor usa hoy; crece por fase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Superbloque {
    /// Número total de inodos del sistema de archivos (`s_inodes_count`).
    pub inodos: u32,
    /// Número total de bloques (`s_blocks_count_lo` + `_hi` si la característica 64BIT está).
    pub bloques: u64,
    /// Tamaño de un bloque en bytes: `1024 << s_log_block_size`.
    pub tamano_bloque: u32,
    /// Inodos por grupo de bloques (`s_inodes_per_group`).
    pub inodos_por_grupo: u32,
    /// Bloques por grupo (`s_blocks_per_group`).
    pub bloques_por_grupo: u32,
    /// Primer bloque de datos (`s_first_data_block`): 1 con bloques de 1 KiB, 0 si son mayores.
    pub primer_bloque_dato: u32,
    /// Tamaño de un inodo en bytes (`s_inode_size`; 128 en ext2 viejo).
    pub tamano_inodo: u16,
    /// Tamaño de un descriptor de grupo en bytes (`s_desc_size` si 64BIT; 32 si no).
    pub tamano_desc: u16,
    /// `s_state`: bit 0 = montado limpiamente. Se expone crudo; interpretarlo es de capa de arriba.
    pub estado: u16,
    /// Banderas `s_feature_compat` (se puede montar aunque no se entiendan).
    pub compat: u32,
    /// Banderas `s_feature_incompat` (NO se debe montar si alguna no se entiende).
    pub incompat: u32,
    /// Banderas `s_feature_ro_compat` (si alguna no se entiende, montar SOLO lectura).
    pub ro_compat: u32,
    /// UUID del volumen (`s_uuid`), 16 bytes.
    pub uuid: [u8; 16],
    /// Etiqueta del volumen (`s_volume_name`), ya recortada en el primer NUL.
    pub etiqueta: String,
}

// --- Banderas INCOMPAT que definen el alcance del motor (subconjunto, crece por fase). ---
/// `INCOMPAT_FILETYPE`: las entradas de directorio llevan el tipo de archivo.
pub const INCOMPAT_FILETYPE: u32 = 0x0002;
/// `INCOMPAT_EXTENTS`: los inodos mapean datos con árboles de extents (no bloques indirectos).
pub const INCOMPAT_EXTENTS: u32 = 0x0040;
/// `INCOMPAT_64BIT`: el recuento de bloques usa 64 bits (`s_blocks_count_hi`).
pub const INCOMPAT_64BIT: u32 = 0x0080;
/// `INCOMPAT_FLEX_BG`: grupos flexibles (metadatos agrupados).
pub const INCOMPAT_FLEX_BG: u32 = 0x0200;
/// `INCOMPAT_CSUM_SEED`: la semilla de las sumas va guardada en `s_checksum_seed`.
pub const INCOMPAT_CSUM_SEED: u32 = 0x2000;
/// `INCOMPAT_RECOVER`: el journal tiene transacciones sin aplicar (montaje sucio).
pub const INCOMPAT_RECOVER: u32 = 0x0004;
/// `INCOMPAT_INLINE_DATA`: datos pequeños dentro del inodo (no soportado al escribir).
pub const INCOMPAT_INLINE_DATA: u32 = 0x8000;
/// `INCOMPAT_ENCRYPT`: cifrado fscrypt (no soportado al escribir).
pub const INCOMPAT_ENCRYPT: u32 = 0x1_0000;
/// `INCOMPAT_CASEFOLD`: nombres insensibles a mayúsculas (otro hash; no soportado al escribir).
pub const INCOMPAT_CASEFOLD: u32 = 0x2_0000;
/// `INCOMPAT_META_BG`: GDT repartida en meta-grupos (otra ubicación; no soportado al escribir).
pub const INCOMPAT_META_BG: u32 = 0x0010;

// --- Banderas RO_COMPAT relevantes. ---
/// `RO_COMPAT_METADATA_CSUM`: sumas de verificación en los metadatos (hay que mantenerlas al escribir).
pub const RO_COMPAT_METADATA_CSUM: u32 = 0x0400;
/// `RO_COMPAT_GDT_CSUM`: suma crc16 antigua de los descriptores (no soportada al escribir).
pub const RO_COMPAT_GDT_CSUM: u32 = 0x0010;
/// `RO_COMPAT_BIGALLOC`: clusters de varios bloques (no soportado al escribir).
pub const RO_COMPAT_BIGALLOC: u32 = 0x0200;
/// `RO_COMPAT_HUGE_FILE`: `i_blocks` puede contarse en bloques de FS (con `EXT4_HUGE_FILE_FL`).
pub const RO_COMPAT_HUGE_FILE: u32 = 0x0008;

impl Superbloque {
    /// Lee un superbloque de `bytes`, que debe ser el bloque de 1024 bytes situado en el
    /// desplazamiento [`DESPLAZAMIENTO`] del volumen (el llamante hace esa lectura). Falla
    /// ruidosamente si la magia, el tamaño o la longitud no cuadran (D20).
    ///
    /// # Errors
    /// [`ErrorSuperbloque`] si el búfer es corto, la magia no es ext4, o el tamaño de bloque
    /// codificado es ilegal.
    pub fn leer(bytes: &[u8]) -> Result<Self, ErrorSuperbloque> {
        if bytes.len() < TAMANO {
            return Err(ErrorSuperbloque::DemasiadoCorto { tenia: bytes.len() });
        }

        let magia = u16(bytes, 0x38);
        if magia != MAGIA {
            return Err(ErrorSuperbloque::MagiaInvalida { vista: magia });
        }

        let s_log_block_size = u32(bytes, 0x18);
        // Legal: 1 KiB (0) a 64 KiB (6). Más allá es un campo corrupto, no un FS raro.
        if s_log_block_size > 6 {
            return Err(ErrorSuperbloque::TamanoBloqueInvalido { s_log_block_size });
        }
        let tamano_bloque = 1024u32 << s_log_block_size;

        let incompat = u32(bytes, 0x60);
        let bloques_lo = u32(bytes, 0x04) as u64;
        let bloques = if incompat & INCOMPAT_64BIT != 0 {
            bloques_lo | ((u32(bytes, 0x150) as u64) << 32)
        } else {
            bloques_lo
        };

        let mut uuid = [0u8; 16];
        uuid.copy_from_slice(&bytes[0x68..0x68 + 16]);

        let crudo = &bytes[0x78..0x78 + 16];
        let fin = crudo.iter().position(|&b| b == 0).unwrap_or(crudo.len());
        let etiqueta = String::from_utf8_lossy(&crudo[..fin]).into_owned();

        // s_inode_size: 128 fijo en ext2 clásico (el campo vale 0); explícito en ext4.
        let tamano_inodo = {
            let v = u16(bytes, 0x58);
            if v == 0 { 128 } else { v }
        };
        // s_desc_size solo es válido con 64BIT; sin él, el descriptor de grupo es de 32 bytes.
        let tamano_desc = if incompat & INCOMPAT_64BIT != 0 {
            let v = u16(bytes, 0xFE);
            if v == 0 { 32 } else { v }
        } else {
            32
        };

        // Cordura (como e2fsck): un bitmap de un bloque cubre como mucho 8·bs bloques o inodos;
        // los inodos van en potencias de 2 entre 128 y el bloque; los descriptores, 32 o una
        // potencia de 2 entre 64 y 1024; y el recuento de inodos cuadra con los grupos.
        let bpg = u32(bytes, 0x20);
        let ipg = u32(bytes, 0x28);
        let malo = |que| Err(ErrorSuperbloque::GeometriaImposible { que });
        if bpg == 0 || bpg > 8 * tamano_bloque {
            return malo("bloques por grupo fuera de rango");
        }
        if ipg == 0 || ipg > 8 * tamano_bloque || !ipg.is_multiple_of(8) {
            return malo("inodos por grupo fuera de rango");
        }
        if !tamano_inodo.is_power_of_two() || tamano_inodo < 128 || u32::from(tamano_inodo) > tamano_bloque {
            return malo("tamaño de inodo imposible");
        }
        if tamano_desc != 32 && (!tamano_desc.is_power_of_two() || !(64..=1024).contains(&tamano_desc)) {
            return malo("tamaño de descriptor imposible");
        }
        let fdb = u32(bytes, 0x14);
        if fdb > 1 || bloques <= u64::from(fdb) {
            return malo("primer bloque de datos o recuento de bloques imposible");
        }
        let n_grupos = (bloques - u64::from(fdb)).div_ceil(u64::from(bpg));
        if u64::from(u32(bytes, 0x00)) != n_grupos * u64::from(ipg) {
            return malo("el recuento de inodos no cuadra con los grupos");
        }
        Ok(Self {
            inodos: u32(bytes, 0x00),
            bloques,
            tamano_bloque,
            inodos_por_grupo: ipg,
            bloques_por_grupo: bpg,
            primer_bloque_dato: fdb,
            tamano_inodo,
            tamano_desc,
            estado: u16(bytes, 0x3A),
            compat: u32(bytes, 0x5C),
            incompat,
            ro_compat: u32(bytes, 0x64),
            uuid,
            etiqueta,
        })
    }

    /// Número de grupos de bloques del volumen: `ceil(bloques / bloques_por_grupo)`.
    #[must_use]
    pub fn n_grupos(&self) -> u64 {
        let bpg = self.bloques_por_grupo.max(1) as u64;
        // El primer bloque de datos no cuenta para el grupo 0 en FS de bloque de 1 KiB; la
        // fórmula estándar resta `s_first_data_block` antes de dividir.
        (self.bloques - self.primer_bloque_dato as u64).div_ceil(bpg)
    }

    /// ¿Usa árboles de extents? (ext4 moderno: sí.)
    #[must_use]
    pub fn usa_extents(&self) -> bool {
        self.incompat & INCOMPAT_EXTENTS != 0
    }

    /// ¿Recuento de bloques de 64 bits?
    #[must_use]
    pub fn es_64bit(&self) -> bool {
        self.incompat & INCOMPAT_64BIT != 0
    }

    /// ¿Lleva sumas de verificación de metadatos que habrá que mantener al escribir?
    #[must_use]
    pub fn tiene_csum_metadatos(&self) -> bool {
        self.ro_compat & RO_COMPAT_METADATA_CSUM != 0
    }

    /// ¿Guarda la semilla de las sumas en el superbloque (`INCOMPAT_CSUM_SEED`)?
    #[must_use]
    pub fn guarda_semilla(&self) -> bool {
        self.incompat & INCOMPAT_CSUM_SEED != 0
    }

    /// Tamaño total del volumen en bytes, según el superbloque.
    #[must_use]
    pub fn tamano_bytes(&self) -> u64 {
        self.bloques * self.tamano_bloque as u64
    }
}

#[inline]
fn u16(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([b[off], b[off + 1]])
}

#[inline]
fn u32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

#[cfg(test)]
mod pruebas {
    use super::*;

    /// Un superbloque mínimo sintético: magia + bloque de 4 KiB, para las pruebas de borde que
    /// NO necesitan un ext4 real. (La validación contra ext4 real vive en `tests/imagen_real.rs`.)
    fn sintetico() -> Vec<u8> {
        let mut b = vec![0u8; TAMANO];
        b[0x38] = 0x53;
        b[0x39] = 0xEF; // magia 0xEF53 en LE
        b[0x18] = 0x02; // s_log_block_size=2 → 4096
        // Geometría mínima coherente: 1 grupo de 32768 bloques con 8192 inodos de 256 bytes.
        b[0x04..0x08].copy_from_slice(&32768u32.to_le_bytes());
        b[0x20..0x24].copy_from_slice(&32768u32.to_le_bytes());
        b[0x28..0x2C].copy_from_slice(&8192u32.to_le_bytes());
        b[0x00..0x04].copy_from_slice(&8192u32.to_le_bytes());
        b[0x58..0x5A].copy_from_slice(&256u16.to_le_bytes());
        b
    }

    #[test]
    fn rechaza_geometrias_imposibles() {
        // Cada una, por separado, sobre el sintético válido: debe dar GeometriaImposible.
        let casos: [(usize, &[u8]); 5] = [
            (0x20, &1_000_000u32.to_le_bytes()), // bloques por grupo > 8·bs
            (0x28, &7u32.to_le_bytes()),         // inodos por grupo no múltiplo de 8
            (0x58, &300u16.to_le_bytes()),       // inodo no potencia de 2
            (0x00, &12345u32.to_le_bytes()),     // inodos que no cuadran con los grupos
            (0x04, &0u32.to_le_bytes()),         // sin bloques
        ];
        for (off, v) in casos {
            let mut b = sintetico();
            b[off..off + v.len()].copy_from_slice(v);
            assert!(matches!(Superbloque::leer(&b), Err(ErrorSuperbloque::GeometriaImposible { .. })), "offset {off:#x}");
        }
    }

    #[test]
    fn lee_un_superbloque_sintetico_valido() {
        let sb = Superbloque::leer(&sintetico()).expect("magia y tamaño válidos");
        assert_eq!(sb.tamano_bloque, 4096);
    }

    // --- PAR D33: el caso que DEBE leer vs los que DEBEN fallar por la vía real ---

    #[test]
    fn rechaza_buffer_corto() {
        assert_eq!(
            Superbloque::leer(&[0u8; 100]),
            Err(ErrorSuperbloque::DemasiadoCorto { tenia: 100 }),
        );
    }

    #[test]
    fn rechaza_magia_invalida() {
        // CONTROL (D33): el rojo falla por la MISMA puerta que el fallo real (la magia), no por
        // el arnés. Un búfer de ceros NO es ext4; si `leer` lo aceptara, la escritura futura
        // corrompería un disco que no es nuestro.
        let ceros = vec![0u8; TAMANO];
        assert_eq!(
            Superbloque::leer(&ceros),
            Err(ErrorSuperbloque::MagiaInvalida { vista: 0 }),
        );
    }

    #[test]
    fn rechaza_tamano_de_bloque_imposible() {
        let mut b = sintetico();
        b[0x18] = 0x07; // > 6 → ilegal
        assert_eq!(
            Superbloque::leer(&b),
            Err(ErrorSuperbloque::TamanoBloqueInvalido { s_log_block_size: 7 }),
        );
    }

    #[test]
    fn mutar_la_magia_vuelve_rojo_el_caso_bueno() {
        // El mutante (D33): partir del artefacto bueno y romper SOLO la magia ⇒ debe fallar.
        let mut b = sintetico();
        b[0x39] = 0x00; // 0xEF53 → 0x0053
        assert!(matches!(Superbloque::leer(&b), Err(ErrorSuperbloque::MagiaInvalida { .. })));
    }
}
