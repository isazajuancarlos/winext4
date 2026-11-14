// SPDX-FileCopyrightText: 2026 Juan Carlos Isaza
// SPDX-License-Identifier: LicenseRef-Xiliux-Propietario
//! F3, pasos 2–4: escritura CON ASIGNACIÓN sobre un ext4 real con `metadata_csum`:
//! crecer, fragmentar hasta profundidad 1 del árbol de extents, y truncar (liberar).
//!
//! Oráculos (D30): `e2fsck -fn` LIMPIO tras cada operación (bitmaps, contadores, sumas,
//! `i_blocks`, árbol) y el contenido VOLCADO por `debugfs` —lector independiente— igual al
//! esperado. Controles (D33): `e2fsck` SÍ detecta una corrupción plantada (si no, su verde no
//! diría nada), y `abrir_escritura` rechaza un volumen sucio o con la suma alterada.

mod comun;

use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};

use comun::{corre, ruta};
use winext4_nucleo::{ErrorFs, Ext4};

static SEQ: AtomicU64 = AtomicU64::new(0);

/// Imagen con `/datos.bin` (`n` bytes de patrón conocido).
fn imagen(n: usize) -> Option<(std::path::PathBuf, Vec<u8>)> {
    if !comun::hay_oraculo() {
        return None;
    }
    let base = std::env::temp_dir().join(format!("winext4-crec-{}-{}", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed)));
    let img = base.with_extension("img");
    let src = base.with_extension("src");
    let contenido: Vec<u8> = (0..n).map(|i| (i % 251) as u8).collect();
    std::fs::File::create(&img).ok()?.set_len(32 * 1024 * 1024).ok()?;
    std::fs::File::create(&src).ok()?.write_all(&contenido).ok()?;
    assert_eq!(corre("mkfs.ext4", &["-F", "-q", "-b", "4096", &ruta(&img)]), Some(true));
    let w = format!("write {} datos.bin", ruta(&src));
    assert_eq!(corre("debugfs", &["-w", "-R", &w, &ruta(&img)]), Some(true));
    let _ = std::fs::remove_file(&src);
    Some((img, contenido))
}

fn abrir_rw(img: &std::path::Path) -> Ext4<std::fs::File> {
    let f = std::fs::OpenOptions::new().read(true).write(true).open(img).unwrap();
    Ext4::abrir(f).unwrap()
}

fn limpio(img: &std::path::Path, paso: &str) {
    if let Err(s) = comun::fsck(img) {
        panic!("e2fsck tras «{paso}»:\n{s}");
    }
}

#[test]
fn crecer_fragmentar_y_truncar_dejan_e2fsck_limpio() {
    let Some((img, mut esperado)) = imagen(100_000) else {
        eprintln!("SALTADA: faltan e2fsprogs");
        return;
    };
    let bs = 4096usize;

    // (1) CRECER: 300 KB más allá del final, contiguos.
    let cola: Vec<u8> = (0..300_000).map(|i| (i % 241) as u8 ^ 0x5A).collect();
    {
        let mut fs = abrir_rw(&img);
        fs.abrir_escritura().unwrap();
        let n = fs.buscar_numero("/datos.bin").unwrap();
        fs.escribir(n, esperado.len() as u64, &cola).unwrap();
        fs.cerrar_escritura().unwrap();
    }
    esperado.extend_from_slice(&cola);
    limpio(&img, "crecer");
    assert_eq!(comun::volcar(&img, "/datos.bin"), esperado, "debugfs lee lo que escribimos al crecer");

    // (2) FRAGMENTAR: 12 bloques sueltos con huecos entre medias, lejos del final → 12 extents
    // nuevos no contiguos en lo lógico → la raíz (4) desborda → árbol de profundidad 1.
    let base_l = esperado.len().div_ceil(bs) + 10;
    {
        let mut fs = abrir_rw(&img);
        fs.abrir_escritura().unwrap();
        let n = fs.buscar_numero("/datos.bin").unwrap();
        for k in 0..12usize {
            let off = (base_l + 2 * k) * bs;
            fs.escribir(n, off as u64, &vec![0xC0 + k as u8; bs]).unwrap();
        }
        fs.cerrar_escritura().unwrap();
        let ino = fs.inodo(n).unwrap();
        assert!(!fs.bloques_de_arbol(&ino).unwrap().is_empty(), "control D30: debe haber subido a profundidad 1");
    }
    for k in 0..12usize {
        let off = (base_l + 2 * k) * bs;
        esperado.resize(off, 0); // los huecos se leen como ceros
        esperado.extend(std::iter::repeat_n(0xC0 + k as u8, bs));
    }
    limpio(&img, "fragmentar (profundidad 1)");
    assert_eq!(comun::volcar(&img, "/datos.bin"), esperado, "debugfs lee el archivo disperso de profundidad 1");

    // (3) TRUNCAR a 50 000 bytes: libera casi todo y el árbol vuelve a caber en el inodo.
    {
        let mut fs = abrir_rw(&img);
        fs.abrir_escritura().unwrap();
        let n = fs.buscar_numero("/datos.bin").unwrap();
        fs.truncar(n, 50_000).unwrap();
        fs.cerrar_escritura().unwrap();
        let ino = fs.inodo(n).unwrap();
        assert!(fs.bloques_de_arbol(&ino).unwrap().is_empty(), "tras truncar, profundidad 0 otra vez");
    }
    esperado.truncate(50_000);
    limpio(&img, "truncar");
    assert_eq!(comun::volcar(&img, "/datos.bin"), esperado);

    // (4) Crecer por truncar + escribir detrás: la cola del bloque cortado debe leerse a CERO,
    // no con lo que había antes de truncar.
    {
        let mut fs = abrir_rw(&img);
        fs.abrir_escritura().unwrap();
        let n = fs.buscar_numero("/datos.bin").unwrap();
        fs.escribir(n, 60_000, b"fin").unwrap();
        fs.cerrar_escritura().unwrap();
    }
    esperado.resize(60_000, 0);
    esperado.extend_from_slice(b"fin");
    limpio(&img, "crecer tras truncar");
    assert_eq!(comun::volcar(&img, "/datos.bin"), esperado, "lo truncado no reaparece");

    let _ = std::fs::remove_file(&img);
}

#[test]
fn arbol_de_extents_de_profundidad_2_y_vuelta_a_0() {
    // 1500 fragmentos (un bloque cada dos): más de los 1360 que caben en profundidad 1.
    let Some((img, _)) = imagen(4096) else {
        eprintln!("SALTADA: faltan e2fsprogs");
        return;
    };
    let bs = 4096usize;
    const N: usize = 1500;
    let n_ino;
    {
        let mut fs = abrir_rw(&img);
        fs.abrir_escritura().unwrap();
        n_ino = fs.buscar_numero("/datos.bin").unwrap();
        for k in 0..N {
            fs.escribir(n_ino, ((10 + 2 * k) * bs) as u64, &vec![(k % 250) as u8 + 1; bs]).unwrap();
        }
        fs.cerrar_escritura().unwrap();
        let raiz = fs.inodo_crudo(n_ino).unwrap();
        let prof = u16::from_le_bytes([raiz[0x28 + 6], raiz[0x28 + 7]]);
        assert_eq!(prof, 2, "control D30: con {N} fragmentos el árbol debe tener profundidad 2");
    }
    limpio(&img, "árbol de profundidad 2");
    let mut esperado: Vec<u8> = (0..4096usize).map(|i| (i % 251) as u8).collect();
    for k in 0..N {
        esperado.resize((10 + 2 * k) * bs, 0);
        esperado.extend(std::iter::repeat_n((k % 250) as u8 + 1, bs));
    }
    assert_eq!(comun::volcar(&img, "/datos.bin"), esperado, "debugfs lee el archivo de profundidad 2");
    // Truncar: el árbol vuelve a caber en el inodo y TODOS sus bloques se liberan.
    {
        let mut fs = abrir_rw(&img);
        fs.abrir_escritura().unwrap();
        // 11 bloques: quedan el 0 (original) y el fragmento del 10 → 2 extents, caben en el inodo.
        fs.truncar(n_ino, 11 * bs as u64).unwrap();
        fs.cerrar_escritura().unwrap();
        let ino = fs.inodo(n_ino).unwrap();
        assert!(fs.bloques_de_arbol(&ino).unwrap().is_empty(), "tras truncar, profundidad 0");
    }
    limpio(&img, "truncar desde profundidad 2");
    let _ = std::fs::remove_file(&img);
}

#[test]
fn lista_blanca_rechaza_quota_y_xattr_en_bloque_se_cuenta() {
    // (a) Pareja de la lista blanca: con `quota` (escribir sin actualizar cuotas corrompe) se
    // NIEGA; el mismo mkfs sin quota se acepta (lo prueban las demás pruebas).
    if !comun::hay_oraculo() {
        eprintln!("SALTADA: faltan e2fsprogs");
        return;
    }
    let base = std::env::temp_dir().join(format!("winext4-lb-{}", std::process::id()));
    let img_q = base.with_extension("quota.img");
    std::fs::File::create(&img_q).unwrap().set_len(32 * 1024 * 1024).unwrap();
    assert_eq!(corre("mkfs.ext4", &["-F", "-q", "-b", "4096", "-O", "quota", &ruta(&img_q)]), Some(true));
    let mut fs = abrir_rw(&img_q);
    match fs.abrir_escritura() {
        Err(ErrorFs::NoSoportado { que }) => assert!(que.contains("quota"), "{que}"),
        otro => panic!("con quota debía negarse, fue {otro:?}"),
    }
    drop(fs);
    let _ = std::fs::remove_file(&img_q);

    // (b) Un archivo con BLOQUE de atributos extendidos (valor de 3000 bytes: no cabe en el
    // inodo): crecerlo debe dejar `i_blocks` contando ese bloque (e2fsck limpio), y borrarlo se
    // NIEGA (el bloque puede estar compartido).
    let Some((img, _)) = imagen(10_000) else { return };
    let valor = img.with_extension("val");
    std::fs::write(&valor, vec![b'v'; 3000]).unwrap();
    let g = format!("ea_set -f {} /datos.bin user.grande", ruta(&valor));
    assert_eq!(corre("debugfs", &["-w", "-R", &g, &ruta(&img)]), Some(true));
    let _ = std::fs::remove_file(&valor);
    limpio(&img, "ea_set");
    {
        let mut fs = abrir_rw(&img);
        let n = fs.buscar_numero("/datos.bin").unwrap();
        let crudo = fs.inodo_crudo(n).unwrap();
        let acl = u32::from_le_bytes(crudo[0x68..0x6C].try_into().unwrap());
        assert_ne!(acl, 0, "control D30: el atributo debe haber ido a un BLOQUE aparte");
        fs.abrir_escritura().unwrap();
        fs.escribir(n, 10_000, &vec![7u8; 50_000]).unwrap();
        fs.cerrar_escritura().unwrap();
    }
    limpio(&img, "crecer un archivo con bloque xattr");
    // Borrarlo: su bloque de atributos (único dueño) se libera con él.
    {
        let mut fs = abrir_rw(&img);
        fs.abrir_escritura().unwrap();
        fs.borrar(2, "datos.bin").unwrap();
        fs.cerrar_escritura().unwrap();
    }
    limpio(&img, "borrar un archivo con bloque xattr");
    let _ = std::fs::remove_file(&img);
}

#[test]
fn borrar_archivos_que_comparten_bloque_de_atributos_respeta_el_recuento() {
    if !comun::hay_oraculo() {
        eprintln!("SALTADA: faltan e2fsprogs");
        return;
    }
    let img = std::env::temp_dir().join(format!("winext4-xcomp-{}.img", std::process::id()));
    if !comun::guion_root("xattr-compartido.sh", &[&ruta(&img)]) {
        eprintln!("SALTADA: no se pudo fabricar el xattr compartido con el kernel");
        return;
    }
    let (acl, refs) = {
        let mut fs = abrir_rw(&img);
        let crudos: Vec<Vec<u8>> = ["/uno", "/dos", "/tres"].iter().map(|r| { let n = fs.buscar_numero(r).unwrap(); fs.inodo_crudo(n).unwrap() }).collect();
        let acls: Vec<u32> = crudos.iter().map(|c| u32::from_le_bytes(c[0x68..0x6C].try_into().unwrap())).collect();
        assert!(acls[0] != 0 && acls.iter().all(|&a| a == acls[0]), "control D30: el kernel debe haberlos COMPARTIDO: {acls:?}");
        let x = fs.leer_bloque(u64::from(acls[0])).unwrap();
        (u64::from(acls[0]), u32::from_le_bytes(x[4..8].try_into().unwrap()))
    };
    assert_eq!(refs, 3, "h_refcount del bloque compartido");
    limpio(&img, "fabricada por el kernel");
    let refs_tras = |img: &std::path::Path| -> u32 {
        let x = abrir_rw(img).leer_bloque(acl).unwrap();
        u32::from_le_bytes(x[4..8].try_into().unwrap())
    };
    for (k, nombre) in ["uno", "dos"].iter().enumerate() {
        let mut fs = abrir_rw(&img);
        fs.abrir_escritura().unwrap();
        fs.borrar(2, nombre).unwrap();
        fs.cerrar_escritura().unwrap();
        drop(fs);
        limpio(&img, &format!("borrar {nombre} (compartido)"));
        assert_eq!(refs_tras(&img), 2 - k as u32, "el recuento baja de uno en uno");
    }
    // El último dueño: el bloque se libera (e2fsck lo comprueba en el bitmap).
    {
        let mut fs = abrir_rw(&img);
        fs.abrir_escritura().unwrap();
        fs.borrar(2, "tres").unwrap();
        fs.cerrar_escritura().unwrap();
    }
    limpio(&img, "borrar el último dueño del bloque de atributos");
    let _ = std::fs::remove_file(&img);
}

#[test]
fn e2fsck_detecta_una_corrupcion_plantada() {
    // CONTROL del oráculo (D33): si e2fsck no pusiera rojo esto, su «limpio» no mediría nada.
    let Some((img, _)) = imagen(10_000) else {
        eprintln!("SALTADA: faltan e2fsprogs");
        return;
    };
    {
        let mut fs = abrir_rw(&img);
        let n = fs.buscar_numero("/datos.bin").unwrap();
        // Corromper el tamaño del inodo SIN rehacer su suma.
        let mut crudo = fs.inodo_crudo(n).unwrap();
        crudo[0x04] ^= 0x40;
        let ino_off = {
            // reescribimos el inodo a mano, a propósito sin sellar
            let fs2 = &mut fs;
            fs2.descriptor(0).unwrap().tabla_inodos() * 4096 + (n as u64 - 1) * fs2.sb.tamano_inodo as u64
        };
        drop(fs);
        let mut f = std::fs::OpenOptions::new().write(true).open(&img).unwrap();
        use std::io::{Seek, SeekFrom};
        f.seek(SeekFrom::Start(ino_off)).unwrap();
        f.write_all(&crudo).unwrap();
    }
    assert!(comun::fsck(&img).is_err(), "e2fsck DEBE detectar el inodo con la suma rota");
    let _ = std::fs::remove_file(&img);
}

#[test]
fn abrir_escritura_rechaza_volumen_sucio_o_con_suma_alterada() {
    let Some((img, _)) = imagen(10_000) else {
        eprintln!("SALTADA: faltan e2fsprogs");
        return;
    };
    // Sucio: abrir y NO cerrar deja la marca de «no limpio» → la siguiente apertura se niega.
    {
        let mut fs = abrir_rw(&img);
        fs.abrir_escritura().unwrap();
    }
    let mut fs = abrir_rw(&img);
    assert!(matches!(fs.abrir_escritura(), Err(ErrorFs::NoSoportado { .. })), "volumen sucio: no se escribe");
    drop(fs);

    // Suma alterada: un byte del superbloque cambiado sin rehacer la suma → se niega.
    let Some((img2, _)) = imagen(10_000) else { return };
    {
        use std::io::{Seek, SeekFrom};
        let mut f = std::fs::OpenOptions::new().read(true).write(true).open(&img2).unwrap();
        f.seek(SeekFrom::Start(1024 + 0x78)).unwrap(); // etiqueta del volumen
        f.write_all(b"X").unwrap();
    }
    let mut fs = abrir_rw(&img2);
    assert!(matches!(fs.abrir_escritura(), Err(ErrorFs::SumaInvalida { .. })), "suma alterada: no se escribe");
    for p in [&img, &img2] {
        let _ = std::fs::remove_file(p);
    }
}
