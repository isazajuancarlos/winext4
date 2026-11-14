// SPDX-FileCopyrightText: 2026 Juan Carlos Isaza
// SPDX-License-Identifier: LicenseRef-Xiliux-Propietario
//! Host **FUSE** del motor ext4: expone [`winext4_nucleo::Ext4`] como un sistema de archivos
//! REAL que el kernel Linux monta y consume (`ls`, `cat`, `cmp`…). Es SOLO lectura, igual que
//! el motor hoy.
//!
//! Por qué vive aquí y no es un juguete: el binding de FUSE —traducir peticiones del VFS
//! (`lookup`/`getattr`/`readdir`/`read`/`readlink`) a llamadas del motor— es **el mismo
//! modelo** que usará el host de **WinFsp** en Windows (WinFsp tiene incluso API compatible
//! con FUSE). Así que probar esto contra el kernel real de Linux **de-arriesga el binding de
//! Windows** y ya entrega un producto montable, sin pagar firma de kernel ni necesitar Windows.
//!
//! El motor (`nucleo`) sigue sin dependencias; esta caja es el adaptador de plataforma (D9).

use std::ffi::OsStr;
use std::time::{Duration, UNIX_EPOCH};

use fuser::{FileAttr, FileType, Filesystem, ReplyAttr, ReplyData, ReplyDirectory, ReplyEntry, Request};
use winext4_nucleo::{Ext4, Inodo};

/// Vigencia de las respuestas de atributos/entradas en la caché del kernel. 1 s basta para un
/// FS de solo lectura (el contenido no cambia bajo los pies).
const TTL: Duration = Duration::from_secs(1);

/// FUSE numera la raíz como inodo **1**; ext4 la numera como **2**. Esta es la única traducción.
fn a_ext4(ino: u64) -> u32 {
    if ino == 1 {
        2
    } else {
        ino as u32
    }
}

fn a_fuse(ino: u32) -> u64 {
    if ino == 2 {
        1
    } else {
        u64::from(ino)
    }
}

fn tipo_de_modo(modo: u16) -> FileType {
    match modo & 0xF000 {
        0x4000 => FileType::Directory,
        0xA000 => FileType::Symlink,
        0x2000 => FileType::CharDevice,
        0x6000 => FileType::BlockDevice,
        0x1000 => FileType::NamedPipe,
        0xC000 => FileType::Socket,
        _ => FileType::RegularFile,
    }
}

/// Traduce el `file_type` de una entrada de directorio (ext4) a [`FileType`] de FUSE.
fn tipo_de_entrada(t: u8) -> FileType {
    match t {
        2 => FileType::Directory,
        7 => FileType::Symlink,
        3 => FileType::CharDevice,
        4 => FileType::BlockDevice,
        5 => FileType::NamedPipe,
        6 => FileType::Socket,
        _ => FileType::RegularFile,
    }
}

/// El host: envuelve el motor. El `Filesystem` de fuser pide `&mut self`, y el motor necesita
/// `&mut` para posicionarse en el dispositivo, así que encaja directo (sesión secuencial).
pub struct Ext4Fuse<R> {
    motor: Ext4<R>,
    uid: u32,
    gid: u32,
}

impl<R: std::io::Read + std::io::Seek> Ext4Fuse<R> {
    /// Crea el host sobre un motor ya abierto. uid/gid = los del proceso (dueño de todo al leer).
    pub fn nuevo(motor: Ext4<R>) -> Self {
        // SAFETY: getuid/getgid no tienen precondiciones y nunca fallan.
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        Self { motor, uid, gid }
    }

    fn atributos(&self, ino_fuse: u64, i: &Inodo) -> FileAttr {
        let kind = tipo_de_modo(i.modo);
        let nlink = if kind == FileType::Directory { 2 } else { 1 };
        FileAttr {
            ino: ino_fuse,
            size: i.tamano,
            blocks: i.tamano.div_ceil(512),
            atime: UNIX_EPOCH,
            mtime: UNIX_EPOCH,
            ctime: UNIX_EPOCH,
            crtime: UNIX_EPOCH,
            kind,
            perm: (i.modo & 0o7777),
            nlink,
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: self.motor.sb.tamano_bloque,
            flags: 0,
        }
    }
}

impl<R: std::io::Read + std::io::Seek + 'static> Filesystem for Ext4Fuse<R> {
    fn lookup(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let Some(nombre) = name.to_str() else {
            reply.error(libc::EINVAL);
            return;
        };
        let dir = match self.motor.inodo(a_ext4(parent)) {
            Ok(i) => i,
            Err(_) => return reply.error(libc::ENOENT),
        };
        let entradas = match self.motor.listar(&dir) {
            Ok(e) => e,
            Err(_) => return reply.error(libc::EIO),
        };
        let Some(e) = entradas.into_iter().find(|e| e.nombre == nombre) else {
            return reply.error(libc::ENOENT);
        };
        match self.motor.inodo(e.ino) {
            Ok(i) => {
                let ino_fuse = a_fuse(e.ino);
                reply.entry(&TTL, &self.atributos(ino_fuse, &i), 0);
            }
            Err(_) => reply.error(libc::EIO),
        }
    }

    fn getattr(&mut self, _req: &Request<'_>, ino: u64, reply: ReplyAttr) {
        match self.motor.inodo(a_ext4(ino)) {
            Ok(i) => reply.attr(&TTL, &self.atributos(ino, &i)),
            Err(_) => reply.error(libc::ENOENT),
        }
    }

    fn readdir(&mut self, _req: &Request<'_>, ino: u64, _fh: u64, offset: i64, mut reply: ReplyDirectory) {
        let dir = match self.motor.inodo(a_ext4(ino)) {
            Ok(i) => i,
            Err(_) => return reply.error(libc::ENOENT),
        };
        if !dir.es_directorio() {
            return reply.error(libc::ENOTDIR);
        }
        let entradas = match self.motor.listar(&dir) {
            Ok(e) => e,
            Err(_) => return reply.error(libc::EIO),
        };
        for (idx, e) in entradas.into_iter().enumerate().skip(offset as usize) {
            // El offset del siguiente es idx+1 (el kernel lo reanuda ahí).
            if reply.add(a_fuse(e.ino), (idx + 1) as i64, tipo_de_entrada(e.tipo), e.nombre) {
                break; // búfer lleno
            }
        }
        reply.ok();
    }

    fn read(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        let i = match self.motor.inodo(a_ext4(ino)) {
            Ok(i) => i,
            Err(_) => return reply.error(libc::ENOENT),
        };
        // Lee el archivo y recorta a [offset, offset+size). (Sin caché aún; se optimiza luego.)
        let datos = match self.motor.leer_datos(&i) {
            Ok(d) => d,
            Err(_) => return reply.error(libc::EIO),
        };
        let ini = (offset.max(0) as usize).min(datos.len());
        let fin = ini.saturating_add(size as usize).min(datos.len());
        reply.data(&datos[ini..fin]);
    }

    fn readlink(&mut self, _req: &Request<'_>, ino: u64, reply: ReplyData) {
        let i = match self.motor.inodo(a_ext4(ino)) {
            Ok(i) => i,
            Err(_) => return reply.error(libc::ENOENT),
        };
        match self.motor.leer_enlace(&i) {
            Ok(destino) => reply.data(destino.as_bytes()),
            Err(_) => reply.error(libc::EINVAL),
        }
    }
}
