// SPDX-FileCopyrightText: 2026 Juan Carlos Isaza
// SPDX-License-Identifier: LicenseRef-Xiliux-Propietario
//! `winext4` (Windows) — monta un ext4 como letra de unidad vía **Dokan**, a través de
//! **rustkan** (nuestra capa Rust sobre el driver firmado de Dokan 2.x: FFI, UTF-16, borrados
//! pendientes, atajo de patrón, Ctrl+C… viven allí, una vez, para winext4 y boveda).
//!
//! Aquí queda solo la traducción de cada operación de Windows a `winext4_nucleo`. Uso:
//! `winext4 <imagen-o-\\.\PhysicalDriveN> <M:\> [--offset <bytes>] [--escritura]`.

#[cfg(not(windows))]
fn main() {
    eprintln!(
        "winext4-windows solo se construye para Windows.\n\
         Cross-compilar desde Linux: cargo build -p winext4-windows --release --target x86_64-pc-windows-gnu"
    );
}

#[cfg(windows)]
fn main() {
    std::process::exit(win::ejecutar());
}

#[cfg(windows)]
mod win {
    use std::fs::File;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use rustkan::{
        atributos, banderas_volumen, Abierto, Busqueda, Disposicion, Entrada, Error, Escritura, Espacio, Info, Opciones,
        Peticion, Resultado, SistemaArchivos, Volumen,
    };
    use winext4_nucleo::cache::{CacheCompartida, ConCache};
    use winext4_nucleo::dispositivo::{offset_particion_ext4, parse_offset, DispositivoBloque};
    use winext4_nucleo::{Agrupado, ErrorFs, Ext4, Inodo, PlanLectura, RegistroLiberaciones};
    use std::sync::atomic::AtomicU64;

    /// Ventana del agrupado de transacciones: lo escrito se hace durable como mucho este tiempo
    /// después (Linux usa 5 s). Un corte de luz pierde, como mucho, esto; el volumen queda
    /// SIEMPRE consistente. `FlushFileBuffers` (guardar «a disco» de verdad) lo confirma ya.
    const VENTANA_AGRUPADO: Duration = Duration::from_secs(1);

    type Motor = Ext4<ConCache<DispositivoBloque<File>>>;

    /// Caché de bloques por defecto (MiB): metadatos y datos. `WINEXT4_CACHE_MB` y
    /// `WINEXT4_CACHE_DATOS_MB` la cambian; 0 apaga ese depósito.
    const CACHE_MB_POR_DEFECTO: usize = 64;
    const CACHE_DATOS_MB_POR_DEFECTO: usize = 512;

    fn mb_de(var: &str, defecto: usize) -> usize {
        match std::env::var(var) {
            Ok(v) => v.trim().parse().unwrap_or_else(|_| {
                eprintln!("{var}={v} no es un número de MiB: uso {defecto}");
                defecto
            }),
            Err(_) => defecto,
        }
    }

    /// Traduce un error del motor al de rustkan (y de ahí al NTSTATUS). Lo que el motor rechaza
    /// por no saber mantenerlo sale como «no soportado», nunca como un éxito silencioso (D20).
    fn error(e: &ErrorFs) -> Error {
        match e {
            ErrorFs::NoEncontrado { .. } => Error::NoEncontrado,
            ErrorFs::YaExiste { .. } => Error::YaExiste,
            ErrorFs::NombreInvalido => Error::NombreInvalido,
            ErrorFs::DirectorioNoVacio => Error::DirectorioNoVacio,
            ErrorFs::EsDirectorio => Error::EsDirectorio,
            ErrorFs::NoEsDirectorio => Error::NoEsDirectorio,
            ErrorFs::SinEspacio | ErrorFs::SinInodos => Error::DiscoLleno,
            ErrorFs::NoSoportado { .. } | ErrorFs::ArbolExtentsLleno => Error::NoSoportado,
            ErrorFs::SumaInvalida { .. } | ErrorFs::ExtentMagia { .. } | ErrorFs::Superbloque(_) => Error::Corrupto,
            ErrorFs::Io(_) => Error::Io,
            _ => Error::AccesoDenegado,
        }
    }

    fn m<T>(r: Result<T, ErrorFs>) -> Resultado<T> {
        r.map_err(|e| error(&e))
    }

    /// Parte `/a/b/c` en (`/a/b`, `c`).
    fn partir(ruta: &str) -> (String, String) {
        let r = ruta.trim_end_matches('/');
        match r.rfind('/') {
            Some(0) | None => ("/".to_string(), r.trim_start_matches('/').to_string()),
            Some(i) => (r[..i].to_string(), r[i + 1..].to_string()),
        }
    }

    fn hija(ruta: &str, nombre: &str) -> String {
        if ruta.ends_with('/') { format!("{ruta}{nombre}") } else { format!("{ruta}/{nombre}") }
    }

    /// Segundos Unix de ext4 → `SystemTime`.
    fn tiempo(seg: u32) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(u64::from(seg))
    }

    /// `SystemTime` → (segundos Unix, nanosegundos), con suelo (antes de 1970: segundos negativos
    /// y nanosegundos positivos), igual que la conversión de `FILETIME` de antes.
    fn a_unix(t: SystemTime) -> (i64, u32) {
        match t.duration_since(UNIX_EPOCH) {
            Ok(d) => (d.as_secs() as i64, d.subsec_nanos()),
            Err(e) => {
                let d = e.duration();
                let (s, ns) = (d.as_secs() as i64, d.subsec_nanos());
                if ns == 0 { (-s, 0) } else { (-s - 1, 1_000_000_000 - ns) }
            }
        }
    }

    /// ¿Ningún bit de escritura (`a-w`)? Es el «solo lectura» de Windows traducido a ext4.
    fn sin_escritura(i: &Inodo) -> bool {
        i.es_regular() && i.modo & 0o222 == 0
    }

    /// Atributos del inodo. El «solo lectura» del VOLUMEN lo añade rustkan.
    fn atributos_de(i: &Inodo) -> u32 {
        if i.es_directorio() {
            atributos::DIRECTORIO
        } else if sin_escritura(i) {
            atributos::SOLO_LECTURA
        } else {
            atributos::NORMAL
        }
    }

    fn entrada(nombre: String, i: &Inodo) -> Entrada {
        Entrada {
            nombre,
            atributos: atributos_de(i),
            tamano: i.tamano,
            creacion: tiempo(if i.crtime != 0 { i.crtime } else { i.ctime }),
            acceso: tiempo(i.atime),
            modificacion: tiempo(i.mtime),
        }
    }

    /// Identidad del archivo de un handle: (generación << 32) | inodo. 0 = nada guardado (los
    /// inodos empiezan en 1).
    fn contexto(fs: &mut Motor, n: u32) -> u64 {
        fs.inodo(n).map_or(0, |i| (u64::from(i.generacion) << 32) | u64::from(n))
    }

    /// El inodo de un handle: el que se guardó al ABRIRLO, si sigue siendo el mismo archivo
    /// (misma generación y con nombres; si se borró y el número se reutilizó, la generación
    /// cambia); si no, se resuelve la ruta. Ahorra recorrer los directorios del camino en CADA
    /// lectura o escritura (Windows trocea un archivo grande en muchas peticiones).
    fn ino_de(fs: &mut Motor, ruta: &str, c: u64) -> Result<u32, ErrorFs> {
        if c != 0 {
            let n = (c & 0xFFFF_FFFF) as u32;
            if fs.inodo(n).is_ok_and(|i| u64::from(i.generacion) == c >> 32 && i.enlaces > 0) {
                return Ok(n);
            }
        }
        fs.buscar_siguiendo(ruta, true)
    }

    /// El ext4 visto por rustkan. El motor va serializado (Dokan es multihilo; los métodos del
    /// motor son `&mut`). Se comparte con el temporizador del agrupado.
    struct Ext4Fs {
        motor: Arc<Mutex<Motor>>,
        escritura: bool,
        /// Los depósitos de la caché, para leer SIN el candado del motor.
        cache: Arc<CacheCompartida>,
        /// Registro de liberaciones del motor (comprobación tras leer sin candado).
        epoca: Arc<RegistroLiberaciones>,
        /// Lecturas servidas sin candado / con él (para medir).
        sin_candado: AtomicU64,
        con_candado: AtomicU64,
        /// Punto de montaje (para desmontarse solo si el motor queda envenenado).
        punto: String,
        /// ¿Ya se pidió el desmontaje por envenenamiento?
        envenenado: AtomicBool,
    }

    /// Nombre que, SOLO en compilaciones de depuración, hace entrar en pánico al motor con el
    /// candado tomado (para probar el desmontaje solo). El binario de release no lo contiene.
    #[cfg(debug_assertions)]
    const NOMBRE_PANICO_DE_PRUEBA: &str = "__winext4_panico_de_prueba__";

    impl Ext4Fs {
        /// Con el motor bloqueado. Un bloqueo ENVENENADO (un pánico en otra operación dejó el
        /// motor a medias) no deja hacer nada más — falla en cerrado: E/S — y además se pide UNA
        /// vez el DESMONTAJE, desde otro hilo (dentro de una llamada de Dokan podría esperarse a sí
        /// mismo): el proceso termina sin marcar el volumen limpio, el servicio lo relanza y el
        /// montaje nuevo reproduce el journal. Antes (2026-11-06) el volumen se quedaba inservible
        /// hasta reiniciar el equipo.
        fn con<R>(&self, f: impl FnOnce(&mut Motor) -> Resultado<R>) -> Resultado<R> {
            let mut g = match self.motor.lock() {
                Ok(g) => g,
                Err(_) => {
                    if !self.envenenado.swap(true, Ordering::SeqCst) {
                        eprintln!("ERROR: el motor quedó envenenado por un pánico; desmonto {} para que se recupere (journal) al volver a montar", self.punto);
                        let p = self.punto.clone();
                        std::thread::spawn(move || {
                            rustkan::desmontar(&p);
                        });
                    }
                    return Err(Error::Io);
                }
            };
            f(&mut g)
        }
    }

    impl SistemaArchivos for Ext4Fs {
        type Handle = u64;

        fn abrir(&self, p: &Peticion<'_>) -> Resultado<Abierto<u64>> {
            let ruta = p.ruta;
            self.con(|fs| {
                let existente = match fs.buscar_siguiendo(ruta, !p.abrir_enlace) {
                    Ok(n) => {
                        let i = m(fs.inodo(n))?;
                        // Un enlace abierto como tal es «de carpeta» si lo que alcanza es carpeta
                        // (el SYMLINKD de Windows): rmdir lo abre con FILE_DIRECTORY_FILE.
                        let es_dir = if i.es_enlace() {
                            fs.buscar_siguiendo(ruta, true).and_then(|t| fs.inodo(t)).is_ok_and(|t| t.es_directorio())
                        } else {
                            i.es_directorio()
                        };
                        Some((n, es_dir, i.es_enlace() || sin_escritura(&i)))
                    }
                    Err(ErrorFs::NoEncontrado { .. }) => None,
                    Err(e) => return Err(error(&e)),
                };
                match existente {
                    Some((n, es_dir, no_editable)) => {
                        // Un enlace no tiene contenido que escribir, y un archivo sin bits de
                        // escritura no se abre para escribir (como NTFS): se niega AL ABRIR (si
                        // se aceptara aquí, la caché de Windows daría la escritura por buena y el
                        // rechazo llegaría tarde, en la escritura diferida, sin que nadie lo vea).
                        if no_editable && p.escribe_contenido() {
                            return Err(Error::AccesoDenegado);
                        }
                        if p.solo_directorio && !es_dir {
                            return Err(Error::NoEsDirectorio);
                        }
                        if p.solo_archivo && es_dir {
                            return Err(Error::EsDirectorio);
                        }
                        if p.disposicion == Disposicion::Crear {
                            return Err(Error::YaExiste);
                        }
                        if p.disposicion.trunca() {
                            if !self.escritura {
                                return Err(Error::ProtegidoContraEscritura);
                            }
                            if es_dir {
                                return Err(Error::EsDirectorio);
                            }
                            m(fs.truncar(n, 0))?;
                        }
                        Ok(Abierto { handle: contexto(fs, n), es_directorio: es_dir, creado: false })
                    }
                    None => {
                        let (padre, nombre) = partir(ruta);
                        if !p.disposicion.puede_crear() {
                            return Err(if fs.buscar_siguiendo(&padre, true).is_ok() { Error::NoEncontrado } else { Error::RutaNoEncontrada });
                        }
                        if !self.escritura {
                            return Err(Error::ProtegidoContraEscritura);
                        }
                        let pd = match fs.buscar_siguiendo(&padre, true) {
                            Ok(d) => d,
                            Err(ErrorFs::NoEncontrado { .. }) => return Err(Error::RutaNoEncontrada),
                            Err(e) => return Err(error(&e)),
                        };
                        #[cfg(debug_assertions)]
                        if nombre == NOMBRE_PANICO_DE_PRUEBA {
                            panic!("pánico de PRUEBA con el candado del motor tomado");
                        }
                        let n = if p.solo_directorio { m(fs.crear_directorio(pd, &nombre, 0o755))? } else { m(fs.crear_archivo(pd, &nombre, 0o644))? };
                        Ok(Abierto { handle: contexto(fs, n), es_directorio: p.solo_directorio, creado: true })
                    }
                }
            })
        }

        fn leer(&self, ruta: &str, h: &u64, desplazamiento: u64, buf: &mut [u8]) -> Resultado<usize> {
            // 1) Con el candado, solo el PLAN (qué bloques: rápido, el mapa está en caché).
            // 2) SIN el candado, la copia desde la caché compartida, válida solo si ninguno de
            //    esos bloques se liberó entretanto (PlanLectura::ejecutar, probado con mutantes en
            //    tests/lectura_sin_candado.rs). Así las lecturas en caché van en PARALELO.
            let plan = self.con(|fs| {
                let n = m(ino_de(fs, ruta, *h))?;
                let i = m(fs.inodo(n))?;
                m(fs.plan_lectura(&i, desplazamiento, buf.len()))
            })?;
            if !matches!(plan, PlanLectura::Motor) {
                if let Some(k) = plan.ejecutar(&self.cache, &self.epoca, buf) {
                    self.sin_candado.fetch_add(1, Ordering::Relaxed);
                    return Ok(k);
                }
            }
            self.con_candado.fetch_add(1, Ordering::Relaxed);
            // 3) Si no estaba todo en caché (o cambió algo): el camino de siempre, con el
            //    candado, que además llena la caché. Solo el rango pedido.
            let datos = self.con(|fs| {
                let n = m(ino_de(fs, ruta, *h))?;
                let i = m(fs.inodo(n))?;
                m(fs.leer_rango(&i, desplazamiento, buf.len()))
            })?;
            let k = datos.len().min(buf.len());
            buf[..k].copy_from_slice(&datos[..k]);
            Ok(k)
        }

        fn escribir(&self, ruta: &str, h: &u64, e: Escritura, datos: &[u8]) -> Resultado<usize> {
            if !self.escritura {
                return Err(Error::ProtegidoContraEscritura);
            }
            self.con(|fs| {
                let ino = m(ino_de(fs, ruta, *h))?;
                let tam = m(fs.inodo(ino))?.tamano;
                // «Al final» y «la paginación nunca extiende» (lo dicta el gestor de caché).
                let (off, d) = e.resolver(tam, datos);
                if d.is_empty() {
                    return Ok(0);
                }
                m(fs.escribir(ino, off, d))?;
                Ok(d.len())
            })
        }

        /// `FlushFileBuffers`: se confirma el grupo de transacciones abierto (con agrupado; si
        /// no, ya estaba confirmado).
        fn vaciar(&self, _ruta: &str, _h: &u64) -> Resultado<()> {
            if !self.escritura {
                return Ok(());
            }
            // El grupo abierto al journal (con su barrera) y además TODO al disco: un dato escrito
            // sin cambio de metadatos no tiene otra barrera (la escritura ya no vacía cada vez).
            self.con(|fs| m(fs.confirmar_pendiente().and_then(|()| fs.sincronizar())))
        }

        fn info(&self, ruta: &str, h: &u64) -> Resultado<Info> {
            self.con(|fs| {
                let n = m(ino_de(fs, ruta, *h))?;
                let crudo = m(fs.inodo_crudo(n))?;
                let enlaces = u16::from_le_bytes([crudo[0x1A], crudo[0x1B]]);
                let i = m(fs.inodo(n))?;
                Ok(Info {
                    atributos: atributos_de(&i),
                    tamano: i.tamano,
                    creacion: tiempo(if i.crtime != 0 { i.crtime } else { i.ctime }),
                    acceso: tiempo(i.atime),
                    modificacion: tiempo(i.mtime),
                    enlaces: u32::from(enlaces.max(1)),
                    // El número de inodo es el identificador ESTABLE del archivo.
                    indice: u64::from(n),
                })
            })
        }

        fn listar(&self, ruta: &str, _h: &u64, rellenar: &mut dyn FnMut(Entrada)) -> Resultado<()> {
            let entradas = self.con(|fs| {
                let n = m(fs.buscar_siguiendo(ruta, true))?;
                let mut salida = Vec::new();
                for e in m(fs.entradas_dir(n))? {
                    if e.nombre == "." || e.nombre == ".." {
                        continue;
                    }
                    // Un enlace a ARCHIVO se lista como el archivo que alcanza; uno que no se
                    // puede seguir (a carpeta, absoluto, roto…), como él mismo: archivo de solo
                    // lectura con su destino de contenido. NUNCA como carpeta: ver
                    // `seguir_enlaces_a_carpetas`.
                    let objetivo = match fs.inodo(e.ino) {
                        Ok(i) if i.es_enlace() => fs.buscar_siguiendo(&hija(ruta, &e.nombre), true).and_then(|n| fs.inodo(n)),
                        otro => otro,
                    };
                    if let Ok(i) = objetivo {
                        salida.push(entrada(e.nombre, &i));
                    }
                }
                Ok(salida)
            })?;
            // Fuera del bloqueo del motor.
            for e in entradas {
                rellenar(e);
            }
            Ok(())
        }

        /// Windows busca un NOMBRE CONCRETO enumerando la carpeta con ese patrón (una vez por
        /// cada archivo creado): con el nombre exacto se contesta con esa sola entrada.
        fn buscar_nombre(&self, ruta: &str, _h: &u64, p: &str) -> Resultado<Busqueda> {
            self.con(|fs| {
                let n = m(fs.buscar_siguiendo(ruta, true))?;
                let Some(e) = m(fs.buscar_en(n, p))? else {
                    // No está TAL CUAL. Dokan filtra sin distinguir mayúsculas: si existe con
                    // otras, que decida él (NoSe); si no existe ni así, «ninguna» — sin leer un
                    // inodo por entrada. Comparación generosa (mayúsculas O minúsculas): ante la
                    // duda, al camino de siempre.
                    let mayus = |s: &str| s.chars().flat_map(char::to_uppercase).collect::<String>();
                    let minus = |s: &str| s.chars().flat_map(char::to_lowercase).collect::<String>();
                    let (pm, pn) = (mayus(p), minus(p));
                    // Rápido sin crear cadenas cuando ambos son ASCII; un nombre no ASCII se
                    // compara entero (en Windows «ı» en mayúscula es «I»).
                    let parecido = m(fs.entradas_dir(n))?.iter().any(|x| {
                        if p.is_ascii() && x.nombre.is_ascii() {
                            x.nombre.eq_ignore_ascii_case(p)
                        } else {
                            mayus(&x.nombre) == pm || minus(&x.nombre) == pn
                        }
                    });
                    return Ok(if parecido { Busqueda::NoSe } else { Busqueda::Ninguna });
                };
                let i = match m(fs.inodo(e.ino))? {
                    i if i.es_enlace() => {
                        let destino = m(fs.buscar_siguiendo(&hija(ruta, &e.nombre), true))?;
                        m(fs.inodo(destino))?
                    }
                    i => i,
                };
                Ok(Busqueda::Una(entrada(e.nombre, &i)))
            })
        }

        /// Del resto de atributos de DOS (oculto, sistema, archivo…) ext4 no guarda nada: se
        /// aceptan sin efecto. SOLO LECTURA sí tiene traducción exacta (la de Samba): marcarlo
        /// quita los bits de escritura (a-w); quitarlo devuelve el del dueño (u+w). En carpetas
        /// Windows lo usa para otra cosa (personalización), así que ahí no se traduce.
        fn poner_atributos(&self, ruta: &str, h: &u64, a: u32) -> Resultado<()> {
            let quiere_ro = a & atributos::SOLO_LECTURA != 0;
            self.con(|fs| {
                let n = m(ino_de(fs, ruta, *h))?;
                let i = m(fs.inodo(n))?;
                if !i.es_regular() {
                    return Ok(());
                }
                let p = i.modo & 0o7777;
                if quiere_ro && p & 0o222 != 0 {
                    m(fs.cambiar_permisos(n, p & !0o222))?;
                } else if !quiere_ro && p & 0o222 == 0 {
                    m(fs.cambiar_permisos(n, p | 0o200))?;
                }
                Ok(())
            })
        }

        /// Windows pone fechas (al COPIAR conserva la original; «Cambiar fecha»…): van al inodo.
        fn poner_tiempos(&self, ruta: &str, h: &u64, c: Option<SystemTime>, a: Option<SystemTime>, w: Option<SystemTime>) -> Resultado<()> {
            self.con(|fs| {
                let n = m(ino_de(fs, ruta, *h))?;
                m(fs.poner_tiempos(n, a.map(a_unix), w.map(a_unix), c.map(a_unix)))
            })
        }

        /// Windows pregunta si se PUEDE borrar; el borrado real llega en `borrar`, al cerrar.
        /// Sobre el NOMBRE (sin seguir el último enlace): borrar un enlace se puede siempre.
        fn puede_borrar(&self, ruta: &str, _h: &u64, es_dir: bool) -> Resultado<()> {
            self.con(|fs| {
                let n = m(fs.buscar_siguiendo(ruta, false))?;
                let i = m(fs.inodo(n))?;
                if !es_dir {
                    return if i.es_directorio() { Err(Error::EsDirectorio) } else { Ok(()) };
                }
                if i.es_enlace() {
                    return Ok(()); // se borra el enlace, no la carpeta a la que apunta
                }
                if !i.es_directorio() {
                    return Err(Error::NoEsDirectorio);
                }
                let hay = m(fs.listar(&i))?.iter().any(|e| e.nombre != "." && e.nombre != "..");
                if hay { Err(Error::DirectorioNoVacio) } else { Ok(()) }
            })
        }

        /// El borrado REAL (Windows ya lo dio por hecho; si falla, rustkan lo dice).
        fn borrar(&self, ruta: &str, _h: &u64, es_dir: bool) -> Resultado<()> {
            let (padre, nombre) = partir(ruta);
            self.con(|fs| {
                let p = m(fs.buscar_siguiendo(&padre, true))?;
                // Borrar un ENLACE borra el enlace, aunque Windows lo viera como carpeta.
                let es_enlace = fs.buscar_siguiendo(ruta, false).and_then(|n| fs.inodo(n)).is_ok_and(|i| i.es_enlace());
                m(if es_dir && !es_enlace { fs.borrar_directorio(p, &nombre) } else { fs.borrar(p, &nombre) })
            })
        }

        fn mover(&self, ruta: &str, _h: &u64, nueva: &str, reemplazar: bool) -> Resultado<()> {
            self.con(|fs| {
                if !reemplazar && fs.buscar_siguiendo(nueva, false).is_ok() {
                    return Err(Error::YaExiste);
                }
                let ((pa, na), (pb, nb)) = (partir(ruta), partir(nueva));
                let da = m(fs.buscar_siguiendo(&pa, true))?;
                let db = match fs.buscar_siguiendo(&pb, true) {
                    Ok(d) => d,
                    Err(ErrorFs::NoEncontrado { .. }) => return Err(Error::RutaNoEncontrada),
                    Err(e) => return Err(error(&e)),
                };
                m(fs.renombrar(da, &na, db, &nb))
            })
        }

        fn poner_tamano(&self, ruta: &str, h: &u64, tamano: u64) -> Resultado<()> {
            self.con(|fs| {
                let n = m(ino_de(fs, ruta, *h))?;
                m(fs.truncar(n, tamano))
            })
        }

        /// Reservar MÁS no cambia el tamaño visible (no preasignamos); reservar MENOS lo recorta.
        fn poner_reserva(&self, ruta: &str, h: &u64, tamano: u64) -> Resultado<()> {
            self.con(|fs| {
                let n = m(ino_de(fs, ruta, *h))?;
                if tamano < m(fs.inodo(n))?.tamano {
                    m(fs.truncar(n, tamano))?;
                }
                Ok(())
            })
        }

        fn espacio(&self) -> Resultado<Espacio> {
            let (total, libres) = self.con(|fs| Ok((fs.sb.tamano_bytes(), fs.bloques_libres() * fs.sb.tamano_bloque as u64)))?;
            // En solo lectura no hay espacio que ofrecer; con escritura, el real del superbloque.
            let disp = if self.escritura { libres } else { 0 };
            Ok(Espacio { disponible: disp, total, libre: disp })
        }

        fn volumen(&self) -> Resultado<Volumen> {
            let etiqueta = self.con(|fs| Ok(fs.sb.etiqueta.clone()))?;
            Ok(Volumen {
                nombre: if etiqueta.is_empty() { "winext4".to_string() } else { etiqueta },
                // Windows comprueba características por el nombre del FS; "NTFS" maximiza
                // compatibilidad.
                sistema: "NTFS".into(),
                serie: 0,
                max_componente: 255,
                // ext4 distingue mayúsculas y guarda nombres Unicode tal cual.
                banderas: banderas_volumen::DISTINGUE_MAYUSCULAS | banderas_volumen::CONSERVA_MAYUSCULAS | banderas_volumen::UNICODE_EN_DISCO,
            })
        }
    }

    pub fn ejecutar() -> i32 {
        let args: Vec<String> = std::env::args().collect();
        // `winext4 --desmontar W:\` — desmonta LIMPIO desde otra consola, sin administrador:
        // la instancia montada ve volver su montaje y repone la marca de limpio.
        if args.len() == 3 && args[1] == "--desmontar" {
            if rustkan::desmontar(&args[2]) {
                eprintln!("pedido el desmontaje de {}; la ventana de winext4 terminará y marcará el volumen limpio.", args[2]);
                return 0;
            }
            eprintln!("no se pudo desmontar {} (¿está montado por winext4?)", args[2]);
            return 1;
        }
        if args.len() < 3 {
            eprintln!(
                "uso: {} <imagen-o-\\\\.\\PhysicalDriveN> <M:\\> [--offset <bytes>] [--escritura]",
                args[0]
            );
            eprintln!("  imagen:  winext4 C:\\ruta\\disco.img M:\\");
            eprintln!("  disco:   winext4 \\\\.\\PhysicalDrive2 M:\\        (auto-detecta la partición ext4)");
            eprintln!("  offset:  winext4 \\\\.\\PhysicalDrive2 M:\\ --offset 17408   (o 0x4400)");
            eprintln!("  escribir:  winext4 C:\\ruta\\disco.img M:\\ --escritura");
            eprintln!("  escribir en un DISCO FÍSICO: añade --autorizo-disco-fisico (verifica antes y lo abre en exclusiva)");
            return 1;
        }
        let ruta = args[1].clone();
        let punto_montaje = args[2].clone();

        let mut offset_explicito: Option<u64> = None;
        let mut pide_escritura = false;
        let mut autorizo_fisico = false;
        let mut dueno: Option<String> = None;
        let mut i = 3;
        while i < args.len() {
            match args[i].as_str() {
                "--escritura" => pide_escritura = true,
                "--autorizo-disco-fisico" => autorizo_fisico = true,
                "--dueno" => {
                    i += 1;
                    match args.get(i) {
                        Some(c) => dueno = Some(c.clone()),
                        None => {
                            eprintln!("--dueno requiere el nombre de una cuenta de Windows");
                            return 1;
                        }
                    }
                }
                "--offset" => {
                    i += 1;
                    match args.get(i).and_then(|s| parse_offset(s)) {
                        Some(o) => offset_explicito = Some(o),
                        None => {
                            eprintln!("--offset requiere un número de bytes (decimal o 0xHEX)");
                            return 1;
                        }
                    }
                }
                otro => {
                    eprintln!("argumento no reconocido: {otro}");
                    return 1;
                }
            }
            i += 1;
        }

        // Un disco físico crudo exige alinear al sector; una imagen en archivo no.
        let es_dispositivo = ruta.to_ascii_lowercase().contains("physicaldrive");
        let sec = if es_dispositivo { 4096 } else { 1 };

        // ESCRITURA SOBRE UN DISCO FÍSICO: exige pedirlo DOS veces (--escritura y
        // --autorizo-disco-fisico). Un disco real tiene datos que no se rehacen; una bandera sola
        // se pone por costumbre. Las imágenes no lo exigen.
        if pide_escritura && es_dispositivo && !autorizo_fisico {
            eprintln!(
                "--escritura sobre un DISCO FÍSICO exige además --autorizo-disco-fisico. Antes de \
                 montar se verifica el volumen entero (0 errores o se niega) y el disco se abre en \
                 EXCLUSIVA (si otro programa, p. ej. el servicio, lo tiene abierto, se niega)."
            );
            return 1;
        }

        let mut abrir = std::fs::OpenOptions::new();
        abrir.read(true).write(pide_escritura);
        if pide_escritura && es_dispositivo {
            // EXCLUSIVA: dos montajes del mismo disco (este con escritura y, p. ej., el servicio
            // en solo lectura) tendrían cachés distintas del mismo volumen → corrupción. Sin
            // compartir, si otro lo tiene abierto la apertura FALLA aquí, y mientras esté abierto
            // nadie más lo abre.
            use std::os::windows::fs::OpenOptionsExt;
            abrir.share_mode(0);
        }
        let archivo = match abrir.open(&ruta) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("no se pudo abrir {ruta}: {e}");
                if es_dispositivo && pide_escritura && e.raw_os_error() == Some(32) {
                    eprintln!("(otro programa tiene abierto el disco — ¿el servicio winext4 lo monta en solo lectura? Quítalo de montajes.txt o para el servicio)");
                } else if es_dispositivo {
                    eprintln!("(abrir un disco físico requiere ejecutar la consola como ADMINISTRADOR)");
                }
                return 1;
            }
        };
        let mut disp = DispositivoBloque::nuevo(archivo, 0, sec);

        // Determinar dónde empieza el ext4 dentro del dispositivo.
        let base = match offset_explicito {
            Some(o) => o,
            None if es_dispositivo => match offset_particion_ext4(&mut disp) {
                Ok(Some(o)) => {
                    eprintln!("partición Linux/ext4 detectada en el offset {o} bytes");
                    o
                }
                Ok(None) => {
                    eprintln!(
                        "no hallé una partición Linux/ext4 en {ruta}. Da el offset a mano con \
                         --offset <bytes> (el campo «Offset» de `Get-Partition`)."
                    );
                    return 1;
                }
                Err(e) => {
                    eprintln!("no pude leer la tabla de particiones de {ruta}: {e}");
                    return 1;
                }
            },
            None => 0, // imagen: el ext4 empieza en el byte 0
        };
        disp.reposicionar(base);
        // Caché de bloques de ESCRITURA DIRECTA (nucleo/src/cache.rs): sobre un disco físico
        // Windows no pone ninguna, y el motor relee los mismos metadatos en cada operación.
        let (mb, mb_datos) = (mb_de("WINEXT4_CACHE_MB", CACHE_MB_POR_DEFECTO), mb_de("WINEXT4_CACHE_DATOS_MB", CACHE_DATOS_MB_POR_DEFECTO));
        eprintln!("caché de bloques: {mb} MiB de metadatos, {mb_datos} MiB de datos");
        let disp = ConCache::con_mb_datos(disp, mb, mb_datos);
        let cache = disp.compartida();

        let mut motor = match Ext4::abrir(disp) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("{ruta} (offset {base}) no es un ext4 legible: {e}");
                return 1;
            }
        };
        // Los enlaces a CARPETA no se siguen en Windows. Dokan no deja anunciarlos como enlace
        // (la etiqueta de reparse no llega al listado), y entonces `Remove-Item -Recurse` o el
        // Explorador ENTRAN en ellos al borrar y vacían el destino: medido el 2026-11-05, la
        // carpeta destino perdió su contenido. Se ven como archivo de solo lectura con su destino.
        motor.seguir_enlaces_a_carpetas = false;
        if pide_escritura && motor.journal_pendiente() {
            // Tras un corte: reproducir lo confirmado, como Linux al montar.
            match motor.recuperar_journal() {
                Ok(inf) => eprintln!(
                    "journal recuperado: {} transacciones, {} bloques reproducidos.",
                    inf.transacciones, inf.bloques_reproducidos
                ),
                Err(e) => {
                    eprintln!("no se pudo recuperar el journal: {e}");
                    return 1;
                }
            }
        }
        // Verificación PREVIA en disco físico (no se escribe sobre un volumen con errores: se
        // amplificarían). La RÁPIDA siempre (segundos: sumas de superbloque, descriptores y
        // bitmaps, marca de errores, raíz); la COMPLETA (minutos en un disco grande) solo con
        // motivo: marca de errores, errores registrados DESPUÉS de la última completa, última
        // completa de más de DIAS_VERIFICACION días, o un fallo de la rápida. Tras una completa
        // limpia se anota la fecha (como e2fsck).
        const DIAS_VERIFICACION: u64 = 30;
        let mut completa_hecha = false;
        if pide_escritura && es_dispositivo && motor.sb.estado & 0x2 != 0 {
            // Con la marca de errores no se escribe aunque verificar saliera limpio (el motor se
            // niega igual): no se gastan minutos en una verificación completa inútil.
            eprintln!(
                "el volumen tiene la MARCA DE ERRORES (ERROR_FS): NO se monta con escritura. \
                 Repáralo (`winext4util reparar ... --autorizo-disco-fisico --deshacer <archivo>` o \
                 e2fsck en Linux/WSL), que es quien la quita."
            );
            return 1;
        }
        if pide_escritura && es_dispositivo {
            let negarse = |n: usize| {
                eprintln!(
                    "el volumen tiene {n} errores: NO se monta con escritura. Revísalo con \
                     `winext4util verificar` y repáralo (`winext4util reparar ... --deshacer`)."
                );
            };
            eprintln!("comprobación rápida del volumen…");
            let rapida = match motor.verificar_rapido() {
                Ok(inf) => inf,
                Err(e) => {
                    eprintln!("no se pudo comprobar el volumen: {e}");
                    return 1;
                }
            };
            let ahora = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
            let motivo = if rapida.errores() > 0 {
                Some(format!("la comprobación rápida encontró {} errores", rapida.errores()))
            } else {
                motor.motivo_verificacion_completa(ahora, DIAS_VERIFICACION)
            };
            match motivo {
                None => eprintln!("comprobación rápida: 0 errores; verificación completa reciente y sin errores nuevos."),
                Some(m) => {
                    eprintln!("verificación COMPLETA antes de escribir — {m}. Puede tardar varios minutos en un disco grande…");
                    match motor.verificar() {
                        Ok(inf) if inf.errores() == 0 => {
                            eprintln!("verificado: 0 errores.");
                            completa_hecha = true;
                        }
                        Ok(inf) => {
                            negarse(inf.errores());
                            return 1;
                        }
                        Err(e) => {
                            eprintln!("no se pudo verificar el volumen: {e}");
                            return 1;
                        }
                    }
                }
            }
        }
        if pide_escritura {
            // La puerta del motor: se niega ante lo que no sabe mantener, y escribe por el journal.
            if let Err(e) = motor.abrir_escritura() {
                eprintln!("no se puede montar con escritura: {e}");
                eprintln!("(puedes montarlo en solo lectura quitando --escritura)");
                return 1;
            }
            // Agrupado: muchas operaciones por transacción (2 barreras por grupo, no por
            // operación). Durable a más tardar VENTANA_AGRUPADO después, o al pedirlo.
            let max_bloques = 2048;
            if let Err(e) = motor.agrupar_transacciones(Some(Agrupado { max_espera: VENTANA_AGRUPADO, max_bloques })) {
                eprintln!("no se pudo activar el agrupado: {e}");
                return 1;
            }
            if completa_hecha {
                if let Err(e) = motor.anotar_verificacion_completa().and_then(|()| motor.confirmar_pendiente()) {
                    eprintln!("no se pudo anotar la verificación: {e}");
                    return 1;
                }
            }
        }
        let epoca = motor.epoca();
        let motor = Arc::new(Mutex::new(motor));

        // Temporizador: confirma el grupo abierto cuando vence la ventana, aunque no llegue
        // ninguna operación más (sin él, lo último escrito podría quedar en memoria). Se PARA
        // antes de cerrar el volumen.
        let parar = Arc::new(AtomicBool::new(false));
        let temporizador = pide_escritura.then(|| {
            let (motor, parar) = (Arc::clone(&motor), Arc::clone(&parar));
            std::thread::spawn(move || {
                while !parar.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(250));
                    let r = motor.lock().map(|mut fs| if fs.grupo_vencido() { fs.confirmar_pendiente() } else { Ok(()) });
                    if let Ok(Err(e)) = r {
                        eprintln!("ERROR al confirmar el grupo de transacciones: {e}");
                    }
                }
            })
        });

        let mut op = Opciones::new(punto_montaje.clone());
        // Dueño de los archivos: el de la cuenta pedida. Sin `--dueno`, el de Dokan por defecto
        // (el usuario del proceso que monta: SYSTEM si es el servicio). Una cuenta que no existe
        // NO cae al defecto: se niega a montar (D20).
        if let Some(c) = &dueno {
            match rustkan::sid_de_cuenta(c) {
                Ok(sid) => {
                    eprintln!("dueño de los archivos: {c} ({sid})");
                    op.seguridad = Some(rustkan::sddl_con_dueno(&sid));
                }
                Err(e) => {
                    eprintln!("--dueno {c}: {e}");
                    return 1;
                }
            }
        }
        op.solo_lectura = !pide_escritura;
        op.gestor_montajes = true;
        // Ctrl+C / cerrar la ventana: DESMONTAR (así el montaje vuelve y `cerrar_escritura`
        // repone la marca de limpio), en vez de morir con el volumen marcado «no limpio».
        op.desmontar_al_cortar = true;
        // Lotes IPC de Dokan: solo si se pide (medirlo antes; ver rustkan::Opciones::lotes_ipc).
        op.lotes_ipc = std::env::var_os("WINEXT4_LOTES_IPC").is_some_and(|v| v == "1");

        let modo = if pide_escritura { "LECTURA Y ESCRITURA" } else { "solo lectura" };
        eprintln!("montando {ruta} en {punto_montaje} ({modo})… Ctrl+C desmonta limpio.");
        let fs = Ext4Fs {
            motor: Arc::clone(&motor),
            escritura: pide_escritura,
            cache,
            epoca,
            sin_candado: AtomicU64::new(0),
            con_candado: AtomicU64::new(0),
            punto: punto_montaje,
            envenenado: AtomicBool::new(false),
        };
        let (r, fs) = match rustkan::montar(fs, op) {
            Ok(fs) => (0, fs),
            Err(f) => {
                eprintln!("no se montó: {}", f.error);
                (1, f.fs)
            }
        };
        eprintln!(
            "lecturas: {} sin candado (desde la caché), {} con candado",
            fs.sin_candado.load(Ordering::Relaxed),
            fs.con_candado.load(Ordering::Relaxed)
        );
        drop(fs);
        parar.store(true, Ordering::SeqCst);
        if let Some(t) = temporizador {
            let _ = t.join();
        }
        if let Ok(fs) = motor.lock() {
            let (ac, fa) = fs.dispositivo().estadisticas();
            eprintln!("caché de bloques: {ac} aciertos, {fa} lecturas al disco");
        }
        if let Some(p) = rustkan::informe_perfil() {
            eprint!("{p}");
        }
        if pide_escritura {
            let cierre = match motor.lock() {
                Ok(mut fs) => fs.cerrar_escritura().map_err(|e| e.to_string()),
                Err(_) => Err("motor envenenado por un pánico".to_string()),
            };
            match cierre {
                Ok(()) => eprintln!("desmontado: volumen marcado LIMPIO."),
                Err(e) => {
                    eprintln!("AVISO: no se pudo marcar el volumen limpio ({e}); pásale e2fsck antes de usarlo en Linux.");
                    return 2;
                }
            }
        }
        r
    }

    #[cfg(test)]
    mod pruebas {
        use super::{a_unix, partir, tiempo};
        use std::time::{Duration, UNIX_EPOCH};

        #[test]
        fn tiempos_de_ext4_ida_y_vuelta() {
            assert_eq!(tiempo(0), UNIX_EPOCH);
            assert_eq!(a_unix(tiempo(1_000_000_000)), (1_000_000_000, 0));
            // SystemTime de Windows tiene resolución de 100 ns (FILETIME): múltiplos de 100.
            assert_eq!(a_unix(UNIX_EPOCH + Duration::new(5, 1_234_500)), (5, 1_234_500));
            // Antes de 1970: suelo, como la conversión de FILETIME de antes.
            assert_eq!(a_unix(UNIX_EPOCH - Duration::new(1, 500_000_000)), (-2, 500_000_000));
            assert_eq!(a_unix(UNIX_EPOCH - Duration::from_secs(3)), (-3, 0));
        }

        #[test]
        fn partir_rutas() {
            assert_eq!(partir("/a/b/c"), ("/a/b".into(), "c".into()));
            assert_eq!(partir("/c"), ("/".into(), "c".into()));
            assert_eq!(partir("/a/b/"), ("/a".into(), "b".into()));
        }
    }
}
