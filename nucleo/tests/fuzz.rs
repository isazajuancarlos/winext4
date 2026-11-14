// SPDX-FileCopyrightText: 2026 Juan Carlos Isaza
// SPDX-License-Identifier: LicenseRef-Xiliux-Propietario
//! H: FUZZING de lectura, verificación, recuperación, reparación y escritura sobre imágenes
//! DAÑADAS a propósito. Determinista (semilla fija: cada fallo se reproduce con su número).
//!
//! Lo único que se exige: que NADA reviente. Devolver un error ante un disco dañado es lo
//! correcto; un PÁNICO, un CUELGUE o una reserva de memoria desbocada son fallos.
//!
//! - `fuzz_corto`: 1500 casos (corre en el banco normal).
//! - `fuzz_largo` (ignorado): `WINEXT4_FUZZ_CASOS=200000 cargo test --release --test fuzz -- --ignored`.

use std::io::Cursor;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::mpsc;
use std::time::Duration;

use winext4_nucleo::{csum, Ext4, INODO_RAIZ};

type Fs = Ext4<Cursor<Vec<u8>>>;
const BS: usize = 4096;

/// xorshift64.
struct Azar(u64);
impl Azar {
    fn sig(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn hasta(&mut self, n: u64) -> u64 {
        self.sig() % n.max(1)
    }
}

/// Semilla: 16 MiB formateados por winext4 y poblados con lo que ejercita más caminos.
fn semilla() -> (Vec<u8>, Vec<u64>) {
    let mut img = Cursor::new(vec![0u8; 16 << 20]);
    winext4_nucleo::formatear::formatear(&mut img, 16 << 20, "FUZZ").unwrap();
    let mut fs = Ext4::abrir(img).unwrap();
    fs.abrir_escritura().unwrap();
    let d = fs.crear_directorio(INODO_RAIZ, "dir", 0o755).unwrap();
    for i in 0..150 {
        let n = fs.crear_archivo(d, &format!("archivo-con-nombre-largo-{i:03}"), 0o644).unwrap();
        if i % 10 == 0 {
            fs.escribir(n, 0, &vec![b'x'; 3000 + i * 50]).unwrap();
        }
    }
    let sub = fs.crear_directorio(d, "sub", 0o755).unwrap();
    let frag = fs.crear_archivo(sub, "fragmentado", 0o644).unwrap();
    for k in 0..12u64 {
        fs.escribir(frag, k * 2 * BS as u64, &vec![k as u8; BS]).unwrap(); // huecos → profundidad 1
    }
    let h = fs.crear_archivo(INODO_RAIZ, "con-hueco", 0o644).unwrap();
    fs.escribir(h, 100_000, b"tras el hueco").unwrap();
    fs.cerrar_escritura().unwrap();
    // Bloques «interesantes» (metadatos) donde concentrar las mutaciones.
    let mut objetivo: Vec<u64> = vec![0, 1];
    let d0 = fs.descriptor(0).unwrap();
    objetivo.extend([d0.bitmap_bloques(), d0.bitmap_inodos(), d0.tabla_inodos(), d0.tabla_inodos() + 1]);
    for r in ["/", "/dir", "/dir/sub"] {
        let ino = fs.buscar(r).unwrap();
        objetivo.extend(fs.bloques_fisicos(&ino).unwrap().into_iter().flatten());
    }
    let ino = fs.buscar("/dir/sub/fragmentado").unwrap();
    objetivo.extend(fs.bloques_de_arbol(&ino).unwrap());
    let j = fs.inodo(8).unwrap();
    objetivo.extend(fs.bloques_fisicos(&j).unwrap().into_iter().flatten().take(3));
    let imagen = fs.dispositivo().get_ref().clone();
    (imagen, objetivo)
}

const INTERESANTES: [u32; 10] = [0, 1, 2, 0x7F, 0xFF, 0xFFFF, 0x7FFF_FFFF, 0x8000_0000, 0xFFFF_FFFF, 4096];

/// Aplica una mutación (y a veces rehace la suma, para que el daño llegue más hondo).
fn mutar(img: &mut [u8], objetivo: &[u64], az: &mut Azar) {
    let bloque = if az.hasta(10) < 9 { objetivo[az.hasta(objetivo.len() as u64) as usize] } else { az.hasta((img.len() / BS) as u64) };
    let base = bloque as usize * BS;
    if base + BS > img.len() {
        return;
    }
    match az.hasta(4) {
        0 => {
            let o = az.hasta(BS as u64) as usize;
            img[base + o] ^= (1 + az.hasta(255)) as u8;
        }
        1 => {
            let o = (az.hasta(BS as u64 / 4) * 4) as usize;
            let v = INTERESANTES[az.hasta(INTERESANTES.len() as u64) as usize];
            img[base + o..base + o + 4].copy_from_slice(&v.to_le_bytes());
        }
        2 => {
            let o = (az.hasta(BS as u64 / 2) * 2) as usize;
            let v = INTERESANTES[az.hasta(INTERESANTES.len() as u64) as usize] as u16;
            img[base + o..base + o + 2].copy_from_slice(&v.to_le_bytes());
        }
        _ => {
            let otro = objetivo[az.hasta(objetivo.len() as u64) as usize] as usize * BS;
            if otro + BS <= img.len() {
                let copia = img[otro..otro + BS].to_vec();
                img[base..base + BS].copy_from_slice(&copia);
            }
        }
    }
    // Rehacer la suma del superbloque la mitad de las veces (si se mutó el bloque 0).
    if bloque == 0 && az.hasta(2) == 0 {
        let c = csum::superbloque(&img[1024..2048]);
        img[1024 + 0x3FC..2048].copy_from_slice(&c.to_le_bytes());
    }
}

/// Todo lo que winext4 hace con un volumen; ningún resultado importa, solo que no reviente.
fn ejercitar(img: Vec<u8>) -> (bool, bool) {
    let mut alcance = (false, false);
    let abrir = |v: Vec<u8>| -> Option<Fs> { Ext4::abrir(Cursor::new(v)).ok() };
    if let Some(mut fs) = abrir(img.clone()) {
        alcance.0 = true;
        let _ = fs.verificar();
        // Recorrido con visitados (un directorio que se contiene a sí mismo no es un bucle).
        let mut pila = vec![INODO_RAIZ];
        let mut vistos = std::collections::HashSet::new();
        while let Some(d) = pila.pop() {
            if !vistos.insert(d) || vistos.len() > 400 {
                continue;
            }
            let Ok(di) = fs.inodo(d) else { continue };
            let Ok(es) = fs.listar(&di) else { continue };
            for e in es.into_iter().take(300) {
                if e.nombre == "." || e.nombre == ".." {
                    continue;
                }
                let Ok(i) = fs.inodo(e.ino) else { continue };
                if i.es_directorio() {
                    pila.push(e.ino);
                } else if i.es_enlace() {
                    let _ = fs.leer_enlace(&i);
                } else {
                    let _ = fs.leer_rango(&i, 0, 65536);
                    let _ = fs.leer_rango(&i, i.tamano.saturating_sub(100), 4096);
                }
            }
        }
    }
    if let Some(mut fs) = abrir(img.clone()) {
        let _ = fs.recuperar_journal();
    }
    if let Some(mut fs) = abrir(img.clone()) {
        let _ = fs.reparar();
    }
    if let Some(mut fs) = abrir(img) {
        if fs.abrir_escritura().is_ok() {
            alcance.1 = true;
            if let Ok(d) = fs.crear_directorio(INODO_RAIZ, "fz", 0o755) {
                if let Ok(n) = fs.crear_archivo(d, "a", 0o644) {
                    let _ = fs.escribir(n, 0, &[7u8; 10_000]);
                    let _ = fs.escribir(n, 50_000, b"hueco");
                    let _ = fs.truncar(n, 3000);
                    let _ = fs.renombrar(d, "a", INODO_RAIZ, "b");
                    let _ = fs.borrar(INODO_RAIZ, "b");
                }
            }
            let _ = fs.borrar(INODO_RAIZ, "con-hueco");
            if let Ok(d) = fs.buscar_numero("/dir") {
                let _ = fs.borrar(d, "archivo-con-nombre-largo-010");
                let _ = fs.crear_archivo(d, "nuevo-en-dir", 0o644);
            }
            let _ = fs.cerrar_escritura();
        }
    }
    alcance
}

fn fuzz(casos: u64, semilla_azar: u64) {
    let (base, objetivo) = semilla();
    // Control D30: la semilla SIN mutar recorre todo sin errores (si no, el fuzz no mide nada).
    {
        let mut fs = Ext4::abrir(Cursor::new(base.clone())).unwrap();
        assert_eq!(fs.verificar().unwrap().errores(), 0, "la semilla debe estar sana");
    }
    let mut az = Azar(semilla_azar);
    let mut fallos: Vec<String> = Vec::new();
    let (mut abiertos, mut escritos) = (0u64, 0u64);
    for caso in 0..casos {
        let mut img = base.clone();
        let n_mut = 1 + az.hasta(4);
        for _ in 0..n_mut {
            mutar(&mut img, &objetivo, &mut az);
        }
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let r = catch_unwind(AssertUnwindSafe(|| ejercitar(img)));
            let _ = tx.send(r.ok());
        });
        match rx.recv_timeout(Duration::from_secs(20)) {
            Ok(Some((a, e))) => {
                abiertos += a as u64;
                escritos += e as u64;
            }
            Ok(None) => fallos.push(format!("caso {caso}: PÁNICO")),
            Err(_) => {
                fallos.push(format!("caso {caso}: COLGADO (> 20 s)"));
                break; // el hilo colgado no se puede matar: se para aquí
            }
        }
        if fallos.len() >= 10 {
            break;
        }
    }
    assert!(fallos.is_empty(), "{} fallos (semilla {semilla_azar:#x}):\n{}", fallos.len(), fallos.join("\n"));
    // Control D30: una buena parte de los mutantes debe pasar la apertura y llegar a escribir;
    // si no, el fuzz solo estaría probando el rechazo del superbloque.
    assert!(abiertos * 3 > casos && escritos * 5 > casos, "alcance pobre: {abiertos} abiertos, {escritos} con escritura de {casos}");
    eprintln!("fuzz: {casos} casos sin pánicos ni cuelgues; {abiertos} abiertos, {escritos} llegaron a escribir (semilla {semilla_azar:#x})");
}

#[test]
fn fuzz_corto() {
    fuzz(1500, 0x00F0_220D_2026_1105);
}

#[test]
#[ignore = "largo: WINEXT4_FUZZ_CASOS=n cargo test --release --test fuzz -- --ignored"]
fn fuzz_largo() {
    let n = std::env::var("WINEXT4_FUZZ_CASOS").ok().and_then(|s| s.parse().ok()).unwrap_or(50_000);
    let s = std::env::var("WINEXT4_FUZZ_SEMILLA").ok().and_then(|s| u64::from_str_radix(s.trim_start_matches("0x"), 16).ok()).unwrap_or(0xC0FF_EE00_1234_5678);
    fuzz(n, s);
}
