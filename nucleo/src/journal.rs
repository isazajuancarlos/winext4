// SPDX-FileCopyrightText: 2026 Juan Carlos Isaza
// SPDX-License-Identifier: LicenseRef-Xiliux-Propietario
//! **Recuperación del journal (jbd2)**: lo que hace el kernel al montar un ext4 que quedó con
//! `needs_recovery` tras un corte de luz o un cuelgue — reproducir sobre el FS las
//! transacciones CONFIRMADAS que no llegaron a su sitio.
//!
//! El journal vive en un inodo (normalmente el 8) y su formato es **big-endian**. Tres pasadas:
//! 1. ESCANEO: desde `s_start` con `s_sequence`, recorrer el registro circular hasta la última
//!    transacción con bloque de COMMIT válido. Una transacción sin commit no existe: se descarta
//!    (reproducirla es resucitar datos a medias — el fallo #86 de lwext4).
//! 2. REVOCACIÓN: anotar los bloques revocados y en qué transacción.
//! 3. REPRODUCCIÓN: escribir cada bloque de datos en su bloque del FS, salvo los revocados por
//!    una transacción igual o posterior, y salvo los que no cuadren con su suma (`csum_v2/v3`).
//!
//! Al terminar: `s_start` = 0 en el journal (vacío) y se quita `needs_recovery` del superbloque
//! del FS, RELEYÉNDOLO del disco (el journal puede haber reescrito el bloque que lo contiene).
//!
//! Validado por DIFERENCIAL contra `e2fsck -E journal_only` sobre journals escritos por el
//! KERNEL de Linux (`tests/journal.rs`).

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom, Write};

use crate::csum::crc32c;
use crate::sistema::{ErrorFs, Ext4};
use crate::superbloque as s;

pub(crate) const J_MAGIA: u32 = 0xC03B_3998;
pub(crate) const J_DESCRIPTOR: u32 = 1;
pub(crate) const J_COMMIT: u32 = 2;
const J_SB_V1: u32 = 3;
const J_SB_V2: u32 = 4;
const J_REVOCACION: u32 = 5;

const JI_REVOKE: u32 = 0x1;
const JI_64BIT: u32 = 0x2;
const JI_ASYNC_COMMIT: u32 = 0x4;
const JI_CSUM_V2: u32 = 0x8;
const JI_CSUM_V3: u32 = 0x10;
const JI_FAST_COMMIT: u32 = 0x20;
/// Lo que este recuperador entiende; cualquier otra bandera incompat → no se toca.
const JI_SABIDAS: u32 = JI_REVOKE | JI_64BIT | JI_ASYNC_COMMIT | JI_CSUM_V2 | JI_CSUM_V3;

pub(crate) const TAG_ESCAPE: u32 = 0x1;
pub(crate) const TAG_MISMO_UUID: u32 = 0x2;
pub(crate) const TAG_ULTIMO: u32 = 0x8;

/// Offset de `s_journal_inum` en el superbloque del FS.
const SB_INODO_JOURNAL: usize = 0xE0;
/// Offset de `s_orphan_file_inum`.
const SB_INODO_HUERFANOS: usize = 0x280;
/// `RO_COMPAT_ORPHAN_PRESENT`.
const RO_ORPHAN_PRESENT: u32 = 0x1_0000;

fn be32(b: &[u8], o: usize) -> u32 {
    u32::from_be_bytes(b[o..o + 4].try_into().expect("4 bytes"))
}
pub(crate) fn pon_be32(b: &mut [u8], o: usize, v: u32) {
    b[o..o + 4].copy_from_slice(&v.to_be_bytes());
}

/// Qué hizo la recuperación (para el informe al usuario y para las pruebas).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InformeJournal {
    /// Transacciones confirmadas reproducidas.
    pub transacciones: u32,
    /// Bloques del FS reescritos desde el journal.
    pub bloques_reproducidos: u32,
    /// Bloques que NO se reescribieron por estar revocados.
    pub revocados_saltados: u32,
    /// Bloques de datos descartados por suma inválida (el kernel hace lo mismo y avisa).
    pub sumas_invalidas: u32,
    /// Si quedaron huérfanos pendientes (`orphan_present` / `s_last_orphan`): los procesa
    /// `e2fsck` o el kernel al montar; aquí solo se informa.
    pub huerfanos_pendientes: bool,
}

/// Geometría y sumas del journal ya leídas.
pub(crate) struct Journal {
    /// Bloque físico del FS para cada bloque lógico del journal.
    pub(crate) mapa: Vec<u64>,
    pub(crate) primero: u32,
    pub(crate) largo: u32,
    pub(crate) inicio: u32,
    pub(crate) secuencia: u32,
    pub(crate) incompat: u32,
    pub(crate) semilla: u32,
    pub(crate) sb_bloque: Vec<u8>,
    /// Escritura: siguiente bloque libre del registro. Las transacciones confirmadas se ACUMULAN
    /// (como en jbd2) en vez de vaciar el journal tras cada una; se vacía al llenarse, al
    /// desmontar, o cuando hace falta (ver `registrados`).
    pub(crate) pos: u32,
    /// Hay transacciones confirmadas en el registro cuyo checkpoint aún no se ha vaciado.
    pub(crate) pendiente: bool,
    /// Bloques del FS que tienen copia en el registro sin vaciar. Si uno se reutiliza para DATOS
    /// (que no pasan por el journal), una recuperación tras un corte pondría ENCIMA la copia
    /// vieja: antes de escribir datos en uno de ellos se vacía el journal (lo que jbd2 resuelve
    /// con registros de revocación).
    pub(crate) registrados: std::collections::HashSet<u64>,
}

impl Journal {
    pub(crate) fn csum(&self) -> bool {
        self.incompat & (JI_CSUM_V2 | JI_CSUM_V3) != 0
    }
    pub(crate) fn v3(&self) -> bool {
        self.incompat & JI_CSUM_V3 != 0
    }
    pub(crate) fn es64(&self) -> bool {
        self.incompat & JI_64BIT != 0
    }
    pub(crate) fn tam_tag(&self) -> usize {
        if self.v3() {
            16
        } else if self.es64() {
            12
        } else {
            8
        }
    }
    fn sig(&self, n: u32) -> u32 {
        if n + 1 >= self.largo { self.primero } else { n + 1 }
    }
}

/// Suma de un bloque con cola (`descriptor`/`revocación`): crc de todo con la cola a cero.
pub(crate) fn suma_con_cola(semilla: u32, blk: &[u8]) -> u32 {
    let n = blk.len();
    let c = crc32c(semilla, &blk[..n - 4]);
    crc32c(c, &[0, 0, 0, 0])
}

/// Suma del bloque de commit: crc del bloque con `h_chksum[0]` (offset 16) a cero.
pub(crate) fn suma_commit(semilla: u32, blk: &[u8]) -> u32 {
    let mut c = crc32c(semilla, &blk[..16]);
    c = crc32c(c, &[0, 0, 0, 0]);
    crc32c(c, &blk[20..])
}

/// Una etiqueta de bloque dentro de un descriptor.
struct Tag {
    destino: u64,
    banderas: u32,
    suma: u32,
}

impl<R: Read + Write + Seek> Ext4<R> {
    /// ¿Tiene el volumen transacciones de journal pendientes de aplicar?
    #[must_use]
    pub fn journal_pendiente(&self) -> bool {
        self.sb.incompat & s::INCOMPAT_RECOVER != 0
    }

    pub(crate) fn abrir_journal(&mut self) -> Result<Journal, ErrorFs> {
        let ino = u32::from_le_bytes(self.sb_crudo[SB_INODO_JOURNAL..SB_INODO_JOURNAL + 4].try_into().expect("4"));
        if ino == 0 {
            return Err(ErrorFs::NoSoportado { que: "journal en un dispositivo externo (journal_dev)" });
        }
        let jino = self.inodo(ino)?;
        let mapa: Vec<u64> = self
            .bloques_fisicos(&jino)?
            .into_iter()
            .map(|b| b.ok_or(ErrorFs::NoSoportado { que: "journal con huecos" }))
            .collect::<Result<_, _>>()?;
        if mapa.is_empty() {
            return Err(ErrorFs::NoSoportado { que: "journal vacío" });
        }
        let sbj = self.leer_bloque(mapa[0])?;
        if be32(&sbj, 0) != J_MAGIA || !matches!(be32(&sbj, 4), J_SB_V1 | J_SB_V2) {
            return Err(ErrorFs::NoSoportado { que: "superbloque del journal con magia inválida" });
        }
        if be32(&sbj, 0x0C) as u64 != self.bs() {
            return Err(ErrorFs::NoSoportado { que: "journal con otro tamaño de bloque" });
        }
        let incompat = if be32(&sbj, 4) == J_SB_V2 { be32(&sbj, 0x28) } else { 0 };
        if incompat & !JI_SABIDAS != 0 {
            return Err(ErrorFs::NoSoportado {
                que: if incompat & JI_FAST_COMMIT != 0 { "journal con fast_commit" } else { "journal con una característica desconocida" },
            });
        }
        let semilla = crc32c(!0, &sbj[0x30..0x40]);
        if incompat & (JI_CSUM_V2 | JI_CSUM_V3) != 0 {
            let mut copia = sbj[..1024].to_vec();
            copia[0xFC..0x100].fill(0);
            if crc32c(!0, &copia) != be32(&sbj, 0xFC) {
                return Err(ErrorFs::SumaInvalida { que: "superbloque del journal" });
            }
        }
        let largo = be32(&sbj, 0x10).min(mapa.len() as u32);
        Ok(Journal {
            mapa,
            primero: be32(&sbj, 0x14),
            largo,
            inicio: be32(&sbj, 0x1C),
            secuencia: be32(&sbj, 0x18),
            incompat,
            semilla,
            pos: be32(&sbj, 0x14),
            sb_bloque: sbj,
            pendiente: false,
            registrados: std::collections::HashSet::new(),
        })
    }

    fn bloque_j(&mut self, j: &Journal, n: u32) -> Result<Vec<u8>, ErrorFs> {
        let fis = *j.mapa.get(n as usize).ok_or(ErrorFs::NoSoportado { que: "bloque de journal fuera del inodo" })?;
        self.leer_bloque(fis)
    }

    /// Las etiquetas de un bloque descriptor (`None` si su suma no cuadra).
    fn etiquetas(j: &Journal, blk: &[u8]) -> Option<Vec<Tag>> {
        if j.csum() && suma_con_cola(j.semilla, blk) != be32(blk, blk.len() - 4) {
            return None;
        }
        let fin = if j.csum() { blk.len() - 4 } else { blk.len() };
        let tt = j.tam_tag();
        let mut v = Vec::new();
        let mut o = 12usize;
        while o + tt <= fin {
            let (destino_lo, banderas, alto, suma) = if j.v3() {
                (be32(blk, o), be32(blk, o + 4), be32(blk, o + 8), be32(blk, o + 12))
            } else {
                let sum16 = u16::from_be_bytes([blk[o + 4], blk[o + 5]]) as u32;
                let fl = u16::from_be_bytes([blk[o + 6], blk[o + 7]]) as u32;
                let alto = if j.es64() { be32(blk, o + 8) } else { 0 };
                (be32(blk, o), fl, alto, sum16)
            };
            let destino = destino_lo as u64 | if j.es64() { (alto as u64) << 32 } else { 0 };
            v.push(Tag { destino, banderas, suma });
            o += tt;
            if banderas & TAG_MISMO_UUID == 0 {
                o += 16;
            }
            if banderas & TAG_ULTIMO != 0 {
                break;
            }
        }
        Some(v)
    }

    /// Recorre el registro. `revocados`: si `Some`, se llenan; `reproducir`: si true, se
    /// escriben los bloques. Devuelve (secuencia siguiente a la última confirmada, informe).
    fn pasada(
        &mut self,
        j: &Journal,
        fin_secuencia: Option<u32>,
        revocados: &mut HashMap<u64, u32>,
        modo: u8,
    ) -> Result<(u32, InformeJournal), ErrorFs> {
        let mut inf = InformeJournal::default();
        let mut seq = j.secuencia;
        let mut pos = j.inicio;
        let mut pendientes: Vec<(u64, Vec<u8>, u32, u32)> = Vec::new(); // (destino, datos, suma, seq)
        loop {
            if let Some(f) = fin_secuencia {
                if seq >= f {
                    break;
                }
            }
            let blk = self.bloque_j(j, pos)?;
            if be32(&blk, 0) != J_MAGIA || be32(&blk, 8) != seq {
                break;
            }
            match be32(&blk, 4) {
                J_DESCRIPTOR => {
                    let Some(tags) = Self::etiquetas(j, &blk) else { break };
                    let mut p = j.sig(pos);
                    for t in tags {
                        let crudo = self.bloque_j(j, p)?;
                        if modo == 2 {
                            // La suma de la etiqueta es de la copia TAL CUAL está en el journal
                            // (escapada); la firma se restaura después, al reproducir.
                            let suma_ok = !j.csum() || {
                                let c = crc32c(crc32c(j.semilla, &seq.to_be_bytes()), &crudo);
                                if j.v3() { c == t.suma } else { (c & 0xFFFF) == t.suma }
                            };
                            let mut datos = crudo;
                            if t.banderas & TAG_ESCAPE != 0 {
                                pon_be32(&mut datos, 0, J_MAGIA);
                            }
                            pendientes.push((t.destino, datos, u32::from(suma_ok), seq));
                        }
                        p = j.sig(p);
                    }
                    pos = p;
                    continue;
                }
                J_COMMIT => {
                    if j.csum() && suma_commit(j.semilla, &blk) != be32(&blk, 16) {
                        break; // commit roto = esta transacción no existe
                    }
                    // La transacción está confirmada: ahora sí se aplica lo que traía.
                    for (destino, datos, suma_ok, tseq) in pendientes.drain(..) {
                        if revocados.get(&destino).is_some_and(|&r| r >= tseq) {
                            inf.revocados_saltados += 1;
                            continue;
                        }
                        if suma_ok == 0 {
                            inf.sumas_invalidas += 1;
                            continue;
                        }
                        self.escribir_bloque(destino, &datos)?;
                        inf.bloques_reproducidos += 1;
                    }
                    inf.transacciones += 1;
                    seq = seq.wrapping_add(1);
                }
                J_REVOCACION => {
                    if j.csum() && suma_con_cola(j.semilla, &blk) != be32(&blk, blk.len() - 4) {
                        break;
                    }
                    if modo == 1 {
                        let usados = (be32(&blk, 12) as usize).min(blk.len());
                        let paso = if j.es64() { 8 } else { 4 };
                        let mut o = 16usize;
                        while o + paso <= usados {
                            let b = if j.es64() {
                                u64::from_be_bytes(blk[o..o + 8].try_into().expect("8"))
                            } else {
                                be32(&blk, o) as u64
                            };
                            let e = revocados.entry(b).or_insert(seq);
                            *e = (*e).max(seq);
                            o += paso;
                        }
                    }
                }
                _ => break,
            }
            pos = j.sig(pos);
        }
        // Lo que quedó en `pendientes` es de una transacción SIN commit: se descarta.
        Ok((seq, inf))
    }

    /// ¿Están a cero todas las entradas del archivo de huérfanos (`orphan_file`)? Cada bloque
    /// es una lista de números de inodo con una cola de 8 bytes (magia + suma) al final.
    fn archivo_huerfanos_vacio(&mut self, ino: u32) -> Result<bool, ErrorFs> {
        let i = self.inodo(ino)?;
        let bs = self.bs() as usize;
        for b in self.bloques_fisicos(&i)?.into_iter().flatten() {
            let blk = self.leer_bloque(b)?;
            if blk[..bs - 8].iter().any(|&x| x != 0) {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Reproduce el journal pendiente sobre el FS y lo deja vacío. Si no hay nada pendiente,
    /// no toca nada.
    ///
    /// # Errors
    /// [`ErrorFs::NoSoportado`] si el journal usa algo que no se sabe reproducir (fast_commit,
    /// journal externo, característica desconocida); [`ErrorFs::SumaInvalida`] si su superbloque
    /// no cuadra; E/S.
    pub fn recuperar_journal(&mut self) -> Result<InformeJournal, ErrorFs> {
        // Escribe bloques por su cuenta (no por las operaciones de directorio): la caché de
        // directorios se vacía antes y después.
        self.cache_dirs.clear();
        let r = self.recuperar_journal_sin_cache();
        self.cache_dirs.clear();
        r
    }

    fn recuperar_journal_sin_cache(&mut self) -> Result<InformeJournal, ErrorFs> {
        if !self.journal_pendiente() {
            return Ok(InformeJournal::default());
        }
        let j = self.abrir_journal()?;
        let mut inf = InformeJournal::default();
        if j.inicio != 0 {
            // 1: escaneo (dónde acaba lo confirmado) — sin escribir nada.
            let mut nada = HashMap::new();
            let (fin, _) = self.pasada(&j, None, &mut nada, 0)?;
            // 2: revocaciones de las transacciones confirmadas.
            let mut revocados = HashMap::new();
            self.pasada(&j, Some(fin), &mut revocados, 1)?;
            // 3: reproducir.
            let (fin2, i) = self.pasada(&j, Some(fin), &mut revocados, 2)?;
            debug_assert_eq!(fin, fin2);
            inf = i;
            // Journal vacío: s_start = 0, y la secuencia SALTA una más allá de la siguiente: si
            // quedó una transacción `fin` a medias (sin commit), sus bloques viejos siguen en
            // el registro y no deben poder confundirse con una transacción `fin` futura.
            let mut sbj = j.sb_bloque.clone();
            pon_be32(&mut sbj, 0x1C, 0);
            pon_be32(&mut sbj, 0x18, fin.wrapping_add(1));
            if j.csum() {
                sbj[0xFC..0x100].fill(0);
                let c = crc32c(!0, &sbj[..1024]);
                pon_be32(&mut sbj, 0xFC, c);
            }
            self.escribir_bloque(j.mapa[0], &sbj)?;
        }
        // Releer el superbloque del FS DEL DISCO (el journal pudo reescribirlo) y quitar
        // needs_recovery.
        let mut buf = vec![0u8; s::TAMANO];
        self.disp.seek(SeekFrom::Start(s::DESPLAZAMIENTO as u64))?;
        self.disp.read_exact(&mut buf)?;
        let incompat = u32::from_le_bytes(buf[0x60..0x64].try_into().expect("4")) & !s::INCOMPAT_RECOVER;
        buf[0x60..0x64].copy_from_slice(&incompat.to_le_bytes());
        let mut ro = u32::from_le_bytes(buf[0x64..0x68].try_into().expect("4"));
        let ultimo_huerfano = u32::from_le_bytes(buf[0xE8..0xEC].try_into().expect("4"));
        // `orphan_present` dice «el archivo de huérfanos estaba en uso». Si está VACÍO (nada a
        // medio borrar en el corte), se quita, como hacen el kernel al montar y e2fsck; si
        // tiene entradas, se deja y se informa (procesarlas es truncar/borrar inodos).
        if ro & RO_ORPHAN_PRESENT != 0 {
            let ino_huerf = u32::from_le_bytes(buf[SB_INODO_HUERFANOS..SB_INODO_HUERFANOS + 4].try_into().expect("4"));
            if ino_huerf != 0 && self.archivo_huerfanos_vacio(ino_huerf)? {
                ro &= !RO_ORPHAN_PRESENT;
                buf[0x64..0x68].copy_from_slice(&ro.to_le_bytes());
            }
        }
        inf.huerfanos_pendientes = ro & RO_ORPHAN_PRESENT != 0 || ultimo_huerfano != 0;
        let sb = s::Superbloque::leer(&buf).map_err(ErrorFs::Superbloque)?;
        if sb.tiene_csum_metadatos() {
            let c = crate::csum::superbloque(&buf);
            buf[crate::csum::SB_OFF_CSUM..].copy_from_slice(&c.to_le_bytes());
        }
        self.disp.seek(SeekFrom::Start(s::DESPLAZAMIENTO as u64))?;
        self.disp.write_all(&buf)?;
        self.disp.flush()?;
        self.semilla = if sb.tiene_csum_metadatos() { crate::csum::semilla(&buf, sb.guarda_semilla(), &sb.uuid) } else { 0 };
        self.sb = sb;
        self.sb_crudo = buf;
        Ok(inf)
    }
}
