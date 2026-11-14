// SPDX-FileCopyrightText: 2026 Juan Carlos Isaza
// SPDX-License-Identifier: LicenseRef-Xiliux-Propietario
//! A2 — **escribir A TRAVÉS del journal** (jbd2, modo ordenado).
//!
//! Cada operación pública (crear, escribir, truncar, borrar, renombrar…) es UNA transacción:
//! los bloques de metadatos que toca se acumulan en memoria (`Tx`) y las lecturas los ven; los
//! datos de archivo van directo al disco. Al confirmar (DOS barreras, no cinco):
//!
//! 1. descriptor(es) + copias, A CONTINUACIÓN de las transacciones anteriores en el registro
//!    (`s_start` solo se escribe si el registro estaba vacío); BARRERA: datos (modo ordenado) +
//!    copias en el disco;
//! 2. bloque de COMMIT; BARRERA — desde aquí la transacción EXISTE: si se corta la luz, Linux,
//!    e2fsck o `winext4util recuperar` la completan;
//! 3. checkpoint: cada bloque a su sitio, SIN barrera (si no llega, la recuperación lo reescribe).
//!
//! El registro se VACÍA (barrera + `s_start` = 0) al llenarse, al desmontar y antes de escribir
//! DATOS en un bloque que tenga copia en él (si no, una recuperación pondría encima la copia
//! vieja: lo que jbd2 resuelve con registros de revocación). Un corte antes del commit = la
//! operación no ocurrió; después = ocurrió entera. Un ERROR a mitad de una operación descarta la
//! transacción entera: nada de lo que tocó llega al disco.
//!//! Validado cortando la luz SIMULADA en cada una de las escrituras de una secuencia de
//! operaciones (`tests/cortes.rs`).

use std::collections::BTreeMap;
use std::io::{Read, Seek, Write};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::csum::crc32c;
use crate::journal::{pon_be32, suma_commit, suma_con_cola, J_COMMIT, J_DESCRIPTOR, J_MAGIA, TAG_ESCAPE, TAG_MISMO_UUID, TAG_ULTIMO};
use crate::sistema::{ErrorFs, Ext4};

/// Una transacción abierta.
#[derive(Debug, Default)]
pub(crate) struct Tx {
    /// Bloque del FS → su contenido nuevo (pendiente).
    pub(crate) bloques: BTreeMap<u64, Vec<u8>>,
    /// Anidamiento: las operaciones públicas se llaman entre sí; se confirma al salir de la
    /// más externa.
    pub(crate) profundidad: u32,
    /// Bloques LIBERADOS en esta transacción: no se reutilizan hasta confirmarla (ver
    /// `asignar_bloques`).
    pub(crate) liberados: std::collections::HashSet<u64>,
    /// Desde cuándo está abierta (agrupado: se confirma al pasar `max_espera`).
    pub(crate) abierta_desde: Option<std::time::Instant>,
    /// Registro para DESHACER la operación en curso: el valor anterior (o su ausencia) de cada
    /// bloque pendiente que ha tocado, la primera vez que lo toca. Coste proporcional a lo que
    /// hace la operación, no al tamaño del grupo (copiar el grupo entero costaba ms por operación).
    pub(crate) deshacer: BTreeMap<u64, Option<Vec<u8>>>,
}

impl Tx {
    /// Antes de modificar `bloque` en la transacción: guarda su valor anterior (una vez por op).
    pub(crate) fn anotar_deshacer(&mut self, bloque: u64) {
        if !self.deshacer.contains_key(&bloque) {
            let antes = self.bloques.get(&bloque).cloned();
            self.deshacer.insert(bloque, antes);
        }
    }
}

/// Agrupado de transacciones (como el commit periódico de jbd2): varias operaciones comparten
/// UNA transacción, que se confirma al pasar `max_espera`, al juntar `max_bloques`, cuando se
/// pide (`confirmar_pendiente`: FlushFileBuffers) y al cerrar. Tras un corte se pierden como
/// mucho las operaciones del grupo abierto; el volumen queda SIEMPRE consistente.
#[derive(Debug, Clone, Copy)]
pub struct Agrupado {
    pub max_espera: std::time::Duration,
    pub max_bloques: usize,
}

/// Lo que hay que restaurar si una operación falla dentro de un grupo: la transacción y el
/// superbloque en memoria TAL COMO ESTABAN antes de ella (no los del disco, que aún no tiene
/// las operaciones anteriores del grupo).
struct PuntoGuardado {
    sb_crudo: Vec<u8>,
    sb: crate::superbloque::Superbloque,
    habia_tx: bool,
}

impl<R: Read + Write + Seek> Ext4<R> {
    /// ¿Se está escribiendo a través del journal?
    #[must_use]
    pub fn con_journal(&self) -> bool {
        self.diario.is_some()
    }

    /// Bloques de metadatos del grupo abierto, aún sin confirmar (para medir y probar).
    #[must_use]
    pub fn bloques_pendientes(&self) -> usize {
        self.tx.as_ref().map_or(0, |t| t.bloques.len())
    }

    /// Ejecuta `f` como UNA transacción (o como parte de la que ya está abierta). Sin journal,
    /// `f` corre tal cual (escritura directa con orden anti-corrupción).
    pub(crate) fn en_tx<T>(&mut self, f: impl FnOnce(&mut Self) -> Result<T, ErrorFs>) -> Result<T, ErrorFs> {
        if self.diario.is_none() {
            let r = f(self);
            if r.is_err() {
                self.cache_dirs.clear(); // la operación quedó a medias: la caché no es de fiar
            }
            return r;
        }
        // Operación EXTERNA: punto de guardado (con agrupado, la transacción ya puede traer
        // operaciones anteriores que no deben perderse si esta falla).
        let externa = self.tx.as_ref().is_none_or(|t| t.profundidad == 0);
        let punto = externa.then(|| PuntoGuardado { sb_crudo: self.sb_crudo.clone(), sb: self.sb.clone(), habia_tx: self.tx.is_some() });
        let t = self.tx.get_or_insert_with(Tx::default);
        if externa {
            t.deshacer.clear();
        }
        t.abierta_desde.get_or_insert_with(std::time::Instant::now);
        t.profundidad += 1;
        let r = f(self);
        let t = self.tx.as_mut().expect("la transacción sigue abierta");
        t.profundidad -= 1;
        if t.profundidad > 0 {
            return r; // la confirma (o la descarta) la operación de fuera
        }
        match r {
            Ok(v) => {
                let toca = match self.agrupado {
                    None => true,
                    Some(a) => {
                        let t = self.tx.as_ref().expect("tx");
                        let cabe = self.diario.as_ref().map_or(usize::MAX, |j| (j.largo.saturating_sub(j.primero) / 4) as usize);
                        t.bloques.len() >= a.max_bloques.min(cabe) || t.abierta_desde.is_some_and(|d| d.elapsed() >= a.max_espera)
                    }
                };
                if toca {
                    self.confirmar_pendiente()?;
                }
                Ok(v)
            }
            Err(e) => {
                // La caché de directorios puede reflejar lo que se va a descartar.
                self.cache_dirs.clear();
                // Se deshace SOLO esta operación: la transacción y el superbloque en memoria
                // vuelven a como estaban antes de ella.
                let p = punto.expect("la operación externa guarda su punto");
                self.sb_crudo = p.sb_crudo;
                self.sb = p.sb;
                if p.habia_tx {
                    let t = self.tx.as_mut().expect("tx");
                    for (b, antes) in std::mem::take(&mut t.deshacer) {
                        match antes {
                            Some(v) => {
                                t.bloques.insert(b, v);
                            }
                            None => {
                                t.bloques.remove(&b);
                            }
                        }
                    }
                } else {
                    self.tx = None;
                }
                Err(e)
            }
        }
    }

    /// Activa (o, con `None`, desactiva) el agrupado de transacciones. Al desactivarlo se
    /// confirma lo pendiente.
    ///
    /// # Errors
    /// E/S al confirmar.
    pub fn agrupar_transacciones(&mut self, a: Option<Agrupado>) -> Result<(), ErrorFs> {
        self.agrupado = a;
        if a.is_none() {
            self.confirmar_pendiente()?;
        }
        Ok(())
    }

    /// Confirma la transacción abierta (si la hay y no está a mitad de una operación). Es lo
    /// que hace durable lo agrupado: `FlushFileBuffers`, el temporizador del host y el cierre.
    ///
    /// # Errors
    /// E/S.
    pub fn confirmar_pendiente(&mut self) -> Result<(), ErrorFs> {
        if !self.tx.as_ref().is_some_and(|t| t.profundidad == 0) {
            return Ok(());
        }
        let t = self.tx.take().expect("transacción");
        if let Err(e) = self.confirmar(t.bloques) {
            self.cache_dirs.clear();
            return Err(e);
        }
        Ok(())
    }

    /// ¿Hay un grupo abierto que ya debería confirmarse por tiempo? (Para el temporizador.)
    #[must_use]
    pub fn grupo_vencido(&self) -> bool {
        match (self.agrupado, self.tx.as_ref()) {
            (Some(a), Some(t)) => t.profundidad == 0 && t.abierta_desde.is_some_and(|d| d.elapsed() >= a.max_espera),
            _ => false,
        }
    }

    /// Escribe la transacción en el journal A CONTINUACIÓN de las anteriores, la confirma y la
    /// lleva a su sitio. El journal NO se vacía tras cada una (eso costaba dos barreras más por
    /// operación): se vacía al llenarse, al desmontar y antes de reutilizar para datos un bloque
    /// que tenga copia en él (`vaciar_journal`).
    fn confirmar(&mut self, bloques: BTreeMap<u64, Vec<u8>>) -> Result<(), ErrorFs> {
        if bloques.is_empty() {
            return Ok(());
        }
        let bs = self.bs() as usize;
        let mut j = self.diario.take().expect("diario");
        let r = self.confirmar_con(&mut j, &bloques, bs);
        self.diario = Some(j);
        r
    }

    fn confirmar_con(&mut self, j: &mut crate::journal::Journal, bloques: &BTreeMap<u64, Vec<u8>>, bs: usize) -> Result<(), ErrorFs> {
        let tt = j.tam_tag();
        let util = bs - 12 - if j.csum() { 4 } else { 0 };
        // Cuántas etiquetas caben por descriptor: `n` etiquetas ocupan `16 + n·tt` bytes de los
        // `util` (la primera lleva además el UUID, 16 bytes). Era `(util - 16) / tt + 1`: una de
        // más, que con 4 KiB y sumas v3 se escribía en los bytes 4096..4100 y el motor entraba en
        // PÁNICO en cuanto una transacción pasaba de 254 bloques (2026-11-06, Z: real, un
        // `git worktree add` de 2 700 archivos). Lo vigila tests/transaccion_grande.rs.
        let por_desc = (util - 16) / tt;
        let lista: Vec<(&u64, &Vec<u8>)> = bloques.iter().collect();
        let n_desc = lista.len().div_ceil(por_desc);
        let necesarios = (lista.len() + n_desc + 1) as u32;
        if necesarios > j.largo.saturating_sub(j.primero) {
            return Err(ErrorFs::NoSoportado { que: "transacción más grande que el journal" });
        }
        if !j.es64() && lista.iter().any(|(b, _)| **b > u64::from(u32::MAX)) {
            return Err(ErrorFs::NoSoportado { que: "bloque más allá de 32 bits con journal de 32 bits" });
        }
        // ¿No cabe detrás de las anteriores? Vaciar y empezar desde el principio del registro
        // (sin vuelta circular: más simple y la recuperación no tiene que unir dos tramos).
        if j.pos + necesarios > j.largo {
            self.vaciar_con(j)?;
        }
        let seq = j.secuencia;
        let vacio = !j.pendiente;

        // (1) Descriptores + copias detrás de lo que ya hay. Los DATOS de la operación ya se
        // escribieron (modo ordenado): la misma barrera de abajo los hace durables a los dos.
        let mut pos = j.pos;
        for trozo in lista.chunks(por_desc) {
            let mut desc = vec![0u8; bs];
            pon_be32(&mut desc, 0, J_MAGIA);
            pon_be32(&mut desc, 4, J_DESCRIPTOR);
            pon_be32(&mut desc, 8, seq);
            let mut o = 12usize;
            let mut copias = Vec::with_capacity(trozo.len());
            for (k, (&b, datos)) in trozo.iter().enumerate() {
                let mut copia = (*datos).clone();
                let mut banderas = 0u32;
                if u32::from_be_bytes(copia[0..4].try_into().expect("4")) == J_MAGIA {
                    copia[0..4].fill(0); // escapado: si no, el escaneo lo tomaría por cabecera
                    banderas |= TAG_ESCAPE;
                }
                if k > 0 {
                    banderas |= TAG_MISMO_UUID;
                }
                if k + 1 == trozo.len() {
                    banderas |= TAG_ULTIMO;
                }
                // La suma va sobre la copia TAL CUAL queda en el journal (escapada).
                let suma = crc32c(crc32c(j.semilla, &seq.to_be_bytes()), &copia);
                if j.v3() {
                    pon_be32(&mut desc, o, b as u32);
                    pon_be32(&mut desc, o + 4, banderas);
                    pon_be32(&mut desc, o + 8, (b >> 32) as u32);
                    pon_be32(&mut desc, o + 12, suma);
                } else {
                    pon_be32(&mut desc, o, b as u32);
                    let s16 = if j.csum() { (suma & 0xFFFF) as u16 } else { 0 };
                    desc[o + 4..o + 6].copy_from_slice(&s16.to_be_bytes());
                    desc[o + 6..o + 8].copy_from_slice(&(banderas as u16).to_be_bytes());
                    if j.es64() {
                        pon_be32(&mut desc, o + 8, (b >> 32) as u32);
                    }
                }
                o += tt;
                if k == 0 {
                    desc[o..o + 16].copy_from_slice(&j.sb_bloque[0x30..0x40]);
                    o += 16;
                }
                copias.push(copia);
            }
            if j.csum() {
                let c = suma_con_cola(j.semilla, &desc);
                pon_be32(&mut desc, bs - 4, c);
            }
            self.escribir_bloque(j.mapa[pos as usize], &desc)?;
            pos += 1;
            for c in copias {
                self.escribir_bloque(j.mapa[pos as usize], &c)?;
                pos += 1;
            }
        }
        // Si el registro estaba vacío, `s_start` pasa a apuntar a ESTA transacción (y `s_sequence`
        // a su número): las siguientes se reconocen por ir a continuación con número + 1.
        if vacio {
            self.escribir_sb_journal(j, j.pos, seq)?;
        }
        // BARRERA 1: datos + descriptores + copias (+ s_start) en el disco antes del commit.
        self.disp.flush()?;

        // (2) COMMIT.
        let mut com = vec![0u8; bs];
        pon_be32(&mut com, 0, J_MAGIA);
        pon_be32(&mut com, 4, J_COMMIT);
        pon_be32(&mut com, 8, seq);
        let d = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
        com[0x30..0x38].copy_from_slice(&d.as_secs().to_be_bytes());
        pon_be32(&mut com, 0x38, d.subsec_nanos());
        if j.csum() {
            let c = suma_commit(j.semilla, &com);
            pon_be32(&mut com, 16, c);
        }
        self.escribir_bloque(j.mapa[pos as usize], &com)?;
        // BARRERA 2: desde aquí la transacción EXISTE (un corte la reproduce).
        self.disp.flush()?;

        // (3) Checkpoint: cada bloque a su sitio, SIN barrera: si se corta antes de que llegue,
        // la recuperación lo reescribe desde el journal (idempotente).
        for (&b, datos) in bloques {
            self.escribir_bloque(b, datos)?;
            j.registrados.insert(b);
        }
        j.pos = pos + 1;
        j.pendiente = true;
        j.secuencia = seq.wrapping_add(1);
        Ok(())
    }

    /// Vacía el journal: los checkpoints pendientes a disco (barrera) y luego `s_start` = 0.
    /// Sin transacciones pendientes no hace nada.
    ///
    /// # Errors
    /// E/S.
    pub(crate) fn vaciar_journal(&mut self) -> Result<(), ErrorFs> {
        let Some(mut j) = self.diario.take() else { return Ok(()) };
        let r = self.vaciar_con(&mut j);
        self.diario = Some(j);
        r
    }

    fn vaciar_con(&mut self, j: &mut crate::journal::Journal) -> Result<(), ErrorFs> {
        if !j.pendiente {
            return Ok(());
        }
        self.disp.flush()?; // los checkpoints, en su sitio y durables
        self.escribir_sb_journal(j, 0, j.secuencia)?;
        self.disp.flush()?;
        j.pendiente = false;
        j.pos = j.primero;
        j.registrados.clear();
        Ok(())
    }
    fn escribir_sb_journal(&mut self, j: &crate::journal::Journal, inicio: u32, secuencia: u32) -> Result<(), ErrorFs> {
        let mut sbj = self.leer_bloque(j.mapa[0])?;
        pon_be32(&mut sbj, 0x1C, inicio);
        pon_be32(&mut sbj, 0x18, secuencia);
        if j.csum() {
            sbj[0xFC..0x100].fill(0);
            let c = crc32c(!0, &sbj[..1024]);
            pon_be32(&mut sbj, 0xFC, c);
        }
        self.escribir_bloque(j.mapa[0], &sbj)
    }
}
