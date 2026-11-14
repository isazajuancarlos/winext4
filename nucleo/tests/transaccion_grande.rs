// SPDX-FileCopyrightText: 2026 Juan Carlos Isaza
// SPDX-License-Identifier: LicenseRef-Xiliux-Propietario
//! TRANSACCIONES GRANDES: un grupo con MILES de bloques de metadatos (varios descriptores del
//! journal LLENOS). Cazado en el disco real el 2026-11-06: el cálculo de etiquetas por
//! descriptor daba una de más y, al pasar de 254 bloques, el motor entraba en pánico escribiendo
//! fuera del bloque. Ninguna prueba llegaba a ese tamaño.
//!
//! Se crea un grupo enorme (agrupado sin tope práctico), se confirma, y se «corta la luz» antes
//! de cerrar (sin `cerrar_escritura`): la recuperación — de e2fsck y de winext4 — tiene que
//! reproducirlo entero y dejar el volumen limpio y con todo lo creado. Control D30: el grupo
//! confirmado tiene que necesitar MÁS de un descriptor lleno.

mod comun;

use std::time::Duration;

use comun::ruta;
use winext4_nucleo::{Agrupado, Ext4};

const N: usize = 10_000;

fn crear_imagen(nombre: &str) -> std::path::PathBuf {
    let img = std::env::temp_dir().join(format!("winext4-txgrande-{nombre}-{}.img", std::process::id()));
    let mut f = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&img).unwrap();
    f.set_len(512 * 1024 * 1024).unwrap();
    winext4_nucleo::formatear::formatear(&mut f, 512 * 1024 * 1024, "TXGRANDE").unwrap();
    img
}

/// Crea N archivos en 30 carpetas en UN solo grupo y lo confirma; NO cierra (el «corte»).
fn grupo_enorme(img: &std::path::Path) -> usize {
    let f = std::fs::OpenOptions::new().read(true).write(true).open(img).unwrap();
    let mut fs = Ext4::abrir(f).unwrap();
    fs.abrir_escritura().unwrap();
    fs.agrupar_transacciones(Some(Agrupado { max_espera: Duration::from_secs(3600), max_bloques: 1_000_000 })).unwrap();
    let mut dirs = Vec::new();
    for d in 0..50 {
        dirs.push(fs.crear_directorio(2, &format!("d{d}"), 0o755).unwrap());
    }
    for i in 0..N {
        let n = fs.crear_archivo(dirs[i % 50], &format!("archivo-con-nombre-largo-{i:05}.txt"), 0o644).unwrap();
        fs.escribir(n, 0, format!("contenido {i}").as_bytes()).unwrap();
    }
    let pendientes = fs.bloques_pendientes();
    fs.confirmar_pendiente().unwrap();
    // «Corte»: sin cerrar_escritura (el journal queda con la transacción por reproducir).
    std::mem::forget(fs);
    pendientes
}

#[test]
fn un_grupo_de_miles_de_bloques_se_confirma_y_se_recupera() {
    if !comun::hay_oraculo() {
        eprintln!("SALTADA: faltan e2fsprogs");
        return;
    }
    // (a) recuperado por e2fsck
    let img = crear_imagen("e2fsck");
    let bloques = grupo_enorme(&img);
    eprintln!("grupo confirmado con {bloques} bloques de metadatos");
    assert!(bloques > 2 * 254, "control D30: el grupo tiene que llenar varios descriptores ({bloques} bloques)");
    let o = comun::e2fs("e2fsck").args(["-E", "journal_only", "-y", &ruta(&img)]).output().unwrap();
    assert!(matches!(o.status.code(), Some(0 | 1)), "e2fsck journal_only: {}", String::from_utf8_lossy(&o.stdout));
    comun::fsck(&img).unwrap_or_else(|s| panic!("tras recuperar con e2fsck:\n{s}"));
    assert_eq!(comun::volcar(&img, &format!("/d7/archivo-con-nombre-largo-{:05}.txt", 7)), b"contenido 7");
    let _ = std::fs::remove_file(&img);

    // (b) recuperado por winext4
    let img = crear_imagen("winext4");
    grupo_enorme(&img);
    {
        let f = std::fs::OpenOptions::new().read(true).write(true).open(&img).unwrap();
        let inf = Ext4::abrir(f).unwrap().recuperar_journal().unwrap();
        assert!(inf.bloques_reproducidos as usize > 2 * 254, "winext4 reprodujo {} bloques", inf.bloques_reproducidos);
    }
    comun::fsck(&img).unwrap_or_else(|s| panic!("tras recuperar con winext4:\n{s}"));
    let v = Ext4::abrir(std::fs::File::open(&img).unwrap()).unwrap().verificar().unwrap();
    assert_eq!(v.errores(), 0, "verificar: {:#?}", v.hallazgos);
    assert_eq!(comun::volcar(&img, &format!("/d49/archivo-con-nombre-largo-{:05}.txt", N - 1)), format!("contenido {}", N - 1).into_bytes());
    let _ = std::fs::remove_file(&img);
}
