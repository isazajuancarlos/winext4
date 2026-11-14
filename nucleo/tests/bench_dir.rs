// SPDX-FileCopyrightText: 2026 Juan Carlos Isaza
// SPDX-License-Identifier: LicenseRef-Xiliux-Propietario
//! Banco del MOTOR solo (sin Dokan ni Windows): crear N archivos en una carpeta, en tandas, para
//! ver si el coste por archivo crece con el tamaño de la carpeta. Ignorado por defecto:
//! `cargo test --release --test bench_dir -- --ignored --nocapture`.

use std::io::Cursor;
use std::time::Instant;

use winext4_nucleo::{Ext4, INODO_RAIZ};

#[test]
#[ignore = "banco de velocidad, no prueba"]
fn crear_muchos_en_una_carpeta() {
    let mut img = Cursor::new(vec![0u8; 256 << 20]);
    winext4_nucleo::formatear::formatear(&mut img, 256 << 20, "BENCH").unwrap();
    let mut fs = Ext4::abrir(img).unwrap();
    fs.abrir_escritura().unwrap();
    let d = fs.crear_directorio(INODO_RAIZ, "carpeta", 0o755).unwrap();
    let (total, tanda) = (3000, 500);
    for t0 in (0..total).step_by(tanda) {
        let t = Instant::now();
        for i in t0..t0 + tanda {
            let n = fs.crear_archivo(d, &format!("archivo-{i}.txt"), 0o644).unwrap();
            fs.escribir(n, 0, b"x").unwrap();
        }
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        eprintln!("{t0:>5}..{:>5}: {ms:>8.1} ms ({:.3} ms/archivo)", t0 + tanda, ms / tanda as f64);
    }
    let t = Instant::now();
    for i in 0..total {
        assert!(fs.buscar_en(d, &format!("archivo-{i}.txt")).unwrap().is_some());
    }
    eprintln!("buscar {total} nombres: {:.1} ms", t.elapsed().as_secs_f64() * 1000.0);
    fs.cerrar_escritura().unwrap();
}
