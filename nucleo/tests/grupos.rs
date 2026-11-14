// SPDX-FileCopyrightText: 2026 Juan Carlos Isaza
// SPDX-License-Identifier: LicenseRef-Xiliux-Propietario
//! Grupos con bitmaps SIN INICIALIZAR (`BLOCK_UNINIT`/`INODE_UNINIT`), que es como `mkfs.ext4`
//! deja casi todos los grupos de un volumen nuevo. Antes winext4 los saltaba: un volumen
//! recién formateado se quedaba «sin inodos» al llenar el grupo 0 (cazado el 2026-11-05 por
//! la prueba del htree de 2 niveles) y «sin espacio» al llenar los grupos ya inicializados.
//! Ahora se inicializan como en el kernel. Oráculo: `e2fsck -fn` (que comprueba bitmaps,
//! relleno, contadores y sumas) y `debugfs` (contenido de vuelta, leído por otro).

mod comun;

use comun::{corre, ruta};
use winext4_nucleo::Ext4;

const BLOCK_UNINIT: u16 = 0x2;
const INODE_UNINIT: u16 = 0x1;

fn contar(fs: &mut Ext4<std::fs::File>, bandera: u16) -> usize {
    let ng = fs.sb.n_grupos();
    (0..ng).filter(|&g| fs.descriptor(g).unwrap().banderas() & bandera != 0).count()
}

#[test]
fn grupos_sin_inicializar_se_inicializan_al_usarlos() {
    // 1 KiB por bloque → 8 grupos de 8 MiB; pocos inodos por grupo para cruzar grupos pronto.
    usar_grupos_sin_inicializar("8g", &["-b", "1024", "-N", "2048"]);
}

#[test]
fn grupos_sin_inicializar_con_relleno_en_el_bitmap() {
    // 4096 bloques por grupo en bloques de 1 KiB: el bitmap ocupa medio bloque y la otra mitad
    // es RELLENO que debe ir a 1 (e2fsck: «padding at end of block bitmap is not set»).
    usar_grupos_sin_inicializar("16g", &["-b", "1024", "-g", "4096", "-N", "4096"]);
}

fn imagen(etiqueta: &str, opciones: &[&str]) -> Option<std::path::PathBuf> {
    if !comun::hay_oraculo() {
        eprintln!("SALTADA: faltan e2fsprogs");
        return None;
    }
    let img = std::env::temp_dir().join(format!("winext4-grupos-{etiqueta}-{}.img", std::process::id()));
    std::fs::File::create(&img).unwrap().set_len(64 * 1024 * 1024).unwrap();
    let r = ruta(&img);
    let mut a = vec!["-F", "-q"];
    a.extend_from_slice(opciones);
    a.push(&r);
    assert_eq!(corre("mkfs.ext4", &a), Some(true));
    Some(img)
}

fn usar_grupos_sin_inicializar(etiqueta: &str, opciones: &[&str]) {
    let Some(img) = imagen(etiqueta, opciones) else { return };
    let f = std::fs::OpenOptions::new().read(true).write(true).open(&img).unwrap();
    let mut fs = Ext4::abrir(f).unwrap();
    let (bu0, iu0) = (contar(&mut fs, BLOCK_UNINIT), contar(&mut fs, INODE_UNINIT));
    assert!(bu0 >= 1 && iu0 >= 3, "control D30: mkfs deja grupos sin inicializar ({bu0} de bloques, {iu0} de inodos)");

    fs.abrir_escritura().unwrap();
    // 600 archivos → 256 inodos por grupo: cruza al menos 2 grupos de inodos.
    let d = fs.crear_directorio(2, "muchos", 0o755).unwrap();
    for i in 0..600 {
        fs.crear_archivo(d, &format!("f{i}"), 0o644).unwrap_or_else(|e| panic!("crear f{i}: {e}"));
    }
    // 52 MiB de datos en un volumen de 64 MiB: no caben en los grupos ya inicializados.
    let grande = fs.crear_archivo(2, "grande.bin", 0o644).unwrap();
    let mut patron = vec![0u8; 1 << 20];
    for (k, b) in patron.iter_mut().enumerate() {
        *b = (k * 31 % 251) as u8;
    }
    for m in 0..52u64 {
        patron[0] = m as u8;
        fs.escribir(grande, m << 20, &patron).unwrap_or_else(|e| panic!("escribir MiB {m}: {e}"));
    }
    fs.cerrar_escritura().unwrap();
    let (bu1, iu1) = (contar(&mut fs, BLOCK_UNINIT), contar(&mut fs, INODE_UNINIT));
    assert!(bu1 < bu0 && iu1 < iu0, "se inicializaron grupos: bloques {bu0}→{bu1}, inodos {iu0}→{iu1}");
    drop(fs);

    if let Err(s) = comun::fsck(&img) {
        panic!("e2fsck tras usar grupos sin inicializar:\n{s}");
    }
    let leido = comun::volcar(&img, "/grande.bin");
    assert_eq!(leido.len(), 52 << 20);
    for m in 0..52usize {
        patron[0] = m as u8;
        assert!(leido[m << 20..(m + 1) << 20] == patron[..], "debugfs lee el MiB {m} tal cual");
    }
    eprintln!("grupos {etiqueta}: BLOCK_UNINIT {bu0}→{bu1}, INODE_UNINIT {iu0}→{iu1}; e2fsck limpio, 52 MiB de vuelta");
    let _ = std::fs::remove_file(&img);
}


/// Pareja de la guarda (D20/D33): si el descriptor de un grupo BLOCK_UNINIT dice tener un
/// bloque libre de menos que lo que se calcula, winext4 se NIEGA (no inventa el bitmap) y no
/// deja el volumen a medias.
#[test]
fn grupo_sin_inicializar_incoherente_se_niega() {
    let Some(img) = imagen("trampa", &["-b", "1024", "-N", "2048"]) else { return };
    let f = std::fs::OpenOptions::new().read(true).write(true).open(&img).unwrap();
    let mut fs = Ext4::abrir(f).unwrap();
    let ng = fs.sb.n_grupos();
    let g = (0..ng).find(|&g| fs.descriptor(g).unwrap().banderas() & BLOCK_UNINIT != 0).expect("control D30: hay un grupo BLOCK_UNINIT");
    let libres = fs.descriptor(g).unwrap().bloques_libres();
    drop(fs);
    let guion = format!("set_bg {g} free_blocks_count {}\nset_bg {g} checksum calc\n", libres - 1);
    let cmds = img.with_extension("cmds");
    std::fs::write(&cmds, guion).unwrap();
    assert_eq!(corre("debugfs", &["-w", "-f", &ruta(&cmds), &ruta(&img)]), Some(true));
    let _ = std::fs::remove_file(&cmds);

    let f = std::fs::OpenOptions::new().read(true).write(true).open(&img).unwrap();
    let mut fs = Ext4::abrir(f).unwrap();
    assert_eq!(fs.descriptor(g).unwrap().bloques_libres(), libres - 1, "control: la trampa quedó puesta");
    fs.abrir_escritura().unwrap();
    let n = fs.crear_archivo(2, "grande.bin", 0o644).unwrap();
    let mut error = None;
    for m in 0..60u64 {
        if let Err(e) = fs.escribir(n, m << 20, &vec![7u8; 1 << 20]) {
            error = Some(e.to_string());
            break;
        }
    }
    let e = error.expect("debía NEGARSE al llegar al grupo incoherente");
    assert!(e.contains("BLOCK_UNINIT"), "se niega por la guarda, no por otra cosa: {e}");
    assert_eq!(fs.descriptor(g).unwrap().banderas() & BLOCK_UNINIT, BLOCK_UNINIT, "el grupo sigue sin tocar");
    let _ = fs.cerrar_escritura();
    drop(fs);
    let _ = std::fs::remove_file(&img);
}