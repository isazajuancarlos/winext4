// SPDX-FileCopyrightText: 2026 Juan Carlos Isaza
// SPDX-License-Identifier: LicenseRef-Xiliux-Propietario
//! AGRUPADO de transacciones (varias operaciones en una, como el commit periódico de jbd2).
//! Lo que se exige, cada cosa con su prueba:
//! 1. cortar la luz en CADA escritura con grupos pequeños → recuperado (winext4 y e2fsck) limpio;
//! 2. si una operación falla dentro de un grupo, se deshace SOLO ella (las anteriores quedan);
//! 3. lo LIBERADO en un grupo no se reutiliza antes de confirmarlo: tras un corte, el archivo
//!    borrado-sin-confirmar conserva su CONTENIDO (e2fsck no lo vería: hay que mirar los bytes);
//! 4. lo confirmado (`confirmar_pendiente`) sobrevive al corte; lo no confirmado, no aparece.

mod comun;

use std::io::{self, Read, Seek, SeekFrom, Write};
use std::time::Duration;

use comun::{corre, ruta};
use winext4_nucleo::{Agrupado, ErrorFs, Ext4, INODO_RAIZ};

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

const GRUPO: Agrupado = Agrupado { max_espera: Duration::from_secs(3600), max_bloques: 16 };

fn imagen(nombre: &str, mb: u64) -> std::path::PathBuf {
    let img = std::env::temp_dir().join(format!("winext4-agr-{nombre}-{}.img", std::process::id()));
    std::fs::File::create(&img).unwrap().set_len(mb * 1024 * 1024).unwrap();
    assert_eq!(corre("mkfs.ext4", &["-F", "-q", "-b", "4096", &ruta(&img)]), Some(true));
    img
}

fn abrir(img: &std::path::Path) -> Ext4<std::fs::File> {
    Ext4::abrir(std::fs::OpenOptions::new().read(true).write(true).open(img).unwrap()).unwrap()
}

/// Recupera una COPIA de `img` por e2fsck o por winext4 y la deja comprobada por e2fsck.
fn recuperar(img: &std::path::Path, por_e2fsck: bool) -> std::path::PathBuf {
    let copia = img.with_extension(if por_e2fsck { "e2.img" } else { "w4.img" });
    std::fs::copy(img, &copia).unwrap();
    if por_e2fsck {
        let o = comun::e2fs("e2fsck").args(["-E", "journal_only", "-y", &ruta(&copia)]).output().unwrap();
        assert!(matches!(o.status.code(), Some(0 | 1)), "e2fsck journal_only falló");
    } else {
        abrir(&copia).recuperar_journal().unwrap();
    }
    if let Err(s) = comun::fsck(&copia) {
        panic!("recuperado por {}: {s}", if por_e2fsck { "e2fsck" } else { "winext4" });
    }
    copia
}

fn secuencia<R: Read + Write + Seek>(fs: &mut Ext4<R>) -> Result<(), ErrorFs> {
    fs.abrir_escritura()?;
    fs.agrupar_transacciones(Some(GRUPO))?;
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
        let n = fs.crear_archivo(sub, &format!("f{i}"), 0o644)?;
        fs.escribir(n, 0, &vec![i as u8 + 1; 9000])?;
    }
    fs.borrar(sub, "f3")?;
    fs.cerrar_escritura()
}

fn con_corte(img: &std::path::Path, limite: u64) -> u64 {
    let escritas = std::rc::Rc::new(std::cell::Cell::new(0));
    let f = std::fs::OpenOptions::new().read(true).write(true).open(img).unwrap();
    let mut fs = Ext4::abrir(Cortable { f, escritas: escritas.clone(), limite }).unwrap();
    let r = secuencia(&mut fs);
    if limite == u64::MAX {
        r.expect("sin corte, la secuencia debe terminar");
    }
    escritas.get()
}

#[test]
fn cortar_la_luz_en_cada_escritura_con_agrupado() {
    if !comun::hay_oraculo() {
        eprintln!("SALTADA: faltan e2fsprogs");
        return;
    }
    let limpia = imagen("cortes", 16);
    let completa = limpia.with_extension("completa.img");
    std::fs::copy(&limpia, &completa).unwrap();
    let total = con_corte(&completa, u64::MAX);
    comun::fsck(&completa).unwrap_or_else(|s| panic!("sin corte: {s}"));
    assert_eq!(comun::volcar(&completa, "/a-movido.txt"), vec![b'a'; 30_000]);
    assert_eq!(comun::volcar(&completa, "/carpeta/sub/f19"), vec![20u8; 9000]);
    let img = limpia.with_extension("corte.img");
    for n in 0..total {
        std::fs::copy(&limpia, &img).unwrap();
        con_corte(&img, n);
        for por_e2fsck in [true, false] {
            let c = recuperar(&img, por_e2fsck);
            let _ = std::fs::remove_file(c);
        }
    }
    eprintln!("agrupado: {total} escrituras, un corte en cada una × 2 recuperaciones, todas limpias");
    for p in [&limpia, &completa, &img] {
        let _ = std::fs::remove_file(p);
    }
}

#[test]
fn una_operacion_que_falla_en_un_grupo_se_deshace_sola() {
    if !comun::hay_oraculo() {
        eprintln!("SALTADA: faltan e2fsprogs");
        return;
    }
    let img = imagen("falla", 16);
    {
        let mut fs = abrir(&img);
        fs.abrir_escritura().unwrap();
        fs.agrupar_transacciones(Some(Agrupado { max_espera: Duration::from_secs(3600), max_bloques: 100_000 })).unwrap();
        let a = fs.crear_archivo(INODO_RAIZ, "a", 0o644).unwrap();
        fs.escribir(a, 0, b"soy A").unwrap();
        // Falla A MITAD (sin espacio tras asignar parte): solo esto se deshace.
        let g = fs.crear_archivo(INODO_RAIZ, "gigante", 0o644).unwrap();
        let e = fs.escribir(g, 0, &vec![1u8; 40 << 20]).unwrap_err();
        assert!(matches!(e, ErrorFs::SinEspacio), "control: la escritura falla por espacio ({e})");
        let c = fs.crear_archivo(INODO_RAIZ, "c", 0o644).unwrap();
        fs.escribir(c, 0, b"soy C").unwrap();
        fs.cerrar_escritura().unwrap();
    }
    if let Err(s) = comun::fsck(&img) {
        panic!("tras una operación fallida dentro de un grupo:\n{s}");
    }
    // e2fsck NO trata como error los totales del superbloque (en ext4 son orientativos y los
    // recalcula): si deshacer la operación fallida no restaurara el superbloque en memoria,
    // quedarían mal sin que nadie lo dijera. Se exige que cuadren con los grupos.
    {
        let mut fs = abrir(&img);
        let ng = fs.sb.n_grupos();
        let (mut bl, mut il) = (0u64, 0u64);
        for g in 0..ng {
            let d = fs.descriptor(g).unwrap();
            bl += u64::from(d.bloques_libres());
            il += u64::from(d.inodos_libres());
        }
        let sbc = fs.superbloque_crudo();
        let sb_inodos = u64::from(u32::from_le_bytes(sbc[0x10..0x14].try_into().unwrap()));
        assert_eq!(fs.bloques_libres(), bl, "bloques libres del superbloque = suma de los grupos");
        assert_eq!(sb_inodos, il, "inodos libres del superbloque = suma de los grupos");
    }
    assert_eq!(comun::volcar(&img, "/a"), b"soy A", "lo anterior del grupo sobrevive");
    assert_eq!(comun::volcar(&img, "/c"), b"soy C", "lo posterior también");
    assert_eq!(comun::volcar(&img, "/gigante"), b"", "el archivo existe (se creó) pero la escritura fallida no dejó nada");
    let _ = std::fs::remove_file(&img);
}

#[test]
fn lo_liberado_en_un_grupo_no_se_reutiliza_antes_de_confirmar() {
    if !comun::hay_oraculo() {
        eprintln!("SALTADA: faltan e2fsprogs");
        return;
    }
    let img = imagen("liberado", 32);
    let patron_x: Vec<u8> = (0..300_000u32).map(|k| (k % 251) as u8 + 1).collect();
    {
        let mut fs = abrir(&img);
        fs.abrir_escritura().unwrap();
        fs.agrupar_transacciones(Some(Agrupado { max_espera: Duration::from_secs(3600), max_bloques: 100_000 })).unwrap();
        let x = fs.crear_archivo(INODO_RAIZ, "x", 0o644).unwrap();
        fs.escribir(x, 0, &patron_x).unwrap();
        fs.confirmar_pendiente().unwrap(); // X es durable
        let ix = fs.inodo(x).unwrap();
        let bloques_x: Vec<u64> = fs.bloques_fisicos(&ix).unwrap().into_iter().flatten().collect();
        // En el grupo ABIERTO: borrar X y crear Y con datos (directos al disco).
        fs.borrar(INODO_RAIZ, "x").unwrap();
        let y = fs.crear_archivo(INODO_RAIZ, "y", 0o644).unwrap();
        fs.escribir(y, 0, &vec![0xEEu8; 600_000]).unwrap();
        let iy = fs.inodo(y).unwrap();
        let pisa = fs.bloques_fisicos(&iy).unwrap().into_iter().flatten().any(|b| bloques_x.contains(&b));
        assert!(!pisa, "Y NO puede usar bloques que X liberó en el grupo sin confirmar");
        // «Se va la luz» sin confirmar el grupo.
    }
    for por_e2fsck in [true, false] {
        let c = recuperar(&img, por_e2fsck);
        assert!(comun::volcar(&c, "/x") == patron_x, "tras el corte, X (su borrado no se confirmó) conserva su CONTENIDO");
        let _ = std::fs::remove_file(c);
    }
    let _ = std::fs::remove_file(&img);
}

#[test]
fn lo_confirmado_sobrevive_y_lo_no_confirmado_no_aparece() {
    if !comun::hay_oraculo() {
        eprintln!("SALTADA: faltan e2fsprogs");
        return;
    }
    let img = imagen("frontera", 16);
    {
        let mut fs = abrir(&img);
        fs.abrir_escritura().unwrap();
        fs.agrupar_transacciones(Some(Agrupado { max_espera: Duration::from_secs(3600), max_bloques: 100_000 })).unwrap();
        let p = fs.crear_archivo(INODO_RAIZ, "confirmado", 0o644).unwrap();
        fs.escribir(p, 0, b"durable").unwrap();
        fs.confirmar_pendiente().unwrap();
        let q = fs.crear_archivo(INODO_RAIZ, "sin-confirmar", 0o644).unwrap();
        fs.escribir(q, 0, b"se pierde").unwrap();
        assert!(fs.buscar("/sin-confirmar").is_ok(), "control: antes del corte, el archivo SÍ se ve (la lectura ve el grupo)");
    }
    for por_e2fsck in [true, false] {
        let c = recuperar(&img, por_e2fsck);
        assert_eq!(comun::volcar(&c, "/confirmado"), b"durable");
        let o = comun::e2fs("debugfs").args(["-R", "stat /sin-confirmar", &ruta(&c)]).output().unwrap();
        let txt = format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
        assert!(txt.contains("not found") || txt.contains("No such"), "lo no confirmado no aparece: {txt}");
        let _ = std::fs::remove_file(c);
    }
    let _ = std::fs::remove_file(&img);
}
