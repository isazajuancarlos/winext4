// SPDX-FileCopyrightText: 2026 Juan Carlos Isaza
// SPDX-License-Identifier: LicenseRef-Xiliux-Propietario
//! **Verificación completa de un ext4** (solo lectura): lo que `e2fsck -n` comprueba en sus
//! cinco pasadas, sin tocar el disco.
//!
//! 1. Superbloque, descriptores de grupo y bitmaps: sumas.
//! 2. Inodos en uso: suma, tipo, árbol de extents (magia, límites, sumas de nodos, bloques
//!    dentro del volumen y NO reclamados por dos inodos), `i_blocks` y tamaño.
//! 3. Directorios: colas de suma, índices htree, `.` y `..`, entradas hacia inodos en uso.
//! 4. Enlaces: el recuento de cada inodo contra los directorios que lo nombran; inodos en uso
//!    que nadie nombra.
//! 5. Resumen de grupos: bitmaps de bloques e inodos contra lo REALMENTE usado, y contadores.
//!
//! El resultado es una lista de hallazgos. **Error** = algo que `e2fsck` corregiría (y que
//! hace inseguro escribir); **Aviso** = informativo. Validado por PAREJAS contra `e2fsck`:
//! imágenes sanas → cero hallazgos; corrupciones plantadas → el hallazgo que toca
//! (`tests/verificar.rs`).

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};

use crate::csum;
use crate::grupos;
use crate::sistema::{ErrorFs, Ext4, Inodo, INODO_RAIZ};

/// Gravedad de un hallazgo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gravedad {
    /// Inconsistencia que `e2fsck` repararía: no es seguro escribir así.
    Error,
    /// Informativo: no impide nada.
    Aviso,
}

/// Una cosa encontrada.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hallazgo {
    /// Gravedad.
    pub gravedad: Gravedad,
    /// Clase corta y estable (para pruebas y para agrupar): `suma-superbloque`, `bloque-libre-en-uso`…
    pub clase: &'static str,
    /// Explicación en lenguaje llano, con números.
    pub detalle: String,
}

/// El informe de una verificación.
#[derive(Debug, Clone, Default)]
pub struct InformeVerificacion {
    /// Lo encontrado (vacío = volumen sano).
    pub hallazgos: Vec<Hallazgo>,
    /// Grupos revisados.
    pub grupos: u64,
    /// Inodos en uso revisados.
    pub inodos: u64,
    /// Directorios recorridos.
    pub directorios: u64,
    /// Bloques en uso calculados (metadatos + datos).
    pub bloques_en_uso: u64,
    /// Bloques marcados en uso que NADIE usa (fugas), todos. Insumo de la reparación.
    pub bloques_fuga: Vec<u64>,
    /// Bloques de atributos extendidos dañados: (bloque, inodos que lo nombran, ¿estructura
    /// interna válida? — si sí, basta rehacer la suma y los atributos se conservan).
    pub xattr_malos: Vec<(u64, Vec<u32>, bool)>,
    /// Por grupo, lo CALCULADO: (bloques libres, inodos libres, directorios, ¿bitmap de
    /// bloques inicializado?). Insumo de la reparación de contadores.
    pub calculado_por_grupo: Vec<(u32, u32, u32, bool)>,
}

impl InformeVerificacion {
    /// Número de hallazgos de gravedad Error.
    #[must_use]
    pub fn errores(&self) -> usize {
        self.hallazgos.iter().filter(|h| h.gravedad == Gravedad::Error).count()
    }
    /// ¿Hay algún hallazgo de esta clase?
    #[must_use]
    pub fn tiene(&self, clase: &str) -> bool {
        self.hallazgos.iter().any(|h| h.clase == clase)
    }
}

/// Mapa de bits sencillo (un bit por bloque o por inodo).
struct Bits(Vec<u64>);
impl Bits {
    fn nuevo(n: u64) -> Self {
        Self(vec![0; (n as usize).div_ceil(64)])
    }
    fn get(&self, i: u64) -> bool {
        self.0[(i / 64) as usize] & (1 << (i % 64)) != 0
    }
    fn set(&mut self, i: u64) {
        self.0[(i / 64) as usize] |= 1 << (i % 64);
    }
}

fn u16le(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn u32le(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().expect("4 bytes"))
}
fn bit(bm: &[u8], i: usize) -> bool {
    bm[i / 8] & (1 << (i % 8)) != 0
}

/// ¿Lleva el grupo `g` copia de superbloque y GDT? (`sparse_super`: 0, 1 y potencias de 3, 5, 7.)
pub(crate) fn tiene_copia(g: u64, sparse: bool) -> bool {
    if !sparse || g <= 1 {
        return true;
    }
    for base in [3u64, 5, 7] {
        let mut p = base;
        while p < g {
            p *= base;
        }
        if p == g {
            return true;
        }
    }
    false
}

/// ¿Es coherente la lista de entradas de un bloque de atributos? Cabecera de 32 bytes, entradas
/// de 16 + nombre (alineadas a 4) hasta 4 bytes a cero; cada valor dentro del bloque.
pub(crate) fn xattr_estructura_valida(blk: &[u8]) -> bool {
    let mut o = 32usize;
    let mut vistas = 0;
    while o + 4 <= blk.len() {
        if blk[o..o + 4] == [0, 0, 0, 0] {
            return vistas > 0;
        }
        if o + 16 > blk.len() {
            return false;
        }
        let nlen = blk[o] as usize;
        let voff = u16le(blk, o + 2) as usize;
        let vino = u32le(blk, o + 4);
        let vlen = u32le(blk, o + 8) as usize;
        if nlen == 0 || o + 16 + nlen > blk.len() {
            return false;
        }
        if vino == 0 && (voff < 32 || voff + vlen > blk.len()) {
            return false;
        }
        vistas += 1;
        o += (16 + nlen).div_ceil(4) * 4;
    }
    false
}

const FL_EXTENTS: u32 = 0x8_0000;
const FL_INDICE: u32 = 0x1000;
const FL_HUGE_FILE: u32 = 0x4_0000;
const FL_INLINE: u32 = 0x1000_0000;
const INODO_RESIZE: u32 = 7;

/// Lo que se recoge de cada inodo en uso para las pasadas siguientes.
struct InfoInodo {
    modo: u16,
    enlaces: u16,
    flags: u32,
    gen: u32,
}

impl<R: Read + Seek> Ext4<R> {
    /// Verifica el volumen entero, sin escribir nada.
    ///
    /// # Errors
    /// Solo errores de E/S o un superbloque ilegible: TODO lo demás sale como hallazgo.
    #[allow(clippy::too_many_lines)]
    /// Comprobación RÁPIDA (segundos, también en un disco de 1 TB): suma del superbloque, marca
    /// de errores, suma de TODOS los descriptores y de TODOS los bitmaps inicializados, y la raíz.
    /// Es la puerta de cada montaje con escritura; la COMPLETA ([`Self::verificar`]) se exige
    /// además cuando [`Self::motivo_verificacion_completa`] da un motivo o esta encuentra algo.
    ///
    /// # Errors
    /// Solo E/S o un superbloque ilegible: lo demás sale como hallazgo.
    pub fn verificar_rapido(&mut self) -> Result<InformeVerificacion, ErrorFs> {
        let mut inf = InformeVerificacion::default();
        let sb = self.sb.clone();
        let csum_on = sb.tiene_csum_metadatos();
        let semilla = self.semilla;
        let (bpg, ipg) = (sb.bloques_por_grupo as usize, sb.inodos_por_grupo as usize);
        if csum_on && csum::superbloque(&self.sb_crudo) != u32le(&self.sb_crudo, csum::SB_OFF_CSUM) {
            inf.hallazgos.push(Hallazgo { gravedad: Gravedad::Error, clase: "suma-superbloque", detalle: "la suma del superbloque no coincide".into() });
        }
        if sb.estado & 0x2 != 0 {
            inf.hallazgos.push(Hallazgo { gravedad: Gravedad::Error, clase: "estado-errores", detalle: "el volumen tiene la marca de errores (ERROR_FS)".into() });
        }
        let ng = sb.n_grupos();
        for g in 0..ng {
            let d = self.descriptor(g)?;
            if csum_on && csum::descriptor(semilla, g as u32, &d.crudo) != d.csum() {
                inf.hallazgos.push(Hallazgo { gravedad: Gravedad::Error, clase: "suma-descriptor", detalle: format!("la suma del descriptor del grupo {g} no coincide") });
            }
            if !csum_on {
                continue;
            }
            if d.banderas() & grupos::BG_BLOCK_UNINIT == 0 {
                let bm = self.leer_bloque(d.bitmap_bloques())?;
                if d.recorta(csum::bitmap(semilla, &bm, bpg / 8)) != d.csum_bitmap_bloques() {
                    inf.hallazgos.push(Hallazgo { gravedad: Gravedad::Error, clase: "suma-bitmap-bloques", detalle: format!("la suma del bitmap de bloques del grupo {g} no coincide") });
                }
            }
            if d.banderas() & grupos::BG_INODE_UNINIT == 0 {
                let bm = self.leer_bloque(d.bitmap_inodos())?;
                if d.recorta(csum::bitmap(semilla, &bm, ipg / 8)) != d.csum_bitmap_inodos() {
                    inf.hallazgos.push(Hallazgo { gravedad: Gravedad::Error, clase: "suma-bitmap-inodos", detalle: format!("la suma del bitmap de inodos del grupo {g} no coincide") });
                }
            }
        }
        match self.inodo(crate::sistema::INODO_RAIZ) {
            Ok(r) if r.es_directorio() => {}
            _ => inf.hallazgos.push(Hallazgo { gravedad: Gravedad::Error, clase: "raiz", detalle: "el inodo raíz no es un directorio legible".into() }),
        }
        inf.grupos = ng;
        Ok(inf)
    }

    /// ¿Hace falta la verificación COMPLETA antes de escribir? `None` = basta la rápida. Motivos
    /// (cualquiera): marca de errores; errores registrados DESPUÉS de la última verificación
    /// completa (`s_last_error_time` > `s_lastcheck`: así unos errores viejos ya reparados y
    /// verificados no obligan a repetir, y uno nuevo sí); o última verificación completa con más
    /// de `max_dias` (o ninguna). `ahora` en segundos Unix.
    #[must_use]
    pub fn motivo_verificacion_completa(&self, ahora: u64, max_dias: u64) -> Option<String> {
        const SB_ESTADO: usize = 0x3A;
        const SB_LASTCHECK: usize = 0x40;
        const SB_ERROR_COUNT: usize = 0x194;
        const SB_LAST_ERROR_TIME: usize = 0x1CC;
        let c = &self.sb_crudo;
        if u16le(c, SB_ESTADO) & 0x2 != 0 {
            return Some("el volumen tiene la marca de errores (ERROR_FS)".into());
        }
        let ultima = u64::from(u32le(c, SB_LASTCHECK));
        let n_err = u32le(c, SB_ERROR_COUNT);
        let ultimo_error = u64::from(u32le(c, SB_LAST_ERROR_TIME));
        if n_err > 0 && ultimo_error > ultima {
            return Some(format!("hay {n_err} errores registrados, el último ({ultimo_error}) posterior a la última verificación completa ({ultima})"));
        }
        if ultima == 0 || ahora.saturating_sub(ultima) > max_dias * 86_400 {
            return Some(format!("la última verificación completa tiene más de {max_dias} días (o no hay ninguna)"));
        }
        None
    }

    pub fn verificar(&mut self) -> Result<InformeVerificacion, ErrorFs> {
        let mut inf = InformeVerificacion::default();
        let mut h = |gravedad, clase, detalle: String| inf.hallazgos.push(Hallazgo { gravedad, clase, detalle });
        let sb = self.sb.clone();
        let csum_on = sb.tiene_csum_metadatos();
        let semilla = self.semilla;
        let bs = self.bs();
        let bpg = sb.bloques_por_grupo as u64;
        let ipg = sb.inodos_por_grupo as u64;
        let fdb = sb.primer_bloque_dato as u64;
        let ng = sb.n_grupos();
        let isz = sb.tamano_inodo as usize;
        let total = sb.bloques;
        let primer_ino = u32le(&self.sb_crudo, 0x54).max(11);
        let dir_nlink = sb.ro_compat & 0x20 != 0;

        // ---- Pasada 1a: superbloque y grupos ----
        if csum_on && csum::superbloque(&self.sb_crudo) != u32le(&self.sb_crudo, csum::SB_OFF_CSUM) {
            h(Gravedad::Error, "suma-superbloque", "la suma del superbloque no coincide".into());
        }
        if sb.estado & 0x1 == 0 {
            h(Gravedad::Aviso, "no-limpio", "el volumen no se desmontó limpio".into());
        }
        if sb.incompat & crate::superbloque::INCOMPAT_RECOVER != 0 {
            h(Gravedad::Aviso, "journal-pendiente", "hay transacciones de journal sin aplicar (recupéralo antes de confiar en el resto)".into());
        }
        let sparse = sb.ro_compat & 0x1 != 0;
        let gdt_bloques = (ng * sb.tamano_desc as u64).div_ceil(bs);
        let reservados_gdt = u16le(&self.sb_crudo, 0xCE) as u64;
        let mut usados = Bits::nuevo(total);
        let marcar_meta = |usados: &mut Bits, b: u64| {
            if b < total {
                usados.set(b);
            }
        };
        let mut descs = Vec::with_capacity(ng as usize);
        let mut bm_bloques: Vec<Option<Vec<u8>>> = Vec::with_capacity(ng as usize);
        let mut bm_inodos: Vec<Option<Vec<u8>>> = Vec::with_capacity(ng as usize);
        for g in 0..ng {
            let d = self.descriptor(g)?;
            if csum_on && csum::descriptor(semilla, g as u32, &d.crudo) != d.csum() {
                h(Gravedad::Error, "suma-descriptor", format!("la suma del descriptor del grupo {g} no coincide"));
            }
            let ini = fdb + g * bpg;
            if tiene_copia(g, sparse) {
                for b in ini..ini + 1 + gdt_bloques + reservados_gdt {
                    marcar_meta(&mut usados, b);
                }
            }
            if fdb == 1 && g == 0 {
                marcar_meta(&mut usados, 0); // bloque de arranque con bloques de 1 KiB
            }
            marcar_meta(&mut usados, d.bitmap_bloques());
            marcar_meta(&mut usados, d.bitmap_inodos());
            let n_tabla = (ipg * isz as u64).div_ceil(bs);
            for b in d.tabla_inodos()..d.tabla_inodos() + n_tabla {
                marcar_meta(&mut usados, b);
            }
            let bb = if d.banderas() & grupos::BG_BLOCK_UNINIT == 0 {
                let bm = self.leer_bloque(d.bitmap_bloques())?;
                if csum_on && d.recorta(csum::bitmap(semilla, &bm, bpg as usize / 8)) != d.csum_bitmap_bloques() {
                    h(Gravedad::Error, "suma-bitmap-bloques", format!("la suma del bitmap de bloques del grupo {g} no coincide"));
                }
                Some(bm)
            } else {
                None
            };
            let bi = if d.banderas() & grupos::BG_INODE_UNINIT == 0 {
                let bm = self.leer_bloque(d.bitmap_inodos())?;
                if csum_on && d.recorta(csum::bitmap(semilla, &bm, ipg as usize / 8)) != d.csum_bitmap_inodos() {
                    h(Gravedad::Error, "suma-bitmap-inodos", format!("la suma del bitmap de inodos del grupo {g} no coincide"));
                }
                Some(bm)
            } else {
                None
            };
            descs.push(d);
            bm_bloques.push(bb);
            bm_inodos.push(bi);
        }
        inf.grupos = ng;

        // ---- Pasada 1b: inodos en uso ----
        let mut info: HashMap<u32, InfoInodo> = HashMap::new();
        let mut dueno: HashMap<u64, u32> = HashMap::new(); // bloque de datos → inodo (dobles reclamos)
        let mut xattr_vistos: HashMap<u64, Vec<u32>> = HashMap::new();
        for g in 0..ng {
            let d = &descs[g as usize];
            let Some(bmi) = bm_inodos[g as usize].clone() else { continue };
            let sin_usar = d.itable_sin_usar() as u64;
            let hasta = ipg.saturating_sub(sin_usar);
            // Tras `bg_itable_unused` no puede haber nada en uso.
            for i in hasta..ipg {
                if bit(&bmi, i as usize) {
                    h(Gravedad::Error, "itable-sin-usar", format!("el inodo {} está en uso pero cae en la zona «nunca usada» del grupo {g}", g * ipg + i + 1));
                }
            }
            if hasta == 0 {
                continue;
            }
            // La parte usada de la tabla, de una vez.
            let mut tabla = vec![0u8; hasta as usize * isz];
            self.disp.seek(SeekFrom::Start(d.tabla_inodos() * bs))?;
            self.disp.read_exact(&mut tabla)?;
            for i in 0..hasta {
                let ino = (g * ipg + i + 1) as u32;
                if !bit(&bmi, i as usize) {
                    continue;
                }
                let c = &tabla[i as usize * isz..(i as usize + 1) * isz];
                let modo = u16le(c, 0);
                let enlaces = u16le(c, 0x1A);
                if ino >= primer_ino || ino == INODO_RAIZ {
                    if csum_on && csum::inodo(semilla, ino, c) != csum::inodo_guardada(c) {
                        h(Gravedad::Error, "suma-inodo", format!("la suma del inodo {ino} no coincide"));
                    }
                    if enlaces == 0 && u32le(c, 0x14) != 0 {
                        h(Gravedad::Error, "inodo-borrado-en-uso", format!("el inodo {ino} está marcado en uso pero tiene dtime y 0 enlaces"));
                    }
                    if !matches!(modo & 0xF000, 0x1000 | 0x2000 | 0x4000 | 0x6000 | 0x8000 | 0xA000 | 0xC000) {
                        h(Gravedad::Error, "tipo-inodo", format!("el inodo {ino} tiene un tipo inválido ({modo:#o})"));
                    }
                }
                inf.inodos += 1;
                let ino_s = Inodo::leer(c);
                let flags = u32le(c, 0x20);
                let gen = u32le(c, csum::INO_OFF_GEN);
                // Bloques del inodo.
                let mut datos = 0u64;
                let mut arbol = 0u64;
                let mut max_logico: Option<u64> = None;
                let es_enlace_rapido = ino_s.es_enlace() && ino_s.tamano < 60 && flags & FL_EXTENTS == 0 && u32le(c, 0x1C) == 0;
                if flags & FL_INLINE != 0 {
                    h(Gravedad::Aviso, "inline", format!("el inodo {ino} usa inline_data (no verificado)"));
                } else if flags & FL_EXTENTS != 0 {
                    let si = csum::semilla_inodo(semilla, ino, gen);
                    let mut pila: Vec<(Vec<u8>, u16, bool)> = vec![(c[0x28..0x28 + 60].to_vec(), u16::MAX, true)];
                    while let Some((nodo, prof_esperada, es_raiz)) = pila.pop() {
                        if u16le(&nodo, 0) != 0xF30A {
                            h(Gravedad::Error, "extent-magia", format!("el inodo {ino} tiene un nodo de extents con la magia rota"));
                            continue;
                        }
                        let (n, max, prof) = (u16le(&nodo, 2) as usize, u16le(&nodo, 4) as usize, u16le(&nodo, 6));
                        let cabe = if es_raiz { 4 } else { (bs as usize - 12) / 12 };
                        if n > max || max > cabe || (prof_esperada != u16::MAX && prof != prof_esperada) {
                            h(Gravedad::Error, "extent-cabecera", format!("el inodo {ino} tiene una cabecera de extents incoherente (n={n}, max={max}, prof={prof})"));
                            continue;
                        }
                        for k in 0..n {
                            let o = 12 + k * 12;
                            if prof == 0 {
                                let logico = u32le(&nodo, o) as u64;
                                let crudo_len = u16le(&nodo, o + 4) as u64;
                                let no_init = crudo_len > 32768;
                                let len = if no_init { crudo_len - 32768 } else { crudo_len };
                                let fis = u32le(&nodo, o + 8) as u64 | ((u16le(&nodo, o + 6) as u64) << 32);
                                if fis < fdb || fis + len > total || len == 0 {
                                    h(Gravedad::Error, "extent-fuera", format!("el inodo {ino} apunta a bloques fuera del volumen ({fis}+{len})"));
                                    continue;
                                }
                                for b in fis..fis + len {
                                    if usados.get(b) {
                                        let otro = dueno.get(&b).copied();
                                        h(Gravedad::Error, "bloque-doble", format!("el bloque {b} lo reclaman el inodo {ino} y {}", otro.map_or("los metadatos".into(), |o| format!("el inodo {o}"))));
                                        break;
                                    }
                                    usados.set(b);
                                    dueno.insert(b, ino);
                                }
                                datos += len;
                                if !no_init {
                                    max_logico = Some(max_logico.map_or(logico + len - 1, |m: u64| m.max(logico + len - 1)));
                                }
                            } else {
                                let hijo = u32le(&nodo, o + 4) as u64 | ((u16le(&nodo, o + 8) as u64) << 32);
                                if hijo < fdb || hijo >= total {
                                    h(Gravedad::Error, "extent-fuera", format!("el inodo {ino} apunta a un nodo fuera del volumen ({hijo})"));
                                    continue;
                                }
                                if usados.get(hijo) {
                                    h(Gravedad::Error, "bloque-doble", format!("el nodo de extents {hijo} del inodo {ino} ya estaba en uso"));
                                    continue;
                                }
                                usados.set(hijo);
                                dueno.insert(hijo, ino);
                                arbol += 1;
                                let b = self.leer_bloque(hijo)?;
                                if csum_on {
                                    let off = csum::extent_off_cola(&b);
                                    if off + 4 <= b.len() && csum::bloque_extents(si, &b) != u32le(&b, off) {
                                        h(Gravedad::Error, "suma-extents", format!("la suma del nodo de extents {hijo} del inodo {ino} no coincide"));
                                    }
                                }
                                pila.push((b, prof - 1, false));
                            }
                        }
                    }
                } else if !es_enlace_rapido && ino != INODO_RESIZE && matches!(modo & 0xF000, 0x8000 | 0x4000 | 0xA000) {
                    // Mapeo clásico (ext2/3): directos + indirectos.
                    let mut cola: Vec<(u64, u32)> = Vec::new();
                    for k in 0..15 {
                        let p = u32le(c, 0x28 + 4 * k) as u64;
                        if p != 0 {
                            cola.push((p, if k < 12 { 0 } else { (k - 11) as u32 }));
                        }
                    }
                    while let Some((p, nivel)) = cola.pop() {
                        if p >= total {
                            h(Gravedad::Error, "bloque-fuera", format!("el inodo {ino} apunta al bloque {p}, fuera del volumen"));
                            continue;
                        }
                        if usados.get(p) {
                            h(Gravedad::Error, "bloque-doble", format!("el bloque {p} lo reclama el inodo {ino} y ya estaba en uso"));
                            continue;
                        }
                        usados.set(p);
                        dueno.insert(p, ino);
                        if nivel == 0 {
                            datos += 1;
                        } else {
                            arbol += 1;
                            let b = self.leer_bloque(p)?;
                            for k in 0..(bs as usize / 4) {
                                let q = u32le(&b, 4 * k) as u64;
                                if q != 0 {
                                    cola.push((q, nivel - 1));
                                }
                            }
                        }
                    }
                } else if ino == INODO_RESIZE {
                    // Su bloque doble-indirecto es suyo; lo que apunta son bloques de GDT reservados.
                    let p = u32le(c, 0x28 + 4 * 13) as u64;
                    if p != 0 && p < total {
                        usados.set(p);
                        arbol += 1;
                    }
                }
                // Bloque de atributos extendidos (puede estar compartido: refcount).
                let acl = u32le(c, 0x68) as u64 | ((u16le(c, 0x76) as u64) << 32);
                let mut xattr = 0;
                if acl != 0 {
                    if acl >= total {
                        h(Gravedad::Error, "xattr-fuera", format!("el bloque de atributos del inodo {ino} está fuera del volumen"));
                    } else {
                        xattr_vistos.entry(acl).or_default().push(ino);
                        usados.set(acl);
                        xattr = 1;
                    }
                }
                // i_blocks.
                if ino >= primer_ino || ino == INODO_RAIZ {
                    let guardado = u32le(c, 0x1C) as u64 | ((u16le(c, 0x74) as u64) << 32);
                    let calc = (datos + arbol + xattr) * if flags & FL_HUGE_FILE != 0 { 1 } else { bs / 512 };
                    if guardado != calc && flags & FL_INLINE == 0 && !es_enlace_rapido {
                        h(Gravedad::Error, "i-blocks", format!("el inodo {ino} dice ocupar {guardado} sectores y ocupa {calc}"));
                    }
                    // Tamaño: nada INICIALIZADO puede quedar más allá del final.
                    if let Some(m) = max_logico {
                        if ino_s.es_regular() && m >= ino_s.tamano.div_ceil(bs) {
                            h(Gravedad::Error, "tamano", format!("el inodo {ino} tiene datos más allá de su tamaño ({} bytes, último bloque {m})", ino_s.tamano));
                        }
                    }
                    if ino_s.es_directorio() && !ino_s.tamano.is_multiple_of(bs) {
                        h(Gravedad::Error, "tamano-dir", format!("el directorio {ino} tiene un tamaño que no es múltiplo del bloque"));
                    }
                }
                info.insert(ino, InfoInodo { modo, enlaces, flags, gen });
            }
        }

        // ---- Pasadas 2 y 3: directorios ----
        let mut refs: HashMap<u32, u32> = HashMap::new();
        let mut subdirs: HashMap<u32, u32> = HashMap::new();
        let mut padre_de: HashMap<u32, u32> = HashMap::new(); // por la entrada que lo nombra
        let mut puntopunto: HashMap<u32, u32> = HashMap::new(); // lo que dice su ".."
        let en_uso = |ino: u32| -> bool {
            if ino == 0 || ino as u64 > ipg * ng {
                return false;
            }
            let (g, i) = ((ino as u64 - 1) / ipg, ((ino as u64 - 1) % ipg) as usize);
            bm_inodos[g as usize].as_ref().is_some_and(|bm| bit(bm, i))
        };
        let dirs: Vec<u32> = info.iter().filter(|(_, v)| v.modo & 0xF000 == 0x4000).map(|(&k, _)| k).collect();
        for &dino in &dirs {
            let di = self.inodo(dino)?;
            let Ok(bloques) = self.bloques_fisicos(&di) else { continue };
            inf.directorios += 1;
            let meta = &info[&dino];
            let si = csum::semilla_inodo(semilla, dino, meta.gen);
            let indexado = meta.flags & FL_INDICE != 0;
            // Bloques índice del htree: el 0 y, con niveles, los nodos que la raíz nombra.
            let mut indices: Vec<usize> = Vec::new();
            if indexado {
                indices.push(0);
                if let Some(Some(b0)) = bloques.first() {
                    let raiz = self.leer_bloque(*b0)?;
                    let base = 0x18 + raiz[0x1D] as usize;
                    if csum_on {
                        let (cola, c) = csum::bloque_dx(si, &raiz, base);
                        if cola + 8 <= raiz.len() && c != u32le(&raiz, cola + 4) {
                            h(Gravedad::Error, "suma-htree", format!("la suma del índice htree del directorio {dino} no coincide"));
                        }
                    }
                    if raiz[0x1E] > 0 {
                        let cuenta = u16le(&raiz, base + 2) as usize;
                        for k in 0..cuenta {
                            let l = u32le(&raiz, base + 4 + 8 * k) as usize;
                            indices.push(l);
                            if let Some(Some(bn)) = bloques.get(l) {
                                let nodo = self.leer_bloque(*bn)?;
                                if csum_on {
                                    let (cola, c) = csum::bloque_dx(si, &nodo, 8);
                                    if cola + 8 <= nodo.len() && c != u32le(&nodo, cola + 4) {
                                        h(Gravedad::Error, "suma-htree", format!("la suma de un nodo htree del directorio {dino} no coincide"));
                                    }
                                }
                            }
                        }
                    }
                }
            }
            for (l, b) in bloques.iter().enumerate() {
                let Some(b) = *b else {
                    h(Gravedad::Error, "dir-hueco", format!("el directorio {dino} tiene un hueco en el bloque lógico {l}"));
                    continue;
                };
                let blk = self.leer_bloque(b)?;
                let es_indice = indices.contains(&l);
                let limite = if csum_on && !es_indice {
                    if !csum::dir_tiene_cola(&blk) {
                        h(Gravedad::Error, "dir-sin-cola", format!("el bloque {l} del directorio {dino} no tiene cola de suma"));
                        blk.len()
                    } else {
                        if csum::bloque_dir(si, &blk) != u32le(&blk, blk.len() - 4) {
                            h(Gravedad::Error, "suma-dir", format!("la suma del bloque {l} del directorio {dino} no coincide"));
                        }
                        blk.len() - csum::DIR_COLA
                    }
                } else {
                    blk.len()
                };
                if es_indice && l != 0 {
                    continue; // nodo interno: no lleva entradas
                }
                let mut i = 0usize;
                while i + 8 <= limite {
                    let e = u32le(&blk, i);
                    let rec = u16le(&blk, i + 4) as usize;
                    let nl = blk[i + 6] as usize;
                    if rec < 8 || !rec.is_multiple_of(4) || i + rec > limite || (e != 0 && rec < (8 + nl).div_ceil(4) * 4) {
                        h(Gravedad::Error, "dir-rec-len", format!("entrada corrupta (rec_len {rec}) en el bloque {l} del directorio {dino}"));
                        break;
                    }
                    if e != 0 {
                        let nombre = &blk[i + 8..i + 8 + nl];
                        if !en_uso(e) {
                            h(Gravedad::Error, "dir-a-libre", format!("«{}» en el directorio {dino} apunta al inodo {e}, que no está en uso", String::from_utf8_lossy(nombre)));
                        } else if nombre == b"." {
                            if e != dino {
                                h(Gravedad::Error, "dir-punto", format!("el «.» del directorio {dino} apunta a {e}"));
                            }
                        } else if nombre == b".." {
                            puntopunto.insert(dino, e);
                        } else {
                            *refs.entry(e).or_insert(0) += 1;
                            if info.get(&e).is_some_and(|x| x.modo & 0xF000 == 0x4000) {
                                *subdirs.entry(dino).or_insert(0) += 1;
                                padre_de.insert(e, dino);
                            }
                        }
                    }
                    i += rec;
                    if es_indice && l == 0 && i >= 24 {
                        break; // en la raíz htree solo «.» y «..»
                    }
                }
            }
        }

        // ---- Pasada 4: enlaces ----
        // Inodos de SISTEMA que el superbloque nombra y ningún directorio (a propósito):
        // journal, archivo de huérfanos y cuotas.
        let especiales: Vec<u32> = [0xE0usize, 0x280, 0x240, 0x244, 0x26C]
            .iter()
            .map(|&o| u32le(&self.sb_crudo, o))
            .filter(|&n| n != 0)
            .collect();
        for (&ino, v) in &info {
            if (ino < primer_ino && ino != INODO_RAIZ) || especiales.contains(&ino) {
                continue;
            }
            let es_dir = v.modo & 0xF000 == 0x4000;
            let nombrado = refs.get(&ino).copied().unwrap_or(0);
            if ino != INODO_RAIZ && nombrado == 0 && v.enlaces > 0 {
                h(Gravedad::Error, "huerfano", format!("el inodo {ino} está en uso pero ningún directorio lo nombra"));
                continue;
            }
            let esperado = if es_dir { 2 + subdirs.get(&ino).copied().unwrap_or(0) } else { nombrado };
            let ok = u32::from(v.enlaces) == esperado || (es_dir && dir_nlink && v.enlaces == 1 && esperado >= 2);
            if !ok {
                h(Gravedad::Error, "enlaces", format!("el inodo {ino} dice tener {} enlaces y tiene {esperado}", v.enlaces));
            }
            if es_dir {
                let esperado_pp = if ino == INODO_RAIZ { INODO_RAIZ } else { padre_de.get(&ino).copied().unwrap_or(0) };
                if puntopunto.get(&ino).copied() != Some(esperado_pp) && esperado_pp != 0 {
                    h(Gravedad::Error, "dir-puntopunto", format!("el «..» del directorio {ino} no apunta a su padre {esperado_pp}"));
                }
            }
        }

        // ---- Pasada 5: bitmaps y contadores ----
        let mut libres_total = 0u64;
        let mut inodos_libres_total = 0u64;
        for g in 0..ng {
            let d = &descs[g as usize];
            let ini = fdb + g * bpg;
            let n = bpg.min(total - ini);
            let usados_g = (ini..ini + n).filter(|&b| usados.get(b)).count() as u64;
            let libres_calc = n - usados_g;
            if let Some(bm) = &bm_bloques[g as usize] {
                let mut libre_en_uso = Vec::new();
                let mut fuga = Vec::new();
                for k in 0..n {
                    match (bit(bm, k as usize), usados.get(ini + k)) {
                        (false, true) => libre_en_uso.push(ini + k),
                        (true, false) => fuga.push(ini + k),
                        _ => {}
                    }
                }
                let muestra = |v: &[u64]| v.iter().take(8).map(u64::to_string).collect::<Vec<_>>().join(", ");
                if !libre_en_uso.is_empty() {
                    h(Gravedad::Error, "bloque-libre-en-uso", format!("grupo {g}: {} bloques EN USO están marcados libres (se sobrescribirían): {}", libre_en_uso.len(), muestra(&libre_en_uso)));
                }
                if !fuga.is_empty() {
                    h(Gravedad::Error, "bloque-fuga", format!("grupo {g}: {} bloques marcados en uso que nadie usa: {}", fuga.len(), muestra(&fuga)));
                }
                inf.bloques_fuga.extend_from_slice(&fuga);
            } else if usados_g > (ini..ini + n).filter(|&b| usados.get(b) && !dueno.contains_key(&b)).count() as u64 {
                h(Gravedad::Error, "bloque-en-grupo-sin-init", format!("grupo {g}: hay datos en un grupo cuyo bitmap dice «sin inicializar»"));
            }
            if d.banderas() & grupos::BG_BLOCK_UNINIT == 0 && u64::from(d.bloques_libres()) != libres_calc {
                h(Gravedad::Error, "contador-bloques", format!("grupo {g}: dice {} bloques libres y hay {libres_calc}", d.bloques_libres()));
            }
            libres_total += libres_calc;
            // Inodos.
            let usados_i = bm_inodos[g as usize].as_ref().map_or(0, |bm| (0..ipg).filter(|&i| bit(bm, i as usize)).count() as u64);
            let libres_i = ipg - usados_i;
            if u64::from(d.inodos_libres()) != libres_i {
                h(Gravedad::Error, "contador-inodos", format!("grupo {g}: dice {} inodos libres y hay {libres_i}", d.inodos_libres()));
            }
            inodos_libres_total += libres_i;
            let dirs_g = info.iter().filter(|(&k, v)| (k as u64 - 1) / ipg == g && v.modo & 0xF000 == 0x4000).count() as u32;
            if d.dirs_usados() != dirs_g {
                h(Gravedad::Error, "contador-dirs", format!("grupo {g}: dice {} directorios y hay {dirs_g}", d.dirs_usados()));
            }
            inf.calculado_por_grupo.push((libres_calc as u32, libres_i as u32, dirs_g, bm_bloques[g as usize].is_some()));
        }
        let sb_libres = self.bloques_libres();
        if sb_libres != libres_total {
            h(Gravedad::Aviso, "contador-sb-bloques", format!("el superbloque dice {sb_libres} bloques libres y hay {libres_total} (el kernel lo recalcula al montar)"));
        }
        let sb_inodos = u64::from(u32le(&self.sb_crudo, 0x10));
        if sb_inodos != inodos_libres_total {
            h(Gravedad::Aviso, "contador-sb-inodos", format!("el superbloque dice {sb_inodos} inodos libres y hay {inodos_libres_total}"));
        }
        inf.bloques_en_uso = total - libres_total;
        // ---- Bloques de atributos extendidos: magia, recuento de dueños, suma ----
        let mut bloques_xattr: Vec<(u64, Vec<u32>)> = xattr_vistos.into_iter().collect();
        bloques_xattr.sort_unstable();
        for (b, duenos) in bloques_xattr {
            let blk = self.leer_bloque(b)?;
            let magia = u32le(&blk, 0);
            let refs = u32le(&blk, 4);
            let estructura = magia == 0xEA02_0000 && xattr_estructura_valida(&blk);
            if magia != 0xEA02_0000 {
                h(Gravedad::Error, "xattr-magia", format!("el bloque de atributos {b} (inodos {duenos:?}) no tiene la magia de xattr"));
                inf.xattr_malos.push((b, duenos.clone(), false));
                continue;
            }
            if refs as usize != duenos.len() {
                h(Gravedad::Error, "xattr-refcount", format!("el bloque de atributos {b} dice tener {refs} dueños y lo nombran {}", duenos.len()));
            }
            if csum_on && csum::bloque_xattr(semilla, b, &blk) != u32le(&blk, 0x10) {
                h(Gravedad::Error, "suma-xattr", format!("la suma del bloque de atributos {b} (inodos {duenos:?}) no coincide{}", if estructura { "; su contenido es legible" } else { " y su contenido está dañado" }));
                inf.xattr_malos.push((b, duenos, estructura));
            }
        }
        Ok(inf)
    }
}
