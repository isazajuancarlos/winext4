// SPDX-FileCopyrightText: 2026 Juan Carlos Isaza
// SPDX-License-Identifier: LicenseRef-Xiliux-Propietario
//! Enlaces simbólicos (rápidos y lentos) y duros CREADOS por winext4, y su RESOLUCIÓN
//! (`buscar_siguiendo`). Oráculo: `e2fsck -fn`, el KERNEL leyéndolos (`readlink`, `cat`
//! a través del enlace, `stat -c %h`) y escribiendo encima, y `debugfs`.
//!
//! Desde el Explorador o `mklink` NO se pueden crear: Dokan 2.3.1 no pasa
//! `FSCTL_SET_REPARSE_POINT` ni `FileLinkInformation` al sistema de archivos (medido y leído en
//! su fuente el 2026-11-05). Se crean con `winext4util enlazar`; el montaje los SIGUE.

mod comun;

use comun::{corre, ruta};
use winext4_nucleo::{ErrorFs, Ext4};

fn largo() -> String {
    // > 60 bytes → enlace LENTO (un bloque de datos). Relativo y roto a propósito.
    format!("{}/x", "d".repeat(90))
}

#[test]
fn enlaces_simbolicos_y_duros() {
    if !comun::hay_oraculo() {
        eprintln!("SALTADA: faltan e2fsprogs");
        return;
    }
    let img = std::env::temp_dir().join(format!("winext4-enlaces-{}.img", std::process::id()));
    std::fs::File::create(&img).unwrap().set_len(32 * 1024 * 1024).unwrap();
    assert_eq!(corre("mkfs.ext4", &["-F", "-q", "-b", "4096", &ruta(&img)]), Some(true));
    let f = std::fs::OpenOptions::new().read(true).write(true).open(&img).unwrap();
    let mut fs = Ext4::abrir(f).unwrap();
    let r = 2;
    fs.abrir_escritura().unwrap();
    let dest = fs.crear_archivo(r, "destino.txt", 0o644).unwrap();
    fs.escribir(dest, 0, b"contenido del destino\n").unwrap();
    let carpeta = fs.crear_directorio(r, "carpeta", 0o755).unwrap();
    let dentro = fs.crear_archivo(carpeta, "f", 0o644).unwrap();
    fs.escribir(dentro, 0, b"dentro\n").unwrap();
    let rapido = fs.crear_enlace_simbolico(r, "rapido", "destino.txt").unwrap();
    let lento = fs.crear_enlace_simbolico(r, "lento", &largo()).unwrap();
    fs.crear_enlace_simbolico(r, "a-carpeta", "carpeta").unwrap();
    let absoluto = fs.crear_enlace_simbolico(r, "absoluto", "/etc/passwd").unwrap();
    let roto = fs.crear_enlace_simbolico(r, "roto", "no-existe").unwrap();
    let bucle_a = fs.crear_enlace_simbolico(r, "bucle-a", "bucle-b").unwrap();
    fs.crear_enlace_simbolico(r, "bucle-b", "bucle-a").unwrap();
    let escape = fs.crear_enlace_simbolico(carpeta, "escape", "../../fuera").unwrap();
    fs.crear_enlace_simbolico(carpeta, "sube", "../destino.txt").unwrap();
    fs.crear_enlace_duro(r, "duro.txt", dest).unwrap();
    // Lo que se niega.
    assert!(matches!(fs.crear_enlace_duro(r, "dir-duro", carpeta), Err(ErrorFs::EsDirectorio)));
    assert!(matches!(fs.crear_enlace_duro(r, "duro.txt", dest), Err(ErrorFs::YaExiste { .. })));
    assert!(matches!(fs.crear_enlace_simbolico(r, "vacio", ""), Err(ErrorFs::NombreInvalido)));
    // Escribir en un enlace simbólico (no en su destino) se niega: no tiene contenido editable.
    assert!(fs.escribir(rapido, 0, b"x").is_err() && fs.truncar(lento, 0).is_err());
    // Crear y borrar (rápido, lento y un duro): no puede dejar fugas.
    fs.crear_enlace_simbolico(r, "tmp-rapido", "destino.txt").unwrap();
    fs.crear_enlace_simbolico(r, "tmp-lento", &largo()).unwrap();
    fs.crear_enlace_duro(r, "tmp-duro", dest).unwrap();
    for n in ["tmp-rapido", "tmp-lento", "tmp-duro"] {
        fs.borrar(r, n).unwrap();
    }
    fs.cerrar_escritura().unwrap();

    // Lectura propia: destino exacto y contenido del enlace rápido = su destino.
    let i = fs.inodo(rapido).unwrap();
    assert_eq!(fs.leer_enlace(&i).unwrap(), "destino.txt");
    assert_eq!(fs.leer_rango(&i, 0, 100).unwrap(), b"destino.txt", "leer un enlace rápido da su destino, no bloques ajenos");
    assert_eq!(fs.leer_rango(&i, 3, 4).unwrap(), b"tino");
    let il = fs.inodo(lento).unwrap();
    assert_eq!(fs.leer_enlace(&il).unwrap(), largo());
    assert_eq!(u16::from_le_bytes(fs.inodo_crudo(dest).unwrap()[0x1A..0x1C].try_into().unwrap()), 2, "destino.txt con 2 enlaces");

    // Resolución.
    let s = |fs: &mut Ext4<std::fs::File>, p: &str, seguir: bool| fs.buscar_siguiendo(p, seguir);
    assert_eq!(s(&mut fs, "/rapido", true).unwrap(), dest, "sigue el rápido");
    assert_eq!(s(&mut fs, "/rapido", false).unwrap(), rapido, "sin seguir el último: el enlace");
    assert_eq!(s(&mut fs, "/a-carpeta/f", false).unwrap(), dentro, "sigue un enlace INTERMEDIO siempre");
    assert_eq!(s(&mut fs, "/a-carpeta", true).unwrap(), carpeta);
    assert_eq!(s(&mut fs, "/carpeta/sube", true).unwrap(), dest, "«..» relativo al directorio del enlace");
    assert_eq!(s(&mut fs, "/duro.txt", true).unwrap(), dest);
    for (p, ino, por) in [("/absoluto", absoluto, "absoluto"), ("/roto", roto, "roto"), ("/bucle-a", bucle_a, "bucle"), ("/carpeta/escape", escape, "sale del volumen"), ("/lento", lento, "lento y roto")] {
        assert_eq!(s(&mut fs, p, true).unwrap(), ino, "{por}: no se sigue, se ve el propio enlace");
    }
    for p in ["/roto/x", "/absoluto/x", "/bucle-a/x", "/no/existe"] {
        assert!(matches!(s(&mut fs, p, true), Err(ErrorFs::NoEncontrado { .. })), "{p}: sin camino");
    }
    drop(fs);

    if let Err(e) = comun::fsck(&img) {
        panic!("e2fsck tras crear enlaces:\n{e}");
    }
    assert!(comun::guion_root("enlaces-kernel.sh", &[&ruta(&img), &largo()]), "el kernel lee y usa los enlaces de winext4");
    if let Err(e) = comun::fsck(&img) {
        panic!("e2fsck tras escribir el kernel encima:\n{e}");
    }
    let _ = std::fs::remove_file(&img);
}
