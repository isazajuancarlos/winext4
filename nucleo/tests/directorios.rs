// SPDX-FileCopyrightText: 2026 Juan Carlos Isaza
// SPDX-License-Identifier: LicenseRef-Xiliux-Propietario
//! F3, paso 5: directorios sobre ext4 real con `metadata_csum`.
//!
//! - Lineales: crear directorios y 200 archivos (el directorio crece de bloques), escribir
//!   contenido, borrar, borrar directorio vacío y renombrar (mismo padre y entre padres,
//!   archivos y directorios). Oráculo: `e2fsck -fn` limpio (enlaces, `..`, sumas, bitmaps) y
//!   `debugfs` ve los nombres y el contenido.
//! - Indexados (htree): directorio de 400 entradas indexado por `e2fsck -D` (e2fsprogs, no
//!   nosotros). (a) nuestro hash manda CADA nombre a la hoja donde e2fsprogs lo puso; con otra
//!   semilla, no (D33: la comprobación discrimina). (b) insertar 120 nombres nuevos deja
//!   `e2fsck -fn` limpio, y e2fsck SÍ verifica el orden de hash de cada hoja.

mod comun;

use std::sync::atomic::{AtomicU64, Ordering};

use comun::{corre, ruta};
use winext4_nucleo::{ErrorFs, Ext4};

static SEQ: AtomicU64 = AtomicU64::new(0);

fn imagen_vacia() -> Option<std::path::PathBuf> {
    if !comun::hay_oraculo() {
        return None;
    }
    let img = std::env::temp_dir().join(format!("winext4-dir-{}-{}.img", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed)));
    std::fs::File::create(&img).ok()?.set_len(32 * 1024 * 1024).ok()?;
    assert_eq!(corre("mkfs.ext4", &["-F", "-q", "-b", "4096", &ruta(&img)]), Some(true));
    Some(img)
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

/// Nombres que ve `debugfs ls -l` en `dir` (lector independiente).
fn ls_debugfs(img: &std::path::Path, dir: &str) -> Vec<String> {
    let o = comun::e2fs("debugfs").args(["-R", &format!("ls -p {dir}"), &ruta(img)]).output().unwrap();
    // `ls -p`: /ino/modo/uid/gid/nombre/tamaño/
    String::from_utf8_lossy(&o.stdout)
        .lines()
        // inodo 0 = entrada borrada que encabezaba su bloque (el nombre queda; debugfs la lista)
        .filter(|l| l.split('/').nth(1).is_some_and(|i| i != "0"))
        .filter_map(|l| l.split('/').nth(5).map(str::to_string))
        .filter(|n| !n.is_empty())
        .collect()
}

#[test]
fn directorios_lineales_crear_borrar_renombrar() {
    let Some(img) = imagen_vacia() else {
        eprintln!("SALTADA: faltan e2fsprogs");
        return;
    };
    {
        let mut fs = abrir_rw(&img);
        fs.abrir_escritura().unwrap();
        let raiz = 2;
        let docs = fs.crear_directorio(raiz, "docs", 0o755).unwrap();
        let otro = fs.crear_directorio(raiz, "otro", 0o755).unwrap();
        let vacio = fs.crear_directorio(docs, "vacío-ñ", 0o700).unwrap();
        let _ = vacio;
        for i in 0..200 {
            let n = fs.crear_archivo(docs, &format!("archivo-de-prueba-con-nombre-largo-{i:03}.txt"), 0o644).unwrap();
            if i % 20 == 1 {
                fs.escribir(n, 0, format!("contenido {i}\n").as_bytes()).unwrap();
            }
        }
        // Duplicado y nombre inválido: se rechazan.
        assert!(matches!(fs.crear_archivo(docs, "archivo-de-prueba-con-nombre-largo-000.txt", 0o644), Err(ErrorFs::YaExiste { .. })));
        assert!(matches!(fs.crear_archivo(docs, "a/b", 0o644), Err(ErrorFs::NombreInvalido)));
        // Borrar 50.
        for i in (0..200).step_by(4) {
            fs.borrar(docs, &format!("archivo-de-prueba-con-nombre-largo-{i:03}.txt")).unwrap();
        }
        // No se borra un directorio con cosas; sí uno vacío.
        assert!(matches!(fs.borrar_directorio(raiz, "docs"), Err(ErrorFs::DirectorioNoVacio)));
        fs.borrar_directorio(docs, "vacío-ñ").unwrap();
        // Renombrar en el mismo padre, mover a otro padre, y mover un directorio.
        // (los múltiplos de 4 ya se borraron arriba: se renombran los que siguen vivos)
        fs.renombrar(docs, "archivo-de-prueba-con-nombre-largo-021.txt", docs, "renombrado.txt").unwrap();
        fs.renombrar(docs, "archivo-de-prueba-con-nombre-largo-041.txt", otro, "movido.txt").unwrap();
        let sub = fs.crear_directorio(docs, "sub", 0o755).unwrap();
        fs.crear_archivo(sub, "dentro", 0o644).unwrap();
        fs.renombrar(docs, "sub", otro, "sub-movido").unwrap();
        // Reemplazar un destino existente.
        fs.crear_archivo(otro, "viejo", 0o644).unwrap();
        fs.renombrar(docs, "archivo-de-prueba-con-nombre-largo-043.txt", otro, "viejo").unwrap();
        // Meter un directorio dentro de sí mismo: se niega.
        assert!(fs.renombrar(raiz, "otro", sub, "bucle").is_err());
        fs.cerrar_escritura().unwrap();
    }
    limpio(&img, "directorios lineales");

    let docs = ls_debugfs(&img, "/docs");
    let n_archivos = docs.iter().filter(|n| n.starts_with("archivo-de-prueba")).count();
    assert_eq!(n_archivos, 200 - 50 - 3, "debugfs cuenta los que quedan en /docs");
    assert!(docs.contains(&"renombrado.txt".to_string()));
    assert!(!docs.contains(&"vacío-ñ".to_string()) && !docs.contains(&"sub".to_string()));
    let otro = ls_debugfs(&img, "/otro");
    for n in ["movido.txt", "sub-movido", "viejo"] {
        assert!(otro.contains(&n.to_string()), "falta {n} en /otro: {otro:?}");
    }
    assert!(ls_debugfs(&img, "/otro/sub-movido").contains(&"dentro".to_string()));
    assert_eq!(comun::volcar(&img, "/docs/renombrado.txt"), b"contenido 21\n");
    assert_eq!(comun::volcar(&img, "/otro/movido.txt"), b"contenido 41\n");
    let _ = std::fs::remove_file(&img);
}

#[test]
fn directorio_indexado_hash_e_insercion() {
    let Some(img) = imagen_vacia() else {
        eprintln!("SALTADA: faltan e2fsprogs");
        return;
    };
    // 400 entradas con debugfs y luego `e2fsck -fyD` para que E2FSPROGS construya el htree.
    let tiny = img.with_extension("tiny");
    let cmds = img.with_extension("cmds");
    std::fs::write(&tiny, b"x").unwrap();
    let mut g = String::from("mkdir /grande\n");
    for i in 0..400 {
        g.push_str(&format!("write {} /grande/entrada-existente-{i:04}\n", ruta(&tiny)));
    }
    std::fs::write(&cmds, g).unwrap();
    assert_eq!(corre("debugfs", &["-w", "-f", &ruta(&cmds), &ruta(&img)]), Some(true));
    let o = comun::e2fs("e2fsck").args(["-fyD", &ruta(&img)]).output().unwrap();
    assert!(matches!(o.status.code(), Some(0 | 1)), "e2fsck -D: {}", String::from_utf8_lossy(&o.stdout));
    let _ = std::fs::remove_file(&tiny);
    let _ = std::fs::remove_file(&cmds);
    limpio(&img, "htree recién construido por e2fsck -D");

    let mut fs = abrir_rw(&img);
    let dir = fs.buscar_numero("/grande").unwrap();
    let ino = fs.inodo(dir).unwrap();
    assert!(ino.flags & 0x1000 != 0, "control D30: /grande debe estar INDEXADO tras e2fsck -D");

    // (0) La suma del bloque ÍNDICE (dx_tail) que escribió e2fsck coincide con la nuestra.
    {
        use winext4_nucleo::csum;
        let b0 = fs.bloques_fisicos(&ino).unwrap()[0].unwrap();
        let raiz = fs.leer_bloque(b0).unwrap();
        let base = 0x18 + raiz[0x1D] as usize;
        let gen = u32::from_le_bytes(fs.inodo_crudo(dir).unwrap()[csum::INO_OFF_GEN..][..4].try_into().unwrap());
        let si = csum::semilla_inodo(fs.semilla(), dir, gen);
        let (cola, c) = csum::bloque_dx(si, &raiz, base);
        let guardada = u32::from_le_bytes(raiz[cola + 4..cola + 8].try_into().unwrap());
        assert_eq!(c, guardada, "suma dx_tail de la raíz del índice");
        let mut m = raiz;
        m[base + 8] ^= 1; // hash de la entrada 1
        assert_ne!(csum::bloque_dx(si, &m, base).1, guardada, "D33: índice mutado");
    }

    // (a) Dónde puso e2fsprogs cada nombre: escanear las hojas.
    let bloques: Vec<u64> = fs.bloques_fisicos(&ino).unwrap().into_iter().map(Option::unwrap).collect();
    let mut hoja_real = std::collections::HashMap::new();
    for (l, &b) in bloques.iter().enumerate() {
        let blk = fs.leer_bloque(b).unwrap();
        let mut i = 0usize;
        while i + 8 <= blk.len() {
            let eno = u32::from_le_bytes(blk[i..i + 4].try_into().unwrap());
            let rec = u16::from_le_bytes([blk[i + 4], blk[i + 5]]) as usize;
            if rec < 8 {
                break;
            }
            let n = blk[i + 6] as usize;
            if eno != 0 && n > 0 && i + 8 + n <= blk.len() {
                let nombre = String::from_utf8_lossy(&blk[i + 8..i + 8 + n]).into_owned();
                if nombre.starts_with("entrada-existente") {
                    hoja_real.insert(nombre, l);
                }
            }
            i += rec;
        }
    }
    assert_eq!(hoja_real.len(), 400, "control D30: las 400 entradas se vieron en las hojas");
    let hojas_distintas: std::collections::HashSet<_> = hoja_real.values().collect();
    assert!(hojas_distintas.len() >= 3, "control: el índice reparte en varias hojas ({})", hojas_distintas.len());
    let mut aciertos = 0;
    let mut aciertos_semilla_falsa = 0;
    for (nombre, &l) in &hoja_real {
        if fs.hoja_de(dir, nombre, None).unwrap() == l {
            aciertos += 1;
        }
        if fs.hoja_de(dir, nombre, Some([1, 2, 3, 4])).unwrap() == l {
            aciertos_semilla_falsa += 1;
        }
    }
    assert_eq!(aciertos, 400, "nuestro hash manda cada nombre a la hoja donde e2fsprogs lo puso");
    assert!(aciertos_semilla_falsa < 360, "D33: con otra semilla la ruta cambia ({aciertos_semilla_falsa}/400 coinciden)");

    // (b) Insertar 600 nombres nuevos (con acentos: el signo de los bytes importa) — llena
    // hojas y obliga a PARTIRLAS — y borrar 31. Ningún rechazo es aceptable.
    let hojas_antes = bloques.len();
    fs.abrir_escritura().unwrap();
    const NUEVOS: usize = 600;
    for i in 0..NUEVOS {
        fs.crear_archivo(dir, &format!("nuevo-é-con-nombre-más-largo-{i:03}"), 0o644)
            .unwrap_or_else(|e| panic!("crear nuevo {i}: {e}"));
    }
    for i in (0..400).step_by(13) {
        fs.borrar(dir, &format!("entrada-existente-{i:04}")).unwrap();
    }
    fs.cerrar_escritura().unwrap();

    // (c) Tras partir: CADA nombre vive en la hoja a la que el índice lo manda.
    let ino = fs.inodo(dir).unwrap();
    let bloques: Vec<u64> = fs.bloques_fisicos(&ino).unwrap().into_iter().map(Option::unwrap).collect();
    assert!(bloques.len() > hojas_antes, "control D30: hubo que partir hojas ({hojas_antes} → {})", bloques.len());
    let mut revisados = 0;
    for (l, &b) in bloques.iter().enumerate() {
        let blk = fs.leer_bloque(b).unwrap();
        let mut i = 0usize;
        while i + 8 <= blk.len() {
            let eno = u32::from_le_bytes(blk[i..i + 4].try_into().unwrap());
            let rec = u16::from_le_bytes([blk[i + 4], blk[i + 5]]) as usize;
            if rec < 8 {
                break;
            }
            let n = blk[i + 6] as usize;
            if eno != 0 && n > 0 && i + 8 + n <= blk.len() {
                let nombre = String::from_utf8_lossy(&blk[i + 8..i + 8 + n]).into_owned();
                if nombre.starts_with("nuevo-") || nombre.starts_with("entrada-") {
                    assert_eq!(fs.hoja_de(dir, &nombre, None).unwrap(), l, "{nombre} está en la hoja {l} pero el índice manda a otra");
                    revisados += 1;
                }
            }
            i += rec;
        }
    }
    assert_eq!(revisados, 400 - 31 + NUEVOS, "control: todos los nombres revisados");
    drop(fs);
    limpio(&img, "insertar (partiendo hojas) y borrar en directorio indexado");
    let vistos = ls_debugfs(&img, "/grande");
    let nuevos = vistos.iter().filter(|n| n.starts_with("nuevo-")).count();
    assert_eq!(nuevos, NUEVOS, "debugfs ve los insertados");
    eprintln!("htree: {nuevos} insertados, hojas {hojas_antes} → {}", bloques.len());
    let _ = std::fs::remove_file(&img);
}

/// Nombre largo (≈180 bytes): en bloques de 1 KiB caben 5 por hoja, así que unos miles
/// bastan para llenar la RAÍZ del índice (→ añadir nivel) y luego sus NODOS (→ partirlos).
fn largo(i: usize) -> String {
    format!("n{i:05}-{}", "x".repeat(172))
}

/// Cada nombre vivo de `dir` está en la hoja a la que el índice lo manda. Devuelve cuántos.
fn todos_en_su_hoja(fs: &mut Ext4<std::fs::File>, dir: u32) -> usize {
    let ino = fs.inodo(dir).unwrap();
    let bloques: Vec<u64> = fs.bloques_fisicos(&ino).unwrap().into_iter().map(Option::unwrap).collect();
    let mut revisados = 0;
    for (l, &b) in bloques.iter().enumerate() {
        let blk = fs.leer_bloque(b).unwrap();
        let mut i = 0usize;
        while i + 8 <= blk.len() {
            let eno = u32::from_le_bytes(blk[i..i + 4].try_into().unwrap());
            let rec = u16::from_le_bytes([blk[i + 4], blk[i + 5]]) as usize;
            if rec < 8 {
                break;
            }
            let n = blk[i + 6] as usize;
            if eno != 0 && n > 0 && i + 8 + n <= blk.len() {
                let nombre = String::from_utf8_lossy(&blk[i + 8..i + 8 + n]).into_owned();
                if nombre != "." && nombre != ".." {
                    assert_eq!(fs.hoja_de(dir, &nombre, None).unwrap(), l, "{nombre} en la hoja {l}, el índice manda a otra");
                    revisados += 1;
                }
            }
            i += rec;
        }
    }
    revisados
}

#[test]
fn indice_htree_anade_nivel_y_parte_nodos() {
    if !comun::hay_oraculo() {
        eprintln!("SALTADA: faltan e2fsprogs");
        return;
    }
    let img = std::env::temp_dir().join(format!("winext4-dx2-{}.img", std::process::id()));
    std::fs::File::create(&img).unwrap().set_len(64 * 1024 * 1024).unwrap();
    assert_eq!(corre("mkfs.ext4", &["-F", "-q", "-b", "1024", "-N", "16384", &ruta(&img)]), Some(true));
    // Directorio indexado por e2fsprogs (un nivel, raíz casi vacía).
    let tiny = img.with_extension("tiny");
    let cmds = img.with_extension("cmds");
    std::fs::write(&tiny, b"x").unwrap();
    let mut g = String::from("mkdir /grande\n");
    for i in 0..30 {
        g.push_str(&format!("write {} /grande/{}\n", ruta(&tiny), largo(i)));
    }
    std::fs::write(&cmds, g).unwrap();
    assert_eq!(corre("debugfs", &["-w", "-f", &ruta(&cmds), &ruta(&img)]), Some(true));
    let o = comun::e2fs("e2fsck").args(["-fyD", &ruta(&img)]).output().unwrap();
    assert!(matches!(o.status.code(), Some(0 | 1)), "e2fsck -D: {}", String::from_utf8_lossy(&o.stdout));
    let _ = std::fs::remove_file(&tiny);
    let _ = std::fs::remove_file(&cmds);
    limpio(&img, "htree de partida");

    let mut fs = abrir_rw(&img);
    let dir = fs.buscar_numero("/grande").unwrap();
    let ino = fs.inodo(dir).unwrap();
    assert!(ino.flags & 0x1000 != 0, "control D30: /grande INDEXADO");
    let b0 = fs.bloques_fisicos(&ino).unwrap()[0].unwrap();
    assert_eq!(fs.leer_bloque(b0).unwrap()[0x1E], 0, "control: el índice empieza con UN nivel");

    const N: usize = 3000;
    fs.abrir_escritura().unwrap();
    for i in 30..N {
        fs.crear_archivo(dir, &largo(i), 0o644).unwrap_or_else(|e| panic!("crear {i}: {e}"));
    }
    for i in (0..N).step_by(7) {
        fs.borrar(dir, &largo(i)).unwrap();
    }
    fs.cerrar_escritura().unwrap();

    // Control D30: el índice CRECIÓ un nivel y la raíz apunta a VARIOS nodos (se partieron).
    let raiz = fs.leer_bloque(b0).unwrap();
    let base = 0x18 + raiz[0x1D] as usize;
    let nodos = u16::from_le_bytes([raiz[base + 2], raiz[base + 3]]);
    assert_eq!(raiz[0x1E], 1, "se añadió un nivel al índice");
    assert!(nodos >= 3, "se partieron nodos índice: la raíz apunta a {nodos}");
    let vivos = N - N.div_ceil(7);
    assert_eq!(todos_en_su_hoja(&mut fs, dir), vivos, "cada nombre vivo, en la hoja que dicta el índice");
    drop(fs);
    limpio(&img, "añadir nivel y partir nodos htree");
    let vistos = ls_debugfs(&img, "/grande");
    assert_eq!(vistos.iter().filter(|n| n.starts_with('n')).count(), vivos, "debugfs ve todos");

    // El KERNEL busca cada nombre por el índice, y escribe encima de él.
    let lista = img.with_extension("lista");
    let nombres: Vec<String> = (0..N).filter(|i| i % 7 != 0).map(largo).collect();
    std::fs::write(&lista, nombres.join("\n") + "\n").unwrap();
    if comun::guion_root("htree-kernel.sh", &[&ruta(&img), "/grande", &ruta(&lista)]) {
        limpio(&img, "el kernel insertó y borró sobre nuestro índice");
    } else {
        panic!("el kernel no encontró todos los nombres por el índice (o registró un error de ext4)");
    }
    let _ = std::fs::remove_file(&lista);
    eprintln!("htree 2 niveles: {vivos} vivos, la raíz apunta a {nodos} nodos");
    let _ = std::fs::remove_file(&img);
}
