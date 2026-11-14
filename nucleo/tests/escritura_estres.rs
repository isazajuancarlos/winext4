// SPDX-FileCopyrightText: 2026 Juan Carlos Isaza
// SPDX-License-Identifier: LicenseRef-Xiliux-Propietario
//! D8: SIMULACIÓN de cientos de operaciones aleatorias (semilla fija, reproducible) contra
//! un MODELO en memoria, con `e2fsck -fn` cada 60 operaciones y al final, y comparación de
//! TODO el árbol (nuestro lector) más una muestra volcada por `debugfs`.
//!
//! Incluye los caminos de ERROR, que es donde se esconden las fugas: operaciones que deben
//! fallar (duplicado, borrar directorio con cosas, nombre inválido) y LLENAR EL DISCO hasta
//! `SinEspacio` — tras cada error el FS debe seguir limpio y el modelo intacto.

mod comun;

use std::collections::BTreeMap;

use comun::{corre, ruta};
use winext4_nucleo::{ErrorFs, Ext4};

/// xorshift64: reproducible sin dependencias.
struct Azar(u64);
impl Azar {
    fn sig(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn hasta(&mut self, n: u64) -> u64 {
        self.sig() % n
    }
}

/// El modelo: ruta → contenido (archivos) o `None` (directorios).
type Modelo = BTreeMap<String, Option<Vec<u8>>>;

fn padre(r: &str) -> &str {
    match r.rfind('/') {
        Some(0) | None => "/",
        Some(i) => &r[..i],
    }
}
fn nombre(r: &str) -> &str {
    &r[r.rfind('/').map_or(0, |i| i + 1)..]
}

fn imagen(mb: u64) -> Option<std::path::PathBuf> {
    if !comun::hay_oraculo() {
        return None;
    }
    let img = std::env::temp_dir().join(format!("winext4-estres-{}-{mb}.img", std::process::id()));
    std::fs::File::create(&img).ok()?.set_len(mb * 1024 * 1024).ok()?;
    assert_eq!(corre("mkfs.ext4", &["-F", "-q", "-b", "4096", &ruta(&img)]), Some(true));
    Some(img)
}

fn abrir(img: &std::path::Path) -> Ext4<std::fs::File> {
    let f = std::fs::OpenOptions::new().read(true).write(true).open(img).unwrap();
    let mut fs = Ext4::abrir(f).unwrap();
    fs.abrir_escritura().unwrap();
    fs
}

/// Compara el árbol entero contra el modelo con NUESTRO lector.
fn comparar(fs: &mut Ext4<std::fs::File>, modelo: &Modelo, paso: usize) {
    for (r, v) in modelo {
        let ino = fs.buscar(r).unwrap_or_else(|e| panic!("op {paso}: falta {r}: {e}"));
        match v {
            None => assert!(ino.es_directorio(), "op {paso}: {r} debería ser directorio"),
            Some(c) => {
                assert_eq!(&fs.leer_datos(&ino).unwrap(), c, "op {paso}: contenido de {r}");
                // leer_rango: tres ventanas que cruzan bloques y el final, contra el modelo.
                for (o, l) in [(0usize, 5000usize), (c.len() / 3, 9000), (c.len().saturating_sub(100), 4096)] {
                    let esperado = &c[o.min(c.len())..(o + l).min(c.len())];
                    assert_eq!(fs.leer_rango(&ino, o as u64, l).unwrap(), esperado, "op {paso}: rango {o}+{l} de {r}");
                }
            }
        }
    }
    // Y nada de más: cada entrada real está en el modelo.
    for dir in modelo.iter().filter(|(_, v)| v.is_none()).map(|(r, _)| r.clone()).chain(["/".to_string()]) {
        let ino = fs.buscar(&dir).unwrap();
        for e in fs.listar(&ino).unwrap() {
            if e.nombre == "." || e.nombre == ".." || (dir == "/" && e.nombre == "lost+found") {
                continue;
            }
            let r = if dir == "/" { format!("/{}", e.nombre) } else { format!("{dir}/{}", e.nombre) };
            assert!(modelo.contains_key(&r), "op {paso}: sobra {r} en disco");
        }
    }
}

fn limpio(img: &std::path::Path, paso: &str) {
    if let Err(s) = comun::fsck(img) {
        panic!("e2fsck tras {paso}:\n{s}");
    }
    // Segundo oráculo: nuestro propio verificador debe coincidir con e2fsck (cero errores).
    let inf = Ext4::abrir(std::fs::File::open(img).unwrap()).unwrap().verificar().unwrap();
    assert_eq!(inf.errores(), 0, "verificar tras {paso}: {:#?}", inf.hallazgos);
}

#[test]
fn trescientas_operaciones_aleatorias_con_errores_inyectados() {
    let Some(img) = imagen(48) else {
        eprintln!("SALTADA: faltan e2fsprogs");
        return;
    };
    let mut az = Azar(0x5EED_2026_1105);
    let mut modelo: Modelo = BTreeMap::new();
    let mut fs = abrir(&img);
    let mut cuenta = [0usize; 9];
    const N: usize = 360;

    for paso in 1..=N {
        let dirs: Vec<String> = std::iter::once("/".to_string())
            .chain(modelo.iter().filter(|(_, v)| v.is_none()).map(|(r, _)| r.clone()))
            .collect();
        let archivos: Vec<String> = modelo.iter().filter(|(_, v)| v.is_some()).map(|(r, _)| r.clone()).collect();
        let op = az.hasta(9);
        match op {
            // crear archivo
            0 | 1 => {
                let d = &dirs[az.hasta(dirs.len() as u64) as usize];
                let n = format!("a{}-ñ", az.hasta(10_000));
                let r = if d == "/" { format!("/{n}") } else { format!("{d}/{n}") };
                let dino = fs.buscar_numero(d).unwrap();
                match fs.crear_archivo(dino, &n, 0o644) {
                    Ok(_) => {
                        assert!(!modelo.contains_key(&r));
                        modelo.insert(r, Some(Vec::new()));
                    }
                    Err(ErrorFs::YaExiste { .. }) => assert!(modelo.contains_key(&r), "op {paso}: YaExiste falso"),
                    Err(e) => panic!("op {paso}: crear {r}: {e}"),
                }
            }
            // escribir en un sitio aleatorio (a veces creciendo, a veces con hueco)
            2 | 3 if !archivos.is_empty() => {
                let r = &archivos[az.hasta(archivos.len() as u64) as usize];
                let contenido = modelo.get_mut(r).unwrap().as_mut().unwrap();
                let off = az.hasta(contenido.len() as u64 + 40_000);
                let len = 1 + az.hasta(70_000) as usize;
                let b = az.sig() as u8;
                let datos: Vec<u8> = (0..len).map(|i| b.wrapping_add(i as u8)).collect();
                let ino = fs.buscar_numero(r).unwrap();
                fs.escribir(ino, off, &datos).unwrap_or_else(|e| panic!("op {paso}: escribir {r}: {e}"));
                let fin = off as usize + len;
                if contenido.len() < fin {
                    contenido.resize(fin, 0);
                }
                contenido[off as usize..fin].copy_from_slice(&datos);
            }
            // truncar
            4 if !archivos.is_empty() => {
                let r = &archivos[az.hasta(archivos.len() as u64) as usize];
                let contenido = modelo.get_mut(r).unwrap().as_mut().unwrap();
                let nuevo = az.hasta(contenido.len() as u64 * 2 + 10) as usize;
                let ino = fs.buscar_numero(r).unwrap();
                fs.truncar(ino, nuevo as u64).unwrap();
                contenido.resize(nuevo, 0);
            }
            // borrar archivo
            5 if !archivos.is_empty() => {
                let r = archivos[az.hasta(archivos.len() as u64) as usize].clone();
                let dino = fs.buscar_numero(padre(&r)).unwrap();
                fs.borrar(dino, nombre(&r)).unwrap();
                modelo.remove(&r);
            }
            // crear directorio
            6 => {
                let d = &dirs[az.hasta(dirs.len() as u64) as usize];
                let n = format!("d{}", az.hasta(1000));
                let r = if d == "/" { format!("/{n}") } else { format!("{d}/{n}") };
                let dino = fs.buscar_numero(d).unwrap();
                match fs.crear_directorio(dino, &n, 0o755) {
                    Ok(_) => {
                        modelo.insert(r, None);
                    }
                    Err(ErrorFs::YaExiste { .. }) => assert!(modelo.contains_key(&r)),
                    Err(e) => panic!("op {paso}: mkdir {r}: {e}"),
                }
            }
            // borrar directorio (error esperado si no está vacío)
            7 if dirs.len() > 1 => {
                let r = dirs[1 + az.hasta(dirs.len() as u64 - 1) as usize].clone();
                let vacio = !modelo.keys().any(|k| k.starts_with(&format!("{r}/")));
                let dino = fs.buscar_numero(padre(&r)).unwrap();
                match fs.borrar_directorio(dino, nombre(&r)) {
                    Ok(()) => {
                        assert!(vacio, "op {paso}: borró {r} con cosas dentro");
                        modelo.remove(&r);
                    }
                    Err(ErrorFs::DirectorioNoVacio) => assert!(!vacio, "op {paso}: {r} estaba vacío"),
                    Err(e) => panic!("op {paso}: rmdir {r}: {e}"),
                }
            }
            // renombrar/mover un archivo
            8 if !archivos.is_empty() => {
                let r = archivos[az.hasta(archivos.len() as u64) as usize].clone();
                let d = dirs[az.hasta(dirs.len() as u64) as usize].clone();
                let n = format!("m{}", az.hasta(10_000));
                let destino = if d == "/" { format!("/{n}") } else { format!("{d}/{n}") };
                if modelo.get(&destino).is_some_and(Option::is_none) {
                    continue; // destino es un directorio: otro caso
                }
                let (da, db) = (fs.buscar_numero(padre(&r)).unwrap(), fs.buscar_numero(&d).unwrap());
                fs.renombrar(da, nombre(&r), db, &n).unwrap_or_else(|e| panic!("op {paso}: mv {r} {destino}: {e}"));
                let c = modelo.remove(&r).unwrap();
                modelo.insert(destino, c);
            }
            _ => {
                // Errores inyectados: nombre inválido y duplicado no deben tocar nada.
                assert!(matches!(fs.crear_archivo(2, "x/y", 0o644), Err(ErrorFs::NombreInvalido)));
                if let Some(d) = dirs.get(1) {
                    let (p, n) = (fs.buscar_numero(padre(d)).unwrap(), nombre(d).to_string());
                    assert!(matches!(fs.crear_directorio(p, &n, 0o755), Err(ErrorFs::YaExiste { .. })));
                }
            }
        }
        cuenta[op as usize] += 1;
        if paso % 60 == 0 || paso == N {
            fs.cerrar_escritura().unwrap();
            drop(fs);
            limpio(&img, &format!("la operación {paso}"));
            fs = abrir(&img);
            comparar(&mut fs, &modelo, paso);
        }
    }
    fs.cerrar_escritura().unwrap();
    drop(fs);
    // Muestra con el lector INDEPENDIENTE.
    let mut vistos = 0;
    for (r, v) in modelo.iter().filter(|(_, v)| v.as_ref().is_some_and(|c| !c.is_empty())).take(8) {
        assert_eq!(&comun::volcar(&img, r), v.as_ref().unwrap(), "debugfs: {r}");
        vistos += 1;
    }
    let n_arch = modelo.values().filter(|v| v.is_some()).count();
    let n_dirs = modelo.len() - n_arch;
    eprintln!("estrés: {N} ops {cuenta:?}; quedan {n_arch} archivos y {n_dirs} directorios; {vistos} volcados por debugfs");
    assert!(n_arch >= 10 && vistos >= 5, "control D30: el modelo final debe tener contenido real");
    let _ = std::fs::remove_file(&img);
}

#[test]
fn llenar_el_disco_falla_limpio_sin_fugas() {
    let Some(img) = imagen(16) else {
        eprintln!("SALTADA: faltan e2fsprogs");
        return;
    };
    let mut fs = abrir(&img);
    let ino = fs.crear_archivo(2, "relleno", 0o644).unwrap();
    let trozo = vec![0xABu8; 1024 * 1024];
    let mut escrito = 0u64;
    let error = loop {
        match fs.escribir(ino, escrito, &trozo) {
            Ok(()) => escrito += trozo.len() as u64,
            Err(e) => break e,
        }
        assert!(escrito < 64 * 1024 * 1024, "nunca se llenó: el contador de libres miente");
    };
    assert!(matches!(error, ErrorFs::SinEspacio), "debe acabar en SinEspacio, fue {error}");
    // Tras el fallo, otras operaciones siguen funcionando y el FS sigue sano.
    fs.truncar(ino, escrito / 2).unwrap();
    fs.crear_archivo(2, "despues", 0o644).unwrap();
    fs.cerrar_escritura().unwrap();
    drop(fs);
    limpio(&img, "llenar el disco hasta SinEspacio");
    eprintln!("lleno tras {} MiB", escrito / (1024 * 1024));
    let _ = std::fs::remove_file(&img);
}
