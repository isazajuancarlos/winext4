// SPDX-FileCopyrightText: 2026 Juan Carlos Isaza
// SPDX-License-Identifier: LicenseRef-Xiliux-Propietario
//! Prueba del lector de superbloque contra un ext4 **REAL** fabricado por `mkfs.ext4`
//! (e2fsprogs). Es la frontera del programa (D28): no se parsea un búfer inventado sino lo
//! que escribe la herramienta canónica. El positivo de control (D30) es que una imagen recién
//! formateada, con etiqueta y tamaño de bloque que NOSOTROS fijamos, se lee y coincide; el
//! negativo (D33, misma vía real) es que otra región de ESA MISMA imagen —el bloque de
//! arranque— se rechaza por magia inválida.
//!
//! Si `mkfs.ext4` no está en el sistema, la prueba se SALTA ruidosamente (no pasa en falso).

mod comun;

use std::io::Read;

use winext4_nucleo::Superbloque;

const ETIQUETA: &str = "WINEXT4RO";
const TAM_BLOQUE: u32 = 4096;
const TAM_IMG: u64 = 16 * 1024 * 1024; // 16 MiB

/// Crea una imagen ext4 real o devuelve `None` si `mkfs.ext4` no existe (D30: decir por qué se calla).
fn imagen_ext4() -> Option<std::path::PathBuf> {
    if !comun::hay("mkfs.ext4") {
        return None;
    }
    let ruta = std::env::temp_dir().join(format!("winext4-prueba-{}.img", std::process::id()));
    let f = std::fs::File::create(&ruta).expect("crear archivo de imagen");
    f.set_len(TAM_IMG).expect("dimensionar la imagen");
    drop(f);

    let salida = comun::e2fs("mkfs.ext4")
        .args(["-F", "-q", "-L", ETIQUETA, "-b", &TAM_BLOQUE.to_string()])
        .arg(comun::ruta(&ruta))
        .output();

    match salida {
        Ok(o) if o.status.success() => Some(ruta),
        Ok(o) => {
            let _ = std::fs::remove_file(&ruta);
            panic!("mkfs.ext4 falló: {}", String::from_utf8_lossy(&o.stderr));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let _ = std::fs::remove_file(&ruta);
            None
        }
        Err(e) => panic!("no se pudo ejecutar mkfs.ext4: {e}"),
    }
}

fn leer_region(ruta: &std::path::Path, desde: u64, largo: usize) -> Vec<u8> {
    use std::io::Seek;
    let mut f = std::fs::File::open(ruta).expect("abrir imagen");
    f.seek(std::io::SeekFrom::Start(desde)).expect("buscar");
    let mut buf = vec![0u8; largo];
    f.read_exact(&mut buf).expect("leer región");
    buf
}

#[test]
fn lee_el_superbloque_de_un_ext4_real() {
    let Some(ruta) = imagen_ext4() else {
        eprintln!("SALTADA: mkfs.ext4 no está instalado (e2fsprogs)");
        return;
    };

    // POSITIVO DE CONTROL (D30): el superbloque real, en el byte 1024.
    let sb_bytes = leer_region(&ruta, winext4_nucleo::superbloque::DESPLAZAMIENTO as u64, 1024);
    let sb = Superbloque::leer(&sb_bytes).expect("un ext4 real DEBE leerse");

    assert_eq!(sb.etiqueta, ETIQUETA, "la etiqueta que pusimos en mkfs");
    assert_eq!(sb.tamano_bloque, TAM_BLOQUE, "el -b que pasamos a mkfs");
    assert!(sb.usa_extents(), "mkfs.ext4 moderno activa EXTENTS por defecto");
    assert_eq!(sb.tamano_bytes(), TAM_IMG, "bloques*tamaño == tamaño de la imagen");
    assert!(sb.inodos > 0 && sb.inodos_por_grupo > 0, "recuentos no nulos");

    // NEGATIVO por la MISMA vía real (D33): el bloque de arranque (byte 0) de ESTA imagen
    // no es un superbloque; `leer` debe rechazarlo por magia, no aceptarlo.
    let arranque = leer_region(&ruta, 0, 1024);
    assert!(
        matches!(
            Superbloque::leer(&arranque),
            Err(winext4_nucleo::ErrorSuperbloque::MagiaInvalida { .. })
        ),
        "el bloque de arranque NO es superbloque y debe rechazarse",
    );

    let _ = std::fs::remove_file(&ruta);
}
