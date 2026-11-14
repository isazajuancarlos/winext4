// SPDX-FileCopyrightText: 2026 Juan Carlos Isaza
// SPDX-License-Identifier: LicenseRef-Xiliux-Propietario
//! # winext4-nucleo — el motor ext4 propio
//!
//! Lee (y, en fases futuras, escribe) el **formato ext4 en disco**, sin suponer plataforma:
//! recibe BYTES/bloques y produce estructuras tipadas. En Linux lo prueba `e2fsck`/`dumpe2fs`
//! (el oráculo); en Windows lo consumirá el FSD de kernel (`winext4.sys`) — pero el formato
//! no sabe de ninguno de los dos. Es el ~90% del trabajo y donde vive TODO el riesgo de
//! corrupción, por eso es lo primero y se valida contra artefactos REALES (D28/D30/D33).
//!
//! Regla de diseño que no se rompe (D20): ante un dato que no cuadra —magia equivocada,
//! tamaño imposible, bandera incompatible— se **falla ruidosamente** con un error; jamás se
//! rellena con un valor por defecto. Un ext4 mal leído en silencio corrompe en la escritura.

pub mod cache;
pub mod csum;
pub mod directorios;
pub mod dispositivo;
pub mod escritura;
pub mod formatear;
pub mod grupos;
pub mod hash;
pub mod journal;
pub mod reparar;
pub mod sistema;
pub mod superbloque;
pub mod transaccion;
pub mod verificar;

pub use journal::InformeJournal;
pub use sistema::{Entrada, ErrorFs, Ext4, Inodo, PlanLectura, RegistroLiberaciones, INODO_RAIZ};
pub use superbloque::{ErrorSuperbloque, Superbloque};
pub use transaccion::Agrupado;
