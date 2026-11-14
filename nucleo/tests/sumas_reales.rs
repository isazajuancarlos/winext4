// SPDX-FileCopyrightText: 2026 Juan Carlos Isaza
// SPDX-License-Identifier: LicenseRef-Xiliux-Propietario
//! F3, paso 1: las fórmulas de suma de `csum.rs` RECALCULAN exactamente lo que `mkfs.ext4` y
//! `debugfs` escribieron en una imagen REAL con `metadata_csum`: superbloque, TODOS los
//! descriptores de grupo, los bitmaps inicializados, inodos, bloques de directorio (cola) y
//! bloques de árbol de extents. Es el requisito de escribir: un metadato con la suma mal
//! calculada es un FS que `e2fsck` rechaza.
//!
//! Positivo de control (D30): cada estructura se cuenta y se exige un mínimo (no vale «0 de 0»).
//! Negativo por la vía real (D33): un byte mutado en cada estructura cambia la suma.

mod comun;

use std::io::Write;

use comun::{corre, ruta};
use winext4_nucleo::{csum, grupos, Ext4};

/// Imagen con: archivos sueltos, un directorio multibloque, y un archivo FRAGMENTADO (escrito
/// en los huecos que dejan archivos borrados) para forzar un árbol de extents de profundidad 1.
fn imagen() -> Option<std::path::PathBuf> {
    if !comun::hay_oraculo() {
        return None;
    }
    let base = std::env::temp_dir().join(format!("winext4-sum-{}", std::process::id()));
    let img = base.with_extension("img");
    let peq = base.with_extension("peq");
    let grande = base.with_extension("grande");
    let cmds = base.with_extension("cmds");
    std::fs::File::create(&img).ok()?.set_len(32 * 1024 * 1024).ok()?;
    std::fs::File::create(&peq).ok()?.write_all(&vec![7u8; 4096]).ok()?;
    std::fs::File::create(&grande).ok()?.write_all(&vec![9u8; 4096 * 60]).ok()?;
    assert_eq!(corre("mkfs.ext4", &["-F", "-q", "-b", "4096", &ruta(&img)]), Some(true));

    let mut g = String::from("mkdir /dir\n");
    for i in 0..300 {
        g.push_str(&format!("write {} /dir/archivo-con-nombre-largo-{i:04}\n", ruta(&peq)));
    }
    // 60 bloques sueltos, borrar uno de cada dos -> 30 huecos de 1 bloque.
    for i in 0..60 {
        g.push_str(&format!("write {} /h{i:02}\n", ruta(&peq)));
    }
    for i in (0..60).step_by(2) {
        g.push_str(&format!("rm /h{i:02}\n"));
    }
    g.push_str(&format!("write {} /fragmentado\n", ruta(&grande)));
    std::fs::write(&cmds, g).ok()?;
    assert_eq!(corre("debugfs", &["-w", "-f", &ruta(&cmds), &ruta(&img)]), Some(true));
    for p in [&peq, &grande, &cmds] {
        let _ = std::fs::remove_file(p);
    }
    Some(img)
}

#[test]
fn las_sumas_recalculadas_coinciden_con_las_de_un_ext4_real() {
    let Some(img) = imagen() else {
        eprintln!("SALTADA: faltan e2fsprogs");
        return;
    };
    // La imagen de partida es sana (si no, no mediríamos nada).
    comun::fsck(&img).expect("la imagen de partida debe pasar e2fsck");

    let mut fs = Ext4::abrir(std::fs::File::open(&img).unwrap()).unwrap();
    assert!(fs.sb.tiene_csum_metadatos(), "mkfs.ext4 moderno trae metadata_csum");
    let semilla = fs.semilla();

    // (1) Superbloque.
    let sb = fs.superbloque_crudo().to_vec();
    let guardada = u32::from_le_bytes(sb[csum::SB_OFF_CSUM..].try_into().unwrap());
    assert_eq!(csum::superbloque(&sb), guardada, "suma del superbloque");
    let mut m = sb;
    m[0x20] ^= 1;
    assert_ne!(csum::superbloque(&m), guardada, "D33: superbloque mutado");

    // (2) Descriptores y bitmaps de TODOS los grupos.
    let bpg = fs.sb.bloques_por_grupo as usize / 8;
    let ipg = fs.sb.inodos_por_grupo as usize / 8;
    let (mut n_desc, mut n_bb, mut n_ib) = (0, 0, 0);
    for g in 0..fs.sb.n_grupos() {
        let d = fs.descriptor(g).unwrap();
        assert_eq!(csum::descriptor(semilla, g as u32, &d.crudo), d.csum(), "descriptor del grupo {g}");
        n_desc += 1;
        if d.banderas() & grupos::BG_BLOCK_UNINIT == 0 {
            let bm = fs.leer_bloque(d.bitmap_bloques()).unwrap();
            assert_eq!(d.recorta(csum::bitmap(semilla, &bm, bpg)), d.csum_bitmap_bloques(), "bitmap de bloques del grupo {g}");
            n_bb += 1;
        }
        if d.banderas() & grupos::BG_INODE_UNINIT == 0 {
            let bm = fs.leer_bloque(d.bitmap_inodos()).unwrap();
            assert_eq!(d.recorta(csum::bitmap(semilla, &bm, ipg)), d.csum_bitmap_inodos(), "bitmap de inodos del grupo {g}");
            n_ib += 1;
        }
    }
    assert!(n_desc >= 1 && n_bb >= 1 && n_ib >= 1, "control D30: {n_desc} desc, {n_bb} bb, {n_ib} ib");
    let mut d0 = fs.descriptor(0).unwrap();
    let bueno = d0.csum();
    d0.crudo[0x0C] ^= 1;
    assert_ne!(csum::descriptor(semilla, 0, &d0.crudo), bueno, "D33: descriptor mutado");

    // (3) Inodos: raíz, directorio, archivo fragmentado, y unos cuantos archivos.
    let mut inos = vec![2u32];
    for r in ["/dir", "/fragmentado", "/h01", "/dir/archivo-con-nombre-largo-0000", "/dir/archivo-con-nombre-largo-0299"] {
        inos.push(fs.buscar_numero(r).unwrap());
    }
    for &n in &inos {
        let crudo = fs.inodo_crudo(n).unwrap();
        assert!(csum::inodo_tiene_hi(&crudo), "inodo de 256 con extra_isize: suma de 32 bits");
        assert_eq!(csum::inodo(semilla, n, &crudo), csum::inodo_guardada(&crudo), "inodo {n}");
        let mut m = crudo.clone();
        m[0x10] ^= 1;
        assert_ne!(csum::inodo(semilla, n, &m), csum::inodo_guardada(&crudo), "D33: inodo {n} mutado");
    }

    // (4) Bloques de directorio (cola de suma) de / y de /dir (multibloque).
    let mut n_dir = 0;
    for r in ["/", "/dir"] {
        let n = fs.buscar_numero(r).unwrap();
        let gen = u32::from_le_bytes(fs.inodo_crudo(n).unwrap()[csum::INO_OFF_GEN..][..4].try_into().unwrap());
        let si = csum::semilla_inodo(semilla, n, gen);
        let ino = fs.inodo(n).unwrap();
        for b in fs.bloques_fisicos(&ino).unwrap().into_iter().flatten() {
            let blk = fs.leer_bloque(b).unwrap();
            assert!(csum::dir_tiene_cola(&blk), "bloque {b} de {r} sin cola de suma");
            let guardada = u32::from_le_bytes(blk[blk.len() - 4..].try_into().unwrap());
            assert_eq!(csum::bloque_dir(si, &blk), guardada, "bloque de directorio {b} de {r}");
            n_dir += 1;
        }
    }
    assert!(n_dir >= 3, "control D30: /dir debe ocupar varios bloques ({n_dir} bloques vistos)");

    // (5) Árbol de extents de profundidad ≥ 1 en el archivo fragmentado.
    let n = fs.buscar_numero("/fragmentado").unwrap();
    let ino = fs.inodo(n).unwrap();
    let nodos = fs.bloques_de_arbol(&ino).unwrap();
    assert!(!nodos.is_empty(), "control D30: el archivo fragmentado debe tener árbol de profundidad ≥ 1");
    let gen = u32::from_le_bytes(fs.inodo_crudo(n).unwrap()[csum::INO_OFF_GEN..][..4].try_into().unwrap());
    let si = csum::semilla_inodo(semilla, n, gen);
    for b in nodos {
        let blk = fs.leer_bloque(b).unwrap();
        let off = csum::extent_off_cola(&blk);
        let guardada = u32::from_le_bytes(blk[off..off + 4].try_into().unwrap());
        assert_eq!(csum::bloque_extents(si, &blk), guardada, "bloque de extents {b}");
        let mut m = blk.clone();
        m[12] ^= 1;
        assert_ne!(csum::bloque_extents(si, &m), guardada, "D33: bloque de extents mutado");
    }
    // Y el contenido del fragmentado se lee entero (el árbol de profundidad 1 funciona).
    assert_eq!(fs.leer_datos(&ino).unwrap(), vec![9u8; 4096 * 60]);

    let _ = std::fs::remove_file(&img);
}
