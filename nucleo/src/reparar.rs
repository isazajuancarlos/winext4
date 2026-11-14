// SPDX-FileCopyrightText: 2026 Juan Carlos Isaza
// SPDX-License-Identifier: LicenseRef-Xiliux-Propietario
//! B2 — **reparación SEGURA**: arregla lo que se puede arreglar sin riesgo y se NIEGA ante lo
//! demás (lo manda a `e2fsck`).
//!
//! Repara (todo en UNA transacción del journal: un corte a mitad deja el volumen como estaba
//! o reparado, nunca peor):
//! - **bloque de atributos extendidos con la suma mala** y estructura legible → rehace la suma
//!   y CONSERVA los atributos (e2fsck los borra); si la estructura está rota → lo desvincula del
//!   inodo (como e2fsck) y queda como fuga, que se libera abajo;
//! - **bloques marcados en uso que nadie usa** (fugas) → se liberan;
//! - **contadores** de grupo y de superbloque → a lo calculado.
//!
//! Al final se verifica otra vez; solo si da CERO errores se quita la marca «con errores» que
//! dejó el kernel.
//!
//! Se niega (sin tocar NADA) si hay cualquier otro error: bloques reclamados por dos inodos,
//! bloques en uso marcados libres, directorios o árboles de extents rotos… Arreglar eso sin las
//! cinco pasadas completas de e2fsck es la forma clásica de convertir un daño pequeño en uno
//! grande.

use std::io::{Read, Seek, Write};

use crate::sistema::{ErrorFs, Ext4};
use crate::verificar::{Gravedad, Hallazgo};

/// Clases de error que esta reparación sabe arreglar.
const REPARABLES: &[&str] = &["suma-xattr", "bloque-fuga", "contador-bloques", "contador-inodos", "contador-dirs"];

/// Qué hizo (o por qué no hizo nada) la reparación.
#[derive(Debug, Clone, Default)]
pub struct InformeReparacion {
    /// Acciones realizadas, en lenguaje llano.
    pub hecho: Vec<String>,
    /// Errores que impidieron reparar (si no está vacío, NO se tocó nada).
    pub no_reparables: Vec<Hallazgo>,
    /// Errores que quedan tras reparar (debería ser 0).
    pub errores_despues: usize,
}

const I_BLOQUES_LO: usize = 0x1C;
const I_FILE_ACL: usize = 0x68;
const I_FILE_ACL_HI: usize = 0x76;

impl<R: Read + Write + Seek> Ext4<R> {
    /// Repara lo seguro (ver el módulo). Si encuentra algo que no sabe reparar con seguridad,
    /// no escribe nada y lo devuelve en `no_reparables`.
    ///
    /// # Errors
    /// E/S, o si el volumen no admite escritura (journal pendiente, característica desconocida…).
    pub fn reparar(&mut self) -> Result<InformeReparacion, ErrorFs> {
        // Escribe bloques por su cuenta (no por las operaciones de directorio): la caché de
        // directorios se vacía antes y después.
        self.cache_dirs.clear();
        let r = self.reparar_sin_cache();
        self.cache_dirs.clear();
        r
    }

    fn reparar_sin_cache(&mut self) -> Result<InformeReparacion, ErrorFs> {
        let mut inf = InformeReparacion::default();
        let v = self.verificar()?;
        inf.no_reparables = v.hallazgos.iter().filter(|h| h.gravedad == Gravedad::Error && !REPARABLES.contains(&h.clase)).cloned().collect();
        if !inf.no_reparables.is_empty() {
            return Ok(inf);
        }
        let marcado = self.sb.estado & 0x2 != 0;
        if v.errores() == 0 && !marcado {
            return Ok(inf); // sano: no se escribe ni un byte
        }
        self.abrir_escritura_con(true)?;
        let bs = self.bs();
        // Bloques de atributos ilegibles que se van a liberar: el cálculo de libres de la
        // verificación los contaba como USADOS (un inodo los nombraba); tras desvincularlos y
        // liberarlos, su grupo tiene uno libre más.
        let mut extra_libres = std::collections::HashMap::<u64, u32>::new();
        for (b, _, legible) in &v.xattr_malos {
            if !legible {
                let g = (b - u64::from(self.sb.primer_bloque_dato)) / u64::from(self.sb.bloques_por_grupo);
                *extra_libres.entry(g).or_insert(0) += 1;
            }
        }
        let r = self.en_tx(|fs| {
            let mut hecho = Vec::new();
            // 1. Bloques de atributos.
            for (b, duenos, legible) in &v.xattr_malos {
                if *legible {
                    let mut blk = fs.leer_bloque(*b)?;
                    let c = crate::csum::bloque_xattr(fs.semilla, *b, &blk);
                    blk[0x10..0x14].copy_from_slice(&c.to_le_bytes());
                    fs.escribir_meta(*b, &blk)?;
                    hecho.push(format!("bloque de atributos {b}: suma rehecha, atributos CONSERVADOS (inodos {duenos:?})"));
                } else {
                    for &ino in duenos {
                        let mut c = fs.inodo_crudo(ino)?;
                        c[I_FILE_ACL..I_FILE_ACL + 4].fill(0);
                        c[I_FILE_ACL_HI..I_FILE_ACL_HI + 2].fill(0);
                        let sec = u32::from_le_bytes(c[I_BLOQUES_LO..I_BLOQUES_LO + 4].try_into().expect("4"));
                        let menos = (bs / 512) as u32;
                        c[I_BLOQUES_LO..I_BLOQUES_LO + 4].copy_from_slice(&sec.saturating_sub(menos).to_le_bytes());
                        fs.guardar_inodo(ino, &mut c)?;
                    }
                    fs.liberar_fuga(*b)?;
                    hecho.push(format!("bloque de atributos {b} ilegible: desvinculado de los inodos {duenos:?} y liberado (se pierden esos atributos)"));
                }
            }
            // 2. Fugas.
            for &b in &v.bloques_fuga {
                fs.liberar_fuga(b)?;
            }
            if !v.bloques_fuga.is_empty() {
                hecho.push(format!("{} bloques en uso que nadie usaba, liberados ({:?}…)", v.bloques_fuga.len(), &v.bloques_fuga[..v.bloques_fuga.len().min(5)]));
            }
            // 3. Contadores: a lo calculado. `libres_b` ya contaba las fugas como LIBRES (nadie
            // las usa): tras liberarlas, el descriptor debe decir exactamente eso.
            let mut corregidos = 0;
            for (g, &(libres_b, libres_i, dirs, _)) in v.calculado_por_grupo.iter().enumerate() {
                let g = g as u64;
                let libres_b = libres_b + extra_libres.get(&g).copied().unwrap_or(0);
                let d = fs.descriptor(g)?;
                if d.bloques_libres() != libres_b || d.inodos_libres() != libres_i || d.dirs_usados() != dirs {
                    fs.fijar_contadores(g, libres_b, libres_i, dirs)?;
                    corregidos += 1;
                }
            }
            if corregidos > 0 {
                hecho.push(format!("contadores de {corregidos} grupos ajustados a lo calculado"));
            }
            Ok(hecho)
        });
        inf.hecho = r?;
        // 4. Comprobar y, solo si quedó limpio, quitar la marca del kernel.
        let despues = self.verificar()?;
        inf.errores_despues = despues.errores();
        if inf.errores_despues == 0 && marcado {
            self.quitar_marca_errores()?;
            inf.hecho.push("marca «con errores» del superbloque retirada (verificado: 0 errores)".into());
        }
        self.cerrar_escritura()?;
        Ok(inf)
    }
}
