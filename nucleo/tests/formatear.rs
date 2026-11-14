// SPDX-FileCopyrightText: 2026 Juan Carlos Isaza
// SPDX-License-Identifier: LicenseRef-Xiliux-Propietario
//! C: `formatear` (mkwinext4). Cinco tamaños que ejercitan geometrías distintas:
//! 16 MiB (mínimo), 100 MiB (un grupo), 300 MiB (3 grupos, último incompleto),
//! 1 GiB + 7 MiB (9 grupos: el último, de 7 MiB, cabe con sus metadatos y se CONSERVA),
//! 1 GiB + 1 MiB (el último, de 256 bloques, NO cabe y se DESCARTA: quedan 8) y 2,5 GiB (20
//! grupos, copias de respaldo en 1, 3, 5, 7, 9). En todos: `e2fsck -fn` limpio, `verificar` = 0 y winext4
//! escribiendo encima. En uno: el KERNEL de Linux lo monta, escribe y desmonta — el oráculo
//! más exigente que hay (D28).

mod comun;

use comun::ruta;
use winext4_nucleo::Ext4;

fn formatear_imagen(nombre: &str, bytes: u64) -> std::path::PathBuf {
    formatear_con_informe(nombre, bytes).0
}

fn formatear_con_informe(nombre: &str, bytes: u64) -> (std::path::PathBuf, winext4_nucleo::formatear::InformeFormato) {
    let img = std::env::temp_dir().join(format!("winext4-mkfs-{nombre}-{}.img", std::process::id()));
    let f = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&img).unwrap();
    f.set_len(bytes).unwrap();
    let mut f = f;
    let inf = winext4_nucleo::formatear::formatear(&mut f, bytes, "WINEXT4").unwrap();
    eprintln!("{nombre}: {} bloques, {} grupos, {} inodos, journal {} bloques", inf.bloques, inf.grupos, inf.inodos, inf.bloques_journal);
    (img, inf)
}

fn sano(img: &std::path::Path, paso: &str) {
    comun::fsck(img).unwrap_or_else(|s| panic!("e2fsck tras «{paso}»:\n{s}"));
    let v = Ext4::abrir(std::fs::File::open(img).unwrap()).unwrap().verificar().unwrap();
    assert_eq!(v.errores(), 0, "verificar tras «{paso}»: {:#?}", v.hallazgos);
}

fn escribir_con_winext4(img: &std::path::Path) {
    let f = std::fs::OpenOptions::new().read(true).write(true).open(img).unwrap();
    let mut fs = Ext4::abrir(f).unwrap();
    fs.abrir_escritura().unwrap();
    let d = fs.crear_directorio(2, "desde-windows", 0o755).unwrap();
    for i in 0..50 {
        let n = fs.crear_archivo(d, &format!("archivo-{i}.txt"), 0o644).unwrap();
        fs.escribir(n, 0, &vec![b'w'; 1000 + i * 4000]).unwrap();
    }
    fs.borrar(d, "archivo-7.txt").unwrap();
    fs.cerrar_escritura().unwrap();
}

#[test]
fn cinco_tamanos_quedan_sanos_y_winext4_escribe_encima() {
    if !comun::hay_oraculo() {
        eprintln!("SALTADA: faltan e2fsprogs");
        return;
    }
    for (nombre, bytes) in [
        ("16m", 16 << 20),
        ("100m", 100 << 20),
        ("300m", 300 << 20),
        ("1g7m", (1 << 30) + (7 << 20)),
        ("1g1m", (1 << 30) + (1 << 20)),
        ("2g5", (5u64 << 30) / 2),
    ] {
        let (img, inf) = formatear_con_informe(nombre, bytes);
        match nombre {
            "1g7m" => assert_eq!(inf.grupos, 9, "el último grupo de 7 MiB se conserva"),
            "1g1m" => assert_eq!((inf.grupos, inf.bloques), (8, 8 * 32768), "el último grupo de 256 bloques se descarta"),
            _ => {}
        }
        sano(&img, &format!("formatear {nombre}"));
        // dumpe2fs: la etiqueta y las características que prometimos.
        let o = comun::e2fs("dumpe2fs").args(["-h", &ruta(&img)]).output().unwrap();
        let txt = String::from_utf8_lossy(&o.stdout);
        assert!(txt.contains("WINEXT4"), "{nombre}: etiqueta");
        for f in ["has_journal", "extent", "64bit", "metadata_csum", "dir_index"] {
            assert!(txt.contains(f), "{nombre}: falta la característica {f}");
        }
        escribir_con_winext4(&img);
        sano(&img, &format!("escribir con winext4 sobre {nombre}"));
        let _ = std::fs::remove_file(&img);
    }
}

#[test]
fn el_kernel_de_linux_monta_escribe_y_desmonta_lo_formateado() {
    if !comun::hay_oraculo() {
        eprintln!("SALTADA: faltan e2fsprogs");
        return;
    }
    let img = formatear_imagen("kernel", 300 << 20);
    if !comun::guion_root("kernel-escribe.sh", &[&ruta(&img)]) {
        eprintln!("SALTADA: hace falta root/WSL para montar con el kernel");
        return;
    }
    sano(&img, "el kernel escribió y desmontó");
    // Lo que escribió el kernel lo lee winext4 exacto.
    let sha_kernel = std::fs::read_to_string(img.with_extension("img.sha")).unwrap_or_default();
    let mut fs = Ext4::abrir(std::fs::File::open(&img).unwrap()).unwrap();
    let ino = fs.buscar("/kernel/grande.bin").unwrap();
    assert_eq!(ino.tamano, 20 << 20);
    let sub = fs.buscar("/kernel/sub").unwrap();
    let n_sub = fs.listar(&sub).unwrap().len();
    assert_eq!(n_sub, 300 - 2 + 2, "298 archivos + «.» y «..»");
    drop(fs);
    assert!(!sha_kernel.trim().is_empty(), "control: el kernel dejó la suma del archivo grande");
    // Y winext4 puede seguir escribiendo después del kernel.
    escribir_con_winext4(&img);
    sano(&img, "winext4 escribe sobre lo que escribió el kernel");
    let _ = std::fs::remove_file(&img);
    let _ = std::fs::remove_file(img.with_extension("img.sha"));
}
