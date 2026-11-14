// SPDX-FileCopyrightText: 2026 Juan Carlos Isaza
// SPDX-License-Identifier: LicenseRef-Xiliux-Propietario
//! **Formatear** (el `mkfs.ext4` de winext4): crea un ext4 nuevo sobre una imagen o una
//! partición.
//!
//! Disposición conservadora y completa: bloques de 4 KiB, grupos de 32768 bloques, inodos de
//! 256 bytes (uno cada 16 KiB), `sparse_super` (copias del superbloque y la GDT en los grupos
//! 0, 1 y potencias de 3, 5 y 7), y características: journal, extents, 64 bits,
//! `metadata_csum`, `dir_index`, `filetype`, `ext_attr`, `large_file`, `huge_file`,
//! `dir_nlink`, `extra_isize`. Sin `resize_inode`, sin `flex_bg`, sin `orphan_file`.
//!
//! - Bitmaps de bloques e inodos INICIALIZADOS en todos los grupos (el asignador de winext4 no
//!   usa grupos sin inicializar).
//! - Tabla de inodos a cero solo en el grupo 0; en el resto `bg_itable_unused` = todos y sin
//!   `ITABLE_ZEROED`: el kernel la pone a cero en segundo plano (como `mke2fs` con
//!   `lazy_itable_init`), y nada la lee mientras tanto.
//! - El JOURNAL se escribe ENTERO a cero: restos de un journal anterior en el disco podrían
//!   confundirse con transacciones válidas.
//! - El primer KiB (arranque) se borra: Windows no debe seguir viendo la firma de un sistema
//!   de archivos anterior.
//!
//! Validado con `e2fsck -fn`, `dumpe2fs`, el KERNEL de Linux montándolo y escribiendo, y la
//! escritura de winext4 encima (`tests/formatear.rs`).

use std::io::{Read, Seek, SeekFrom, Write};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::csum;
use crate::escritura::{escribir_hoja, pon16, pon32, Ext};
use crate::sistema::ErrorFs;

const BS: u64 = 4096;
const BPG: u64 = 32768;
const ISZ: u64 = 256;
const BYTES_POR_INODO: u64 = 16384;
const PRIMER_INODO: u32 = 11;
const INO_RAIZ: u32 = 2;
const INO_JOURNAL: u32 = 8;
const INO_LPF: u32 = 11;
const BLOQUES_LPF: u64 = 4;

const COMPAT: u32 = 0x4 | 0x8 | 0x20; // has_journal, ext_attr, dir_index
const INCOMPAT: u32 = 0x2 | 0x40 | 0x80; // filetype, extent, 64bit
const RO_COMPAT: u32 = 0x1 | 0x2 | 0x8 | 0x20 | 0x40 | 0x400; // sparse_super, large_file, huge_file, dir_nlink, extra_isize, metadata_csum

/// Lo que quedó creado.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InformeFormato {
    /// Bloques del volumen.
    pub bloques: u64,
    /// Grupos.
    pub grupos: u64,
    /// Inodos totales.
    pub inodos: u64,
    /// Bloques del journal.
    pub bloques_journal: u64,
    /// UUID del volumen.
    pub uuid: [u8; 16],
}

/// 16 bytes aleatorios sin dependencias: `RandomState` se siembra del generador del sistema.
fn aleatorio16() -> [u8; 16] {
    use std::hash::{BuildHasher, Hasher};
    let mut o = [0u8; 16];
    for (i, c) in o.chunks_mut(8).enumerate() {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_usize(i);
        h.write_u128(SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos());
        c.copy_from_slice(&h.finish().to_le_bytes());
    }
    o[6] = (o[6] & 0x0F) | 0x40; // UUID v4
    o[8] = (o[8] & 0x3F) | 0x80;
    o
}

fn tiene_copia(g: u64) -> bool {
    if g <= 1 {
        return true;
    }
    [3u64, 5, 7].iter().any(|&b| {
        let mut p = b;
        while p < g {
            p *= b;
        }
        p == g
    })
}

/// Tamaño del journal (en bloques) según el del volumen, con tope de 128 MiB.
fn tamano_journal(bloques: u64) -> u64 {
    match bloques {
        0..32_768 => 1024,
        32_768..262_144 => 4096,
        262_144..524_288 => 8192,
        524_288..1_048_576 => 16_384,
        _ => 32_768,
    }
}

fn pon_bit(bm: &mut [u8], i: u64) {
    bm[(i / 8) as usize] |= 1 << (i % 8);
}

/// Geometría de un grupo.
struct Grupo {
    inicio: u64,
    bloques: u64,
    bitmap_bloques: u64,
    bitmap_inodos: u64,
    tabla: u64,
    fin_meta: u64,
}

/// Formatea `disp` como ext4 de `tamano` bytes con la etiqueta `etiqueta` (≤ 16 bytes).
/// BORRA lo que hubiera: el llamante pide la confirmación.
///
/// # Errors
/// [`ErrorFs::NoSoportado`] si el tamaño es demasiado pequeño o la etiqueta es larga; E/S.
#[allow(clippy::too_many_lines)]
pub fn formatear<D: Read + Write + Seek>(disp: &mut D, tamano: u64, etiqueta: &str) -> Result<InformeFormato, ErrorFs> {
    if etiqueta.len() > 16 {
        return Err(ErrorFs::NoSoportado { que: "la etiqueta tiene más de 16 bytes" });
    }
    let mut bloques = tamano / BS;
    if bloques < 4096 {
        return Err(ErrorFs::NoSoportado { que: "el volumen debe tener al menos 16 MiB" });
    }
    let mut ng = bloques.div_ceil(BPG);
    // Inodos por grupo: uno cada 16 KiB, múltiplo de 16 (la tabla llena bloques enteros).
    let ipg = |bloques: u64, ng: u64| -> u64 {
        let total = bloques * BS / BYTES_POR_INODO;
        (total.div_ceil(ng)).div_ceil(16).clamp(1, BPG / 16) * 16
    };
    let mut ipg_v = ipg(bloques, ng);
    let gdt_bloques = |ng: u64| (ng * 64).div_ceil(BS);
    // Si el último grupo no cabe sus propios metadatos con holgura, se descarta (mke2fs igual).
    let ultimo = bloques - (ng - 1) * BPG;
    let meta_ultimo = if tiene_copia(ng - 1) { 1 + gdt_bloques(ng) } else { 0 } + 2 + ipg_v * ISZ / BS;
    if ng > 1 && ultimo < meta_ultimo + 64 {
        ng -= 1;
        bloques = ng * BPG;
        ipg_v = ipg(bloques, ng);
    }
    let itb = ipg_v * ISZ / BS;
    let gdtb = gdt_bloques(ng);
    let grupos: Vec<Grupo> = (0..ng)
        .map(|g| {
            let inicio = g * BPG;
            let n = BPG.min(bloques - inicio);
            let mut p = inicio;
            if tiene_copia(g) {
                p += 1 + gdtb;
            }
            Grupo { inicio, bloques: n, bitmap_bloques: p, bitmap_inodos: p + 1, tabla: p + 2, fin_meta: p + 2 + itb }
        })
        .collect();

    // Bloques de raíz, lost+found y journal, en lo libre del grupo 0 (y siguientes).
    let raiz_b = grupos[0].fin_meta;
    let lpf_b = raiz_b + 1;
    // jbd2 exige al menos 1024 bloques de journal (JBD2_MIN_JOURNAL_BLOCKS); sin pasar de un
    // cuarto del volumen (el mínimo de 16 MiB lo garantiza: 4096 bloques).
    let jbloques = tamano_journal(bloques).min(bloques / 4).max(1024);
    let mut exts: Vec<Ext> = Vec::new();
    let mut quedan = jbloques;
    let mut logico = 0u32;
    for (g, gr) in grupos.iter().enumerate() {
        if quedan == 0 {
            break;
        }
        let desde = if g == 0 { lpf_b + BLOQUES_LPF } else { gr.fin_meta };
        let hasta = gr.inicio + gr.bloques;
        let mut p = desde;
        while p < hasta && quedan > 0 {
            let len = (hasta - p).min(quedan).min(32768);
            exts.push(Ext { logico, len: len as u32, fisico: p });
            logico += len as u32;
            p += len;
            quedan -= len;
        }
    }
    if quedan > 0 {
        return Err(ErrorFs::NoSoportado { que: "no cabe el journal" });
    }

    let uuid = aleatorio16();
    let semilla_hash = aleatorio16();
    let semilla = csum::crc32c(!0, &uuid);
    let ahora = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let (t, ns) = (ahora.as_secs() as u32, ahora.subsec_nanos());

    // ---- Bitmaps y contadores ----
    let mut bm_bloques: Vec<Vec<u8>> = Vec::with_capacity(ng as usize);
    for gr in &grupos {
        let mut bm = vec![0u8; BS as usize];
        for b in gr.inicio..gr.fin_meta {
            pon_bit(&mut bm, b - gr.inicio);
        }
        for k in gr.bloques..BPG {
            pon_bit(&mut bm, k); // relleno más allá del final del último grupo
        }
        bm_bloques.push(bm);
    }
    let marcar = |bms: &mut Vec<Vec<u8>>, b: u64| pon_bit(&mut bms[(b / BPG) as usize], b % BPG);
    marcar(&mut bm_bloques, raiz_b);
    for k in 0..BLOQUES_LPF {
        marcar(&mut bm_bloques, lpf_b + k);
    }
    for e in &exts {
        for b in e.fisico..e.fisico + u64::from(e.len) {
            marcar(&mut bm_bloques, b);
        }
    }
    // El journal ocupa ≤ 128 MiB: con grupos de 128 MiB cabe en ≤ 2 trozos, siempre en la raíz
    // del inodo (4 extents). Si alguna vez no, se niega en vez de improvisar un árbol.
    if exts.len() > 4 {
        return Err(ErrorFs::NoSoportado { que: "el journal no cabe en 4 trozos" });
    }
    let libres_grupo = |bm: &[u8], n: u64| -> u32 { (0..n).filter(|&k| bm[(k / 8) as usize] & (1 << (k % 8)) == 0).count() as u32 };
    let mut bm_inodos: Vec<Vec<u8>> = Vec::with_capacity(ng as usize);
    for g in 0..ng {
        let mut bm = vec![0u8; BS as usize];
        if g == 0 {
            for i in 0..u64::from(PRIMER_INODO) {
                pon_bit(&mut bm, i);
            }
        }
        for k in ipg_v..BS * 8 {
            pon_bit(&mut bm, k);
        }
        bm_inodos.push(bm);
    }

    // ---- Inodos del grupo 0 ----
    let mut tabla0 = vec![0u8; (itb * BS) as usize];
    // Reservados (1, 3-7, 9, 10): a cero, pero con su suma.
    for n in [1u32, 3, 4, 5, 6, 7, 9, 10] {
        let o = ((n - 1) as u64 * ISZ) as usize;
        let c = &mut tabla0[o..o + ISZ as usize];
        pon16(c, 0x80, 32);
        csum::sellar_inodo(semilla, n, c);
    }
    let mut inodo = |n: u32, modo: u16, enlaces: u16, tam: u64, sectores: u64, raiz: Option<[u8; 60]>| {
        let o = ((n - 1) as u64 * ISZ) as usize;
        let c = &mut tabla0[o..o + ISZ as usize];
        pon16(c, 0x00, modo);
        pon32(c, 0x04, tam as u32);
        pon32(c, 0x6C, (tam >> 32) as u32);
        for off in [0x08, 0x0C, 0x10, 0x90] {
            pon32(c, off, t);
        }
        pon16(c, 0x1A, enlaces);
        pon32(c, 0x1C, sectores as u32);
        pon16(c, 0x74, (sectores >> 32) as u16);
        if let Some(r) = raiz {
            pon32(c, 0x20, 0x8_0000); // EXTENTS
            c[0x28..0x28 + 60].copy_from_slice(&r);
        }
        pon16(c, 0x80, 32); // extra_isize
        for off in [0x84, 0x88, 0x8C, 0x94] {
            pon32(c, off, ns << 2);
        }
        csum::sellar_inodo(semilla, n, c);
    };
    let raiz_ext = |fisico: u64, len: u32| {
        let mut r = [0u8; 60];
        escribir_hoja(&mut r, 4, &[Ext { logico: 0, len, fisico }]);
        r
    };
    inodo(INO_RAIZ, 0o40755, 3, BS, BS / 512, Some(raiz_ext(raiz_b, 1)));
    inodo(INO_LPF, 0o40700, 2, BLOQUES_LPF * BS, BLOQUES_LPF * BS / 512, Some(raiz_ext(lpf_b, BLOQUES_LPF as u32)));
    let mut jraiz = [0u8; 60];
    escribir_hoja(&mut jraiz, 4, &exts);
    inodo(INO_JOURNAL, 0o100600, 1, jbloques * BS, jbloques * BS / 512, Some(jraiz));

    // ---- Directorios: raíz y lost+found ----
    let dir_semilla = |n: u32| csum::semilla_inodo(semilla, n, 0);
    let entrada = |blk: &mut [u8], o: usize, ino: u32, rec: usize, nombre: &[u8]| {
        pon32(blk, o, ino);
        pon16(blk, o + 4, rec as u16);
        blk[o + 6] = nombre.len() as u8;
        blk[o + 7] = 2; // directorio
        blk[o + 8..o + 8 + nombre.len()].copy_from_slice(nombre);
    };
    let util = BS as usize - csum::DIR_COLA;
    let mut b_raiz = vec![0u8; BS as usize];
    entrada(&mut b_raiz, 0, INO_RAIZ, 12, b".");
    entrada(&mut b_raiz, 12, INO_RAIZ, 12, b"..");
    entrada(&mut b_raiz, 24, INO_LPF, util - 24, b"lost+found");
    csum::sellar_bloque_dir(dir_semilla(INO_RAIZ), &mut b_raiz);
    let mut b_lpf: Vec<Vec<u8>> = Vec::new();
    for k in 0..BLOQUES_LPF {
        let mut b = vec![0u8; BS as usize];
        if k == 0 {
            entrada(&mut b, 0, INO_LPF, 12, b".");
            entrada(&mut b, 12, INO_RAIZ, util - 12, b"..");
        } else {
            pon16(&mut b, 4, util as u16); // una entrada vacía que ocupa el bloque
        }
        csum::sellar_bloque_dir(dir_semilla(INO_LPF), &mut b);
        b_lpf.push(b);
    }

    // ---- Descriptores ----
    let mut gdt = vec![0u8; (gdtb * BS) as usize];
    let mut libres_total = 0u64;
    for (g, gr) in grupos.iter().enumerate() {
        let d = &mut gdt[g * 64..(g + 1) * 64];
        let pon64 = |d: &mut [u8], lo: usize, hi: usize, v: u64| {
            pon32(d, lo, v as u32);
            pon32(d, hi, (v >> 32) as u32);
        };
        pon64(d, 0x00, 0x20, gr.bitmap_bloques);
        pon64(d, 0x04, 0x24, gr.bitmap_inodos);
        pon64(d, 0x08, 0x28, gr.tabla);
        let lb = libres_grupo(&bm_bloques[g], gr.bloques);
        libres_total += u64::from(lb);
        let li = if g == 0 { ipg_v as u32 - PRIMER_INODO } else { ipg_v as u32 };
        pon16(d, 0x0C, lb as u16);
        pon16(d, 0x2C, (lb >> 16) as u16);
        pon16(d, 0x0E, li as u16);
        pon16(d, 0x2E, (li >> 16) as u16);
        let dirs = if g == 0 { 2u32 } else { 0 };
        pon16(d, 0x10, dirs as u16);
        pon16(d, 0x30, (dirs >> 16) as u16);
        pon16(d, 0x12, if g == 0 { 0x4 } else { 0 }); // ITABLE_ZEROED solo en el 0
        let sin_usar = li;
        pon16(d, 0x1C, sin_usar as u16);
        pon16(d, 0x32, (sin_usar >> 16) as u16);
        let cb = csum::bitmap(semilla, &bm_bloques[g], (BPG / 8) as usize);
        let ci = csum::bitmap(semilla, &bm_inodos[g], (ipg_v / 8) as usize);
        pon16(d, 0x18, cb as u16);
        pon16(d, 0x38, (cb >> 16) as u16);
        pon16(d, 0x1A, ci as u16);
        pon16(d, 0x3A, (ci >> 16) as u16);
        let c = csum::descriptor(semilla, g as u32, d);
        pon16(d, 0x1E, c);
    }

    // ---- Superbloque ----
    let mut sb = vec![0u8; 1024];
    let inodos = ipg_v * ng;
    let reservados = bloques / 20;
    pon32(&mut sb, 0x00, inodos as u32);
    pon32(&mut sb, 0x04, bloques as u32);
    pon32(&mut sb, 0x150, (bloques >> 32) as u32);
    pon32(&mut sb, 0x08, reservados as u32);
    pon32(&mut sb, 0x154, (reservados >> 32) as u32);
    pon32(&mut sb, 0x0C, libres_total as u32);
    pon32(&mut sb, 0x158, (libres_total >> 32) as u32);
    pon32(&mut sb, 0x10, (inodos - u64::from(PRIMER_INODO)) as u32);
    pon32(&mut sb, 0x14, 0);
    pon32(&mut sb, 0x18, 2);
    pon32(&mut sb, 0x1C, 2);
    pon32(&mut sb, 0x20, BPG as u32);
    pon32(&mut sb, 0x24, BPG as u32);
    pon32(&mut sb, 0x28, ipg_v as u32);
    pon32(&mut sb, 0x30, t);
    pon16(&mut sb, 0x36, 0xFFFF); // sin límite de montajes
    pon16(&mut sb, 0x38, 0xEF53);
    pon16(&mut sb, 0x3A, 1); // limpio
    pon16(&mut sb, 0x3C, 1); // errores: continuar
    pon32(&mut sb, 0x40, t);
    pon32(&mut sb, 0x4C, 1); // rev dinámica
    pon32(&mut sb, 0x54, PRIMER_INODO);
    pon16(&mut sb, 0x58, ISZ as u16);
    pon32(&mut sb, 0x5C, COMPAT);
    pon32(&mut sb, 0x60, INCOMPAT);
    pon32(&mut sb, 0x64, RO_COMPAT);
    sb[0x68..0x78].copy_from_slice(&uuid);
    sb[0x78..0x78 + etiqueta.len()].copy_from_slice(etiqueta.as_bytes());
    pon32(&mut sb, 0xE0, INO_JOURNAL);
    sb[0xEC..0xFC].copy_from_slice(&semilla_hash);
    sb[0xFC] = 1; // half_md4
    sb[0xFD] = 1; // copia de los bloques del journal en el superbloque
    pon16(&mut sb, 0xFE, 64);
    pon32(&mut sb, 0x100, 0x000C); // user_xattr, acl
    pon32(&mut sb, 0x108, t);
    sb[0x10C..0x10C + 60].copy_from_slice(&jraiz);
    pon32(&mut sb, 0x14C, (jbloques * BS) as u32);
    pon16(&mut sb, 0x15C, 32);
    pon16(&mut sb, 0x15E, 32);
    pon32(&mut sb, 0x160, 0x1); // hash de directorio con signo
    sb[0x175] = 1; // crc32c

    // ---- Superbloque del journal (v2, 64 bits, sumas v3, revocación) ----
    let mut jsb = vec![0u8; BS as usize];
    let be = |b: &mut [u8], o: usize, v: u32| b[o..o + 4].copy_from_slice(&v.to_be_bytes());
    be(&mut jsb, 0x00, 0xC03B_3998);
    be(&mut jsb, 0x04, 4);
    be(&mut jsb, 0x0C, BS as u32);
    be(&mut jsb, 0x10, jbloques as u32);
    be(&mut jsb, 0x14, 1);
    be(&mut jsb, 0x18, 1);
    be(&mut jsb, 0x28, 0x1 | 0x2 | 0x10);
    jsb[0x30..0x40].copy_from_slice(&uuid);
    be(&mut jsb, 0x40, 1);
    jsb[0x50] = 4; // crc32c
    jsb[0x100..0x110].copy_from_slice(&uuid);
    let c = csum::crc32c(!0, &jsb[..1024]);
    be(&mut jsb, 0xFC, c);

    // ---- Escribir ----
    let escribe = |disp: &mut D, b: u64, datos: &[u8]| -> Result<(), ErrorFs> {
        disp.seek(SeekFrom::Start(b * BS))?;
        disp.write_all(datos)?;
        Ok(())
    };
    let ceros = vec![0u8; BS as usize];
    // Bloque 0: arranque a cero + superbloque primario.
    let mut b0 = vec![0u8; BS as usize];
    let mut sb0 = sb.clone();
    let c = csum::superbloque(&sb0);
    sb0[0x3FC..].copy_from_slice(&c.to_le_bytes());
    b0[1024..2048].copy_from_slice(&sb0);
    escribe(disp, 0, &b0)?;
    for (g, gr) in grupos.iter().enumerate() {
        if tiene_copia(g as u64) {
            if g > 0 {
                let mut copia = sb.clone();
                pon16(&mut copia, 0x5A, g as u16);
                let c = csum::superbloque(&copia);
                copia[0x3FC..].copy_from_slice(&c.to_le_bytes());
                let mut blk = vec![0u8; BS as usize];
                blk[..1024].copy_from_slice(&copia);
                escribe(disp, gr.inicio, &blk)?;
            }
            for k in 0..gdtb {
                escribe(disp, gr.inicio + 1 + k, &gdt[(k * BS) as usize..((k + 1) * BS) as usize])?;
            }
        }
        escribe(disp, gr.bitmap_bloques, &bm_bloques[g])?;
        escribe(disp, gr.bitmap_inodos, &bm_inodos[g])?;
    }
    // Tabla de inodos del grupo 0, entera.
    for k in 0..itb {
        escribe(disp, grupos[0].tabla + k, &tabla0[(k * BS) as usize..((k + 1) * BS) as usize])?;
    }
    escribe(disp, raiz_b, &b_raiz)?;
    for (k, b) in b_lpf.iter().enumerate() {
        escribe(disp, lpf_b + k as u64, b)?;
    }
    // Journal entero a cero, y su superbloque.
    for e in &exts {
        for b in e.fisico..e.fisico + u64::from(e.len) {
            escribe(disp, b, &ceros)?;
        }
    }
    escribe(disp, exts[0].fisico, &jsb)?;
    disp.flush()?;
    Ok(InformeFormato { bloques, grupos: ng, inodos, bloques_journal: jbloques, uuid })
}
