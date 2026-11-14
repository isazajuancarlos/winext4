// SPDX-FileCopyrightText: 2026 Juan Carlos Isaza
// SPDX-License-Identifier: LicenseRef-Xiliux-Propietario
//! F3 — **directorios**: insertar y quitar entradas (lineales e indexados htree), y encima
//! las operaciones que ve el usuario: crear archivo, crear directorio, borrar, borrar
//! directorio y renombrar.
//!
//! Orden anti-corrupción (sin journal todavía): al CREAR, primero el inodo completo y
//! después la entrada que lo nombra (un corte deja un inodo huérfano que `e2fsck` lleva a
//! `lost+found`, nunca una entrada hacia un inodo a medio hacer); al BORRAR, primero se quita
//! la entrada y después se libera el inodo.
//!
//! Directorios indexados: el nombre nuevo va a la hoja que dicta su hash (`hash.rs`). Si esa
//! hoja está llena, se PARTE (mitad alta por hash a un bloque nuevo + entrada en el índice
//! padre). Si ese nodo índice también está lleno, se parte el nodo (o, si es la raíz, se le
//! añade un nivel), como `ext4_dx_add_entry`. Con 2 niveles llenos → `SinEspacio` (`ENOSPC`).

use std::io::{Read, Seek, Write};

use crate::csum;
use crate::escritura::{pon16, pon32, u16le, u32le, I_CTIME, I_ENLACES, I_GEN, I_GID, I_MODO, I_UID};
use crate::hash;
use crate::sistema::{ErrorFs, Ext4, Inodo};
use crate::superbloque::INCOMPAT_FILETYPE;

/// `EXT4_INDEX_FL`: el directorio tiene índice htree.
const FL_INDICE: u32 = 0x1000;
/// `s_flags` (superbloque): hash de directorio con bytes con signo / sin signo.
const SB_FLAGS: usize = 0x160;
const SB_FLAG_HASH_CON_SIGNO: u32 = 0x1;
const SB_FLAG_HASH_SIN_SIGNO: u32 = 0x2;
/// `s_hash_seed`.
const SB_SEMILLA_HASH: usize = 0xEC;
/// Máximo de enlaces de un directorio antes de pasar a «1 = muchos» (`dir_nlink`).
const MAX_ENLACES_DIR: u16 = 65000;

/// Un paso del camino por el índice htree: nodo (bloque lógico), offset de su
/// `dx_countlimit`, y la entrada que se eligió para bajar.
struct PasoDx {
    logico: usize,
    base: usize,
    idx: usize,
}

/// Tamaño real de una entrada con un nombre de `n` bytes (8 de cabecera, alineado a 4).
fn tam_entrada(n: usize) -> usize {
    (8 + n + 3) & !3
}

/// `file_type` de una entrada según el `i_mode` del inodo.
fn tipo_de_modo(modo: u16) -> u8 {
    match modo & 0xF000 {
        0x8000 => 1,
        0x4000 => 2,
        0x2000 => 3,
        0x6000 => 4,
        0x1000 => 5,
        0xC000 => 6,
        0xA000 => 7,
        _ => 0,
    }
}

fn validar_nombre(nombre: &str) -> Result<(), ErrorFs> {
    let b = nombre.as_bytes();
    if b.is_empty() || b.len() > 255 || b.contains(&b'/') || b.contains(&0) || nombre == "." || nombre == ".." {
        return Err(ErrorFs::NombreInvalido);
    }
    Ok(())
}

/// Intenta colocar la entrada en `blk` (bytes útiles hasta `limite`). `true` si cupo.
fn meter_en_bloque(blk: &mut [u8], limite: usize, nombre: &[u8], ino: u32, tipo: u8) -> Result<bool, ErrorFs> {
    let hace_falta = tam_entrada(nombre.len());
    let mut i = 0usize;
    while i < limite {
        let eno = u32le(blk, i);
        let rec = u16le(blk, i + 4) as usize;
        if rec < 8 || i + rec > limite {
            return Err(ErrorFs::NoSoportado { que: "bloque de directorio con rec_len incoherente" });
        }
        let (donde, rec_nuevo) = if eno == 0 && rec >= hace_falta {
            (i, rec)
        } else {
            let real = tam_entrada(blk[i + 6] as usize);
            if eno != 0 && rec >= real + hace_falta {
                pon16(blk, i + 4, real as u16);
                (i + real, rec - real)
            } else {
                i += rec;
                continue;
            }
        };
        pon32(blk, donde, ino);
        pon16(blk, donde + 4, rec_nuevo as u16);
        blk[donde + 6] = nombre.len() as u8;
        blk[donde + 7] = tipo;
        blk[donde + 8..donde + 8 + nombre.len()].copy_from_slice(nombre);
        return Ok(true);
    }
    Ok(false)
}

/// Quita la entrada `nombre` de `blk`; devuelve su inodo si estaba.
fn quitar_de_bloque(blk: &mut [u8], limite: usize, nombre: &[u8]) -> Option<u32> {
    let mut i = 0usize;
    let mut previa: Option<usize> = None;
    while i + 8 <= limite {
        let eno = u32le(blk, i);
        let rec = u16le(blk, i + 4) as usize;
        if rec < 8 || i + rec > limite {
            return None;
        }
        let n = blk[i + 6] as usize;
        if eno != 0 && &blk[i + 8..i + 8 + n] == nombre {
            match previa {
                Some(p) => {
                    let r = u16le(blk, p + 4) as usize + rec;
                    pon16(blk, p + 4, r as u16);
                }
                None => pon32(blk, i, 0),
            }
            return Some(eno);
        }
        previa = Some(i);
        i += rec;
    }
    None
}

impl<R: Read + Write + Seek> Ext4<R> {
    /// Si el FS lleva sumas, cada bloque hoja de directorio reserva 12 bytes de cola.
    fn hueco_cola(&self) -> usize {
        if self.sb.tiene_csum_metadatos() { csum::DIR_COLA } else { 0 }
    }

    fn semilla_de(&mut self, ino: u32) -> Result<u32, ErrorFs> {
        let crudo = self.inodo_crudo(ino)?;
        Ok(csum::semilla_inodo(self.semilla, ino, u32le(&crudo, I_GEN)))
    }

    /// Sella (si toca) y escribe un bloque hoja del directorio `dir`.
    fn guardar_bloque_dir(&mut self, dir: u32, fisico: u64, blk: &mut [u8]) -> Result<(), ErrorFs> {
        if self.sb.tiene_csum_metadatos() {
            let s = self.semilla_de(dir)?;
            csum::sellar_bloque_dir(s, blk);
        }
        self.escribir_meta(fisico, blk)
    }

    /// Bloques físicos del directorio (todos mapeados; un hueco en un directorio es corrupción).
    fn bloques_dir(&mut self, ino: &Inodo) -> Result<Vec<u64>, ErrorFs> {
        if !ino.es_directorio() {
            return Err(ErrorFs::NoEsDirectorio);
        }
        self.bloques_fisicos(ino)?
            .into_iter()
            .map(|b| b.ok_or(ErrorFs::NoSoportado { que: "directorio con huecos" }))
            .collect()
    }

    /// DIAGNÓSTICO: la hoja (bloque lógico) del directorio indexado `dir` donde el índice
    /// manda a `nombre`. `semilla` sustituye a la del volumen (solo para pruebas de que la
    /// comprobación discrimina); `None` = la real.
    ///
    /// # Errors
    /// [`ErrorFs::NoSoportado`] si el directorio no es indexado o el hash no se soporta.
    pub fn hoja_de(&mut self, dir: u32, nombre: &str, semilla: Option<[u32; 4]>) -> Result<usize, ErrorFs> {
        let d = self.inodo(dir)?;
        if d.flags & FL_INDICE == 0 {
            return Err(ErrorFs::NoSoportado { que: "directorio no indexado" });
        }
        let bloques = self.bloques_dir(&d)?;
        self.hoja_por_hash_con(&bloques, nombre.as_bytes(), semilla)
    }

    /// La hoja (bloque LÓGICO) de un directorio indexado donde debe vivir `nombre`.
    fn hoja_por_hash(&mut self, bloques: &[u64], nombre: &[u8]) -> Result<usize, ErrorFs> {
        self.hoja_por_hash_con(bloques, nombre, None)
    }

    fn hoja_por_hash_con(&mut self, bloques: &[u64], nombre: &[u8], otra_semilla: Option<[u32; 4]>) -> Result<usize, ErrorFs> {
        Ok(self.ruta_dx(bloques, nombre, otra_semilla)?.1)
    }

    /// Cómo se calcula el hash en este directorio: (versión, semilla, con signo).
    fn config_hash(&self, raiz: &[u8], otra_semilla: Option<[u32; 4]>) -> Result<(u8, [u32; 4], bool), ErrorFs> {
        let mut version = raiz[0x1C];
        let flags = u32le(&self.sb_crudo, SB_FLAGS);
        let con_signo = if flags & SB_FLAG_HASH_SIN_SIGNO != 0 {
            false
        } else if flags & SB_FLAG_HASH_CON_SIGNO != 0 {
            true
        } else {
            return Err(ErrorFs::NoSoportado { que: "superbloque sin marca de signo del hash" });
        };
        if version > 2 {
            version -= 3; // las variantes *_UNSIGNED explícitas
        }
        let mut semilla = [0u32; 4];
        for (k, w) in semilla.iter_mut().enumerate() {
            *w = u32le(&self.sb_crudo, SB_SEMILLA_HASH + 4 * k);
        }
        if let Some(s) = otra_semilla {
            semilla = s;
        }
        Ok((version, semilla, con_signo))
    }

    /// El camino del índice hasta la hoja de `nombre`: los nodos índice recorridos
    /// (bloque lógico, offset de su `countlimit`, entrada elegida), la hoja, y el hash.
    fn ruta_dx(&mut self, bloques: &[u64], nombre: &[u8], otra_semilla: Option<[u32; 4]>) -> Result<(Vec<PasoDx>, usize, u32), ErrorFs> {
        let raiz = self.leer_bloque(bloques[0])?;
        let (version, semilla, con_signo) = self.config_hash(&raiz, otra_semilla)?;
        let h = hash::hash_nombre(nombre, version, semilla, con_signo)
            .ok_or(ErrorFs::NoSoportado { que: "hash de directorio legacy" })?;
        let largo_info = raiz[0x1D] as usize;
        let niveles = raiz[0x1E];
        let mut ruta = Vec::new();
        let mut nodo = raiz;
        let mut logico = 0usize;
        let mut base = 0x18 + largo_info;
        for nivel in 0..=niveles {
            let cuenta = u16le(&nodo, base + 2) as usize;
            let mut idx = 0usize;
            let mut elegido = u32le(&nodo, base + 4) as usize; // la entrada 0 cubre desde 0
            for k in 1..cuenta {
                let o = base + 8 * k;
                if u32le(&nodo, o) <= h {
                    idx = k;
                    elegido = u32le(&nodo, o + 4) as usize;
                } else {
                    break;
                }
            }
            if elegido >= bloques.len() {
                return Err(ErrorFs::NoSoportado { que: "índice htree apunta fuera del directorio" });
            }
            ruta.push(PasoDx { logico, base, idx });
            if nivel == niveles {
                return Ok((ruta, elegido, h));
            }
            nodo = self.leer_bloque(bloques[elegido])?;
            logico = elegido;
            base = 8;
        }
        unreachable!("el bucle siempre devuelve en el último nivel")
    }

    /// Parte la hoja llena `hoja` del directorio indexado `dir` para meter `nombre`: ordena
    /// por hash sus entradas más la nueva, pasa la mitad alta a un bloque NUEVO al final del
    /// directorio, y añade al nodo índice padre la entrada (hash de corte → bloque nuevo).
    /// `Ok(false)` SIN escribir nada si el nodo índice padre está lleno: el llamante hace sitio
    /// en el índice ([`Self::hacer_sitio_en_indice`]) y reintenta. Se niega si todos los hashes
    /// de la hoja son iguales (no hay corte posible sin la marca de colisión).
    fn partir_hoja(&mut self, dir: u32, bloques: &[u64], nombre: &str, ino: u32, tipo: u8) -> Result<bool, ErrorFs> {
        let (ruta, hoja, _) = self.ruta_dx(bloques, nombre.as_bytes(), None)?;
        let raiz = self.leer_bloque(bloques[0])?;
        let (version, semilla, con_signo) = self.config_hash(&raiz, None)?;
        let hash_de = |n: &[u8]| hash::hash_nombre(n, version, semilla, con_signo).unwrap_or(0);
        let bs = self.bs() as usize;
        let limite = bs - self.hueco_cola();

        // Entradas vivas de la hoja + la nueva, ordenadas por hash.
        let blk = self.leer_bloque(bloques[hoja])?;
        let mut ents: Vec<(u32, u32, u8, Vec<u8>)> = Vec::new();
        let mut i = 0usize;
        while i + 8 <= limite {
            let rec = u16le(&blk, i + 4) as usize;
            if rec < 8 || i + rec > limite {
                return Err(ErrorFs::NoSoportado { que: "hoja htree con rec_len incoherente" });
            }
            let eno = u32le(&blk, i);
            let n = blk[i + 6] as usize;
            if eno != 0 && n > 0 {
                let nom = blk[i + 8..i + 8 + n].to_vec();
                ents.push((hash_de(&nom), eno, blk[i + 7], nom));
            }
            i += rec;
        }
        ents.push((hash_de(nombre.as_bytes()), ino, tipo, nombre.as_bytes().to_vec()));
        ents.sort_by_key(|e| e.0);
        let n = ents.len();
        let mut m = n / 2;
        while m < n && ents[m].0 == ents[m - 1].0 {
            m += 1;
        }
        if m == n {
            m = n / 2;
            while m > 0 && ents[m].0 == ents[m - 1].0 {
                m -= 1;
            }
        }
        if m == 0 {
            return Err(ErrorFs::NoSoportado { que: "hoja htree con todos los hashes iguales" });
        }
        let corte = ents[m].0;

        // El padre debe tener sitio para una entrada más.
        let padre = ruta.last().expect("la ruta siempre tiene la raíz");
        let mut pblk = self.leer_bloque(bloques[padre.logico])?;
        let (lim_p, cnt_p) = (u16le(&pblk, padre.base) as usize, u16le(&pblk, padre.base + 2) as usize);
        if cnt_p >= lim_p {
            return Ok(false);
        }

        let construir = |trozo: &[(u32, u32, u8, Vec<u8>)]| -> Result<Vec<u8>, ErrorFs> {
            let mut b = vec![0u8; bs];
            let mut o = 0usize;
            for (k, (_, eno, t, nom)) in trozo.iter().enumerate() {
                let tam = tam_entrada(nom.len());
                let rec = if k + 1 == trozo.len() { limite - o } else { tam };
                if o + tam > limite {
                    return Err(ErrorFs::NoSoportado { que: "la mitad de la hoja no cabe en un bloque" });
                }
                pon32(&mut b, o, *eno);
                pon16(&mut b, o + 4, rec as u16);
                b[o + 6] = nom.len() as u8;
                b[o + 7] = *t;
                b[o + 8..o + 8 + nom.len()].copy_from_slice(nom);
                o += rec;
            }
            Ok(b)
        };
        let mut a = construir(&ents[..m])?;
        let mut b = construir(&ents[m..])?;

        // (1) La hoja nueva, al final del directorio (lo asigna `escribir`).
        let nuevo_logico = bloques.len() as u32;
        if self.sb.tiene_csum_metadatos() {
            let s = self.semilla_de(dir)?;
            csum::sellar_bloque_dir(s, &mut b);
        }
        let tam_dir = self.inodo(dir)?.tamano;
        self.escribir(dir, tam_dir, &b)?;

        // (2) El índice padre: entrada nueva justo detrás de la elegida.
        let pos = padre.base + 8 * (padre.idx + 1);
        let fin = padre.base + 8 * cnt_p;
        pblk.copy_within(pos..fin, pos + 8);
        pon32(&mut pblk, pos, corte);
        pon32(&mut pblk, pos + 4, nuevo_logico);
        pon16(&mut pblk, padre.base + 2, (cnt_p + 1) as u16);
        if self.sb.tiene_csum_metadatos() {
            let s = self.semilla_de(dir)?;
            csum::sellar_bloque_dx(s, &mut pblk, padre.base);
        }
        self.escribir_meta(bloques[padre.logico], &pblk)?;

        // (3) La hoja vieja, con la mitad baja.
        self.guardar_bloque_dir(dir, bloques[hoja], &mut a)?;
        self.tocar_dir(dir, 0)?;
        Ok(true)
    }

    /// `(limit, count)` del nodo índice de un paso de la ruta.
    fn cuenta_dx(&mut self, bloques: &[u64], paso: &PasoDx) -> Result<(usize, usize), ErrorFs> {
        let b = self.leer_bloque(bloques[paso.logico])?;
        Ok((u16le(&b, paso.base) as usize, u16le(&b, paso.base + 2) as usize))
    }

    /// Un `dx_node` vacío: registro falso que abarca el bloque (los lectores lineales lo
    /// saltan) y `dx_countlimit` en el offset 8, con sitio para la cola `dx_tail` si hay sumas.
    fn nodo_dx_vacio(&self) -> Vec<u8> {
        let bs = self.bs() as usize;
        let mut n = vec![0u8; bs];
        pon16(&mut n, 4, bs.min(0xFFFF) as u16);
        let cola = if self.sb.tiene_csum_metadatos() { 8 } else { 0 };
        pon16(&mut n, 8, ((bs - 8 - cola) / 8) as u16);
        n
    }

    fn sellar_dx(&mut self, dir: u32, blk: &mut [u8], base: usize) -> Result<(), ErrorFs> {
        if self.sb.tiene_csum_metadatos() {
            let s = self.semilla_de(dir)?;
            csum::sellar_bloque_dx(s, blk, base);
        }
        Ok(())
    }

    /// Hace sitio en el índice htree para que la hoja de `nombre` se pueda partir (su nodo
    /// padre está lleno). Lo que hace el kernel (`ext4_dx_add_entry`): sube por la ruta hasta
    /// el nodo lleno más alto cuyo padre tiene sitio y lo PARTE en dos mitades (la alta, a un
    /// bloque nuevo, con su hash de corte en el padre); si el lleno es la RAÍZ, le AÑADE UN
    /// NIVEL (sus entradas bajan a un `dx_node` nuevo y la raíz queda con una sola). Sin
    /// `largedir` (que se niega a escribir) el índice tiene como mucho 2 niveles: con la raíz
    /// y su nodo llenos, el directorio está lleno → [`ErrorFs::SinEspacio`], como `ENOSPC`.
    fn hacer_sitio_en_indice(&mut self, dir: u32, bloques: &[u64], nombre: &str) -> Result<(), ErrorFs> {
        let (ruta, _, _) = self.ruta_dx(bloques, nombre.as_bytes(), None)?;
        let mut k = ruta.len() - 1;
        while k > 0 {
            let (lim, cnt) = self.cuenta_dx(bloques, &ruta[k - 1])?;
            if cnt < lim {
                break;
            }
            k -= 1;
        }
        let tam_dir = self.inodo(dir)?.tamano;
        let nuevo_logico = bloques.len() as u32;
        if k == 0 {
            // La raíz está llena: añadir un nivel.
            let mut raiz = self.leer_bloque(bloques[0])?;
            let base = ruta[0].base;
            let niveles = raiz[0x1E];
            if niveles >= 1 {
                return Err(ErrorFs::SinEspacio);
            }
            let cnt = u16le(&raiz, base + 2) as usize;
            let mut n = self.nodo_dx_vacio();
            let lim_n = u16le(&n, 8) as usize;
            if cnt > lim_n {
                return Err(ErrorFs::Corrupto { que: "raíz htree con más entradas de las que caben en un nodo" });
            }
            n[12..8 + 8 * cnt].copy_from_slice(&raiz[base + 4..base + 8 * cnt]);
            pon16(&mut n, 10, cnt as u16);
            self.sellar_dx(dir, &mut n, 8)?;
            self.escribir(dir, tam_dir, &n)?;
            pon16(&mut raiz, base + 2, 1);
            pon32(&mut raiz, base + 4, nuevo_logico);
            raiz[0x1E] = niveles + 1;
            self.sellar_dx(dir, &mut raiz, base)?;
            return self.escribir_meta(bloques[0], &raiz);
        }
        // Partir el nodo ruta[k] (lleno); su padre ruta[k-1] tiene sitio.
        let (hijo, padre) = (&ruta[k], &ruta[k - 1]);
        let mut hb = self.leer_bloque(bloques[hijo.logico])?;
        let cnt = u16le(&hb, hijo.base + 2) as usize;
        if cnt < 2 {
            return Err(ErrorFs::Corrupto { que: "nodo htree lleno con menos de 2 entradas" });
        }
        let m = cnt / 2;
        let corte = u32le(&hb, hijo.base + 8 * m);
        let mut n = self.nodo_dx_vacio();
        if cnt - m > u16le(&n, 8) as usize {
            return Err(ErrorFs::Corrupto { que: "nodo htree con más entradas de las que caben" });
        }
        // La entrada m pasa a ser la 0 del nodo nuevo: su hash lo guarda el padre.
        n[12..8 + 8 * (cnt - m)].copy_from_slice(&hb[hijo.base + 8 * m + 4..hijo.base + 8 * cnt]);
        pon16(&mut n, 10, (cnt - m) as u16);
        self.sellar_dx(dir, &mut n, 8)?;
        self.escribir(dir, tam_dir, &n)?;
        let mut pb = self.leer_bloque(bloques[padre.logico])?;
        let pcnt = u16le(&pb, padre.base + 2) as usize;
        let pos = padre.base + 8 * (padre.idx + 1);
        let fin = padre.base + 8 * pcnt;
        pb.copy_within(pos..fin, pos + 8);
        pon32(&mut pb, pos, corte);
        pon32(&mut pb, pos + 4, nuevo_logico);
        pon16(&mut pb, padre.base + 2, (pcnt + 1) as u16);
        self.sellar_dx(dir, &mut pb, padre.base)?;
        self.escribir_meta(bloques[padre.logico], &pb)?;
        pon16(&mut hb, hijo.base + 2, m as u16);
        self.sellar_dx(dir, &mut hb, hijo.base)?;
        self.escribir_meta(bloques[hijo.logico], &hb)
    }

    /// Inserta la entrada `nombre` → `ino` en el directorio `dir`, y la refleja en la caché de
    /// directorios (sin releer el directorio: es lo que quita el coste cuadrático de crear).
    fn insertar_entrada(&mut self, dir: u32, nombre: &str, ino: u32, modo: u16) -> Result<(), ErrorFs> {
        validar_nombre(nombre)?;
        if self.buscar_en(dir, nombre)?.is_some() {
            return Err(ErrorFs::YaExiste { nombre: nombre.to_string() });
        }
        let tipo = if self.sb.incompat & INCOMPAT_FILETYPE != 0 { tipo_de_modo(modo) } else { 0 };
        match self.insertar_entrada_en_disco(dir, nombre, ino, modo) {
            Ok(()) => {
                if let Some(v) = self.cache_dirs.get_mut(&dir) {
                    v.push(crate::sistema::Entrada { ino, nombre: nombre.to_string(), tipo });
                }
                Ok(())
            }
            Err(e) => {
                self.cache_dirs.clear(); // a medias: nada de la caché es de fiar
                Err(e)
            }
        }
    }

    fn insertar_entrada_en_disco(&mut self, dir: u32, nombre: &str, ino: u32, modo: u16) -> Result<(), ErrorFs> {
        let d = self.inodo(dir)?;
        let bloques = self.bloques_dir(&d)?;
        let tipo = if self.sb.incompat & INCOMPAT_FILETYPE != 0 { tipo_de_modo(modo) } else { 0 };
        let bs = self.bs() as usize;
        let limite = bs - self.hueco_cola();
        let indexado = d.flags & FL_INDICE != 0;
        let candidatos: Vec<usize> = if indexado {
            vec![self.hoja_por_hash(&bloques, nombre.as_bytes())?]
        } else {
            // Lineal: primero el ÚLTIMO bloque (donde casi siempre hay sitio), luego el resto (los
            // huecos de entradas borradas). Empezar por el primero leía el directorio entero en
            // cada inserción: crear N archivos costaba N² (medido el 2026-11-05).
            let n = bloques.len();
            std::iter::once(n.saturating_sub(1)).chain(0..n.saturating_sub(1)).collect()
        };
        for l in candidatos {
            let mut blk = self.leer_bloque(bloques[l])?;
            if self.sb.tiene_csum_metadatos() && !csum::dir_tiene_cola(&blk) {
                return Err(ErrorFs::NoSoportado { que: "bloque de directorio sin cola de suma" });
            }
            if meter_en_bloque(&mut blk, limite, nombre.as_bytes(), ino, tipo)? {
                self.guardar_bloque_dir(dir, bloques[l], &mut blk)?;
                return self.tocar_dir(dir, 0);
            }
        }
        if indexado {
            // Partir la hoja; si su nodo índice está lleno, hacer sitio y reintentar. Cada vuelta
            // o termina o hace crecer el índice (un nivel o un nodo); 4 vueltas sobran (2 niveles).
            let mut bloques = bloques;
            for _ in 0..4 {
                if self.partir_hoja(dir, &bloques, nombre, ino, tipo)? {
                    return Ok(());
                }
                self.hacer_sitio_en_indice(dir, &bloques, nombre)?;
                let d = self.inodo(dir)?;
                bloques = self.bloques_dir(&d)?;
            }
            return Err(ErrorFs::Corrupto { que: "el índice htree no hace sitio tras crecer" });
        }
        // Directorio lineal lleno: un bloque más, con la entrada ocupándolo entero.
        let mut blk = vec![0u8; bs];
        pon32(&mut blk, 0, ino);
        pon16(&mut blk, 4, limite as u16);
        blk[6] = nombre.len() as u8;
        blk[7] = tipo;
        blk[8..8 + nombre.len()].copy_from_slice(nombre.as_bytes());
        if self.sb.tiene_csum_metadatos() {
            let s = self.semilla_de(dir)?;
            csum::sellar_bloque_dir(s, &mut blk);
        }
        let tam = d.tamano;
        self.escribir(dir, tam, &blk)
    }

    /// Quita la entrada `nombre` del directorio `dir`; devuelve el inodo al que apuntaba.
    fn quitar_entrada(&mut self, dir: u32, nombre: &str) -> Result<u32, ErrorFs> {
        match self.quitar_entrada_en_disco(dir, nombre) {
            Ok(ino) => {
                if let Some(v) = self.cache_dirs.get_mut(&dir) {
                    v.retain(|e| e.nombre != nombre);
                }
                Ok(ino)
            }
            Err(e) => {
                self.cache_dirs.clear();
                Err(e)
            }
        }
    }

    fn quitar_entrada_en_disco(&mut self, dir: u32, nombre: &str) -> Result<u32, ErrorFs> {
        let d = self.inodo(dir)?;
        let bloques = self.bloques_dir(&d)?;
        let limite = self.bs() as usize - self.hueco_cola();
        for b in bloques {
            let mut blk = self.leer_bloque(b)?;
            if let Some(ino) = quitar_de_bloque(&mut blk, limite, nombre.as_bytes()) {
                self.guardar_bloque_dir(dir, b, &mut blk)?;
                self.tocar_dir(dir, 0)?;
                return Ok(ino);
            }
        }
        Err(ErrorFs::NoEncontrado { nombre: nombre.to_string() })
    }

    /// Actualiza mtime/ctime del directorio y suma `delta` a sus enlaces (subdirectorios).
    fn tocar_dir(&mut self, dir: u32, delta: i32) -> Result<(), ErrorFs> {
        let mut crudo = self.inodo_crudo(dir)?;
        if delta != 0 {
            let n = u16le(&crudo, I_ENLACES);
            // `dir_nlink`: 1 significa «más de los que caben»; ahí no se cuenta.
            let nuevo = if n == 1 {
                1
            } else {
                let v = n as i32 + delta;
                if v >= MAX_ENLACES_DIR as i32 { 1 } else { v.max(2) as u16 }
            };
            pon16(&mut crudo, I_ENLACES, nuevo);
        }
        Self::tocar_mtime(&mut crudo);
        self.guardar_inodo(dir, &mut crudo)
    }

    fn dueno(&mut self, dir: u32) -> Result<(u32, u32), ErrorFs> {
        let c = self.inodo_crudo(dir)?;
        let uid = u16le(&c, I_UID) as u32 | ((u16le(&c, 0x78) as u32) << 16);
        let gid = u16le(&c, I_GID) as u32 | ((u16le(&c, 0x7A) as u32) << 16);
        Ok((uid, gid))
    }

    /// Crea un archivo regular vacío `nombre` en `dir` con permisos `permisos` (p. ej. 0o644).
    /// El dueño se hereda del directorio padre. Devuelve el inodo nuevo.
    ///
    /// # Errors
    /// [`ErrorFs::YaExiste`], [`ErrorFs::NombreInvalido`], sin espacio o E/S.
    pub fn crear_archivo(&mut self, dir: u32, nombre: &str, permisos: u16) -> Result<u32, ErrorFs> {
        self.en_tx(|fs| fs.crear_archivo_con(dir, nombre, permisos))
    }

    fn crear_archivo_con(&mut self, dir: u32, nombre: &str, permisos: u16) -> Result<u32, ErrorFs> {
        validar_nombre(nombre)?;
        let (uid, gid) = self.dueno(dir)?;
        let modo = 0x8000 | (permisos & 0o7777);
        let ino = self.crear_inodo(modo, uid, gid, 1, dir)?;
        if let Err(e) = self.insertar_entrada(dir, nombre, ino, modo) {
            // No quedó nombrado: se devuelve el inodo para no dejar un huérfano.
            self.borrar_inodo(ino)?;
            return Err(e);
        }
        Ok(ino)
    }

    /// Crea el enlace SIMBÓLICO `nombre` en `dir` que apunta a `destino` (texto libre, como
    /// `ln -s`: puede no existir). Como el kernel: si el destino cabe en los 60 bytes de
    /// `i_block` es un enlace RÁPIDO (en línea, sin bloques ni marca de extents); si no, LENTO
    /// (un bloque de datos con el destino). Permisos 0777 (los de un symlink no se usan).
    ///
    /// # Errors
    /// [`ErrorFs::NombreInvalido`] (también si el destino está vacío, tiene NUL o no cabe en
    /// un bloque), [`ErrorFs::YaExiste`], sin espacio o E/S.
    pub fn crear_enlace_simbolico(&mut self, dir: u32, nombre: &str, destino: &str) -> Result<u32, ErrorFs> {
        self.en_tx(|fs| fs.crear_enlace_simbolico_con(dir, nombre, destino))
    }

    fn crear_enlace_simbolico_con(&mut self, dir: u32, nombre: &str, destino: &str) -> Result<u32, ErrorFs> {
        use crate::escritura::{FL_EXTENTS, I_BLOQUE, I_FLAGS, I_TAM_HI, I_TAM_LO};
        validar_nombre(nombre)?;
        let d = destino.as_bytes();
        if d.is_empty() || d.contains(&0) || d.len() >= self.bs() as usize {
            return Err(ErrorFs::NombreInvalido);
        }
        let (uid, gid) = self.dueno(dir)?;
        let modo = 0xA000 | 0o777;
        let ino = self.crear_inodo(modo, uid, gid, 1, dir)?;
        if d.len() < 60 {
            // Rápido: el destino en i_block, sin extents (lo que hace ext4_symlink).
            let mut c = self.inodo_crudo(ino)?;
            let fl = u32le(&c, I_FLAGS) & !FL_EXTENTS;
            pon32(&mut c, I_FLAGS, fl);
            c[I_BLOQUE..I_BLOQUE + 60].fill(0);
            c[I_BLOQUE..I_BLOQUE + d.len()].copy_from_slice(d);
            pon32(&mut c, I_TAM_LO, d.len() as u32);
            pon32(&mut c, I_TAM_HI, 0);
            self.guardar_inodo(ino, &mut c)?;
        } else {
            // Lento: un bloque de datos (por el camino interno: la guarda pública lo prohíbe).
            let mut nuevos = Vec::new();
            self.escribir_con(ino, 0, d, &mut nuevos)?;
        }
        if let Err(e) = self.insertar_entrada(dir, nombre, ino, modo) {
            self.borrar_inodo(ino)?;
            return Err(e);
        }
        Ok(ino)
    }

    /// Crea el enlace DURO `nombre` en `dir` hacia el inodo `ino` (otro nombre del mismo
    /// archivo; sube su recuento de enlaces). Como el kernel, se niega para directorios y por
    /// encima de 65 000 enlaces (`EXT4_LINK_MAX`).
    ///
    /// # Errors
    /// [`ErrorFs::EsDirectorio`], [`ErrorFs::YaExiste`], [`ErrorFs::NombreInvalido`],
    /// [`ErrorFs::NoSoportado`] (demasiados enlaces), E/S.
    pub fn crear_enlace_duro(&mut self, dir: u32, nombre: &str, ino: u32) -> Result<(), ErrorFs> {
        self.en_tx(|fs| fs.crear_enlace_duro_con(dir, nombre, ino))
    }

    fn crear_enlace_duro_con(&mut self, dir: u32, nombre: &str, ino: u32) -> Result<(), ErrorFs> {
        validar_nombre(nombre)?;
        let mut c = self.inodo_crudo(ino)?;
        let modo = u16le(&c, I_MODO);
        if modo & 0xF000 == 0x4000 {
            return Err(ErrorFs::EsDirectorio);
        }
        let enlaces = u16le(&c, I_ENLACES);
        if enlaces == 0 {
            return Err(ErrorFs::Corrupto { que: "enlazar un inodo sin enlaces (libre o huérfano)" });
        }
        if enlaces >= 65000 {
            return Err(ErrorFs::NoSoportado { que: "el archivo ya tiene el máximo de enlaces duros (65000)" });
        }
        // Primero el RECUENTO, luego el nombre (como ext4_link). Dentro de la transacción el
        // orden no se ve; sin journal, un corte deja como mucho un recuento ALTO (e2fsck lo
        // baja; el inodo, a lo sumo, tarda en liberarse). Al revés sería peligroso: con un
        // recuento BAJO, borrar un nombre liberaría el inodo que el otro nombre aún usa.
        pon16(&mut c, I_ENLACES, enlaces + 1);
        Self::tocar_ctime(&mut c);
        self.guardar_inodo(ino, &mut c)?;
        if let Err(e) = self.insertar_entrada(dir, nombre, ino, modo) {
            let mut c = self.inodo_crudo(ino)?;
            pon16(&mut c, I_ENLACES, enlaces);
            self.guardar_inodo(ino, &mut c)?;
            return Err(e);
        }
        Ok(())
    }

    /// Crea el directorio `nombre` en `dir` (con `.` y `..`). Devuelve su inodo.
    ///
    /// # Errors
    /// Como [`Self::crear_archivo`].
    pub fn crear_directorio(&mut self, dir: u32, nombre: &str, permisos: u16) -> Result<u32, ErrorFs> {
        self.en_tx(|fs| fs.crear_directorio_con(dir, nombre, permisos))
    }

    fn crear_directorio_con(&mut self, dir: u32, nombre: &str, permisos: u16) -> Result<u32, ErrorFs> {
        validar_nombre(nombre)?;
        let padre = self.inodo(dir)?;
        if self.listar(&padre)?.iter().any(|e| e.nombre == nombre) {
            return Err(ErrorFs::YaExiste { nombre: nombre.to_string() });
        }
        let (uid, gid) = self.dueno(dir)?;
        let modo = 0x4000 | (permisos & 0o7777);
        let ino = self.crear_inodo(modo, uid, gid, 2, dir)?;
        let bs = self.bs() as usize;
        let limite = bs - self.hueco_cola();
        let tipo = if self.sb.incompat & INCOMPAT_FILETYPE != 0 { 2 } else { 0 };
        let mut blk = vec![0u8; bs];
        pon32(&mut blk, 0, ino);
        pon16(&mut blk, 4, 12);
        blk[6] = 1;
        blk[7] = tipo;
        blk[8] = b'.';
        pon32(&mut blk, 12, dir);
        pon16(&mut blk, 16, (limite - 12) as u16);
        blk[18] = 2;
        blk[19] = tipo;
        blk[20] = b'.';
        blk[21] = b'.';
        if self.sb.tiene_csum_metadatos() {
            let s = self.semilla_de(ino)?;
            csum::sellar_bloque_dir(s, &mut blk);
        }
        self.escribir(ino, 0, &blk)?;
        if let Err(e) = self.insertar_entrada(dir, nombre, ino, modo) {
            self.borrar_inodo(ino)?;
            return Err(e);
        }
        self.tocar_dir(dir, 1)?;
        Ok(ino)
    }

    /// Borra el archivo (no directorio) `nombre` de `dir`: quita la entrada y, si era el
    /// último enlace, libera el inodo y sus bloques.
    ///
    /// # Errors
    /// [`ErrorFs::NoEncontrado`], [`ErrorFs::EsDirectorio`] o E/S.
    pub fn borrar(&mut self, dir: u32, nombre: &str) -> Result<(), ErrorFs> {
        self.en_tx(|fs| fs.borrar_con(dir, nombre))
    }

    fn borrar_con(&mut self, dir: u32, nombre: &str) -> Result<(), ErrorFs> {
        let ino = self.numero_en(dir, nombre)?;
        if self.inodo(ino)?.es_directorio() {
            return Err(ErrorFs::EsDirectorio);
        }
        let n = u16le(&self.inodo_crudo(ino)?, I_ENLACES);
        if n <= 1 {
            // Si luego no se pudiera liberar, decidirlo AHORA, antes de tocar el directorio.
            self.comprobar_borrable(ino)?;
        }
        self.quitar_entrada(dir, nombre)?;
        let mut crudo = self.inodo_crudo(ino)?;
        if n <= 1 {
            self.borrar_inodo(ino)
        } else {
            pon16(&mut crudo, I_ENLACES, n - 1);
            Self::tocar_ctime(&mut crudo);
            self.guardar_inodo(ino, &mut crudo)
        }
    }

    /// Borra el directorio VACÍO `nombre` de `dir`.
    ///
    /// # Errors
    /// [`ErrorFs::DirectorioNoVacio`], [`ErrorFs::NoEsDirectorio`], [`ErrorFs::NoEncontrado`] o E/S.
    pub fn borrar_directorio(&mut self, dir: u32, nombre: &str) -> Result<(), ErrorFs> {
        self.en_tx(|fs| fs.borrar_directorio_con(dir, nombre))
    }

    fn borrar_directorio_con(&mut self, dir: u32, nombre: &str) -> Result<(), ErrorFs> {
        validar_nombre(nombre)?;
        let ino = self.numero_en(dir, nombre)?;
        let hijo = self.inodo(ino)?;
        if !hijo.es_directorio() {
            return Err(ErrorFs::NoEsDirectorio);
        }
        if self.listar(&hijo)?.iter().any(|e| e.nombre != "." && e.nombre != "..") {
            return Err(ErrorFs::DirectorioNoVacio);
        }
        self.cache_dirs.remove(&ino); // deja de existir
        self.comprobar_borrable(ino)?;
        self.quitar_entrada(dir, nombre)?;
        self.borrar_inodo(ino)?;
        self.tocar_dir(dir, -1)
    }

    /// Renombra/mueve `nombre_a` de `dir_a` a `nombre_b` en `dir_b`. Si el destino existe, se
    /// reemplaza (un archivo por un archivo; un directorio solo por un directorio vacío).
    ///
    /// # Errors
    /// [`ErrorFs::NoEncontrado`], [`ErrorFs::DirectorioNoVacio`], [`ErrorFs::EsDirectorio`],
    /// [`ErrorFs::NoEsDirectorio`] o E/S.
    pub fn renombrar(&mut self, dir_a: u32, nombre_a: &str, dir_b: u32, nombre_b: &str) -> Result<(), ErrorFs> {
        self.en_tx(|fs| fs.renombrar_con(dir_a, nombre_a, dir_b, nombre_b))
    }

    fn renombrar_con(&mut self, dir_a: u32, nombre_a: &str, dir_b: u32, nombre_b: &str) -> Result<(), ErrorFs> {
        validar_nombre(nombre_a)?;
        validar_nombre(nombre_b)?;
        let ino = self.numero_en(dir_a, nombre_a)?;
        if dir_a == dir_b && nombre_a == nombre_b {
            return Ok(());
        }
        let es_dir = self.inodo(ino)?.es_directorio();
        if es_dir && self.es_ancestro(ino, dir_b)? {
            return Err(ErrorFs::NoSoportado { que: "mover un directorio dentro de sí mismo" });
        }
        if let Ok(destino) = self.numero_en(dir_b, nombre_b) {
            if destino == ino {
                return self.quitar_entrada(dir_a, nombre_a).map(|_| ());
            }
            let dest_es_dir = self.inodo(destino)?.es_directorio();
            match (es_dir, dest_es_dir) {
                (true, true) => self.borrar_directorio(dir_b, nombre_b)?,
                (false, false) => self.borrar(dir_b, nombre_b)?,
                (true, false) => return Err(ErrorFs::NoEsDirectorio),
                (false, true) => return Err(ErrorFs::EsDirectorio),
            }
        }
        let modo = u16le(&self.inodo_crudo(ino)?, I_MODO);
        self.insertar_entrada(dir_b, nombre_b, ino, modo)?;
        self.quitar_entrada(dir_a, nombre_a)?;
        if es_dir && dir_a != dir_b {
            self.cambiar_padre(ino, dir_b)?;
            self.tocar_dir(dir_a, -1)?;
            self.tocar_dir(dir_b, 1)?;
        }
        let mut crudo = self.inodo_crudo(ino)?;
        Self::tocar_ctime(&mut crudo);
        self.guardar_inodo(ino, &mut crudo)
    }

    /// Reescribe el `..` del directorio `ino` para que apunte a `padre`.
    fn cambiar_padre(&mut self, ino: u32, padre: u32) -> Result<(), ErrorFs> {
        self.cache_dirs.remove(&ino); // su «..» cambia
        let d = self.inodo(ino)?;
        let b0 = self.bloques_dir(&d)?[0];
        let mut blk = self.leer_bloque(b0)?;
        let rec_punto = u16le(&blk, 4) as usize;
        if &blk[rec_punto + 8..rec_punto + 10] != b".." {
            return Err(ErrorFs::NoSoportado { que: "directorio sin «..» en su segunda entrada" });
        }
        pon32(&mut blk, rec_punto, padre);
        if d.flags & FL_INDICE != 0 {
            // Bloque raíz de un htree: su suma va en la cola dx, no en una cola de hoja.
            return Err(ErrorFs::NoSoportado { que: "mover un directorio indexado a otro padre" });
        }
        self.guardar_bloque_dir(ino, b0, &mut blk)
    }

    /// ¿Es `ancestro` el propio `dir` o uno de sus padres? (Evita meter un directorio en sí.)
    fn es_ancestro(&mut self, ancestro: u32, mut dir: u32) -> Result<bool, ErrorFs> {
        for _ in 0..4096 {
            if dir == ancestro {
                return Ok(true);
            }
            if dir == crate::sistema::INODO_RAIZ {
                return Ok(false);
            }
            dir = self.numero_en(dir, "..")?;
        }
        Err(ErrorFs::NoSoportado { que: "cadena de «..» demasiado larga (¿bucle?)" })
    }

    /// Número de inodo de `nombre` dentro de `dir`.
    fn numero_en(&mut self, dir: u32, nombre: &str) -> Result<u32, ErrorFs> {
        self.buscar_en(dir, nombre)?
            .map(|e| e.ino)
            .ok_or_else(|| ErrorFs::NoEncontrado { nombre: nombre.to_string() })
    }

    fn tocar_ctime(crudo: &mut [u8]) {
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as u32);
        pon32(crudo, I_CTIME, t);
    }
}

#[cfg(test)]
mod pruebas {
    use super::*;

    #[test]
    fn meter_parte_la_ultima_entrada_y_quitar_la_funde() {
        let mut blk = vec![0u8; 64];
        // Una sola entrada «a» (ino 5) que ocupa todo el bloque útil (64).
        pon32(&mut blk, 0, 5);
        pon16(&mut blk, 4, 64);
        blk[6] = 1;
        blk[8] = b'a';
        assert!(meter_en_bloque(&mut blk, 64, b"nuevo", 9, 1).unwrap());
        assert_eq!(u16le(&blk, 4), 12, "«a» se encoge a su tamaño real");
        assert_eq!(u32le(&blk, 12), 9);
        assert_eq!(u16le(&blk, 16), 52, "la nueva hereda el resto");
        assert_eq!(quitar_de_bloque(&mut blk, 64, b"nuevo"), Some(9));
        assert_eq!(u16le(&blk, 4), 64, "al quitarla, «a» recupera el bloque");
        // Pareja (D33): lo que no está no se quita.
        assert_eq!(quitar_de_bloque(&mut blk, 64, b"nuevo"), None);
    }

    #[test]
    fn no_cabe_si_no_hay_sitio() {
        let mut blk = vec![0u8; 16];
        pon32(&mut blk, 0, 5);
        pon16(&mut blk, 4, 16);
        blk[6] = 1;
        blk[8] = b'a';
        assert!(!meter_en_bloque(&mut blk, 16, b"nombre-largo", 9, 1).unwrap());
    }

    #[test]
    fn nombres_invalidos() {
        for n in ["", ".", "..", "a/b", "a\0b"] {
            assert!(matches!(validar_nombre(n), Err(ErrorFs::NombreInvalido)), "{n:?}");
        }
        assert!(validar_nombre(&"x".repeat(256)).is_err());
        assert!(validar_nombre("normal.txt").is_ok());
    }
}
