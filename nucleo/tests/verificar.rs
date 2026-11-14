// SPDX-FileCopyrightText: 2026 Juan Carlos Isaza
// SPDX-License-Identifier: LicenseRef-Xiliux-Propietario
//! B1: `verificar` (el «winext4fsck» de solo lectura), por PAREJAS con `e2fsck -fn`:
//! - imágenes SANAS de cuatro orígenes (mkfs, escrita por winext4 con htree partido, journal
//!   del kernel recuperado, ext2 clásica) → cero errores, igual que e2fsck;
//! - siete corrupciones PLANTADAS con sus sumas REHECHAS (el error es semántico, no una suma
//!   rota) → nuestro verificador nombra la clase que toca Y e2fsck también la marca.

mod comun;

use std::io::{Read, Seek, SeekFrom, Write};

use comun::{corre, ruta};
use winext4_nucleo::verificar::InformeVerificacion;
use winext4_nucleo::{csum, Ext4};

fn nueva(nombre: &str, opciones: &[&str]) -> Option<std::path::PathBuf> {
    if !comun::hay_oraculo() {
        return None;
    }
    let img = std::env::temp_dir().join(format!("winext4-ver-{nombre}-{}.img", std::process::id()));
    std::fs::File::create(&img).ok()?.set_len(32 * 1024 * 1024).ok()?;
    let mut a = vec!["-F", "-q", "-b", "4096"];
    a.extend_from_slice(opciones);
    let r = ruta(&img);
    a.push(&r);
    assert_eq!(corre("mkfs.ext4", &a), Some(true));
    Some(img)
}

fn verificar(img: &std::path::Path) -> InformeVerificacion {
    let mut fs = Ext4::abrir(std::fs::File::open(img).unwrap()).unwrap();
    fs.verificar().unwrap()
}

fn sana(img: &std::path::Path, que: &str) {
    comun::fsck(img).unwrap_or_else(|s| panic!("{que}: e2fsck no la ve sana:\n{s}"));
    let inf = verificar(img);
    assert_eq!(inf.errores(), 0, "{que}: falsos errores: {:#?}", inf.hallazgos);
    assert!(inf.inodos >= 11 && inf.directorios >= 2 && inf.bloques_en_uso > 0, "{que}: control D30, no recorrió nada: {inf:?}");
    eprintln!("{que}: sana — {} inodos, {} dirs, {} bloques en uso", inf.inodos, inf.directorios, inf.bloques_en_uso);
}

/// Poblar con winext4: directorios, archivos con datos, y un htree partido.
fn poblar(img: &std::path::Path) {
    let f = std::fs::OpenOptions::new().read(true).write(true).open(img).unwrap();
    let mut fs = Ext4::abrir(f).unwrap();
    fs.abrir_escritura().unwrap();
    let d = fs.crear_directorio(2, "docs", 0o755).unwrap();
    for i in 0..40 {
        let n = fs.crear_archivo(d, &format!("archivo-{i:02}.txt"), 0o644).unwrap();
        fs.escribir(n, 0, &vec![b'a' + (i % 26) as u8; 3000 + i * 997]).unwrap();
    }
    let sub = fs.crear_directorio(d, "sub", 0o755).unwrap();
    fs.crear_archivo(sub, "dentro", 0o644).unwrap();
    fs.cerrar_escritura().unwrap();
}

#[test]
fn imagenes_sanas_de_cuatro_origenes_dan_cero_errores() {
    let Some(a) = nueva("mkfs", &[]) else {
        eprintln!("SALTADA: faltan e2fsprogs");
        return;
    };
    sana(&a, "recién formateada");
    poblar(&a);
    // htree: e2fsck -D indexa /docs y luego winext4 inserta hasta partir hojas.
    let o = comun::e2fs("e2fsck").args(["-fyD", &ruta(&a)]).output().unwrap();
    assert!(matches!(o.status.code(), Some(0 | 1)));
    {
        let f = std::fs::OpenOptions::new().read(true).write(true).open(&a).unwrap();
        let mut fs = Ext4::abrir(f).unwrap();
        fs.abrir_escritura().unwrap();
        let d = fs.buscar_numero("/docs").unwrap();
        for i in 0..400 {
            fs.crear_archivo(d, &format!("muchos-nombres-para-partir-{i:04}"), 0o644).unwrap();
        }
        fs.cerrar_escritura().unwrap();
    }
    sana(&a, "escrita por winext4 (htree partido)");

    let Some(b) = nueva("ext2", &["-O", "^extent,^64bit,^has_journal,^metadata_csum"]) else { return };
    let src = b.with_extension("src");
    std::fs::write(&src, vec![7u8; 5_000_000]).unwrap();
    assert_eq!(corre("debugfs", &["-w", "-R", &format!("write {} grande", ruta(&src)), &ruta(&b)]), Some(true));
    let _ = std::fs::remove_file(&src);
    sana(&b, "ext2 clásica (indirectos dobles)");

    let k = std::env::temp_dir().join(format!("winext4-ver-kernel-{}.img", std::process::id()));
    if comun::guion_root("journal-kernel.sh", &[&ruta(&k)]) {
        let f = std::fs::OpenOptions::new().read(true).write(true).open(&k).unwrap();
        Ext4::abrir(f).unwrap().recuperar_journal().unwrap();
        sana(&k, "del kernel, recuperada por winext4");
        let _ = std::fs::remove_file(&k);
    }
    for p in [&a, &b] {
        let _ = std::fs::remove_file(p);
    }
}

// ---------- Herramientas para PLANTAR corrupciones con las sumas rehechas ----------

fn rw(img: &std::path::Path) -> std::fs::File {
    std::fs::OpenOptions::new().read(true).write(true).open(img).unwrap()
}

/// Modifica el inodo `ino` crudo y le rehace la suma.
fn tocar_inodo(img: &std::path::Path, ino: u32, f: impl FnOnce(&mut [u8])) {
    let (pos, semilla, isz) = {
        let mut fs = Ext4::abrir(std::fs::File::open(img).unwrap()).unwrap();
        let ipg = fs.sb.inodos_por_grupo as u64;
        let tabla = fs.descriptor((ino as u64 - 1) / ipg).unwrap().tabla_inodos();
        let isz = fs.sb.tamano_inodo as u64;
        (tabla * 4096 + ((ino as u64 - 1) % ipg) * isz, fs.semilla(), isz as usize)
    };
    let mut a = rw(img);
    let mut c = vec![0u8; isz];
    a.seek(SeekFrom::Start(pos)).unwrap();
    a.read_exact(&mut c).unwrap();
    f(&mut c);
    csum::sellar_inodo(semilla, ino, &mut c);
    a.seek(SeekFrom::Start(pos)).unwrap();
    a.write_all(&c).unwrap();
}

/// Modifica el descriptor del grupo 0 y le rehace la suma.
fn tocar_descriptor0(img: &std::path::Path, f: impl FnOnce(&mut winext4_nucleo::grupos::Descriptor)) {
    let (mut d, semilla) = {
        let mut fs = Ext4::abrir(std::fs::File::open(img).unwrap()).unwrap();
        (fs.descriptor(0).unwrap(), fs.semilla())
    };
    f(&mut d);
    d.sellar(semilla, 0);
    let mut a = rw(img);
    a.seek(SeekFrom::Start(4096)).unwrap(); // GDT en el bloque 1
    a.write_all(&d.crudo).unwrap();
}

/// Cambia un bit del bitmap de bloques del grupo 0 y rehace su suma en el descriptor.
fn tocar_bitmap0(img: &std::path::Path, bloque: u64, usado: bool) {
    let (bmb, semilla, bpg) = {
        let mut fs = Ext4::abrir(std::fs::File::open(img).unwrap()).unwrap();
        (fs.descriptor(0).unwrap().bitmap_bloques(), fs.semilla(), fs.sb.bloques_por_grupo as usize)
    };
    let mut a = rw(img);
    let mut bm = vec![0u8; 4096];
    a.seek(SeekFrom::Start(bmb * 4096)).unwrap();
    a.read_exact(&mut bm).unwrap();
    let (i, m) = ((bloque / 8) as usize, 1u8 << (bloque % 8));
    if usado { bm[i] |= m } else { bm[i] &= !m }
    a.seek(SeekFrom::Start(bmb * 4096)).unwrap();
    a.write_all(&bm).unwrap();
    drop(a);
    let c = csum::bitmap(semilla, &bm, bpg / 8);
    tocar_descriptor0(img, |d| d.pon_csum_bitmap_bloques(d.recorta(c)));
}

/// Imagen con /docs y archivos, más (ino de /docs/archivo-00.txt, su primer bloque físico).
fn con_archivos(nombre: &str) -> Option<(std::path::PathBuf, u32, u64)> {
    let img = nueva(nombre, &[])?;
    poblar(&img);
    let mut fs = Ext4::abrir(std::fs::File::open(&img).unwrap()).unwrap();
    let n = fs.buscar_numero("/docs/archivo-00.txt").unwrap();
    let ino = fs.inodo(n).unwrap();
    let b = fs.bloques_fisicos(&ino).unwrap()[0].unwrap();
    Some((img, n, b))
}

/// La pareja: nuestro verificador nombra `clase` Y e2fsck no la ve sana.
fn detectan(img: &std::path::Path, clase: &str) {
    let inf = verificar(img);
    assert!(inf.tiene(clase), "winext4 no detectó «{clase}»: {:#?}", inf.hallazgos);
    assert!(comun::fsck(img).is_err(), "e2fsck tampoco la ve rota: la corrupción plantada no corrompe ({clase})");
    eprintln!("«{clase}»: detectada por winext4 ({} hallazgos) y por e2fsck", inf.hallazgos.len());
    let _ = std::fs::remove_file(img);
}

#[test]
fn siete_corrupciones_plantadas_las_detectan_winext4_y_e2fsck() {
    let Some((img, _, b)) = con_archivos("libre") else {
        eprintln!("SALTADA: faltan e2fsprogs");
        return;
    };
    // 1. Un bloque EN USO marcado libre (lo más peligroso: se sobrescribiría).
    tocar_bitmap0(&img, b, false);
    detectan(&img, "bloque-libre-en-uso");

    // 2. Contador de libres del grupo falso.
    let (img, _, _) = con_archivos("contador").unwrap();
    tocar_descriptor0(&img, |d| d.pon_bloques_libres(d.bloques_libres() + 7));
    detectan(&img, "contador-bloques");

    // 3. Suma de inodo rota (sin rehacerla).
    let (img, n, _) = con_archivos("suma").unwrap();
    {
        let (pos, isz) = {
            let mut fs = Ext4::abrir(std::fs::File::open(&img).unwrap()).unwrap();
            let ipg = fs.sb.inodos_por_grupo as u64;
            let t = fs.descriptor((n as u64 - 1) / ipg).unwrap().tabla_inodos();
            (t * 4096 + ((n as u64 - 1) % ipg) * fs.sb.tamano_inodo as u64, fs.sb.tamano_inodo as u64)
        };
        let _ = isz;
        let mut a = rw(&img);
        a.seek(SeekFrom::Start(pos + 0x10)).unwrap();
        a.write_all(&[0x5A]).unwrap(); // mtime cambiado sin rehacer la suma
    }
    detectan(&img, "suma-inodo");

    // 4. Recuento de enlaces falso (con la suma rehecha).
    let (img, n, _) = con_archivos("enlaces").unwrap();
    tocar_inodo(&img, n, |c| c[0x1A] = 3);
    detectan(&img, "enlaces");

    // 5. Extent que apunta fuera del volumen (suma rehecha).
    let (img, n, _) = con_archivos("fuera").unwrap();
    tocar_inodo(&img, n, |c| c[0x28 + 12 + 8..0x28 + 12 + 12].copy_from_slice(&0x00FF_FFFFu32.to_le_bytes()));
    detectan(&img, "extent-fuera");

    // 6. Dos inodos reclamando el mismo bloque: el extent de archivo-01 apunta al de archivo-00.
    let (img, _, b0) = con_archivos("doble").unwrap();
    let n1 = Ext4::abrir(std::fs::File::open(&img).unwrap()).unwrap().buscar_numero("/docs/archivo-01.txt").unwrap();
    tocar_inodo(&img, n1, |c| c[0x28 + 12 + 8..0x28 + 12 + 12].copy_from_slice(&(b0 as u32).to_le_bytes()));
    detectan(&img, "bloque-doble");

    // 7. Una entrada de directorio hacia un inodo LIBRE (bloque de directorio con la suma rehecha).
    let (img, _, _) = con_archivos("a-libre").unwrap();
    {
        let mut fs = Ext4::abrir(std::fs::File::open(&img).unwrap()).unwrap();
        let d = fs.buscar_numero("/docs").unwrap();
        let ino = fs.inodo(d).unwrap();
        let bloque = fs.bloques_fisicos(&ino).unwrap()[0].unwrap();
        let gen = u32::from_le_bytes(fs.inodo_crudo(d).unwrap()[csum::INO_OFF_GEN..][..4].try_into().unwrap());
        let si = csum::semilla_inodo(fs.semilla(), d, gen);
        let mut blk = fs.leer_bloque(bloque).unwrap();
        drop(fs);
        // tercera entrada (tras «.» y «..»): su número de inodo pasa a uno libre alto.
        let rec0 = u16::from_le_bytes([blk[4], blk[5]]) as usize;
        let rec1 = u16::from_le_bytes([blk[rec0 + 4], blk[rec0 + 5]]) as usize;
        blk[rec0 + rec1..rec0 + rec1 + 4].copy_from_slice(&8000u32.to_le_bytes());
        csum::sellar_bloque_dir(si, &mut blk);
        let mut a = rw(&img);
        a.seek(SeekFrom::Start(bloque * 4096)).unwrap();
        a.write_all(&blk).unwrap();
    }
    detectan(&img, "dir-a-libre");
}
