// SPDX-FileCopyrightText: 2026 Juan Carlos Isaza
// SPDX-License-Identifier: LicenseRef-Xiliux-Propietario
//! F3 (escritura, primer tramo): **escritura EN SITIO** sobre un ext4 REAL, con `e2fsck` de
//! ORÁCULO. Positivo de control (D30): tras sobrescribir bytes de un archivo, `e2fsck -fn` da
//! el FS LIMPIO (no tocamos metadatos) y la relectura devuelve exactamente lo escrito, con el
//! resto intacto. Negativo por la vía real (D33): salirse del tamaño da `NoCreceAun`, no una
//! escritura fuera de rango.
//!
//! Se salta si faltan `mkfs.ext4`/`debugfs`/`e2fsck`.

mod comun;

use std::io::Write;

use comun::{corre, ruta};
use winext4_nucleo::{ErrorFs, Ext4};

#[test]
fn escritura_en_sitio_deja_e2fsck_limpio_y_relee_lo_escrito() {
    if !comun::hay_oraculo() {
        eprintln!("SALTADA: faltan e2fsprogs");
        return;
    }
    let base = std::env::temp_dir().join(format!("winext4-w-{}", std::process::id()));
    let img = base.with_extension("img");
    let src = base.with_extension("src");

    const N: usize = 100_000;
    let original: Vec<u8> = (0..N).map(|i| (i % 251) as u8).collect();
    {
        let f = std::fs::File::create(&img).unwrap();
        f.set_len(16 * 1024 * 1024).unwrap();
    }
    std::fs::File::create(&src).unwrap().write_all(&original).unwrap();
    assert_eq!(corre("mkfs.ext4", &["-F", "-q", "-b", "4096", &ruta(&img)]), Some(true));
    let w = format!("write {} datos.bin", ruta(&src));
    assert_eq!(corre("debugfs", &["-w", "-R", &w, &ruta(&img)]), Some(true));

    // Dos parches: uno parcial dentro del bloque 0, otro que cruza bloques (lleno + cola).
    let parche_a = b"PARCHE-winext4-A!!!"; // 19 bytes en offset 1000 (bloque 0, parcial)
    let parche_b: Vec<u8> = (0..5000).map(|i| 200u8.wrapping_add(i as u8)).collect(); // offset 4096
    {
        let archivo = std::fs::OpenOptions::new().read(true).write(true).open(&img).unwrap();
        let mut fs = Ext4::abrir(archivo).expect("abrir ext4 R/W");
        let ino = fs.buscar("/datos.bin").expect("buscar datos.bin");

        fs.escribir_en_sitio(&ino, 1000, parche_a).expect("escribir parche A");
        fs.escribir_en_sitio(&ino, 4096, &parche_b).expect("escribir parche B");

        // NEGATIVO (D33): salirse del tamaño NO escribe, da NoCreceAun.
        let r = fs.escribir_en_sitio(&ino, (N - 2) as u64, b"XXXX");
        assert!(matches!(r, Err(ErrorFs::NoCreceAun { .. })), "crecer debe fallar, no escribir fuera");
    }

    // POSITIVO DE CONTROL (D30): e2fsck -fn debe dar el FS LIMPIO (salida 0) tras la escritura.
    if let Err(salida) = comun::fsck(&img) {
        panic!("e2fsck reportó problemas:\n{salida}");
    }

    // Relectura: el archivo = original con los dos parches aplicados, nada más cambiado.
    let mut esperado = original; // ya no se usa `original` después: se mueve
    esperado[1000..1000 + parche_a.len()].copy_from_slice(parche_a);
    esperado[4096..4096 + parche_b.len()].copy_from_slice(&parche_b);

    let archivo = std::fs::File::open(&img).unwrap();
    let mut fs = Ext4::abrir(archivo).unwrap();
    let ino = fs.buscar("/datos.bin").unwrap();
    assert_eq!(fs.leer_datos(&ino).unwrap(), esperado, "contenido = original + parches, exacto");

    let _ = std::fs::remove_file(&img);
    let _ = std::fs::remove_file(&src);
}
