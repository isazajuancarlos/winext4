// SPDX-FileCopyrightText: 2026 Juan Carlos Isaza
// SPDX-License-Identifier: LicenseRef-Xiliux-Propietario
//! F3 — **escritura con asignación**: superbloque y descriptores (con sus sumas), asignar y
//! liberar bloques e inodos por sus bitmaps, crear inodos, y crecer/truncar archivos
//! reconstruyendo su árbol de extents.
//!
//! Reglas que no se rompen (D20):
//! - Antes de escribir NADA, [`Ext4::abrir_escritura`] comprueba que el volumen solo usa lo
//!   que este código sabe mantener; si no, se niega.
//! - Mientras se escribe, el superbloque queda marcado «no limpio»: si el proceso muere a
//!   mitad, Linux exige `e2fsck` antes de montar. [`Ext4::cerrar_escritura`] lo repone.
//! - Sin journal todavía (F2): el orden de escritura deja, como mucho, bloques marcados en uso
//!   que nadie referencia (fuga que `e2fsck` recoge), nunca un bloque referenciado y libre.
//! - Un grupo con bitmap SIN INICIALIZAR (`BLOCK_UNINIT`/`INODE_UNINIT`) se inicializa al
//!   usarlo, como el kernel: el bitmap se CALCULA (metadatos del grupo) y se exige que cuadre
//!   con el descriptor; si no cuadra, se niega en vez de inventar. (Antes se saltaba el grupo:
//!   un volumen recién formateado se quedaba «sin inodos» al llenar el grupo 0.)

use std::io::{Read, Seek, SeekFrom, Write};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::csum;
use crate::grupos::{self, Descriptor};
use crate::sistema::{ErrorFs, Ext4};
use crate::superbloque as s;

// --- Superbloque: offsets de los campos que la escritura mantiene. ---
const SB_BLOQUES_LIBRES_LO: usize = 0x0C;
const SB_INODOS_LIBRES: usize = 0x10;
const SB_MTIME: usize = 0x2C;
const SB_WTIME: usize = 0x30;
const SB_ESTADO: usize = 0x3A;
const SB_PRIMER_INODO: usize = 0x54;
const SB_BLOQUES_LIBRES_HI: usize = 0x158;
const SB_QUIERE_EXTRA: usize = 0x15E;
/// `incompat` que la escritura sabe mantener: filetype, extents, 64bit, flex_bg, csum_seed.
const INCOMPAT_SABIDAS: u32 = s::INCOMPAT_FILETYPE | s::INCOMPAT_EXTENTS | s::INCOMPAT_64BIT | s::INCOMPAT_FLEX_BG | s::INCOMPAT_CSUM_SEED;
/// `ro_compat` que la escritura sabe mantener: sparse_super (0x1), large_file (0x2),
/// huge_file (0x8, solo a nivel de volumen: el inodo con HUGE_FILE_FL se rechaza), dir_nlink
/// (0x20), extra_isize (0x40) y metadata_csum (0x400). NO: gdt_csum sin metadata_csum, quota,
/// bigalloc, project, orphan_present, verity, snapshot, readonly, shared_blocks.
const RO_COMPAT_SABIDAS: u32 = 0x1 | 0x2 | s::RO_COMPAT_HUGE_FILE | 0x20 | 0x40 | s::RO_COMPAT_METADATA_CSUM;
/// `COMPAT_HAS_JOURNAL`.
const COMPAT_HAS_JOURNAL: u32 = 0x4;
/// `EXT4_VALID_FS`: desmontado limpio.
const ESTADO_LIMPIO: u16 = 0x1;
/// `EXT4_ERROR_FS`: el kernel detectó errores.
const ESTADO_ERRORES: u16 = 0x2;

// --- Inodo: offsets. ---
pub(crate) const I_MODO: usize = 0x00;
pub(crate) const I_UID: usize = 0x02;
pub(crate) const I_TAM_LO: usize = 0x04;
const I_ATIME: usize = 0x08;
pub(crate) const I_CTIME: usize = 0x0C;
const I_MTIME: usize = 0x10;
const I_DTIME: usize = 0x14;
pub(crate) const I_GID: usize = 0x18;
pub(crate) const I_ENLACES: usize = 0x1A;
pub(crate) const I_BLOQUES_LO: usize = 0x1C;
pub(crate) const I_FLAGS: usize = 0x20;
pub(crate) const I_BLOQUE: usize = 0x28;
pub(crate) const I_GEN: usize = 0x64;
pub(crate) const I_TAM_HI: usize = 0x6C;
const I_BLOQUES_HI: usize = 0x74;
const I_EXTRA: usize = 0x80;
const I_CTIME_EXTRA: usize = 0x84;
const I_MTIME_EXTRA: usize = 0x88;
const I_ATIME_EXTRA: usize = 0x8C;
const I_CRTIME: usize = 0x90;
const I_CRTIME_EXTRA: usize = 0x94;

/// `EXT4_EXTENTS_FL`.
pub(crate) const FL_EXTENTS: u32 = 0x8_0000;
/// `EXT4_HUGE_FILE_FL`: `i_blocks` en bloques de FS (no soportado).
const FL_HUGE_FILE: u32 = 0x4_0000;
/// `EXT4_INLINE_DATA_FL`.
const FL_INLINE: u32 = 0x1000_0000;

const EXTENT_MAGIA: u16 = 0xF30A;
/// Largo máximo de un extent INICIALIZADO (más es la marca de «no inicializado»).
const EXTENT_MAX_LEN: u32 = 32768;
/// Entradas que caben en la raíz del árbol (en `i_block`).
const RAIZ_MAX: usize = 4;

/// Un extent de datos: `len` bloques lógicos desde `logico`, físicos desde `fisico`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Ext {
    pub(crate) logico: u32,
    pub(crate) len: u32,
    pub(crate) fisico: u64,
}

/// El mapa completo de un archivo, más los bloques que ocupa su árbol fuera del inodo.
struct Mapa {
    exts: Vec<Ext>,
    hojas: Vec<u64>,
}

pub(crate) fn u16le(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
pub(crate) fn u32le(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().expect("4 bytes"))
}
pub(crate) fn pon16(b: &mut [u8], o: usize, v: u16) {
    b[o..o + 2].copy_from_slice(&v.to_le_bytes());
}
pub(crate) fn pon32(b: &mut [u8], o: usize, v: u32) {
    b[o..o + 4].copy_from_slice(&v.to_le_bytes());
}

fn bit(bm: &[u8], i: usize) -> bool {
    bm[i / 8] & (1 << (i % 8)) != 0
}
fn pon_bit(bm: &mut [u8], i: usize, v: bool) {
    if v {
        bm[i / 8] |= 1 << (i % 8);
    } else {
        bm[i / 8] &= !(1 << (i % 8));
    }
}

/// Segundos y nanosegundos actuales (para los tiempos del inodo).
fn ahora() -> (u32, u32) {
    let d = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    (d.as_secs() as u32, d.subsec_nanos())
}

/// Profundidad máxima del árbol de extents (la del kernel: `EXT4_MAX_EXTENT_DEPTH`).
const PROF_MAX: u16 = 5;

/// Escribe un nodo ÍNDICE de profundidad `prof`: cada entrada = (primer lógico, bloque hijo).
pub(crate) fn escribir_indice(nodo: &mut [u8], max: u16, prof: u16, hijos: &[(u32, u64)]) {
    nodo[..12].fill(0);
    pon16(nodo, 0, EXTENT_MAGIA);
    pon16(nodo, 2, hijos.len() as u16);
    pon16(nodo, 4, max);
    pon16(nodo, 6, prof);
    for (i, &(logico, b)) in hijos.iter().enumerate() {
        let o = 12 + i * 12;
        pon32(nodo, o, logico);
        pon32(nodo, o + 4, b as u32);
        pon16(nodo, o + 8, (b >> 32) as u16);
        pon16(nodo, o + 10, 0);
    }
}

/// Escribe los extents en un nodo HOJA (`nodo` = cabecera + entradas), profundidad 0.
pub(crate) fn escribir_hoja(nodo: &mut [u8], max: u16, exts: &[Ext]) {
    nodo[..12].fill(0);
    pon16(nodo, 0, EXTENT_MAGIA);
    pon16(nodo, 2, exts.len() as u16);
    pon16(nodo, 4, max);
    pon16(nodo, 6, 0);
    for (i, e) in exts.iter().enumerate() {
        let o = 12 + i * 12;
        pon32(nodo, o, e.logico);
        pon16(nodo, o + 4, e.len as u16);
        pon16(nodo, o + 6, (e.fisico >> 32) as u16);
        pon32(nodo, o + 8, e.fisico as u32);
    }
}

/// Tope de un tramo de datos contiguos que va al disco en UNA escritura (bytes).
const TRAMO_MAX: u64 = 4 * 1024 * 1024;

impl<R: Read + Write + Seek> Ext4<R> {
    /// Comprueba que el volumen solo usa lo que este motor sabe mantener al escribir, y lo
    /// marca «no limpio» hasta [`Self::cerrar_escritura`].
    ///
    /// # Errors
    /// [`ErrorFs::NoSoportado`] por cada característica que no se sabe mantener, o si el FS no
    /// está limpio; [`ErrorFs::SumaInvalida`] si la suma del superbloque no cuadra.
    pub fn abrir_escritura(&mut self) -> Result<(), ErrorFs> {
        self.abrir_escritura_con(false)
    }

    /// Como [`Self::abrir_escritura`], pero admite un volumen con «errores marcados» por el
    /// kernel: SOLO para la reparación, que es justo quien debe poder entrar ahí.
    pub(crate) fn abrir_escritura_con(&mut self, para_reparar: bool) -> Result<(), ErrorFs> {
        let sb = &self.sb;
        // LISTA BLANCA (la política del kernel, más estricta): se escribe solo si TODO lo que el
        // volumen declara es algo que este motor sabe mantener. Una característica desconocida o
        // no soportada = no se escribe, aunque se pueda leer (D20/D21: se rechaza lo que hoy no
        // es seguro).
        if sb.incompat & !INCOMPAT_SABIDAS != 0 {
            let otras = sb.incompat & !INCOMPAT_SABIDAS;
            // Las conocidas-pero-no-soportadas tienen su mensaje propio abajo; si sobra algo más,
            // es una característica que este motor no conoce.
            let conocidas_malas = s::INCOMPAT_RECOVER | s::INCOMPAT_INLINE_DATA | s::INCOMPAT_ENCRYPT | s::INCOMPAT_CASEFOLD | s::INCOMPAT_META_BG;
            if otras & !conocidas_malas != 0 {
                return Err(ErrorFs::NoSoportado { que: "el volumen usa una característica incompat que winext4 no sabe escribir (mmp, ea_inode, large_dir, journal_dev, dirdata o desconocida)" });
            }
        }
        if sb.ro_compat & !RO_COMPAT_SABIDAS != 0 {
            return Err(ErrorFs::NoSoportado { que: "el volumen usa una característica ro_compat que winext4 no sabe mantener (quota, project, orphan_present, verity, snapshot, bigalloc o desconocida)" });
        }
        let incompat_malas = [
            (s::INCOMPAT_RECOVER, "el journal tiene transacciones sin aplicar (recupéralo en Linux)"),
            (s::INCOMPAT_INLINE_DATA, "inline_data"),
            (s::INCOMPAT_ENCRYPT, "cifrado (encrypt)"),
            (s::INCOMPAT_CASEFOLD, "casefold"),
            (s::INCOMPAT_META_BG, "meta_bg"),
        ];
        for (b, que) in incompat_malas {
            if sb.incompat & b != 0 {
                return Err(ErrorFs::NoSoportado { que });
            }
        }
        if !sb.usa_extents() {
            return Err(ErrorFs::NoSoportado { que: "volumen sin extents (ext2/ext3)" });
        }
        if sb.ro_compat & s::RO_COMPAT_BIGALLOC != 0 {
            return Err(ErrorFs::NoSoportado { que: "bigalloc" });
        }
        if sb.ro_compat & s::RO_COMPAT_GDT_CSUM != 0 && !sb.tiene_csum_metadatos() {
            return Err(ErrorFs::NoSoportado { que: "gdt_csum (crc16) sin metadata_csum" });
        }
        if sb.estado & ESTADO_LIMPIO == 0 {
            return Err(ErrorFs::NoSoportado { que: "el volumen no se desmontó limpio (pasa e2fsck en Linux)" });
        }
        if sb.estado & ESTADO_ERRORES != 0 && !para_reparar {
            return Err(ErrorFs::NoSoportado { que: "el volumen tiene errores marcados (pasa e2fsck en Linux)" });
        }
        if sb.tiene_csum_metadatos() {
            let guardada = u32le(&self.sb_crudo, csum::SB_OFF_CSUM);
            if csum::superbloque(&self.sb_crudo) != guardada {
                return Err(ErrorFs::SumaInvalida { que: "superbloque" });
            }
        }
        let (t, _) = ahora();
        pon32(&mut self.sb_crudo, SB_MTIME, t);
        if self.sb.compat & COMPAT_HAS_JOURNAL != 0 {
            // CON JOURNAL (lo normal): se escribe a través de él. `needs_recovery` puesto
            // mientras está abierto, como hace el kernel: si se corta la luz, Linux reproduce
            // lo confirmado y el volumen queda consistente sin e2fsck.
            let j = self.abrir_journal()?;
            if j.inicio != 0 {
                return Err(ErrorFs::NoSoportado { que: "el journal no está vacío (recupéralo antes de escribir)" });
            }
            let inc = u32le(&self.sb_crudo, 0x60) | s::INCOMPAT_RECOVER;
            pon32(&mut self.sb_crudo, 0x60, inc);
            self.guardar_superbloque()?;
            self.disp.flush()?;
            self.diario = Some(j);
            return Ok(());
        }
        // SIN journal: marca «no limpio» hasta cerrar (un corte obliga a e2fsck en Linux).
        let estado = u16le(&self.sb_crudo, SB_ESTADO) & !ESTADO_LIMPIO;
        pon16(&mut self.sb_crudo, SB_ESTADO, estado);
        self.guardar_superbloque()?;
        self.disp.flush()?;
        Ok(())
    }

    /// Repone la marca de desmontaje limpio (o quita `needs_recovery` con journal) y vacía
    /// los búferes al dispositivo.
    ///
    /// # Errors
    /// [`ErrorFs::Io`] si falla el dispositivo.
    pub fn cerrar_escritura(&mut self) -> Result<(), ErrorFs> {
        // Lo agrupado y aún sin confirmar, al journal; luego todo a su sitio y el journal vacío:
        // solo entonces se puede quitar `needs_recovery`.
        self.confirmar_pendiente()?;
        self.vaciar_journal()?;
        if self.diario.take().is_some() {
            let inc = u32le(&self.sb_crudo, 0x60) & !s::INCOMPAT_RECOVER;
            pon32(&mut self.sb_crudo, 0x60, inc);
        }
        let estado = u16le(&self.sb_crudo, SB_ESTADO) | ESTADO_LIMPIO;
        pon16(&mut self.sb_crudo, SB_ESTADO, estado);
        self.guardar_superbloque()?;
        self.disp.flush()?;
        Ok(())
    }

    /// Sella y escribe el superbloque primario (las copias de respaldo no se tocan, igual que
    /// hace el kernel fuera de un redimensionado). Dentro de una transacción va a ella.
    fn guardar_superbloque(&mut self) -> Result<(), ErrorFs> {
        let (t, _) = ahora();
        pon32(&mut self.sb_crudo, SB_WTIME, t);
        if self.sb.tiene_csum_metadatos() {
            let c = csum::superbloque(&self.sb_crudo);
            pon32(&mut self.sb_crudo, csum::SB_OFF_CSUM, c);
        }
        if self.tx.is_some() {
            let bs = self.bs();
            let (b, o) = (s::DESPLAZAMIENTO as u64 / bs, (s::DESPLAZAMIENTO as u64 % bs) as usize);
            let mut blk = self.leer_bloque(b)?;
            blk[o..o + s::TAMANO].copy_from_slice(&self.sb_crudo);
            self.escribir_meta(b, &blk)?;
        } else {
            self.disp.seek(SeekFrom::Start(s::DESPLAZAMIENTO as u64))?;
            self.disp.write_all(&self.sb_crudo)?;
        }
        self.sb = s::Superbloque::leer(&self.sb_crudo).map_err(ErrorFs::Superbloque)?;
        Ok(())
    }

    fn sb_bloques_libres(&self) -> u64 {
        let lo = u32le(&self.sb_crudo, SB_BLOQUES_LIBRES_LO) as u64;
        let hi = if self.sb.es_64bit() { u32le(&self.sb_crudo, SB_BLOQUES_LIBRES_HI) as u64 } else { 0 };
        lo | (hi << 32)
    }

    fn sb_pon_bloques_libres(&mut self, v: u64) {
        pon32(&mut self.sb_crudo, SB_BLOQUES_LIBRES_LO, v as u32);
        if self.sb.es_64bit() {
            pon32(&mut self.sb_crudo, SB_BLOQUES_LIBRES_HI, (v >> 32) as u32);
        }
    }

    fn sb_ajusta_inodos_libres(&mut self, delta: i64) {
        let v = u32le(&self.sb_crudo, SB_INODOS_LIBRES) as i64 + delta;
        pon32(&mut self.sb_crudo, SB_INODOS_LIBRES, v.max(0) as u32);
    }

    /// Sella y escribe el descriptor del grupo `grupo`.
    fn guardar_descriptor(&mut self, grupo: u64, d: &mut Descriptor) -> Result<(), ErrorFs> {
        if self.sb.tiene_csum_metadatos() {
            d.sellar(self.semilla, grupo as u32);
        }
        let (bloque, off) = self.pos_descriptor(grupo);
        let mut datos = self.leer_bloque(bloque)?;
        datos[off..off + d.crudo.len()].copy_from_slice(&d.crudo);
        self.escribir_meta(bloque, &datos)
    }

    /// Sella (suma) y escribe un inodo crudo (por el bloque de la tabla que lo contiene).
    pub(crate) fn guardar_inodo(&mut self, ino: u32, crudo: &mut [u8]) -> Result<(), ErrorFs> {
        if self.sb.tiene_csum_metadatos() {
            csum::sellar_inodo(self.semilla, ino, crudo);
        }
        let byte = self.byte_inodo(ino)?;
        let bs = self.bs();
        let mut blk = self.leer_bloque(byte / bs)?;
        let o = (byte % bs) as usize;
        blk[o..o + crudo.len()].copy_from_slice(crudo);
        self.escribir_meta(byte / bs, &blk)
    }

    /// Quita la marca «errores» del superbloque (tras una reparación verificada a cero).
    pub(crate) fn quitar_marca_errores(&mut self) -> Result<(), ErrorFs> {
        let estado = u16le(&self.sb_crudo, SB_ESTADO) & !ESTADO_ERRORES;
        pon16(&mut self.sb_crudo, SB_ESTADO, estado);
        self.guardar_superbloque()
    }

    /// Anota en el superbloque que se acaba de hacer una verificación COMPLETA limpia: lo que
    /// hace `e2fsck` (`s_lastcheck` = ahora, `s_mnt_count` = 0). (La marca de errores no se toca: con
    /// ella `abrir_escritura` ya se niega, y quitarla es cosa de `reparar` o de e2fsck.) Así los errores registrados
    /// antes de hoy (ya reparados) dejan de exigir otra verificación completa, y uno nuevo sí.
    ///
    /// # Errors
    /// E/S.
    pub fn anotar_verificacion_completa(&mut self) -> Result<(), ErrorFs> {
        const SB_LASTCHECK: usize = 0x40;
        const SB_MNT_COUNT: usize = 0x34;
        self.en_tx(|fs| {
            let (t, _) = ahora();
            pon32(&mut fs.sb_crudo, SB_LASTCHECK, t);
            pon16(&mut fs.sb_crudo, SB_MNT_COUNT, 0);
            fs.guardar_superbloque()
        })
    }

    /// Fija los contadores de un grupo y del superbloque a lo CALCULADO (reparación).
    pub(crate) fn fijar_contadores(&mut self, g: u64, bloques_libres: u32, inodos_libres: u32, dirs: u32) -> Result<(), ErrorFs> {
        let mut d = self.descriptor(g)?;
        let (vb, vi) = (d.bloques_libres(), d.inodos_libres());
        if d.banderas() & grupos::BG_BLOCK_UNINIT == 0 {
            d.pon_bloques_libres(bloques_libres);
        }
        d.pon_inodos_libres(inodos_libres);
        d.pon_dirs_usados(dirs);
        self.guardar_descriptor(g, &mut d)?;
        // El superbloque acompaña la diferencia.
        let sbl = self.sb_bloques_libres() as i64 + i64::from(d.bloques_libres()) - i64::from(vb);
        self.sb_pon_bloques_libres(sbl.max(0) as u64);
        self.sb_ajusta_inodos_libres(i64::from(inodos_libres) - i64::from(vi));
        self.guardar_superbloque()
    }

    /// Libera un bloque que figura en uso sin que nadie lo use (reparación).
    pub(crate) fn liberar_fuga(&mut self, b: u64) -> Result<(), ErrorFs> {
        self.liberar_bloques(b, 1)
    }

    /// Bloques del grupo `g` (el último puede ser más corto).
    fn bloques_en_grupo(&self, g: u64) -> u64 {
        let bpg = self.sb.bloques_por_grupo as u64;
        let ini = self.sb.primer_bloque_dato as u64 + g * bpg;
        bpg.min(self.sb.bloques - ini)
    }

    /// El bitmap de bloques de un grupo `BLOCK_UNINIT`, CALCULADO como `ext4_init_block_bitmap`:
    /// en uso solo los metadatos que caen en el grupo (copia de superbloque + GDT + GDT
    /// reservada si el grupo la lleva, y los bitmaps y tablas de inodos de CUALQUIER grupo que
    /// vivan aquí, por `flex_bg`), y a 1 el relleno tras el último bloque del grupo. Se exige
    /// que los libres calculados cuadren con el descriptor: si no, el grupo no es lo que dice
    /// ser y se niega (D20) en vez de inventar.
    fn bitmap_bloques_inicial(&mut self, g: u64, libres_desc: u32) -> Result<Vec<u8>, ErrorFs> {
        const COMPAT_SPARSE_SUPER2: u32 = 0x200;
        if u32le(&self.sb_crudo, 0x5C) & COMPAT_SPARSE_SUPER2 != 0 {
            return Err(ErrorFs::NoSoportado { que: "inicializar un grupo con sparse_super2" });
        }
        let bs = self.bs() as usize;
        let bpg = self.sb.bloques_por_grupo as u64;
        let ini = self.sb.primer_bloque_dato as u64 + g * bpg;
        let n_en = self.bloques_en_grupo(g);
        let mut bm = vec![0u8; bs];
        let mut marcar = |b: u64| {
            if b >= ini && b < ini + n_en {
                pon_bit(&mut bm, (b - ini) as usize, true);
            }
        };
        let ng = self.sb.n_grupos();
        if crate::verificar::tiene_copia(g, self.sb.ro_compat & 0x1 != 0) {
            let gdt = (ng * self.sb.tamano_desc as u64).div_ceil(bs as u64);
            let reservados = u16le(&self.sb_crudo, 0xCE) as u64;
            for b in ini..ini + 1 + gdt + reservados {
                marcar(b);
            }
        }
        let n_tabla = (self.sb.inodos_por_grupo as u64 * self.sb.tamano_inodo as u64).div_ceil(bs as u64);
        for h in 0..ng {
            let d = self.descriptor(h)?;
            marcar(d.bitmap_bloques());
            marcar(d.bitmap_inodos());
            for b in d.tabla_inodos()..d.tabla_inodos() + n_tabla {
                marcar(b);
            }
        }
        for i in n_en as usize..bs * 8 {
            pon_bit(&mut bm, i, true);
        }
        let libres = (0..n_en as usize).filter(|&i| !bit(&bm, i)).count();
        if libres != libres_desc as usize {
            return Err(ErrorFs::Corrupto { que: "grupo BLOCK_UNINIT: los libres calculados no cuadran con el descriptor" });
        }
        Ok(bm)
    }

    /// Si el grupo `g` tiene el bitmap de bloques SIN INICIALIZAR, lo calcula, lo escribe y
    /// quita la marca (lo que hace el kernel al usarlo por primera vez).
    fn inicializar_bitmap_bloques(&mut self, g: u64, d: &mut Descriptor) -> Result<(), ErrorFs> {
        if d.banderas() & grupos::BG_BLOCK_UNINIT == 0 {
            return Ok(());
        }
        let bm = self.bitmap_bloques_inicial(g, d.bloques_libres())?;
        if self.sb.tiene_csum_metadatos() {
            let c = csum::bitmap(self.semilla, &bm, self.sb.bloques_por_grupo as usize / 8);
            d.pon_csum_bitmap_bloques(d.recorta(c));
        }
        self.escribir_meta(d.bitmap_bloques(), &bm)?;
        d.pon_banderas(d.banderas() & !grupos::BG_BLOCK_UNINIT);
        self.guardar_descriptor(g, d)
    }

    /// Ídem para el bitmap de INODOS (`ext4_init_inode_bitmap`): todos libres, relleno a 1.
    fn inicializar_bitmap_inodos(&mut self, g: u64, d: &mut Descriptor) -> Result<(), ErrorFs> {
        if d.banderas() & grupos::BG_INODE_UNINIT == 0 {
            return Ok(());
        }
        let ipg = self.sb.inodos_por_grupo as usize;
        if d.inodos_libres() as usize != ipg {
            return Err(ErrorFs::Corrupto { que: "grupo INODE_UNINIT con inodos en uso según su descriptor" });
        }
        let bs = self.bs() as usize;
        let mut bm = vec![0u8; bs];
        for i in ipg..bs * 8 {
            pon_bit(&mut bm, i, true);
        }
        if self.sb.tiene_csum_metadatos() {
            let c = csum::bitmap(self.semilla, &bm, ipg / 8);
            d.pon_csum_bitmap_inodos(d.recorta(c));
        }
        self.escribir_meta(d.bitmap_inodos(), &bm)?;
        d.pon_banderas(d.banderas() & !grupos::BG_INODE_UNINIT);
        // Toda la tabla sigue «sin usar» hasta que se asigne su primer inodo.
        d.pon_itable_sin_usar(ipg as u32);
        self.guardar_descriptor(g, d)
    }

    /// Reserva hasta `max` bloques CONTIGUOS, empezando a buscar en `objetivo`. Devuelve
    /// (primer bloque, cuántos). Orden: bitmap → descriptor → superbloque.
    fn asignar_bloques(&mut self, objetivo: u64, max: u32) -> Result<(u64, u32), ErrorFs> {
        let bpg = self.sb.bloques_por_grupo as u64;
        let fdb = self.sb.primer_bloque_dato as u64;
        let ng = self.sb.n_grupos();
        let objetivo = objetivo.clamp(fdb, self.sb.bloques - 1);
        let g0 = (objetivo - fdb) / bpg;
        for k in 0..ng {
            let g = (g0 + k) % ng;
            let mut d = self.descriptor(g)?;
            if d.bloques_libres() == 0 {
                continue;
            }
            self.inicializar_bitmap_bloques(g, &mut d)?;
            let n_en_grupo = self.bloques_en_grupo(g) as usize;
            let mut bm = self.leer_bloque(d.bitmap_bloques())?;
            let desde = if k == 0 { ((objetivo - fdb) % bpg) as usize } else { 0 };
            // Lo liberado en la transacción ABIERTA no se reutiliza hasta confirmarla: si un corte
            // la deshace, ese bloque sigue siendo del archivo de antes, y unos datos nuevos
            // escritos en él (directos, sin journal) lo habrían destrozado.
            let base = fdb + g * bpg;
            let reservado = |i: usize| self.tx.as_ref().is_some_and(|t| t.liberados.contains(&(base + i as u64)));
            let libre = (desde..n_en_grupo).chain(0..desde).find(|&i| !bit(&bm, i) && !reservado(i));
            let Some(i0) = libre else { continue };
            let mut n = 0usize;
            while (n as u32) < max && i0 + n < n_en_grupo && !bit(&bm, i0 + n) && !reservado(i0 + n) {
                pon_bit(&mut bm, i0 + n, true);
                n += 1;
            }
            if self.sb.tiene_csum_metadatos() {
                let c = csum::bitmap(self.semilla, &bm, bpg as usize / 8);
                d.pon_csum_bitmap_bloques(d.recorta(c));
            }
            self.escribir_meta(d.bitmap_bloques(), &bm)?;
            d.pon_bloques_libres(d.bloques_libres() - n as u32);
            self.guardar_descriptor(g, &mut d)?;
            let libres = self.sb_bloques_libres() - n as u64;
            self.sb_pon_bloques_libres(libres);
            self.guardar_superbloque()?;
            return Ok((fdb + g * bpg + i0 as u64, n as u32));
        }
        Err(ErrorFs::SinEspacio)
    }

    /// Libera `n` bloques desde `inicio` (pueden cruzar grupos).
    fn liberar_bloques(&mut self, inicio: u64, n: u64) -> Result<(), ErrorFs> {
        // ANTES de liberar: quien esté leyendo sin el candado con un plan anterior verá la época
        // cambiada y repetirá la lectura (estos bloques pueden pasar a otro archivo).
        self.epoca.anotar(inicio, n);
        if let Some(t) = self.tx.as_mut() {
            // (Si la operación falla, no hace falta quitarlos: deshacerla deja esos bloques
            // OCUPADOS en el bitmap, así que el asignador no los tomaría de todos modos —
            // comprobado: quitar ese deshacer no cambia nada observable.)
            t.liberados.extend(inicio..inicio + n);
        }
        let bpg = self.sb.bloques_por_grupo as u64;
        let fdb = self.sb.primer_bloque_dato as u64;
        let mut b = inicio;
        let fin = inicio + n;
        while b < fin {
            let g = (b - fdb) / bpg;
            let i0 = ((b - fdb) % bpg) as usize;
            let cuantos = ((bpg - i0 as u64).min(fin - b)) as usize;
            let mut d = self.descriptor(g)?;
            let mut bm = self.leer_bloque(d.bitmap_bloques())?;
            for i in i0..i0 + cuantos {
                if !bit(&bm, i) {
                    // Liberar un bloque ya libre = el mapa del archivo y el bitmap no cuadran.
                    return Err(ErrorFs::NoSoportado { que: "liberar un bloque que ya está libre (FS incoherente)" });
                }
                pon_bit(&mut bm, i, false);
            }
            if self.sb.tiene_csum_metadatos() {
                let c = csum::bitmap(self.semilla, &bm, bpg as usize / 8);
                d.pon_csum_bitmap_bloques(d.recorta(c));
            }
            self.escribir_meta(d.bitmap_bloques(), &bm)?;
            d.pon_bloques_libres(d.bloques_libres() + cuantos as u32);
            self.guardar_descriptor(g, &mut d)?;
            let libres = self.sb_bloques_libres() + cuantos as u64;
            self.sb_pon_bloques_libres(libres);
            b += cuantos as u64;
        }
        self.guardar_superbloque()
    }

    /// Reserva un inodo libre, prefiriendo el grupo `grupo_pref`.
    fn asignar_inodo(&mut self, grupo_pref: u64, es_dir: bool) -> Result<u32, ErrorFs> {
        let ipg = self.sb.inodos_por_grupo as u64;
        let ng = self.sb.n_grupos();
        let primero = u32le(&self.sb_crudo, SB_PRIMER_INODO).max(11);
        for k in 0..ng {
            let g = (grupo_pref + k) % ng;
            let mut d = self.descriptor(g)?;
            if d.inodos_libres() == 0 {
                continue;
            }
            // Como `ext4_new_inode`: el grupo que recibe un inodo deja de tener bitmaps sin
            // inicializar (también el de bloques, que e2fsck relaciona con los inodos).
            self.inicializar_bitmap_inodos(g, &mut d)?;
            self.inicializar_bitmap_bloques(g, &mut d)?;
            let mut bm = self.leer_bloque(d.bitmap_inodos())?;
            let libre = (0..ipg as usize).find(|&i| !bit(&bm, i) && (g * ipg + i as u64 + 1) as u32 >= primero);
            let Some(i) = libre else { continue };
            pon_bit(&mut bm, i, true);
            if self.sb.tiene_csum_metadatos() {
                let c = csum::bitmap(self.semilla, &bm, ipg as usize / 8);
                d.pon_csum_bitmap_inodos(d.recorta(c));
            }
            self.escribir_meta(d.bitmap_inodos(), &bm)?;
            d.pon_inodos_libres(d.inodos_libres() - 1);
            if es_dir {
                d.pon_dirs_usados(d.dirs_usados() + 1);
            }
            // bg_itable_unused cuenta los inodos NUNCA usados al final de la tabla.
            let usados_hasta = ipg as u32 - d.itable_sin_usar();
            if i as u32 >= usados_hasta {
                d.pon_itable_sin_usar(ipg as u32 - i as u32 - 1);
            }
            self.guardar_descriptor(g, &mut d)?;
            self.sb_ajusta_inodos_libres(-1);
            self.guardar_superbloque()?;
            return Ok((g * ipg + i as u64 + 1) as u32);
        }
        Err(ErrorFs::SinInodos)
    }

    /// Devuelve el inodo `ino` al bitmap.
    fn liberar_inodo(&mut self, ino: u32, es_dir: bool) -> Result<(), ErrorFs> {
        let ipg = self.sb.inodos_por_grupo as u64;
        let idx = ino as u64 - 1;
        let (g, i) = (idx / ipg, (idx % ipg) as usize);
        let mut d = self.descriptor(g)?;
        let mut bm = self.leer_bloque(d.bitmap_inodos())?;
        if !bit(&bm, i) {
            return Err(ErrorFs::NoSoportado { que: "liberar un inodo que ya está libre (FS incoherente)" });
        }
        pon_bit(&mut bm, i, false);
        if self.sb.tiene_csum_metadatos() {
            let c = csum::bitmap(self.semilla, &bm, ipg as usize / 8);
            d.pon_csum_bitmap_inodos(d.recorta(c));
        }
        self.escribir_meta(d.bitmap_inodos(), &bm)?;
        d.pon_inodos_libres(d.inodos_libres() + 1);
        if es_dir {
            d.pon_dirs_usados(d.dirs_usados().saturating_sub(1));
        }
        self.guardar_descriptor(g, &mut d)?;
        self.sb_ajusta_inodos_libres(1);
        self.guardar_superbloque()
    }

    /// Crea un inodo NUEVO, vacío, con `modo` (tipo + permisos), dueño `uid`/`gid` y
    /// `enlaces`, cerca del grupo de `cerca_de` (el directorio padre). Devuelve su número.
    ///
    /// # Errors
    /// [`ErrorFs::SinInodos`] si no hay; [`ErrorFs`] de E/S.
    pub fn crear_inodo(&mut self, modo: u16, uid: u32, gid: u32, enlaces: u16, cerca_de: u32) -> Result<u32, ErrorFs> {
        self.en_tx(|fs| fs.crear_inodo_con(modo, uid, gid, enlaces, cerca_de))
    }

    fn crear_inodo_con(&mut self, modo: u16, uid: u32, gid: u32, enlaces: u16, cerca_de: u32) -> Result<u32, ErrorFs> {
        let es_dir = modo & 0xF000 == 0x4000;
        let grupo = (cerca_de.max(1) as u64 - 1) / self.sb.inodos_por_grupo as u64;
        let n = self.asignar_inodo(grupo, es_dir)?;
        let tam = self.sb.tamano_inodo as usize;
        let mut c = vec![0u8; tam];
        let (t, ns) = ahora();
        pon16(&mut c, I_MODO, modo);
        pon16(&mut c, I_UID, uid as u16);
        pon16(&mut c, 0x78, (uid >> 16) as u16);
        pon16(&mut c, I_GID, gid as u16);
        pon16(&mut c, 0x7A, (gid >> 16) as u16);
        pon16(&mut c, I_ENLACES, enlaces);
        for o in [I_ATIME, I_CTIME, I_MTIME] {
            pon32(&mut c, o, t);
        }
        pon32(&mut c, I_FLAGS, FL_EXTENTS);
        escribir_hoja(&mut c[I_BLOQUE..I_BLOQUE + 60], RAIZ_MAX as u16, &[]);
        // Generación: distinta por inodo y por momento (no tiene que ser secreta).
        pon32(&mut c, I_GEN, ns ^ n.rotate_left(16) ^ t);
        if tam > 128 {
            let quiere = u16le(&self.sb_crudo, SB_QUIERE_EXTRA);
            let extra = if quiere == 0 { 32 } else { quiere };
            pon16(&mut c, I_EXTRA, extra);
            if extra >= 24 {
                for o in [I_CTIME_EXTRA, I_MTIME_EXTRA, I_ATIME_EXTRA, I_CRTIME_EXTRA] {
                    pon32(&mut c, o, ns << 2);
                }
                pon32(&mut c, I_CRTIME, t);
            }
        }
        self.guardar_inodo(n, &mut c)?;
        Ok(n)
    }

    /// El mapa de extents de un inodo crudo (profundidad 0 o 1; más → no soportado).
    fn leer_mapa(&mut self, crudo: &[u8]) -> Result<Mapa, ErrorFs> {
        let flags = u32le(crudo, I_FLAGS);
        if flags & FL_EXTENTS == 0 {
            return Err(ErrorFs::NoSoportado { que: "archivo sin extents (mapeo clásico)" });
        }
        if flags & (FL_HUGE_FILE | FL_INLINE) != 0 {
            return Err(ErrorFs::NoSoportado { que: "archivo huge_file o inline" });
        }
        let raiz = &crudo[I_BLOQUE..I_BLOQUE + 60];
        if u16le(raiz, 0) != EXTENT_MAGIA {
            return Err(ErrorFs::ExtentMagia { vista: u16le(raiz, 0) });
        }
        if u16le(raiz, 6) > PROF_MAX {
            return Err(ErrorFs::NoSoportado { que: "árbol de extents más profundo que el máximo del kernel" });
        }
        let mut mapa = Mapa { exts: Vec::new(), hojas: Vec::new() };
        // Recorrido por pila de cualquier profundidad; `hojas` recoge TODOS los bloques del
        // árbol fuera del inodo (índices y hojas), para reusarlos al reescribirlo.
        let mut pila: Vec<(Vec<u8>, u16)> = vec![(raiz.to_vec(), u16le(raiz, 6))];
        while let Some((nodo, prof)) = pila.pop() {
            if u16le(&nodo, 0) != EXTENT_MAGIA || u16le(&nodo, 6) != prof {
                return Err(ErrorFs::ExtentMagia { vista: u16le(&nodo, 0) });
            }
            if u16le(&nodo, 2) as usize > (nodo.len() - 12) / 12 {
                return Err(ErrorFs::Corrupto { que: "nodo de extents con más entradas de las que caben" });
            }
            for i in 0..u16le(&nodo, 2) as usize {
                let o = 12 + i * 12;
                if prof == 0 {
                    let len = u16le(&nodo, o + 4) as u32;
                    if len > EXTENT_MAX_LEN {
                        return Err(ErrorFs::NoSoportado { que: "extent no inicializado (preasignado)" });
                    }
                    mapa.exts.push(Ext {
                        logico: u32le(&nodo, o),
                        len,
                        fisico: u32le(&nodo, o + 8) as u64 | ((u16le(&nodo, o + 6) as u64) << 32),
                    });
                } else {
                    let hijo = u32le(&nodo, o + 4) as u64 | ((u16le(&nodo, o + 8) as u64) << 32);
                    mapa.hojas.push(hijo);
                    let b = self.leer_bloque(hijo)?;
                    pila.push((b, prof - 1));
                }
            }
        }
        mapa.exts.sort_by_key(|e| e.logico);
        Ok(mapa)
    }

    /// Escribe el mapa en el inodo crudo: ≤ 4 extents en la raíz (profundidad 0); si no,
    /// hojas de un bloque (profundidad 1), reusando las que había. Devuelve cuántos bloques de
    /// árbol quedan (para `i_blocks`) y las hojas SOBRANTES, que el llamante libera DESPUÉS de
    /// guardar el inodo (nunca un bloque libre que el inodo en disco aún referencia).
    fn guardar_mapa(&mut self, ino: u32, crudo: &mut [u8], mut mapa: Mapa, nuevos: &mut Vec<(u64, u32)>) -> Result<(u64, Vec<u64>), ErrorFs> {
        // Fusionar extents contiguos (lógico Y físico) para no gastar entradas.
        mapa.exts.sort_by_key(|e| e.logico);
        let mut fusion: Vec<Ext> = Vec::with_capacity(mapa.exts.len());
        for e in mapa.exts {
            if let Some(u) = fusion.last_mut() {
                if u.logico + u.len == e.logico && u.fisico + u.len as u64 == e.fisico && u.len + e.len <= EXTENT_MAX_LEN {
                    u.len += e.len;
                    continue;
                }
            }
            fusion.push(e);
        }
        let bs = self.bs() as usize;
        let por_hoja = (bs - 12) / 12;
        let gen = u32le(crudo, I_GEN);
        let semilla_ino = csum::semilla_inodo(self.semilla, ino, gen);
        let mut bloques_arbol = mapa.hojas;
        if fusion.len() <= RAIZ_MAX {
            escribir_hoja(&mut crudo[I_BLOQUE..I_BLOQUE + 60], RAIZ_MAX as u16, &fusion);
            return Ok((0, bloques_arbol));
        }
        // Cuántos nodos hace falta en cada nivel (0 = hojas) hasta que el de arriba quepa en la
        // raíz del inodo (4 entradas).
        let mut por_nivel = vec![fusion.len().div_ceil(por_hoja)];
        while *por_nivel.last().expect("≥1 nivel") > RAIZ_MAX {
            let n = por_nivel.last().expect("≥1").div_ceil(por_hoja);
            por_nivel.push(n);
            if por_nivel.len() > PROF_MAX as usize {
                return Err(ErrorFs::ArbolExtentsLleno);
            }
        }
        let total: usize = por_nivel.iter().sum();
        while bloques_arbol.len() < total {
            let objetivo = fusion.last().map_or(0, |e| e.fisico + e.len as u64);
            let (b, _) = self.asignar_bloques(objetivo, 1)?;
            nuevos.push((b, 1));
            bloques_arbol.push(b);
        }
        let sobrantes = bloques_arbol.split_off(total);
        let csum_on = self.sb.tiene_csum_metadatos();
        // Nivel 0: hojas. Cada entrada del nivel siguiente = (primer lógico, bloque del hijo).
        let mut usados = 0usize;
        let mut hijos: Vec<(u32, u64)> = Vec::with_capacity(por_nivel[0]);
        for trozo in fusion.chunks(por_hoja) {
            let b = bloques_arbol[usados];
            usados += 1;
            let mut nodo = vec![0u8; bs];
            escribir_hoja(&mut nodo, por_hoja as u16, trozo);
            if csum_on {
                csum::sellar_bloque_extents(semilla_ino, &mut nodo);
            }
            self.escribir_meta(b, &nodo)?;
            hijos.push((trozo[0].logico, b));
        }
        // Niveles de índice hasta el que cabe en la raíz.
        let mut prof = 1u16;
        while hijos.len() > RAIZ_MAX {
            let mut arriba = Vec::with_capacity(hijos.len().div_ceil(por_hoja));
            for trozo in hijos.chunks(por_hoja) {
                let b = bloques_arbol[usados];
                usados += 1;
                let mut nodo = vec![0u8; bs];
                escribir_indice(&mut nodo, por_hoja as u16, prof, trozo);
                if csum_on {
                    csum::sellar_bloque_extents(semilla_ino, &mut nodo);
                }
                self.escribir_meta(b, &nodo)?;
                arriba.push((trozo[0].0, b));
            }
            hijos = arriba;
            prof += 1;
        }
        escribir_indice(&mut crudo[I_BLOQUE..I_BLOQUE + 60], RAIZ_MAX as u16, prof, &hijos);
        debug_assert_eq!(usados, total);
        Ok((total as u64, sobrantes))
    }

    fn pon_tamano_y_bloques(&self, crudo: &mut [u8], tam: u64, bloques_fs: u64) {
        pon32(crudo, I_TAM_LO, tam as u32);
        pon32(crudo, I_TAM_HI, (tam >> 32) as u32);
        // `i_blocks` cuenta también el bloque de atributos extendidos (SELinux en Fedora, ACLs)
        // si el inodo tiene uno aparte: omitirlo es un error que e2fsck marca.
        let xattr = u64::from(bloque_xattr(crudo) != 0);
        let sectores = (bloques_fs + xattr) * (self.bs() / 512);
        pon32(crudo, I_BLOQUES_LO, sectores as u32);
        pon16(crudo, I_BLOQUES_HI, (sectores >> 32) as u16);
    }

    pub(crate) fn tocar_mtime(crudo: &mut [u8]) {
        let (t, ns) = ahora();
        pon32(crudo, I_MTIME, t);
        pon32(crudo, I_CTIME, t);
        if crudo.len() > 128 && u16le(crudo, I_EXTRA) >= 12 {
            pon32(crudo, I_MTIME_EXTRA, ns << 2);
            pon32(crudo, I_CTIME_EXTRA, ns << 2);
        }
    }

    /// Escribe `datos` en el archivo `ino` desde `offset`, **creciendo** si hace falta
    /// (asigna los bloques que toque; un salto más allá del final queda como hueco disperso).
    ///
    /// # Errors
    /// [`ErrorFs::SinEspacio`], [`ErrorFs::ArbolExtentsLleno`], [`ErrorFs::NoSoportado`], E/S.
    pub fn escribir(&mut self, ino: u32, offset: u64, datos: &[u8]) -> Result<(), ErrorFs> {
        if datos.is_empty() {
            return Ok(());
        }
        self.exigir_contenido_editable(ino)?;
        self.en_tx(|fs| {
            // Lo reservado en ESTA llamada se apunta: si algo falla antes de guardar el inodo
            // que lo nombra, se devuelve al bitmap (sin journal; con journal, además, la
            // transacción entera se descarta).
            let mut nuevos = Vec::new();
            let r = fs.escribir_con(ino, offset, datos, &mut nuevos);
            if r.is_err() && !fs.con_journal() {
                for (b, n) in nuevos {
                    fs.liberar_bloques(b, n as u64)?;
                }
            }
            r
        })
    }

    /// Escribe un bloque de contenido de un inodo: de metadatos (directorio) o de datos.
    fn escribir_contenido(&mut self, meta: bool, b: u64, datos: &[u8]) -> Result<(), ErrorFs> {
        if meta { self.escribir_meta(b, datos) } else { self.escribir_datos(b, datos) }
    }

    pub(crate) fn escribir_con(&mut self, ino: u32, offset: u64, datos: &[u8], nuevos: &mut Vec<(u64, u32)>) -> Result<(), ErrorFs> {
        let mut crudo = self.inodo_crudo(ino)?;
        // El contenido de un DIRECTORIO son metadatos (van al journal); el de un archivo, datos.
        let meta = u16le(&crudo, I_MODO) & 0xF000 == 0x4000;
        let tam = u32le(&crudo, I_TAM_LO) as u64 | ((u32le(&crudo, I_TAM_HI) as u64) << 32);
        let mut mapa = self.leer_mapa(&crudo)?;
        let bs = self.bs();
        let fin = offset + datos.len() as u64;

        // Si se escribe más allá del final, la cola del último bloque viejo debe quedar a cero
        // (lo que el archivo NO contenía no puede reaparecer al crecer).
        if offset > tam && !tam.is_multiple_of(bs) {
            let l = tam / bs;
            if let Some(p) = buscar(&mapa.exts, l) {
                let mut blk = self.leer_bloque(p)?;
                let hasta = if offset / bs == l { (offset % bs) as usize } else { bs as usize };
                blk[(tam % bs) as usize..hasta].fill(0);
                self.escribir_contenido(meta,p, &blk)?;
            }
        }

        let mut l = offset / bs;
        let l_fin = (fin - 1) / bs;
        // Bloques de DATOS enteros y físicamente contiguos se juntan en UN tramo y van al disco
        // en una sola escritura (antes: una por bloque de 4 KiB). Los metadatos (directorio) no:
        // van a la transacción bloque a bloque.
        let mut tramo: Option<(u64, Vec<u8>)> = None;
        while l <= l_fin {
            let fisico = match buscar(&mapa.exts, l) {
                Some(p) => p,
                None => {
                    // Asignar una racha hasta el próximo bloque ya mapeado o el fin de la escritura.
                    let mut hasta = l;
                    while hasta < l_fin && buscar(&mapa.exts, hasta + 1).is_none() {
                        hasta += 1;
                    }
                    let objetivo = mapa
                        .exts
                        .iter()
                        .filter(|e| (e.logico as u64) < l)
                        .max_by_key(|e| e.logico)
                        .map_or_else(|| self.objetivo_de_inodo(ino), |e| e.fisico + e.len as u64);
                    let quiero = (hasta - l + 1).min(EXTENT_MAX_LEN as u64) as u32;
                    let (b, n) = self.asignar_bloques(objetivo, quiero)?;
                    nuevos.push((b, n));
                    // Bloques recién asignados que esta escritura NO cubre ENTEROS: a cero antes,
                    // para que nada viejo del disco aparezca en las partes no escritas (fuga de un
                    // archivo borrado; lo vigila tests/bloques_reutilizados.rs). Los que cubre
                    // enteros se escriben una sola vez, con los datos (antes iban dos: ceros y
                    // datos).
                    let ceros = vec![0u8; bs as usize];
                    for k in 0..n as u64 {
                        let lb = l + k;
                        let entero = lb * bs >= offset && (lb + 1) * bs <= fin;
                        if !entero {
                            self.escribir_contenido(meta, b + k, &ceros)?;
                        }
                    }
                    mapa.exts.push(Ext { logico: l as u32, len: n, fisico: b });
                    mapa.exts.sort_by_key(|e| e.logico);
                    b
                }
            };
            let ini_blk = l * bs;
            let desde = offset.max(ini_blk);
            let hasta = fin.min(ini_blk + bs);
            let trozo = &datos[(desde - offset) as usize..(hasta - offset) as usize];
            if desde == ini_blk && hasta == ini_blk + bs {
                if meta {
                    self.escribir_contenido(meta, fisico, trozo)?;
                } else {
                    let sigue = tramo.as_ref().is_some_and(|(ini, v)| ini + v.len() as u64 / bs == fisico && (v.len() as u64) < TRAMO_MAX);
                    if sigue {
                        if let Some((_, v)) = tramo.as_mut() {
                            v.extend_from_slice(trozo);
                        }
                    } else {
                        if let Some((ini, v)) = tramo.take() {
                            self.escribir_datos_tramo(ini, &v)?;
                        }
                        tramo = Some((fisico, trozo.to_vec()));
                    }
                }
            } else {
                if let Some((ini, v)) = tramo.take() {
                    self.escribir_datos_tramo(ini, &v)?;
                }
                let mut blk = self.leer_bloque(fisico)?;
                blk[(desde - ini_blk) as usize..(hasta - ini_blk) as usize].copy_from_slice(trozo);
                self.escribir_contenido(meta, fisico, &blk)?;
            }
            l += 1;
        }
        if let Some((ini, v)) = tramo.take() {
            self.escribir_datos_tramo(ini, &v)?;
        }

        let datos_fs: u64 = mapa.exts.iter().map(|e| e.len as u64).sum();
        let (arbol, sobrantes) = self.guardar_mapa(ino, &mut crudo, mapa, nuevos)?;
        self.pon_tamano_y_bloques(&mut crudo, tam.max(fin), datos_fs + arbol);
        Self::tocar_mtime(&mut crudo);
        self.guardar_inodo(ino, &mut crudo)?;
        // El inodo ya los nombra: dejan de ser «reservados en esta llamada».
        nuevos.clear();
        for b in sobrantes {
            self.liberar_bloques(b, 1)?;
        }
        // Con journal NO se vacía aquí: la BARRERA 1 del commit (transaccion.rs) hace durables
        // los datos antes del commit que los nombra (modo ordenado), y FlushFileBuffers vacía el
        // disco por `sincronizar`. Vaciar en CADA escritura costaba un FlushFileBuffers del disco
        // entero por cada WriteFile de Windows (medido: 3–6 ms, 25 MB/s secuencial). Sin journal
        // se conserva lo de siempre.
        if !self.con_journal() {
            self.disp.flush()?;
        }
        Ok(())
    }

    /// Vacía al DISCO todo lo escrito (FlushFileBuffers de Windows): con journal, los datos de
    /// archivo escritos sin cambiar metadatos no tienen otra barrera que los haga durables.
    ///
    /// # Errors
    /// E/S.
    pub fn sincronizar(&mut self) -> Result<(), ErrorFs> {
        self.disp.flush()?;
        Ok(())
    }

    /// Cambia el tamaño del archivo `ino` a `nuevo`. Encoger LIBERA los bloques de más y pone
    /// a cero la cola del último; crecer deja un hueco disperso (se lee como ceros).
    ///
    /// # Errors
    /// [`ErrorFs::NoSoportado`] o de E/S.
    pub fn truncar(&mut self, ino: u32, nuevo: u64) -> Result<(), ErrorFs> {
        self.exigir_contenido_editable(ino)?;
        self.en_tx(|fs| fs.truncar_con(ino, nuevo))
    }

    /// Solo un archivo regular o un directorio tienen CONTENIDO que se edite. Un enlace
    /// simbólico rápido guarda su destino en `i_block` (escribirlo lo trataría como raíz de
    /// extents); uno lento, en un bloque que es su destino, no datos; un dispositivo o una
    /// tubería no tienen contenido en disco. Se niega en vez de corromper.
    fn exigir_contenido_editable(&mut self, ino: u32) -> Result<(), ErrorFs> {
        match u16le(&self.inodo_crudo(ino)?, I_MODO) & 0xF000 {
            0x8000 | 0x4000 => Ok(()),
            _ => Err(ErrorFs::NoSoportado { que: "escribir o truncar algo que no es un archivo regular (enlace, dispositivo…)" }),
        }
    }

    fn truncar_con(&mut self, ino: u32, nuevo: u64) -> Result<(), ErrorFs> {
        let mut crudo = self.inodo_crudo(ino)?;
        let meta = u16le(&crudo, I_MODO) & 0xF000 == 0x4000;
        let tam = u32le(&crudo, I_TAM_LO) as u64 | ((u32le(&crudo, I_TAM_HI) as u64) << 32);
        let mut mapa = self.leer_mapa(&crudo)?;
        let bs = self.bs();
        // Cola del bloque que pasa a ser el último (encoger) o del último viejo (crecer).
        let corte = nuevo.min(tam);
        if !corte.is_multiple_of(bs) {
            if let Some(p) = buscar(&mapa.exts, corte / bs) {
                let mut blk = self.leer_bloque(p)?;
                blk[(corte % bs) as usize..].fill(0);
                self.escribir_contenido(meta,p, &blk)?;
            }
        }
        // Lo que sobra se APUNTA y se libera solo tras guardar el inodo que ya no lo nombra.
        let mut a_liberar: Vec<(u64, u64)> = Vec::new();
        if nuevo < tam {
            let quedan = nuevo.div_ceil(bs) as u32; // bloques lógicos que sobreviven
            let mut nuevos = Vec::with_capacity(mapa.exts.len());
            for e in mapa.exts {
                if e.logico >= quedan {
                    a_liberar.push((e.fisico, e.len as u64));
                } else if e.logico + e.len > quedan {
                    let guarda = quedan - e.logico;
                    a_liberar.push((e.fisico + guarda as u64, (e.len - guarda) as u64));
                    nuevos.push(Ext { len: guarda, ..e });
                } else {
                    nuevos.push(e);
                }
            }
            mapa.exts = nuevos;
        }
        let datos_fs: u64 = mapa.exts.iter().map(|e| e.len as u64).sum();
        // Encoger puede necesitar UNA hoja de árbol (raro: un hueco parte un extent); si esa
        // reserva falla tras otras, se devuelven.
        let mut nuevos = Vec::new();
        let (arbol, sobrantes) = match self.guardar_mapa(ino, &mut crudo, mapa, &mut nuevos) {
            Ok(v) => v,
            Err(e) => {
                for (b, n) in nuevos {
                    self.liberar_bloques(b, n as u64)?;
                }
                return Err(e);
            }
        };
        self.pon_tamano_y_bloques(&mut crudo, nuevo, datos_fs + arbol);
        Self::tocar_mtime(&mut crudo);
        self.guardar_inodo(ino, &mut crudo)?;
        for (b, n) in a_liberar {
            self.liberar_bloques(b, n)?;
        }
        for b in sobrantes {
            self.liberar_bloques(b, 1)?;
        }
        // Con journal NO se vacía aquí: la BARRERA 1 del commit (transaccion.rs) hace durables
        // los datos antes del commit que los nombra (modo ordenado), y FlushFileBuffers vacía el
        // disco por `sincronizar`. Vaciar en CADA escritura costaba un FlushFileBuffers del disco
        // entero por cada WriteFile de Windows (medido: 3–6 ms, 25 MB/s secuencial). Sin journal
        // se conserva lo de siempre.
        if !self.con_journal() {
            self.disp.flush()?;
        }
        Ok(())
    }

    /// Borra el inodo `ino` (ya sin enlaces que lo nombren): libera sus bloques, lo marca
    /// borrado (`links`=0, `dtime`) y lo devuelve al bitmap.
    ///
    /// # Errors
    /// [`ErrorFs`] de E/S o de un mapa no soportado.
    pub fn borrar_inodo(&mut self, ino: u32) -> Result<(), ErrorFs> {
        self.en_tx(|fs| fs.borrar_inodo_con(ino))
    }

    fn borrar_inodo_con(&mut self, ino: u32) -> Result<(), ErrorFs> {
        self.comprobar_borrable(ino)?;
        let es_dir = u16le(&self.inodo_crudo(ino)?, I_MODO) & 0xF000 == 0x4000;
        // Un enlace simbólico rápido no tiene bloques: su destino vive en i_block.
        let crudo = self.inodo_crudo(ino)?;
        if u32le(&crudo, I_FLAGS) & FL_EXTENTS != 0 {
            // `truncar_con`, no `truncar`: un enlace LENTO tiene extents y también se libera.
            self.truncar_con(ino, 0)?;
        }
        let mut crudo = self.inodo_crudo(ino)?;
        // Bloque de atributos: COMPARTIDO por recuento (el kernel deduplica atributos iguales).
        // Se descuenta este inodo; solo si era el último dueño, el bloque se libera.
        let acl = bloque_xattr(&crudo);
        let mut liberar_acl = false;
        if acl != 0 {
            let mut x = self.leer_bloque(acl)?;
            let refs = u32le(&x, 4);
            if refs > 1 {
                pon32(&mut x, 4, refs - 1);
                if self.sb.tiene_csum_metadatos() {
                    let c = csum::bloque_xattr(self.semilla, acl, &x);
                    pon32(&mut x, 0x10, c);
                }
                self.escribir_meta(acl, &x)?;
            } else {
                liberar_acl = true;
            }
            pon32(&mut crudo, 0x68, 0);
            pon16(&mut crudo, 0x76, 0);
            pon32(&mut crudo, I_BLOQUES_LO, 0);
            pon16(&mut crudo, I_BLOQUES_HI, 0);
        }
        let (t, _) = ahora();
        pon16(&mut crudo, I_ENLACES, 0);
        pon32(&mut crudo, I_DTIME, t);
        self.guardar_inodo(ino, &mut crudo)?;
        if liberar_acl {
            self.liberar_bloques(acl, 1)?;
        }
        self.liberar_inodo(ino, es_dir)
    }

    /// Pone las fechas de `ino` (segundos Unix + nanosegundos); `None` = no tocar esa. Es lo que
    /// pide Windows al copiar (conservar la fecha original) o con «Cambiar fecha». `ctime` (cambio
    /// de inodo) pasa a ahora, como en Linux.
    ///
    /// # Errors
    /// E/S.
    pub fn poner_tiempos(&mut self, ino: u32, acceso: Option<(i64, u32)>, modificacion: Option<(i64, u32)>, creacion: Option<(i64, u32)>) -> Result<(), ErrorFs> {
        self.en_tx(|fs| {
            let mut c = fs.inodo_crudo(ino)?;
            let extra = c.len() > 128 && u16le(&c, I_EXTRA) >= 24;
            // Segundos de 34 bits: los 32 bajos en el campo; los 2 altos (época) en el extra,
            // junto a los nanosegundos (bits 2..31).
            let pon = |c: &mut [u8], off: usize, off_extra: usize, (s, ns): (i64, u32)| {
                pon32(c, off, s as u32);
                if extra {
                    let epoca = ((s >> 32) as u32) & 0x3;
                    pon32(c, off_extra, (ns.min(999_999_999) << 2) | epoca);
                }
            };
            if let Some(t) = acceso {
                pon(&mut c, I_ATIME, I_ATIME_EXTRA, t);
            }
            if let Some(t) = modificacion {
                pon(&mut c, I_MTIME, I_MTIME_EXTRA, t);
            }
            if let Some(t) = creacion {
                if extra {
                    pon(&mut c, I_CRTIME, I_CRTIME_EXTRA, t);
                }
            }
            let (t, ns) = ahora();
            pon32(&mut c, I_CTIME, t);
            if extra {
                pon32(&mut c, I_CTIME_EXTRA, ns << 2);
            }
            fs.guardar_inodo(ino, &mut c)
        })
    }

    /// `chmod`: pone los 12 bits de permisos (`rwx` ×3 + setuid/setgid/sticky) de `ino`, sin
    /// tocar el TIPO. `ctime` pasa a ahora, como en Linux. Devuelve los permisos que tenía.
    ///
    /// # Errors
    /// [`ErrorFs::NoSoportado`] si `permisos` trae bits fuera de 0o7777; E/S.
    pub fn cambiar_permisos(&mut self, ino: u32, permisos: u16) -> Result<u16, ErrorFs> {
        if permisos & !0o7777 != 0 {
            return Err(ErrorFs::NoSoportado { que: "permisos con bits fuera de 0o7777" });
        }
        self.en_tx(|fs| {
            let mut c = fs.inodo_crudo(ino)?;
            let modo = u16le(&c, I_MODO);
            pon16(&mut c, I_MODO, (modo & 0xF000) | permisos);
            let extra = c.len() > 128 && u16le(&c, I_EXTRA) >= 24;
            let (t, ns) = ahora();
            pon32(&mut c, I_CTIME, t);
            if extra {
                pon32(&mut c, I_CTIME_EXTRA, ns << 2);
            }
            fs.guardar_inodo(ino, &mut c)?;
            Ok(modo & 0o7777)
        })
    }

    /// ¿Se puede liberar `ino`? Se pregunta ANTES de quitar la entrada que lo nombra: una
    /// negativa a mitad dejaría un inodo huérfano.
    ///
    /// # Errors
    /// [`ErrorFs::NoSoportado`] si tiene bloque de atributos extendidos (puede estar compartido
    /// por refcount: liberarlo mal corrompe a los demás inodos que lo usan), o el mapa no se
    /// sabe recorrer; [`ErrorFs`] de E/S.
    pub(crate) fn comprobar_borrable(&mut self, ino: u32) -> Result<(), ErrorFs> {
        let crudo = self.inodo_crudo(ino)?;
        let acl = bloque_xattr(&crudo);
        if acl != 0 {
            // Se sabe borrar, PERO solo si el bloque es de verdad un bloque de atributos sano:
            // descontar el recuento de algo que no lo es corrompería otra cosa.
            let x = self.leer_bloque(acl)?;
            let sano = u32le(&x, 0) == 0xEA02_0000
                && u32le(&x, 4) >= 1
                && (!self.sb.tiene_csum_metadatos() || csum::bloque_xattr(self.semilla, acl, &x) == u32le(&x, 0x10));
            if !sano {
                return Err(ErrorFs::NoSoportado { que: "borrar un archivo cuyo bloque de atributos está dañado (repáralo antes)" });
            }
        }
        if u32le(&crudo, I_FLAGS) & FL_EXTENTS != 0 {
            self.leer_mapa(&crudo)?; // profundidad > 1, extents no inicializados…: se sabe ya
        }
        Ok(())
    }

    /// Bloque físico donde conviene empezar a asignar para `ino` sin datos todavía: el inicio
    /// del grupo de su inodo (localidad con sus metadatos).
    fn objetivo_de_inodo(&self, ino: u32) -> u64 {
        let g = (ino as u64 - 1) / self.sb.inodos_por_grupo as u64;
        self.sb.primer_bloque_dato as u64 + g * self.sb.bloques_por_grupo as u64
    }
}

/// Bloque de atributos extendidos del inodo (`i_file_acl` lo + hi), 0 si no tiene.
fn bloque_xattr(crudo: &[u8]) -> u64 {
    u32le(crudo, 0x68) as u64 | ((u16le(crudo, 0x76) as u64) << 32)
}

/// Físico del bloque lógico `l` según los extents, o `None` si es hueco.
fn buscar(exts: &[Ext], l: u64) -> Option<u64> {
    exts.iter().find_map(|e| {
        let ini = e.logico as u64;
        (l >= ini && l < ini + e.len as u64).then(|| e.fisico + (l - ini))
    })
}
