// SPDX-FileCopyrightText: 2026 Juan Carlos Isaza
// SPDX-License-Identifier: LicenseRef-Xiliux-Propietario
//! A2: CORTES DE LUZ simulados en CADA escritura de una secuencia de operaciones reales.
//!
//! Un dispositivo de prueba deja de escribir a partir de la escritura nº N (lo que no llegó al
//! disco no existe: exactamente un corte). Para cada N:
//!   - recuperado por e2fsprogs (`e2fsck -E journal_only`) → `e2fsck -fn` LIMPIO;
//!   - recuperado por winext4 (`recuperar_journal`)        → `e2fsck -fn` LIMPIO y `verificar` = 0.
//!
//! Positivo de control (D30): sin corte la secuencia termina con todo su contenido; y hay
//! cortes en los que la recuperación REPRODUCE transacciones confirmadas. Mutante verificado:
//! llevar los bloques a su sitio ANTES del commit → ROJO en el corte 12.

mod comun;

use std::io::{self, Read, Seek, SeekFrom, Write};

use comun::{corre, ruta};
use winext4_nucleo::cache::ConCache;
use winext4_nucleo::{ErrorFs, Ext4};

/// Archivo que cuenta sus escrituras y deja de escribir tras `limite` (el corte).
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
            return Ok(b.len()); // se fue la luz: lo «escrito» no llega
        }
        self.f.write(b)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.f.flush()
    }
}

/// La secuencia de operaciones reales.
fn operaciones<R: Read + Write + Seek>(fs: &mut Ext4<R>) -> Result<(), ErrorFs> {
    fs.abrir_escritura()?;
    let d = fs.crear_directorio(2, "carpeta", 0o755)?;
    let a = fs.crear_archivo(d, "a.txt", 0o644)?;
    fs.escribir(a, 0, &vec![b'a'; 30_000])?;
    let b = fs.crear_archivo(d, "b.bin", 0o644)?;
    fs.escribir(b, 0, &vec![b'b'; 200_000])?;
    fs.renombrar(d, "a.txt", 2, "a-movido.txt")?;
    let c = fs.crear_archivo(d, "c", 0o644)?;
    fs.escribir(c, 5000, b"con hueco delante")?;
    fs.borrar(d, "b.bin")?;
    fs.truncar(c, 10)?;
    let sub = fs.crear_directorio(d, "sub", 0o755)?;
    for i in 0..20 {
        fs.crear_archivo(sub, &format!("f{i}"), 0o644)?;
    }
    fs.cerrar_escritura()
}

/// Corre la secuencia con un corte tras `limite` escrituras; devuelve cuántas intentó. Con
/// `cache > 0`, el motor ve el dispositivo a través de la caché de bloques (`ConCache`), como
/// en el host de Windows: el corte tiene que dejar el volumen igual de consistente.
fn con_corte(img: &std::path::Path, limite: u64, cache: usize) -> u64 {
    let escritas = std::rc::Rc::new(std::cell::Cell::new(0));
    let f = std::fs::OpenOptions::new().read(true).write(true).open(img).unwrap();
    let c = Cortable { f, escritas: escritas.clone(), limite };
    let r = if cache == 0 {
        operaciones(&mut Ext4::abrir(c).unwrap())
    } else {
        operaciones(&mut Ext4::abrir(ConCache::nuevo(c, cache)).unwrap())
    };
    if limite == u64::MAX {
        r.expect("sin corte, la secuencia debe terminar");
    }
    // Con corte, lo de después no importa (el proceso «murió»): se ignoran sus errores.
    escritas.get()
}

#[test]
fn cortar_la_luz_en_cada_escritura_deja_el_volumen_consistente() {
    barrido_de_cortes(0);
}

/// El MISMO barrido con la caché de bloques entre el motor y el disco (8 unidades: obliga a
/// reemplazar sin parar). La caché es de escritura directa: no puede cambiar nada de esto.
#[test]
fn cortar_la_luz_con_cache_de_bloques_deja_el_volumen_consistente() {
    barrido_de_cortes(8);
}

fn barrido_de_cortes(cache: usize) {
    if !comun::hay_oraculo() {
        eprintln!("SALTADA: faltan e2fsprogs");
        return;
    }
    let base = std::env::temp_dir().join(format!("winext4-cortes-{}-c{cache}", std::process::id()));
    let limpia = base.with_extension("base.img");
    std::fs::File::create(&limpia).unwrap().set_len(16 * 1024 * 1024).unwrap();
    assert_eq!(corre("mkfs.ext4", &["-F", "-q", "-b", "4096", &ruta(&limpia)]), Some(true));

    // Sin corte: contar las escrituras y comprobar el resultado completo (control D30).
    let completa = base.with_extension("completa.img");
    std::fs::copy(&limpia, &completa).unwrap();
    let total = con_corte(&completa, u64::MAX, cache);
    comun::fsck(&completa).unwrap_or_else(|s| panic!("sin corte: {s}"));
    assert_eq!(comun::volcar(&completa, "/a-movido.txt"), vec![b'a'; 30_000]);
    assert!(total > 50, "control: la secuencia escribe de verdad ({total})");
    eprintln!("la secuencia hace {total} escrituras; se corta en cada una");

    let img = base.with_extension("corte.img");
    let mut con_reproduccion = 0;
    for n in 0..total {
        // (a) recuperado por e2fsprogs
        std::fs::copy(&limpia, &img).unwrap();
        con_corte(&img, n, cache);
        let o = comun::e2fs("e2fsck").args(["-E", "journal_only", "-y", &ruta(&img)]).output().unwrap();
        assert!(matches!(o.status.code(), Some(0 | 1)), "corte {n}: e2fsck journal_only falló: {}", String::from_utf8_lossy(&o.stdout));
        if let Err(s) = comun::fsck(&img) {
            panic!("corte en la escritura {n}/{total}, recuperado por e2fsck:\n{s}");
        }
        // (b) recuperado por winext4
        std::fs::copy(&limpia, &img).unwrap();
        con_corte(&img, n, cache);
        {
            let f = std::fs::OpenOptions::new().read(true).write(true).open(&img).unwrap();
            let inf = Ext4::abrir(f).unwrap().recuperar_journal().unwrap();
            if inf.transacciones > 0 {
                con_reproduccion += 1;
            }
        }
        if let Err(s) = comun::fsck(&img) {
            panic!("corte en la escritura {n}/{total}, recuperado por winext4:\n{s}");
        }
        let v = Ext4::abrir(std::fs::File::open(&img).unwrap()).unwrap().verificar().unwrap();
        assert_eq!(v.errores(), 0, "corte {n}/{total}: verificar: {:#?}", v.hallazgos);
    }
    eprintln!("cache={cache}: {total} cortes, {con_reproduccion} con transacciones que hubo que reproducir");
    assert!(con_reproduccion > 0, "control D30: algún corte debe caer entre commit y checkpoint");
    for p in [&limpia, &completa, &img] {
        let _ = std::fs::remove_file(p);
    }
}
