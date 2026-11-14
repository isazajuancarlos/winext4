// SPDX-FileCopyrightText: 2026 Juan Carlos Isaza
// SPDX-License-Identifier: LicenseRef-Xiliux-Propietario
//! LECTURA SIN EL CANDADO DEL MOTOR: plan con el candado (`plan_lectura`), copia sin él desde la
//! caché compartida y comprobación de la ÉPOCA de liberación (`PlanLectura::ejecutar`).
//!
//! 1. Equivalencia: lo que da el plan es EXACTAMENTE lo que da `leer_rango` (archivos con
//!    huecos, rangos al azar, enlaces rápidos).
//! 2. La FUGA, determinista: planificar A, borrar A, crear B que reutiliza sus bloques y
//!    escribirlo (la caché queda con los datos de B), ejecutar el plan de A → hay que
//!    RECHAZARLO. Control D30: la copia sí trae datos de B (el peligro es real). Mutante
//!    verificado: sin subir la época al liberar, la prueba se pone roja.
//! 3. Carga concurrente (D8): un hilo escribe, borra y recrea archivos sin parar; varios leen
//!    sin candado. Ninguna lectura aceptada puede contener datos de OTRO archivo.

use std::fs::File;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use winext4_nucleo::cache::{CacheCompartida, ConCache};
use winext4_nucleo::{Ext4, PlanLectura};

type Fs = Ext4<ConCache<File>>;

fn montar(nombre: &str, mb: u64) -> (Fs, Arc<CacheCompartida>, std::path::PathBuf) {
    let img = std::env::temp_dir().join(format!("winext4-sincandado-{nombre}-{}.img", std::process::id()));
    let mut f = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&img).unwrap();
    f.set_len(mb * 1024 * 1024).unwrap();
    winext4_nucleo::formatear::formatear(&mut f, mb * 1024 * 1024, "SINCAND").unwrap();
    let cc = ConCache::con_unidades(f, 2048, 8192);
    let comp = cc.compartida();
    let mut fs = Ext4::abrir(cc).unwrap();
    fs.abrir_escritura().unwrap();
    (fs, comp, img)
}

struct Azar(u64);
impl Azar {
    fn n(&mut self, max: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % max.max(1)
    }
}

#[test]
fn el_plan_da_lo_mismo_que_leer_rango() {
    let (mut fs, comp, img) = montar("equiv", 64);
    let epoca = fs.epoca();
    let mut a = Azar(42);
    let mut probados = 0;
    for k in 0..12u32 {
        let n = fs.crear_archivo(2, &format!("f{k}"), 0o644).unwrap();
        // Contenido con huecos: escrituras sueltas lejos unas de otras.
        for _ in 0..6 {
            let off = a.n(600_000);
            let datos: Vec<u8> = (0..1 + a.n(70_000)).map(|i| (i % 250) as u8 + 1).collect();
            fs.escribir(n, off, &datos).unwrap();
        }
        let ino = fs.inodo(n).unwrap();
        let _ = fs.leer_rango(&ino, 0, ino.tamano as usize).unwrap(); // a la caché
        for _ in 0..40 {
            let off = a.n(ino.tamano + 10);
            let largo = 1 + a.n(150_000) as usize;
            let esperado = fs.leer_rango(&ino, off, largo).unwrap();
            let plan = fs.plan_lectura(&ino, off, largo).unwrap();
            let mut buf = vec![0xEEu8; largo];
            if let Some(m) = plan.ejecutar(&comp, &epoca, &mut buf) {
                assert_eq!(&buf[..m], &esperado[..], "archivo {k}, desde {off}, {largo} bytes");
                probados += 1;
            }
        }
    }
    // Enlace simbólico rápido: el destino sale del plan tal cual.
    let s = fs.crear_enlace_simbolico(2, "enl", "destino/corto").unwrap();
    let is = fs.inodo(s).unwrap();
    let mut buf = vec![0u8; 64];
    let m = fs.plan_lectura(&is, 0, 64).unwrap().ejecutar(&comp, &epoca, &mut buf).unwrap();
    assert_eq!(&buf[..m], b"destino/corto");
    assert!(probados > 400, "control D30: casi todas las lecturas salen de la caché ({probados})");
    drop(fs);
    let _ = std::fs::remove_file(img);
}

#[test]
fn un_plan_viejo_no_entrega_datos_de_otro_archivo() {
    let (mut fs, comp, img) = montar("fuga", 64);
    let epoca = fs.epoca();
    const LARGO: usize = 64 * 4096;
    let a = fs.crear_archivo(2, "a", 0o644).unwrap();
    fs.escribir(a, 0, &vec![0xA1; LARGO]).unwrap();
    let ia = fs.inodo(a).unwrap();
    let _ = fs.leer_rango(&ia, 0, LARGO).unwrap(); // los bloques de A, a la caché
    let plan = fs.plan_lectura(&ia, 0, LARGO).unwrap();
    let PlanLectura::Bloques { fisicos, .. } = &plan else { panic!("se esperaba un plan de bloques: {plan:?}") };
    let de_a: std::collections::HashSet<u64> = fisicos.iter().flatten().copied().collect();

    // «Mientras» el lector copia: A se borra y B reutiliza sus bloques.
    fs.borrar(2, "a").unwrap();
    let b = fs.crear_archivo(2, "b", 0o644).unwrap();
    fs.escribir(b, 0, &vec![0xB2; LARGO]).unwrap();
    let ib = fs.inodo(b).unwrap();
    let reusa = fs.bloques_fisicos(&ib).unwrap().into_iter().flatten().filter(|x| de_a.contains(x)).count();
    assert!(reusa > 0, "control D30: B tiene que reutilizar bloques de A");

    // Control D30: la copia en bruto SÍ trae datos de B (el peligro es real)…
    let PlanLectura::Bloques { fisicos, bs, ini, largo, .. } = &plan else { unreachable!() };
    let mut bruto = vec![0u8; *largo];
    assert!(comp.copiar_plan(fisicos, *bs, *ini, &mut bruto), "control: los bloques siguen en la caché");
    assert!(bruto.contains(&0xB2), "control D30: sin la época, la copia entregaría datos de B");
    // …y la lectura sin candado la RECHAZA.
    let mut buf = vec![0u8; LARGO];
    assert_eq!(plan.ejecutar(&comp, &epoca, &mut buf), None, "un plan anterior a la liberación no puede darse por bueno");
    drop(fs);
    let _ = std::fs::remove_file(img);
}

/// D8: un escritor que borra y recrea sin parar y 3 lectores sin candado durante ~3 s. Cada
/// archivo tiene su byte; una lectura aceptada solo puede contener el suyo o ceros (huecos y
/// zonas aún sin escribir). Positivo de control: hubo lecturas aceptadas Y rechazadas.
#[test]
fn carga_concurrente_sin_fugas() {
    let (fs, comp, img) = montar("carga", 96);
    let epoca = fs.epoca();
    let fs = Arc::new(Mutex::new(fs));
    let parar = Arc::new(AtomicBool::new(false));
    let (aceptadas, rechazadas) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
    const N: u8 = 6;
    const LARGO: usize = 40 * 4096;
    {
        let mut g = fs.lock().unwrap();
        for k in 0..N {
            let n = g.crear_archivo(2, &format!("f{k}"), 0o644).unwrap();
            g.escribir(n, 0, &vec![0x10 + k; LARGO]).unwrap();
        }
    }
    let escritor = {
        let (fs, parar) = (Arc::clone(&fs), Arc::clone(&parar));
        std::thread::spawn(move || {
            let mut a = Azar(7);
            while !parar.load(Ordering::SeqCst) {
                let k = a.n(N as u64) as u8;
                let mut g = fs.lock().unwrap();
                let nombre = format!("f{k}");
                let _ = g.borrar(2, &nombre);
                let n = g.crear_archivo(2, &nombre, 0o644).unwrap();
                g.escribir(n, 0, &vec![0x10 + k; LARGO - a.n(8) as usize * 4096]).unwrap();
            }
        })
    };
    let mut lectores = Vec::new();
    for h in 0..3u64 {
        let (fs, parar, comp, epoca) = (Arc::clone(&fs), Arc::clone(&parar), Arc::clone(&comp), Arc::clone(&epoca));
        let (aceptadas, rechazadas) = (Arc::clone(&aceptadas), Arc::clone(&rechazadas));
        lectores.push(std::thread::spawn(move || {
            let mut a = Azar(100 + h);
            let mut buf = vec![0u8; LARGO];
            while !parar.load(Ordering::SeqCst) {
                let k = a.n(N as u64) as u8;
                let plan = {
                    let mut g = fs.lock().unwrap();
                    let Ok(n) = g.buscar_siguiendo(&format!("/f{k}"), true) else { continue };
                    let ino = g.inodo(n).unwrap();
                    let _ = g.leer_rango(&ino, 0, LARGO); // a la caché
                    g.plan_lectura(&ino, 0, LARGO).unwrap()
                };
                // Sin el candado: dar tiempo al escritor a liberar y reutilizar.
                std::thread::sleep(Duration::from_micros(a.n(300)));
                match plan.ejecutar(&comp, &epoca, &mut buf) {
                    Some(m) => {
                        aceptadas.fetch_add(1, Ordering::Relaxed);
                        let ajeno = buf[..m].iter().find(|&&x| x != 0 && x != 0x10 + k);
                        assert!(ajeno.is_none(), "FUGA: el archivo f{k} devolvió el byte {:#x} de otro", ajeno.unwrap());
                    }
                    None => {
                        rechazadas.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }));
    }
    let fin = Instant::now() + Duration::from_secs(3);
    while Instant::now() < fin {
        std::thread::sleep(Duration::from_millis(50));
    }
    parar.store(true, Ordering::SeqCst);
    escritor.join().unwrap();
    for l in lectores {
        l.join().expect("un lector encontró una fuga");
    }
    let (ac, re) = (aceptadas.load(Ordering::SeqCst), rechazadas.load(Ordering::SeqCst));
    eprintln!("lecturas sin candado: {ac} aceptadas, {re} rechazadas por la época");
    assert!(ac > 50 && re > 0, "control D30: tiene que haber de las dos ({ac} aceptadas, {re} rechazadas)");
    drop(fs);
    let _ = std::fs::remove_file(img);
}
