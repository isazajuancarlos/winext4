// SPDX-FileCopyrightText: 2026 Juan Carlos Isaza
// SPDX-License-Identifier: LicenseRef-Xiliux-Propietario
//! Verificación PREVIA a montar con escritura: la RÁPIDA siempre (segundos) y la COMPLETA solo
//! con motivo. Las situaciones se fabrican con `debugfs` (otro programa, no el nuestro) y cada
//! regla tiene su pareja: lo que DEBE pedir la completa y lo que NO.

mod comun;

use comun::{corre, ruta};
use winext4_nucleo::Ext4;

fn imagen(nombre: &str) -> std::path::PathBuf {
    let img = std::env::temp_dir().join(format!("winext4-vprev-{nombre}-{}.img", std::process::id()));
    std::fs::File::create(&img).unwrap().set_len(32 * 1024 * 1024).unwrap();
    assert_eq!(corre("mkfs.ext4", &["-F", "-q", "-b", "4096", &ruta(&img)]), Some(true));
    img
}

fn ssv(img: &std::path::Path, ordenes: &[&str]) {
    let cmds = img.with_extension("cmds");
    std::fs::write(&cmds, ordenes.iter().map(|o| format!("ssv {o}\n")).collect::<String>()).unwrap();
    assert_eq!(corre("debugfs", &["-w", "-f", &ruta(&cmds), &ruta(img)]), Some(true));
    let _ = std::fs::remove_file(&cmds);
}

fn ahora() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()
}

fn abrir(img: &std::path::Path) -> Ext4<std::fs::File> {
    Ext4::abrir(std::fs::OpenOptions::new().read(true).write(true).open(img).unwrap()).unwrap()
}

#[test]
fn cuando_hace_falta_la_verificacion_completa() {
    if !comun::hay_oraculo() {
        eprintln!("SALTADA: faltan e2fsprogs");
        return;
    }
    // (0) Recién formateado: rápida limpia y SIN motivo (pareja negativa: no pide de más).
    let img = imagen("limpio");
    {
        let mut fs = abrir(&img);
        assert_eq!(fs.verificar_rapido().unwrap().errores(), 0, "rápida limpia en un volumen nuevo");
        assert_eq!(fs.motivo_verificacion_completa(ahora(), 30), None, "un volumen nuevo no pide la completa");
    }
    // (1) Marca de errores → completa.
    ssv(&img, &["state 3"]);
    {
        let mut fs = abrir(&img);
        assert!(fs.motivo_verificacion_completa(ahora(), 30).is_some_and(|m| m.contains("ERROR_FS")));
        assert!(fs.verificar_rapido().unwrap().tiene("estado-errores"), "la rápida también la ve");
    }
    // Con la marca de errores, escribir se NIEGA en seco (no basta con verificar: hay que
    // reparar —winext4util reparar o e2fsck—, que es quien la quita).
    assert!(abrir(&img).abrir_escritura().is_err(), "con ERROR_FS, abrir_escritura se niega");
    ssv(&img, &["state 1"]);
    // (2) Errores registrados DESPUÉS de la última verificación → completa.
    ssv(&img, &["error_count 516", "lastcheck 20261001", "last_error_time 20261104"]);
    assert!(abrir(&img).motivo_verificacion_completa(ahora(), 3650).is_some_and(|m| m.contains("516 errores")), "errores nuevos piden la completa");
    // (3) Pareja de (2): los mismos errores, pero ANTERIORES a la última verificación → no.
    ssv(&img, &["lastcheck 20261105", "last_error_time 20261104"]);
    assert_eq!(abrir(&img).motivo_verificacion_completa(ahora(), 3650), None, "errores ya verificados no piden repetir");
    // (4) Última verificación vieja → completa; con un margen mayor, no.
    ssv(&img, &["error_count 0", "lastcheck 20260101"]);
    assert!(abrir(&img).motivo_verificacion_completa(ahora(), 30).is_some_and(|m| m.contains("30 días")));
    assert_eq!(abrir(&img).motivo_verificacion_completa(ahora(), 3650), None, "pareja: con 10 años de margen, no");
    // (5) Anotar la verificación completa: el motivo desaparece y queda escrito en el disco.
    ssv(&img, &["error_count 516", "lastcheck 20261001", "last_error_time 20261104"]);
    {
        let mut fs = abrir(&img);
        assert!(fs.motivo_verificacion_completa(ahora(), 3650).is_some(), "control: antes de anotar hay motivo");
        fs.abrir_escritura().unwrap();
        fs.anotar_verificacion_completa().unwrap();
        fs.cerrar_escritura().unwrap();
    }
    assert_eq!(abrir(&img).motivo_verificacion_completa(ahora(), 30), None, "tras anotar, basta la rápida");
    comun::fsck(&img).unwrap_or_else(|s| panic!("tras anotar la verificación: {s}"));
    let o = comun::e2fs("dumpe2fs").args(["-h", &ruta(&img)]).output().unwrap();
    let txt = String::from_utf8_lossy(&o.stdout);
    let linea = txt.lines().find(|l| l.starts_with("Last checked:")).unwrap_or("").to_string();
    assert!(!linea.contains("Oct"), "dumpe2fs ve la fecha nueva: {linea}");
    let _ = std::fs::remove_file(&img);
}

#[test]
fn la_rapida_ve_descriptores_y_bitmaps_danados() {
    if !comun::hay_oraculo() {
        eprintln!("SALTADA: faltan e2fsprogs");
        return;
    }
    let img = imagen("danos");
    let (gdt, bmb) = {
        let mut fs = abrir(&img);
        assert_eq!(fs.verificar_rapido().unwrap().errores(), 0, "control: limpia antes de dañar");
        (fs.sb.primer_bloque_dato as u64 + 1, fs.descriptor(0).unwrap().bitmap_bloques())
    };
    let tocar = |bloque: u64, off: u64| {
        use std::io::{Read, Seek, SeekFrom, Write};
        let mut f = std::fs::OpenOptions::new().read(true).write(true).open(&img).unwrap();
        let mut b = [0u8; 1];
        f.seek(SeekFrom::Start(bloque * 4096 + off)).unwrap();
        f.read_exact(&mut b).unwrap();
        b[0] ^= 0x01;
        f.seek(SeekFrom::Start(bloque * 4096 + off)).unwrap();
        f.write_all(&b).unwrap();
    };
    tocar(gdt, 0x0C); // bg_free_blocks_count del grupo 0, sin rehacer la suma
    assert!(abrir(&img).verificar_rapido().unwrap().tiene("suma-descriptor"), "la rápida ve el descriptor dañado");
    tocar(gdt, 0x0C); // deshecho
    assert_eq!(abrir(&img).verificar_rapido().unwrap().errores(), 0, "pareja: deshecho el daño, limpia");
    tocar(bmb, 4000); // un bit lejano del bitmap de bloques del grupo 0
    assert!(abrir(&img).verificar_rapido().unwrap().tiene("suma-bitmap-bloques"), "la rápida ve el bitmap dañado");
    let _ = std::fs::remove_file(&img);
}
