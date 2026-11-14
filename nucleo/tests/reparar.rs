// SPDX-FileCopyrightText: 2026 Juan Carlos Isaza
// SPDX-License-Identifier: LicenseRef-Xiliux-Propietario
//! B2: `reparar`, contra e2fsck, empezando por el daño REAL encontrado en el nvme de Juan
//! (2026-11-05): un archivo con su bloque de atributos extendidos con la suma mala (escrito por
//! Ext2Fsd, que no mantiene metadata_csum), un bloque fugado con bitmap/descriptor/superbloque
//! coherentes entre sí, y la marca «con errores» del kernel.
//!
//! Parejas (D33): imagen sana → no se escribe ni un byte; daño grave → se niega SIN tocar nada;
//! bloque de atributos ilegible → se desvincula. Y cortes de luz en cada escritura de la
//! reparación: nunca queda PEOR (o como estaba, o reparado).

mod comun;

use std::io::{Read, Seek, SeekFrom, Write};

use comun::{corre, ruta};
use winext4_nucleo::{csum, Ext4};

const BS: u64 = 4096;

fn rw(img: &std::path::Path) -> std::fs::File {
    std::fs::OpenOptions::new().read(true).write(true).open(img).unwrap()
}

fn leer(img: &std::path::Path, b: u64) -> Vec<u8> {
    let mut f = std::fs::File::open(img).unwrap();
    let mut v = vec![0u8; BS as usize];
    f.seek(SeekFrom::Start(b * BS)).unwrap();
    f.read_exact(&mut v).unwrap();
    v
}

fn escribir(img: &std::path::Path, off: u64, datos: &[u8]) {
    let mut f = rw(img);
    f.seek(SeekFrom::Start(off)).unwrap();
    f.write_all(datos).unwrap();
}

/// Imagen con `/Ext2Fsd-setup.exe` y un atributo de 3000 bytes (va a un BLOQUE aparte).
/// Devuelve (imagen, número del bloque de atributos).
fn con_xattr(nombre: &str) -> Option<(std::path::PathBuf, u64)> {
    if !comun::hay_oraculo() {
        return None;
    }
    let img = std::env::temp_dir().join(format!("winext4-rep-{nombre}-{}.img", std::process::id()));
    std::fs::File::create(&img).ok()?.set_len(32 * 1024 * 1024).ok()?;
    assert_eq!(corre("mkfs.ext4", &["-F", "-q", "-b", "4096", &ruta(&img)]), Some(true));
    let exe = img.with_extension("exe");
    let val = img.with_extension("val");
    std::fs::write(&exe, vec![0x4Du8; 600_000]).unwrap();
    std::fs::write(&val, vec![b'v'; 3000]).unwrap();
    assert_eq!(corre("debugfs", &["-w", "-R", &format!("write {} Ext2Fsd-setup.exe", ruta(&exe)), &ruta(&img)]), Some(true));
    assert_eq!(corre("debugfs", &["-w", "-R", &format!("ea_set -f {} /Ext2Fsd-setup.exe user.firma", ruta(&val)), &ruta(&img)]), Some(true));
    let _ = std::fs::remove_file(&exe);
    let _ = std::fs::remove_file(&val);
    let mut fs = Ext4::abrir(std::fs::File::open(&img).unwrap()).unwrap();
    let n = fs.buscar_numero("/Ext2Fsd-setup.exe").unwrap();
    let c = fs.inodo_crudo(n).unwrap();
    let acl = u64::from(u32::from_le_bytes(c[0x68..0x6C].try_into().unwrap()));
    assert_ne!(acl, 0, "control D30: el atributo debe estar en un bloque aparte");
    Some((img, acl))
}

/// Planta el daño del nvme: suma del bloque xattr rota, un bloque fugado (bitmap, descriptor y
/// superbloque coherentes) y la marca «con errores» del kernel.
fn plantar_dano_nvme(img: &std::path::Path, acl: u64) -> u64 {
    // (1) suma del bloque de atributos alterada (estructura intacta).
    let mut x = leer(img, acl);
    x[0x10] ^= 0xFF;
    escribir(img, acl * BS, &x);
    // (2) fuga: el bloque de justo antes del de atributos, si está libre; si no, uno alto.
    let (semilla, bmb, mut d) = {
        let mut fs = Ext4::abrir(std::fs::File::open(img).unwrap()).unwrap();
        let d = fs.descriptor(0).unwrap();
        (fs.semilla(), d.bitmap_bloques(), d)
    };
    let mut bm = leer(img, bmb);
    let mut fuga = acl - 1;
    if bm[(fuga / 8) as usize] & (1 << (fuga % 8)) != 0 {
        fuga = 7000;
    }
    bm[(fuga / 8) as usize] |= 1 << (fuga % 8);
    escribir(img, bmb * BS, &bm);
    let c = csum::bitmap(semilla, &bm, 32768 / 8);
    d.pon_csum_bitmap_bloques(d.recorta(c));
    d.pon_bloques_libres(d.bloques_libres() - 1);
    d.sellar(semilla, 0);
    escribir(img, BS, &d.crudo);
    // (3) superbloque: un libre menos, estado «con errores», 516 errores registrados.
    let mut sb = vec![0u8; 1024];
    {
        let mut f = std::fs::File::open(img).unwrap();
        f.seek(SeekFrom::Start(1024)).unwrap();
        f.read_exact(&mut sb).unwrap();
    }
    let libres = u32::from_le_bytes(sb[0x0C..0x10].try_into().unwrap()) - 1;
    sb[0x0C..0x10].copy_from_slice(&libres.to_le_bytes());
    let estado = u16::from_le_bytes([sb[0x3A], sb[0x3B]]) | 0x2;
    sb[0x3A..0x3C].copy_from_slice(&estado.to_le_bytes());
    sb[0x194..0x198].copy_from_slice(&516u32.to_le_bytes());
    let c = csum::superbloque(&sb);
    sb[0x3FC..0x400].copy_from_slice(&c.to_le_bytes());
    escribir(img, 1024, &sb);
    fuga
}

fn atributo(img: &std::path::Path) -> Vec<u8> {
    let destino = img.with_extension("atrib");
    let o = comun::e2fs("debugfs").args(["-R", &format!("ea_get -f {} /Ext2Fsd-setup.exe user.firma", ruta(&destino)), &ruta(img)]).output().unwrap();
    assert!(o.status.success());
    let v = std::fs::read(&destino).unwrap_or_default();
    let _ = std::fs::remove_file(&destino);
    v
}

#[test]
fn repara_el_dano_real_del_nvme_y_conserva_los_atributos() {
    let Some((img, acl)) = con_xattr("nvme") else {
        eprintln!("SALTADA: faltan e2fsprogs");
        return;
    };
    let fuga = plantar_dano_nvme(&img, acl);
    // Pareja: el daño plantado es real para los dos verificadores.
    assert!(comun::fsck(&img).is_err(), "e2fsck debe ver el daño plantado");
    let v = Ext4::abrir(std::fs::File::open(&img).unwrap()).unwrap().verificar().unwrap();
    for clase in ["suma-xattr", "bloque-fuga", "contador-bloques"] {
        assert!(v.tiene(clase), "verificar debía ver «{clase}»: {:#?}", v.hallazgos);
    }
    assert_eq!(v.bloques_fuga, vec![fuga]);

    let inf = rw_fs(&img).reparar().unwrap();
    eprintln!("reparación: {:#?}", inf.hecho);
    assert!(inf.no_reparables.is_empty(), "{:#?}", inf.no_reparables);
    assert_eq!(inf.errores_despues, 0);
    comun::fsck(&img).unwrap_or_else(|s| panic!("e2fsck tras reparar:\n{s}"));
    assert_eq!(atributo(&img), vec![b'v'; 3000], "el atributo se CONSERVA (e2fsck lo habría borrado)");
    let o = comun::e2fs("dumpe2fs").args(["-h", &ruta(&img)]).output().unwrap();
    let txt = String::from_utf8_lossy(&o.stdout);
    let estado = txt.lines().find(|l| l.starts_with("Filesystem state")).unwrap_or_default();
    assert!(estado.contains("clean") && !estado.contains("error"), "estado tras reparar: {estado}");
    let _ = std::fs::remove_file(&img);
}

#[test]
fn reparar_con_archivo_para_deshacer_y_deshacer_devuelve_el_original_exacto() {
    use winext4_nucleo::dispositivo::{aplicar_deshacer, ConDeshacer};
    let Some((img, acl)) = con_xattr("deshacer") else {
        eprintln!("SALTADA: faltan e2fsprogs");
        return;
    };
    plantar_dano_nvme(&img, acl);
    let danada = std::fs::read(&img).unwrap();
    let undo = img.with_extension("deshacer");
    let _ = std::fs::remove_file(&undo);
    let trozos = {
        let mut fs = Ext4::abrir(ConDeshacer::nuevo(rw(&img), &undo).unwrap()).unwrap();
        let inf = fs.reparar().unwrap();
        assert_eq!(inf.errores_despues, 0, "{:#?}", inf);
        fs.dispositivo().trozos_guardados()
    };
    comun::fsck(&img).unwrap_or_else(|s| panic!("reparada con registro: {s}"));
    assert!(trozos > 0, "control: se guardaron originales ({trozos})");
    // Deshacer: la imagen vuelve a ser EXACTAMENTE la dañada (y e2fsck vuelve a verla rota).
    let n = aplicar_deshacer(&mut rw(&img), &undo).unwrap();
    assert_eq!(n, trozos);
    assert_eq!(std::fs::read(&img).unwrap(), danada, "deshacer devuelve el original byte a byte");
    assert!(comun::fsck(&img).is_err(), "pareja: tras deshacer, el daño vuelve a estar");
    eprintln!("deshacer: {trozos} bloques guardados y restaurados");
    let _ = std::fs::remove_file(&img);
    let _ = std::fs::remove_file(&undo);
}

fn rw_fs(img: &std::path::Path) -> Ext4<std::fs::File> {
    Ext4::abrir(rw(img)).unwrap()
}

#[test]
fn parejas_sana_no_se_toca_grave_se_niega_ilegible_se_desvincula() {
    let Some((img, acl)) = con_xattr("parejas") else {
        eprintln!("SALTADA: faltan e2fsprogs");
        return;
    };
    // (a) SANA: no se escribe ni un byte.
    let antes = std::fs::read(&img).unwrap();
    let inf = rw_fs(&img).reparar().unwrap();
    assert!(inf.hecho.is_empty() && inf.no_reparables.is_empty());
    assert_eq!(std::fs::read(&img).unwrap(), antes, "una imagen sana no se toca");

    // (b) GRAVE (un bloque de datos marcado LIBRE): se niega y no toca NADA.
    let (bmb, dato) = {
        let mut fs = Ext4::abrir(std::fs::File::open(&img).unwrap()).unwrap();
        let ino = fs.buscar("/Ext2Fsd-setup.exe").unwrap();
        (fs.descriptor(0).unwrap().bitmap_bloques(), fs.bloques_fisicos(&ino).unwrap()[0].unwrap())
    };
    let mut bm = leer(&img, bmb);
    bm[(dato / 8) as usize] &= !(1 << (dato % 8));
    escribir(&img, bmb * BS, &bm);
    let roto = std::fs::read(&img).unwrap();
    let inf = rw_fs(&img).reparar().unwrap();
    assert!(!inf.no_reparables.is_empty(), "lo grave debe negarse");
    assert_eq!(std::fs::read(&img).unwrap(), roto, "al negarse no toca NADA");
    std::fs::write(&img, &antes).unwrap();

    // (c) Bloque de atributos ILEGIBLE (una entrada apunta fuera del bloque) con la suma rota:
    // se desvincula del inodo y se libera; e2fsck queda limpio.
    let mut x = leer(&img, acl);
    x[32 + 2..32 + 4].copy_from_slice(&5000u16.to_le_bytes()); // e_value_offs fuera del bloque
    escribir(&img, acl * BS, &x);
    let inf = rw_fs(&img).reparar().unwrap();
    eprintln!("ilegible: {:#?}", inf.hecho);
    assert!(inf.hecho.iter().any(|h| h.contains("desvinculado")), "{:#?}", inf.hecho);
    assert_eq!(inf.errores_despues, 0);
    comun::fsck(&img).unwrap_or_else(|s| panic!("e2fsck tras desvincular:\n{s}"));
    let _ = std::fs::remove_file(&img);
}

/// Archivo que cuenta sus escrituras y deja de escribir tras `limite`.
struct Cortable {
    f: std::fs::File,
    n: std::rc::Rc<std::cell::Cell<u64>>,
    limite: u64,
}
impl Read for Cortable {
    fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
        self.f.read(b)
    }
}
impl Seek for Cortable {
    fn seek(&mut self, p: SeekFrom) -> std::io::Result<u64> {
        self.f.seek(p)
    }
}
impl Write for Cortable {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.n.set(self.n.get() + 1);
        if self.n.get() > self.limite {
            return Ok(b.len());
        }
        self.f.write(b)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.f.flush()
    }
}

#[test]
fn un_corte_durante_la_reparacion_nunca_deja_el_volumen_peor() {
    let Some((base, acl)) = con_xattr("cortes") else {
        eprintln!("SALTADA: faltan e2fsprogs");
        return;
    };
    plantar_dano_nvme(&base, acl);
    let clases_antes: std::collections::BTreeSet<&str> = Ext4::abrir(std::fs::File::open(&base).unwrap()).unwrap().verificar().unwrap().hallazgos.iter().map(|h| h.clase).collect();
    // Cuántas escrituras hace la reparación entera (sin corte).
    let img = base.with_extension("corte.img");
    std::fs::copy(&base, &img).unwrap();
    let cuenta = std::rc::Rc::new(std::cell::Cell::new(0));
    Ext4::abrir(Cortable { f: rw(&img), n: cuenta.clone(), limite: u64::MAX }).unwrap().reparar().unwrap();
    let n_total = cuenta.get();
    assert!(n_total > 5, "control: la reparación escribe ({n_total})");
    let mut reparados = 0;
    for n in 0..n_total {
        std::fs::copy(&base, &img).unwrap();
        {
            let c = Cortable { f: rw(&img), n: std::rc::Rc::new(std::cell::Cell::new(0)), limite: n };
            let mut fs = Ext4::abrir(c).unwrap();
            let _ = fs.reparar();
        }
        rw_fs(&img).recuperar_journal().unwrap();
        let v = Ext4::abrir(std::fs::File::open(&img).unwrap()).unwrap().verificar().unwrap();
        let clases: std::collections::BTreeSet<&str> = v.hallazgos.iter().filter(|h| h.gravedad == winext4_nucleo::verificar::Gravedad::Error).map(|h| h.clase).collect();
        if clases.is_empty() {
            reparados += 1;
        } else {
            assert!(clases.is_subset(&clases_antes), "corte {n}: apareció un daño NUEVO {:?} (antes {:?})", clases, clases_antes);
        }
    }
    eprintln!("{n_total} cortes: en {reparados} el volumen quedó reparado; en el resto, como estaba");
    assert!(reparados > 0 && reparados < n_total as usize, "control D30: hay cortes antes y después del commit");
    for p in [&base, &img] {
        let _ = std::fs::remove_file(p);
    }
}
