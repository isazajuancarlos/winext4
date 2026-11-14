// SPDX-FileCopyrightText: 2026 Juan Carlos Isaza
// SPDX-License-Identifier: LicenseRef-Xiliux-Propietario
//! El journal ACUMULA transacciones confirmadas (como jbd2) en vez de vaciarse tras cada una:
//! 2 barreras por operación en vez de 5. Dos riesgos nuevos, cada uno con su prueba:
//!
//! 1. El registro se LLENA y hay que vaciarlo y volver al principio: journal mínimo y cientos de
//!    operaciones (varias vueltas), con cortes de luz repartidos → `e2fsck` limpio tras recuperar
//!    con winext4 y con e2fsprogs.
//! 2. Un bloque de METADATOS con copia en el journal se libera y se reutiliza para DATOS: tras
//!    un corte, la recuperación pondría encima la copia vieja (e2fsck no lo ve: los metadatos
//!    cuadran). Se exige el CONTENIDO exacto tras recuperar por las dos vías.

mod comun;

use std::io::{self, Read, Seek, SeekFrom, Write};

use comun::{corre, ruta};
use winext4_nucleo::{ErrorFs, Ext4, INODO_RAIZ};

struct Cortable {
    f: std::fs::File,
    escritas: std::rc::Rc<std::cell::Cell<u64>>,
    limite: u64,
}
impl Read for Cortable {
    fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
        self.f.read(b)
    }
}
impl Seek for Cortable {
    fn seek(&mut self, p: SeekFrom) -> io::Result<u64> {
        self.f.seek(p)
    }
}
impl Write for Cortable {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        let n = self.escritas.get() + 1;
        self.escritas.set(n);
        if n > self.limite {
            return Ok(b.len());
        }
        self.f.write(b)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.f.flush()
    }
}

fn muchas<R: Read + Write + Seek>(fs: &mut Ext4<R>) -> Result<(), ErrorFs> {
    fs.abrir_escritura()?;
    let d = fs.crear_directorio(INODO_RAIZ, "muchos", 0o755)?;
    for i in 0..300 {
        let n = fs.crear_archivo(d, &format!("archivo-{i:03}"), 0o644)?;
        fs.escribir(n, 0, format!("contenido {i}").as_bytes())?;
        if i % 3 == 0 {
            fs.borrar(d, &format!("archivo-{i:03}"))?;
        }
    }
    fs.cerrar_escritura()
}

fn correr(img: &std::path::Path, limite: u64) -> u64 {
    let escritas = std::rc::Rc::new(std::cell::Cell::new(0));
    let f = std::fs::OpenOptions::new().read(true).write(true).open(img).unwrap();
    let mut fs = Ext4::abrir(Cortable { f, escritas: escritas.clone(), limite }).unwrap();
    let r = muchas(&mut fs);
    if limite == u64::MAX {
        r.expect("sin corte, la secuencia debe terminar");
    }
    escritas.get()
}

#[test]
fn el_registro_da_la_vuelta_con_cortes_repartidos() {
    if !comun::hay_oraculo() {
        eprintln!("SALTADA: faltan e2fsprogs");
        return;
    }
    let base = std::env::temp_dir().join(format!("winext4-jvuelta-{}", std::process::id()));
    let limpia = base.with_extension("base.img");
    std::fs::File::create(&limpia).unwrap().set_len(32 * 1024 * 1024).unwrap();
    // Bloques de 1 KiB y el journal MÍNIMO (1024 bloques = 1 MiB): ~500 transacciones de 5-8
    // bloques cada una no caben ni de lejos → el registro tiene que vaciarse y volver varias veces.
    assert_eq!(corre("mkfs.ext4", &["-F", "-q", "-b", "1024", "-J", "size=1", &ruta(&limpia)]), Some(true));
    let completa = base.with_extension("completa.img");
    std::fs::copy(&limpia, &completa).unwrap();
    let total = correr(&completa, u64::MAX);
    comun::fsck(&completa).unwrap_or_else(|s| panic!("sin corte: {s}"));
    assert_eq!(comun::volcar(&completa, "/muchos/archivo-299"), b"contenido 299");
    // Control D30: más escrituras de journal que bloques tiene (≥ 3 por transacción × ~500).
    assert!(total > 3 * 1024, "control: la secuencia tiene que desbordar el journal ({total} escrituras)");
    let img = base.with_extension("corte.img");
    let paso = (total / 60).max(1);
    let mut cortes = 0;
    for n in (paso..total).step_by(paso as usize) {
        for por_e2fsck in [true, false] {
            std::fs::copy(&limpia, &img).unwrap();
            correr(&img, n);
            if por_e2fsck {
                let o = comun::e2fs("e2fsck").args(["-E", "journal_only", "-y", &ruta(&img)]).output().unwrap();
                assert!(matches!(o.status.code(), Some(0 | 1)), "corte {n}: e2fsck journal_only falló");
            } else {
                let f = std::fs::OpenOptions::new().read(true).write(true).open(&img).unwrap();
                Ext4::abrir(f).unwrap().recuperar_journal().unwrap();
            }
            if let Err(s) = comun::fsck(&img) {
                panic!("corte en {n}/{total}, recuperado por {}:\n{s}", if por_e2fsck { "e2fsck" } else { "winext4" });
            }
        }
        cortes += 1;
    }
    eprintln!("{total} escrituras; {cortes} cortes × 2 recuperaciones, todas limpias");
    for p in [&limpia, &completa, &img] {
        let _ = std::fs::remove_file(p);
    }
}

#[test]
fn reutilizar_para_datos_un_bloque_del_journal_no_pierde_los_datos() {
    if !comun::hay_oraculo() {
        eprintln!("SALTADA: faltan e2fsprogs");
        return;
    }
    let base = std::env::temp_dir().join(format!("winext4-jreuso-{}", std::process::id()));
    let img = base.with_extension("img");
    std::fs::File::create(&img).unwrap().set_len(32 * 1024 * 1024).unwrap();
    assert_eq!(corre("mkfs.ext4", &["-F", "-q", "-b", "4096", &ruta(&img)]), Some(true));
    const BS: u64 = 4096;
    let patron: Vec<u8> = (0..BS * 200).map(|k| (k * 7 % 251) as u8 + 1).collect(); // sin ceros
    let indice;
    {
        let f = std::fs::OpenOptions::new().read(true).write(true).open(&img).unwrap();
        let mut fs = Ext4::abrir(f).unwrap();
        fs.abrir_escritura().unwrap();
        // A: fragmentado con huecos → árbol de extents con un NODO ÍNDICE (metadatos, al journal).
        let a = fs.crear_archivo(INODO_RAIZ, "a", 0o644).unwrap();
        for k in 0..12u64 {
            fs.escribir(a, k * 2 * BS, &vec![9u8; BS as usize]).unwrap();
        }
        let ia = fs.inodo(a).unwrap();
        let arbol = fs.bloques_de_arbol(&ia).unwrap();
        assert!(!arbol.is_empty(), "control: A tiene un nodo índice de extents");
        indice = arbol[0];
        fs.borrar(INODO_RAIZ, "a").unwrap();
        // B: grande, empieza en la misma zona → reutiliza los bloques de A, incluido el índice.
        let b = fs.crear_archivo(INODO_RAIZ, "b", 0o644).unwrap();
        fs.escribir(b, 0, &patron).unwrap();
        let ib = fs.inodo(b).unwrap();
        let reutiliza = fs.bloques_fisicos(&ib).unwrap().into_iter().flatten().any(|b| b == indice);
        assert!(reutiliza, "control D30: B reutiliza el bloque {indice} (era el índice de A, con copia en el journal)");
        // «Se va la luz»: sin cerrar_escritura (el journal queda con transacciones y needs_recovery).
    }
    for por_e2fsck in [true, false] {
        let copia = base.with_extension(if por_e2fsck { "e2.img" } else { "w4.img" });
        std::fs::copy(&img, &copia).unwrap();
        if por_e2fsck {
            let o = comun::e2fs("e2fsck").args(["-E", "journal_only", "-y", &ruta(&copia)]).output().unwrap();
            assert!(matches!(o.status.code(), Some(0 | 1)), "e2fsck journal_only falló");
        } else {
            let f = std::fs::OpenOptions::new().read(true).write(true).open(&copia).unwrap();
            Ext4::abrir(f).unwrap().recuperar_journal().unwrap();
        }
        if let Err(s) = comun::fsck(&copia) {
            panic!("recuperado por {}: {s}", if por_e2fsck { "e2fsck" } else { "winext4" });
        }
        let leido = comun::volcar(&copia, "/b");
        assert!(leido == patron, "recuperado por {}: el CONTENIDO de B cambió (la copia vieja del índice cayó encima)", if por_e2fsck { "e2fsck" } else { "winext4" });
        let _ = std::fs::remove_file(&copia);
    }
    let _ = std::fs::remove_file(&img);
}
