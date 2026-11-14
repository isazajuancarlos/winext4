// SPDX-FileCopyrightText: 2026 Juan Carlos Isaza
// SPDX-License-Identifier: LicenseRef-Xiliux-Propietario
//! Camino de LECTURA de punta a punta contra un ext4 **REAL** poblado con `debugfs` en espacio
//! de usuario (sin root): se escriben archivos de contenido conocido, y el motor debe listarlos
//! y leer sus bytes EXACTOS. El oráculo es `debugfs` (e2fsprogs), la herramienta canónica
//! (D28). Positivo de control (D30): el contenido leído == el contenido que escribimos. Negativo
//! por la misma vía (D33): una ruta inexistente da `NoEncontrado`, no un acierto silencioso.
//!
//! Se salta ruidosamente si faltan `mkfs.ext4`/`debugfs`.

mod comun;

use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};

use comun::{corre, ruta};

use winext4_nucleo::{ErrorFs, Ext4};

/// Sufijo único por imagen: los tests corren en paralelo en el MISMO proceso, así que el `pid`
/// solo no basta — dos helpers con el mismo `pid` chocarían de archivo temporal.
static SEQ: AtomicU64 = AtomicU64::new(0);
fn unico() -> String {
    format!("{}-{}", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed))
}

const TAM_IMG: u64 = 16 * 1024 * 1024;
const CONTENIDO_HOLA: &[u8] = b"hola winext4\n";

/// Construye un ext4 con `/hola.txt` y `/datos.bin` (10000 bytes) de contenido conocido, usando
/// `mkfs.ext4` + `debugfs -w` (todo en userspace). `None` si falta alguna herramienta.
fn imagen_poblada(datos_bin: &[u8]) -> Option<std::path::PathBuf> {
    let base = std::env::temp_dir().join(format!("winext4-lec-{}", unico()));
    let img = base.with_extension("img");
    let fhola = base.with_extension("hola");
    let fdatos = base.with_extension("datos");

    let f = std::fs::File::create(&img).ok()?;
    f.set_len(TAM_IMG).ok()?;
    drop(f);
    std::fs::File::create(&fhola).ok()?.write_all(CONTENIDO_HOLA).ok()?;
    std::fs::File::create(&fdatos).ok()?.write_all(datos_bin).ok()?;

    if !corre("mkfs.ext4", &["-F", "-q", "-b", "4096", &ruta(&img)])? {
        panic!("mkfs.ext4 falló");
    }
    let w1 = format!("write {} hola.txt", ruta(&fhola));
    let w2 = format!("write {} datos.bin", ruta(&fdatos));
    for guion in [&w1, &w2] {
        if !corre("debugfs", &["-w", "-R", guion, &ruta(&img)])? {
            panic!("debugfs write falló");
        }
    }
    let _ = std::fs::remove_file(&fhola);
    let _ = std::fs::remove_file(&fdatos);
    Some(img)
}

/// Imagen SIN extents (`-O ^extent`): fuerza el mapeo por bloques indirectos clásicos (ext2/3),
/// con un único archivo `/grande.bin` de `bytes` de contenido conocido.
fn imagen_clasica(nombre: &str, contenido: &[u8]) -> Option<std::path::PathBuf> {
    let base = std::env::temp_dir().join(format!("winext4-cla-{}", unico()));
    let img = base.with_extension("img");
    let fsrc = base.with_extension("src");
    let f = std::fs::File::create(&img).ok()?;
    f.set_len(TAM_IMG).ok()?;
    drop(f);
    std::fs::File::create(&fsrc).ok()?.write_all(contenido).ok()?;
    // ^extent exige también ^64bit (mke2fs: los extents son obligatorios en un FS de 64 bits).
    if !corre("mkfs.ext4", &["-F", "-q", "-b", "4096", "-O", "^extent,^64bit", &ruta(&img)])? {
        panic!("mkfs.ext4 ^extent,^64bit falló");
    }
    let w = format!("write {} {nombre}", ruta(&fsrc));
    if !corre("debugfs", &["-w", "-R", &w, &ruta(&img)])? {
        panic!("debugfs write falló");
    }
    let _ = std::fs::remove_file(&fsrc);
    Some(img)
}

/// Imagen ext4 con un directorio `/<dir>` que contiene `n` archivos `fNNNN`, creados en un solo
/// lote de `debugfs`. `None` si falta alguna herramienta.
fn imagen_con_dir_grande(dir: &str, n: usize) -> Option<std::path::PathBuf> {
    let base = std::env::temp_dir().join(format!("winext4-dir-{}", unico()));
    let img = base.with_extension("img");
    let tiny = base.with_extension("tiny");
    let cmds = base.with_extension("cmds");
    let f = std::fs::File::create(&img).ok()?;
    f.set_len(TAM_IMG).ok()?;
    drop(f);
    std::fs::File::create(&tiny).ok()?.write_all(b"x\n").ok()?;

    if !corre("mkfs.ext4", &["-F", "-q", "-b", "4096", &ruta(&img)])? {
        panic!("mkfs.ext4 falló");
    }
    let mut guion = format!("mkdir /{dir}\n");
    for i in 0..n {
        guion.push_str(&format!("write {} /{dir}/f{i:04}\n", ruta(&tiny)));
    }
    std::fs::write(&cmds, guion).ok()?;
    if !corre("debugfs", &["-w", "-f", &ruta(&cmds), &ruta(&img)])? {
        panic!("debugfs batch falló");
    }
    let _ = std::fs::remove_file(&tiny);
    let _ = std::fs::remove_file(&cmds);
    Some(img)
}

#[test]
fn lee_archivo_por_bloques_indirectos_clasicos() {
    // 5 MB con bloques de 4 KiB = ~1221 bloques. El indirecto simple cubre 12 + 1024 = 1036
    // bloques (~4,04 MB); pasarlo EXIGE el indirecto DOBLE. Así se ejerce directo + simple +
    // doble de `fisico_clasico`/`indirecto` contra un ext2 real.
    const N: usize = 5_000_000;
    let contenido: Vec<u8> = (0..N).map(|i| (i % 253) as u8).collect();
    let Some(img) = imagen_clasica("grande.bin", &contenido) else {
        eprintln!("SALTADA: faltan mkfs.ext4/debugfs (e2fsprogs)");
        return;
    };
    let archivo = std::fs::File::open(&img).expect("abrir imagen");
    let mut fs = Ext4::abrir(archivo).expect("abrir ext2");

    let ino = fs.buscar("/grande.bin").expect("buscar grande.bin");
    // Confirmar que de verdad ejercemos el camino CLÁSICO, no el de extents.
    assert_eq!(ino.flags & 0x8_0000, 0, "la imagen ^extent NO debe usar extents");
    assert_eq!(ino.tamano, N as u64);
    assert_eq!(fs.leer_datos(&ino).expect("leer grande.bin"), contenido, "5 MB exactos");

    let _ = std::fs::remove_file(&img);
}

#[test]
fn lee_destino_de_un_symlink_rapido() {
    let Some(img) = imagen_poblada(&[]) else {
        eprintln!("SALTADA: faltan mkfs.ext4/debugfs");
        return;
    };
    // Enlace rápido: destino < 60 bytes, embebido en i_block.
    const DESTINO: &str = "/un/destino/corto";
    let w = format!("symlink /ln {DESTINO}");
    assert_eq!(corre("debugfs", &["-w", "-R", &w, &ruta(&img)]), Some(true));

    let archivo = std::fs::File::open(&img).expect("abrir");
    let mut fs = Ext4::abrir(archivo).expect("abrir");
    let ino = fs.buscar("/ln").expect("buscar /ln");
    assert!(ino.es_enlace(), "el inodo debe ser symlink");
    assert_eq!(fs.leer_enlace(&ino).expect("leer destino"), DESTINO);
    // El negativo (D33): pedir el destino de un archivo normal NO da un acierto silencioso.
    let reg = fs.buscar("/hola.txt").expect("buscar hola.txt");
    assert!(matches!(fs.leer_enlace(&reg), Err(ErrorFs::NoEsEnlace)));

    let _ = std::fs::remove_file(&img);
}

#[test]
fn lista_un_directorio_grande_multibloque() {
    // 2000 entradas: el directorio ocupa varios bloques (mapeados por extents). Valida que
    // `listar` recorre TODOS los bloques, no solo el primero. (3000 destapó que `debugfs` deja
    // de escribir al llenar el dir lineal ~2803 — D30: contar lo REAL, no asumir lo pedido.)
    const N: usize = 2000;
    let Some(img) = imagen_con_dir_grande("big", N) else {
        eprintln!("SALTADA: faltan mkfs.ext4/debugfs");
        return;
    };
    let archivo = std::fs::File::open(&img).expect("abrir");
    let mut fs = Ext4::abrir(archivo).expect("abrir");
    let dir = fs.buscar("/big").expect("buscar /big");
    let nombres: std::collections::HashSet<String> =
        fs.listar(&dir).expect("listar /big").into_iter().map(|e| e.nombre).collect();
    // Todas presentes...
    for i in 0..N {
        assert!(nombres.contains(&format!("f{i:04}")), "falta f{i:04}");
    }
    // ...y ni una de más: exactamente N entradas 'fNNNN' (ni basura de bloques mal leídos).
    let fs_count = nombres.iter().filter(|n| n.starts_with('f') && n.len() == 5).count();
    assert_eq!(fs_count, N, "exactamente {N} entradas f, sin faltantes ni sobrantes");
    let _ = std::fs::remove_file(&img);
}

#[test]
fn lee_listado_y_contenido_de_un_ext4_real() {
    // datos.bin: 10000 bytes deterministas (no aleatorios, para que el fallo sea reproducible).
    let datos_bin: Vec<u8> = (0..10000u32).map(|i| (i % 251) as u8).collect();

    let Some(img) = imagen_poblada(&datos_bin) else {
        eprintln!("SALTADA: faltan mkfs.ext4/debugfs (e2fsprogs)");
        return;
    };

    let archivo = std::fs::File::open(&img).expect("abrir imagen");
    let mut fs = Ext4::abrir(archivo).expect("un ext4 real DEBE abrir");

    // (1) listar la raíz: deben estar nuestros dos archivos (+ lost+found que pone mkfs).
    let raiz = fs.raiz().expect("inodo raíz");
    let nombres: Vec<String> = fs.listar(&raiz).expect("listar /").into_iter().map(|e| e.nombre).collect();
    assert!(nombres.contains(&"hola.txt".to_string()), "falta hola.txt en {nombres:?}");
    assert!(nombres.contains(&"datos.bin".to_string()), "falta datos.bin en {nombres:?}");

    // (2) POSITIVO DE CONTROL (D30): el contenido leído == el que escribimos, byte a byte.
    let ino_hola = fs.buscar("/hola.txt").expect("buscar hola.txt");
    assert!(ino_hola.es_regular());
    assert_eq!(fs.leer_datos(&ino_hola).expect("leer hola.txt"), CONTENIDO_HOLA);

    let ino_datos = fs.buscar("/datos.bin").expect("buscar datos.bin");
    assert_eq!(ino_datos.tamano, 10000, "el tamaño del inodo");
    let leido = fs.leer_datos(&ino_datos).expect("leer datos.bin");
    assert_eq!(leido.len(), 10000);
    assert_eq!(leido, datos_bin, "los 10000 bytes deben coincidir exactos");

    // (3) NEGATIVO por la misma vía real (D33): algo que NO existe da NoEncontrado, no un acierto.
    assert!(matches!(fs.buscar("/no-existe.xyz"), Err(ErrorFs::NoEncontrado { .. })));

    let _ = std::fs::remove_file(&img);
}
