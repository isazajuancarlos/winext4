// SPDX-FileCopyrightText: 2026 Juan Carlos Isaza
// SPDX-License-Identifier: LicenseRef-Xiliux-Propietario
//! **Caché de bloques** (`ConCache<D>`): envoltorio de cualquier dispositivo `Read + Write +
//! Seek`, como [`crate::dispositivo::ConDeshacer`].
//!
//! Por qué existe (medido el 2026-11-05): sobre un disco físico Windows NO pone caché entre el
//! motor y el disco, y Dokan TAMPOCO cachea los datos de los archivos (su driver no inicializa
//! el Cache Manager: `PrivateCacheMap = NULL`, ver `rustkan/DOKAN.md`). Sin caché propia, el
//! motor releía los mismos metadatos en cada operación y cada relectura de un archivo volvía al
//! disco.
//!
//! **Dos depósitos**, para que los datos no echen a los metadatos:
//! - **metadatos**: lo que el motor lee en trozos de hasta [`MAX_CACHEABLE`] (inodos, directorios,
//!   bitmaps, árbol de extents… y archivos pequeños);
//! - **datos**: lo que se lee en trozos grandes (contenido de archivos).
//!
//! **Escritura DIRECTA al disco (write-through), nunca diferida**: cada escritura llega al
//! dispositivo ANTES de tocar la caché, y la caché solo se actualiza si el dispositivo la
//! aceptó; si falla, las unidades tocadas se INVALIDAN (no se sabe qué quedó escrito). La caché
//! nunca tiene nada que el disco no tenga: durabilidad, journal y barreras, como antes.
//!
//! **Compartida**: los depósitos viven en una [`CacheCompartida`] (`Arc`) que el host usa para
//! leer SIN el candado del motor ([`CacheCompartida::copiar_plan`]); el motor la usa por debajo
//! de `Read`/`Write`. Quien lee sin candado tiene que comprobar después la ÉPOCA de liberación
//! del motor (un bloque liberado y reasignado entre el plan y la copia sería de OTRO archivo).
//!
//! Supuesto (el de toda la escritura de winext4): el motor es el ÚNICO que escribe en el
//! dispositivo mientras está abierto (el host lo abre en exclusiva para escribir).

use std::collections::HashMap;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

/// Tamaño de la unidad de caché.
pub const UNIDAD: u64 = 4096;
/// Lecturas de hasta esto van al depósito de METADATOS; las mayores, al de DATOS.
pub const MAX_CACHEABLE: usize = 64 * 1024;

struct Ranura {
    clave: u64,
    datos: Box<[u8]>,
    usada: bool,
}

/// Un depósito con reemplazo CLOCK.
struct Deposito {
    indice: HashMap<u64, usize>,
    ranuras: Vec<Ranura>,
    mano: usize,
    max: usize,
}

impl Deposito {
    fn nuevo(max: usize) -> Self {
        Deposito { indice: HashMap::new(), ranuras: Vec::new(), mano: 0, max }
    }

    fn buscar(&mut self, clave: u64) -> Option<&[u8]> {
        let r = *self.indice.get(&clave)?;
        self.ranuras[r].usada = true;
        Some(&self.ranuras[r].datos)
    }

    fn contiene(&self, clave: u64) -> bool {
        self.indice.contains_key(&clave)
    }

    fn insertar(&mut self, clave: u64, datos: Box<[u8]>) {
        if self.max == 0 {
            return;
        }
        if let Some(&r) = self.indice.get(&clave) {
            self.ranuras[r].datos = datos;
            self.ranuras[r].usada = true;
            return;
        }
        if self.ranuras.len() < self.max {
            self.ranuras.push(Ranura { clave, datos, usada: true });
            self.indice.insert(clave, self.ranuras.len() - 1);
            return;
        }
        loop {
            let r = self.mano;
            self.mano = (self.mano + 1) % self.max;
            if self.ranuras[r].usada {
                self.ranuras[r].usada = false;
                continue;
            }
            self.indice.remove(&self.ranuras[r].clave);
            self.ranuras[r] = Ranura { clave, datos, usada: true };
            self.indice.insert(clave, r);
            return;
        }
    }

    /// Parchea `bytes` en la unidad `clave` desde `desde`, si está.
    fn parchear(&mut self, clave: u64, desde: usize, bytes: &[u8]) -> bool {
        match self.indice.get(&clave) {
            Some(&r) => {
                self.ranuras[r].datos[desde..desde + bytes.len()].copy_from_slice(bytes);
                true
            }
            None => false,
        }
    }

    fn invalidar(&mut self, clave: u64) {
        if let Some(r) = self.indice.remove(&clave) {
            // La ranura queda libre para el CLOCK: clave imposible y sin uso.
            self.ranuras[r].clave = u64::MAX;
            self.ranuras[r].usada = false;
        }
    }
}

struct Depositos {
    meta: Deposito,
    datos: Deposito,
}

impl Depositos {
    /// La unidad `clave` de cualquiera de los dos depósitos.
    fn buscar(&mut self, clave: u64) -> Option<&[u8]> {
        if self.meta.contiene(clave) {
            return self.meta.buscar(clave);
        }
        self.datos.buscar(clave)
    }
}

/// Los depósitos y sus contadores, compartibles entre el motor y quien lee sin candado.
pub struct CacheCompartida {
    depositos: Mutex<Depositos>,
    aciertos: AtomicU64,
    fallos: AtomicU64,
}

impl CacheCompartida {
    fn bloquear(&self) -> MutexGuard<'_, Depositos> {
        // Un pánico con el candado tomado no deja datos a medias: cada operación sobre un
        // depósito es completa o no ocurrió. Se sigue con lo que hay.
        self.depositos.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Copia en `buf` el rango de un plan de lectura SI TODOS sus bloques están en caché.
    /// `fisicos` son los bloques físicos del rango lógico (`None` = hueco, se lee como ceros);
    /// `bs` el tamaño de bloque; `ini` dónde empieza `buf` dentro del primer bloque. Devuelve
    /// `false` (sin garantía sobre `buf`) si falta alguno o si `bs` no es la unidad de la caché.
    ///
    /// No basta con que devuelva `true`: quien lo usa SIN el candado del motor debe comprobar
    /// después que la época de liberación no cambió (ver el módulo).
    pub fn copiar_plan(&self, fisicos: &[Option<u64>], bs: u64, ini: usize, buf: &mut [u8]) -> bool {
        if bs != UNIDAD {
            return false;
        }
        let u = UNIDAD as usize;
        let mut d = self.bloquear();
        let mut hecho = 0usize;
        let mut k = ini / u;
        let mut dentro = ini % u;
        while hecho < buf.len() {
            let Some(f) = fisicos.get(k) else { return false };
            let m = (u - dentro).min(buf.len() - hecho);
            match f {
                None => buf[hecho..hecho + m].fill(0),
                Some(p) => match d.buscar(p * UNIDAD) {
                    Some(datos) => buf[hecho..hecho + m].copy_from_slice(&datos[dentro..dentro + m]),
                    None => {
                        self.fallos.fetch_add(1, Ordering::Relaxed);
                        return false;
                    }
                },
            }
            hecho += m;
            k += 1;
            dentro = 0;
        }
        self.aciertos.fetch_add(1, Ordering::Relaxed);
        true
    }
}

/// Caché de bloques de escritura directa sobre `D`.
pub struct ConCache<D> {
    disp: D,
    pos: u64,
    comp: Arc<CacheCompartida>,
}

impl<D: Read + Write + Seek> ConCache<D> {
    /// Envuelve `disp` con `max_unidades` unidades de [`UNIDAD`] bytes para METADATOS y ninguna
    /// para datos (0 = sin caché).
    pub fn nuevo(disp: D, max_unidades: usize) -> Self {
        Self::con_unidades(disp, max_unidades, 0)
    }

    /// Unidades para metadatos y para datos.
    pub fn con_unidades(disp: D, meta: usize, datos: usize) -> Self {
        Self {
            disp,
            pos: 0,
            comp: Arc::new(CacheCompartida {
                depositos: Mutex::new(Depositos { meta: Deposito::nuevo(meta), datos: Deposito::nuevo(datos) }),
                aciertos: AtomicU64::new(0),
                fallos: AtomicU64::new(0),
            }),
        }
    }

    /// Caché de `mb` MiB para metadatos (sin depósito de datos).
    pub fn con_mb(disp: D, mb: usize) -> Self {
        Self::con_mb_datos(disp, mb, 0)
    }

    /// `meta_mb` MiB para metadatos y `datos_mb` MiB para datos.
    pub fn con_mb_datos(disp: D, meta_mb: usize, datos_mb: usize) -> Self {
        let por_mb = 1024 * 1024 / UNIDAD as usize;
        Self::con_unidades(disp, meta_mb * por_mb, datos_mb * por_mb)
    }

    /// Los depósitos, para leer sin el candado del motor.
    pub fn compartida(&self) -> Arc<CacheCompartida> {
        Arc::clone(&self.comp)
    }

    /// El dispositivo de debajo.
    pub fn interno(&self) -> &D {
        &self.disp
    }

    /// (aciertos, fallos) de lectura por unidad, para medir.
    pub fn estadisticas(&self) -> (u64, u64) {
        (self.comp.aciertos.load(Ordering::Relaxed), self.comp.fallos.load(Ordering::Relaxed))
    }

    fn leer_completo(&mut self, desde: u64, buf: &mut [u8]) -> io::Result<usize> {
        self.disp.seek(SeekFrom::Start(desde))?;
        let mut n = 0;
        while n < buf.len() {
            match self.disp.read(&mut buf[n..]) {
                Ok(0) => break,
                Ok(k) => n += k,
                Err(ref e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(n)
    }

    /// Lectura GRANDE: de la caché si están todas las unidades; si no, del disco, guardando las
    /// unidades enteras en el depósito de DATOS.
    fn leer_grande(&mut self, ini: u64, buf: &mut [u8]) -> io::Result<usize> {
        let u = UNIDAD;
        if ini.is_multiple_of(u) {
            let fisicos: Vec<Option<u64>> = (0..(buf.len() as u64).div_ceil(u)).map(|k| Some(ini / u + k)).collect();
            if self.comp.copiar_plan(&fisicos, u, 0, buf) {
                return Ok(buf.len());
            }
        }
        let n = self.leer_completo(ini, buf)?;
        let mut d = self.comp.bloquear();
        if d.datos.max > 0 {
            let primera = ini.div_ceil(u) * u;
            let mut clave = primera;
            while clave + u <= ini + n as u64 {
                let o = (clave - ini) as usize;
                if !d.meta.contiene(clave) {
                    d.datos.insertar(clave, buf[o..o + u as usize].to_vec().into_boxed_slice());
                }
                clave += u;
            }
        }
        Ok(n)
    }

    /// Quita de los dos depósitos toda unidad que toque `[desde, desde+largo)`.
    fn invalidar(&mut self, desde: u64, largo: usize) {
        let mut d = self.comp.bloquear();
        let mut k = desde - desde % UNIDAD;
        let fin = desde + largo as u64;
        while k < fin {
            d.meta.invalidar(k);
            d.datos.invalidar(k);
            k += UNIDAD;
        }
    }
}

impl<D: Read + Write + Seek> Read for ConCache<D> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let ini = self.pos;
        let (meta_max, datos_max) = {
            let d = self.comp.bloquear();
            (d.meta.max, d.datos.max)
        };
        if buf.len() > MAX_CACHEABLE || meta_max == 0 {
            let n = if datos_max > 0 { self.leer_grande(ini, buf)? } else { self.leer_completo(ini, buf)? };
            self.pos += n as u64;
            return Ok(n);
        }
        let u = UNIDAD as usize;
        let mut hecho = 0usize;
        while hecho < buf.len() {
            let abs = ini + hecho as u64;
            let clave = abs - abs % UNIDAD;
            let off = (abs - clave) as usize;
            let m = (u - off).min(buf.len() - hecho);
            {
                let mut d = self.comp.bloquear();
                if let Some(datos) = d.buscar(clave) {
                    buf[hecho..hecho + m].copy_from_slice(&datos[off..off + m]);
                    self.comp.aciertos.fetch_add(1, Ordering::Relaxed);
                    hecho += m;
                    continue;
                }
            }
            self.comp.fallos.fetch_add(1, Ordering::Relaxed);
            let mut b = vec![0u8; u].into_boxed_slice();
            let n = self.leer_completo(clave, &mut b)?;
            if n < u {
                // Final del dispositivo: lo que haya, sin guardarlo.
                let disp = n.saturating_sub(off).min(buf.len() - hecho);
                buf[hecho..hecho + disp].copy_from_slice(&b[off..off + disp]);
                hecho += disp;
                break;
            }
            buf[hecho..hecho + m].copy_from_slice(&b[off..off + m]);
            self.comp.bloquear().meta.insertar(clave, b);
            hecho += m;
        }
        self.pos += hecho as u64;
        Ok(hecho)
    }
}

impl<D: Read + Write + Seek> Write for ConCache<D> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let ini = self.pos;
        // 1) Al DISPOSITIVO primero. Si falla, lo tocado deja de ser de fiar en la caché.
        let r = self.disp.seek(SeekFrom::Start(ini)).and_then(|_| self.disp.write_all(buf));
        if let Err(e) = r {
            self.invalidar(ini, buf.len());
            return Err(e);
        }
        // 2) Solo entonces, la caché: se parchea lo que ya estaba (en cualquier depósito); una
        //    unidad que una escritura PEQUEÑA cubre entera se guarda en metadatos.
        let fin = ini + buf.len() as u64;
        let mut d = self.comp.bloquear();
        let mut clave = ini - ini % UNIDAD;
        while clave < fin {
            let a = clave.max(ini);
            let b = (clave + UNIDAD).min(fin);
            let (en_buf, en_uni) = ((a - ini) as usize, (a - clave) as usize);
            let largo = (b - a) as usize;
            let trozo = &buf[en_buf..en_buf + largo];
            let en_meta = d.meta.parchear(clave, en_uni, trozo);
            let en_datos = d.datos.parchear(clave, en_uni, trozo);
            if !en_meta && !en_datos && largo == UNIDAD as usize && buf.len() <= MAX_CACHEABLE {
                d.meta.insertar(clave, trozo.to_vec().into_boxed_slice());
            }
            clave += UNIDAD;
        }
        drop(d);
        self.pos += buf.len() as u64;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.disp.flush()
    }
}

impl<D: Read + Write + Seek> Seek for ConCache<D> {
    fn seek(&mut self, de: SeekFrom) -> io::Result<u64> {
        self.pos = match de {
            SeekFrom::Start(n) => n,
            SeekFrom::Current(d) => self.pos.saturating_add_signed(d),
            SeekFrom::End(_) => return Err(io::Error::new(io::ErrorKind::Unsupported, "ConCache: SeekFrom::End no soportado")),
        };
        Ok(self.pos)
    }
}

#[cfg(test)]
mod pruebas {
    use super::*;
    use std::io::Cursor;

    /// Generador determinista (xorshift) para la prueba diferencial.
    struct Azar(u64);
    impl Azar {
        fn n(&mut self, max: u64) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0 % max.max(1)
        }
    }

    fn leer_en<R: Read + Seek>(d: &mut R, desde: u64, largo: usize) -> Vec<u8> {
        d.seek(SeekFrom::Start(desde)).unwrap();
        let mut v = vec![0u8; largo];
        let mut n = 0;
        while n < largo {
            match d.read(&mut v[n..]).unwrap() {
                0 => break,
                k => n += k,
            }
        }
        v.truncate(n);
        v
    }

    /// DIFERENCIAL: miles de lecturas y escrituras al azar (desalineadas, cruzando unidades,
    /// grandes y pequeñas) contra un modelo plano; depósitos diminutos para forzar el
    /// reemplazo, con y sin depósito de datos. Cada lectura debe ser idéntica al modelo, y al
    /// final el dispositivo también.
    #[test]
    fn diferencial_contra_un_modelo_plano() {
        for (semilla, meta, datos) in [(1u64, 3usize, 0usize), (7, 17, 5), (99, 64, 64), (12345, 1, 1), (5, 8, 200)] {
            let tam = 512 * 1024usize;
            let mut modelo: Vec<u8> = (0..tam).map(|i| (i * 31 % 251) as u8).collect();
            let mut c = ConCache::con_unidades(Cursor::new(modelo.clone()), meta, datos);
            let mut a = Azar(semilla);
            for _ in 0..4000 {
                let largo = match a.n(10) {
                    0 => 1 + a.n(200_000) as usize, // grande: depósito de datos
                    1..=3 => 4096,
                    _ => 1 + a.n(9000) as usize,
                };
                let desde = if a.n(4) == 0 { a.n(100) * 4096 } else { a.n((tam - largo.min(tam - 1)) as u64) };
                let largo = largo.min(tam - desde as usize);
                if a.n(3) == 0 {
                    let datos: Vec<u8> = (0..largo).map(|_| a.n(256) as u8).collect();
                    c.seek(SeekFrom::Start(desde)).unwrap();
                    c.write_all(&datos).unwrap();
                    modelo[desde as usize..desde as usize + largo].copy_from_slice(&datos);
                } else {
                    let v = leer_en(&mut c, desde, largo);
                    assert_eq!(v, modelo[desde as usize..desde as usize + largo], "lectura distinta del modelo (semilla {semilla}, meta {meta}, datos {datos}, desde {desde}, largo {largo})");
                }
            }
            assert_eq!(c.interno().get_ref(), &modelo, "el dispositivo debe tener lo mismo que el modelo");
            let (ac, fa) = c.estadisticas();
            assert!(ac > 0 && fa > 0, "control: la caché se usó ({ac} aciertos, {fa} fallos)");
        }
    }

    /// Lecturas repetidas de lo mismo NO vuelven al disco (lo que la caché promete).
    #[test]
    fn releer_no_vuelve_al_disco() {
        let mut c = ConCache::nuevo(Cursor::new(vec![7u8; 64 * 1024]), 8);
        for _ in 0..100 {
            assert_eq!(leer_en(&mut c, 4096 + 10, 300), vec![7u8; 300]);
        }
        assert_eq!(c.estadisticas(), (99, 1));
    }

    /// Una lectura GRANDE se guarda en el depósito de datos y la segunda sale de la caché (el
    /// dispositivo de debajo se cambia por debajo para PROBAR que no se volvió a leer).
    #[test]
    fn lectura_grande_se_guarda_en_datos() {
        let mut c = ConCache::con_unidades(Cursor::new(vec![1u8; 1024 * 1024]), 4, 64);
        assert_eq!(leer_en(&mut c, 0, 128 * 1024), vec![1u8; 128 * 1024]);
        c.disp.get_mut().fill(9); // si se volviera a leer del disco, saldría 9
        assert_eq!(leer_en(&mut c, 0, 128 * 1024), vec![1u8; 128 * 1024], "la relectura sale de la caché");
        // Pareja: sin depósito de datos, la relectura va al disco.
        let mut s = ConCache::con_unidades(Cursor::new(vec![1u8; 1024 * 1024]), 4, 0);
        let _ = leer_en(&mut s, 0, 128 * 1024);
        s.disp.get_mut().fill(9);
        assert_eq!(leer_en(&mut s, 0, 128 * 1024), vec![9u8; 128 * 1024], "control: sin depósito de datos no hay caché de datos");
    }

    #[test]
    fn copiar_plan_con_huecos_y_fallos() {
        let mut disco = vec![0u8; 64 * 1024];
        for (i, b) in disco.iter_mut().enumerate() {
            *b = (i / 4096) as u8 + 1; // cada bloque, su número + 1
        }
        let mut c = ConCache::con_unidades(Cursor::new(disco), 16, 16);
        let _ = leer_en(&mut c, 0, 16 * 4096); // todo a la caché (lectura grande)
        let comp = c.compartida();
        let mut buf = vec![0u8; 4096 + 200];
        // Bloques lógicos: [3, hueco]; desde el byte 100 del primero.
        assert!(comp.copiar_plan(&[Some(3), None], 4096, 100, &mut buf));
        assert!(buf[..3996].iter().all(|&x| x == 4) && buf[3996..].iter().all(|&x| x == 0));
        // Un bloque que no está → false.
        assert!(!comp.copiar_plan(&[Some(200)], 4096, 0, &mut buf[..10]));
        // Otro tamaño de bloque → false (no se adivina).
        assert!(!comp.copiar_plan(&[Some(3)], 1024, 0, &mut buf[..10]));
    }

    /// Dispositivo que falla las escrituras: la caché NO puede quedarse con lo que no llegó.
    struct Falla {
        d: Cursor<Vec<u8>>,
        fallar: bool,
        ya_parcial: bool,
    }
    impl Read for Falla {
        fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
            self.d.read(b)
        }
    }
    impl Seek for Falla {
        fn seek(&mut self, p: SeekFrom) -> io::Result<u64> {
            self.d.seek(p)
        }
    }
    impl Write for Falla {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            if self.fallar {
                // Falla A MEDIAS (el caso peligroso): la primera mitad llega, el resto no.
                if b.len() > 1 && !self.ya_parcial {
                    self.ya_parcial = true;
                    return self.d.write(&b[..b.len() / 2]);
                }
                return Err(io::Error::other("disco caído"));
            }
            self.d.write(b)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn una_escritura_fallida_no_queda_en_la_cache() {
        let mut c = ConCache::nuevo(Falla { d: Cursor::new(vec![1u8; 32 * 1024]), fallar: false, ya_parcial: false }, 8);
        assert_eq!(leer_en(&mut c, 8192, 16), vec![1u8; 16]); // la unidad entra en la caché
        c.disp.fallar = true;
        c.seek(SeekFrom::Start(8192)).unwrap();
        assert!(c.write_all(&[9u8; 16]).is_err());
        c.disp.fallar = false;
        // Lo que se lee es EXACTAMENTE lo del disco (media escritura llegó), no lo que había
        // en la caché antes ni lo que se intentó escribir.
        let disco = c.interno().d.get_ref()[8192..8192 + 16].to_vec();
        assert_eq!(&disco[..8], &[9u8; 8], "control: la mitad SÍ llegó al disco");
        assert_eq!(leer_en(&mut c, 8192, 16), disco);
        // Pareja: una escritura que SÍ llega se lee.
        c.seek(SeekFrom::Start(8192)).unwrap();
        c.write_all(&[5u8; 16]).unwrap();
        assert_eq!(leer_en(&mut c, 8192, 16), vec![5u8; 16]);
    }

    #[test]
    fn capacidad_cero_es_paso_directo() {
        let mut c = ConCache::nuevo(Cursor::new(vec![3u8; 16 * 1024]), 0);
        assert_eq!(leer_en(&mut c, 100, 5000), vec![3u8; 5000]);
        c.seek(SeekFrom::Start(100)).unwrap();
        c.write_all(&[4u8; 10]).unwrap();
        assert_eq!(leer_en(&mut c, 100, 10), vec![4u8; 10]);
        assert_eq!(c.estadisticas(), (0, 0));
    }

    #[test]
    fn final_del_dispositivo_lee_lo_que_hay() {
        let mut c = ConCache::nuevo(Cursor::new(vec![2u8; 10_000]), 8);
        assert_eq!(leer_en(&mut c, 9000, 4000), vec![2u8; 1000]);
        assert_eq!(leer_en(&mut c, 9000, 500), vec![2u8; 500]);
    }
}
