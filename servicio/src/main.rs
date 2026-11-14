// SPDX-FileCopyrightText: 2026 Juan Carlos Isaza
// SPDX-License-Identifier: LicenseRef-Xiliux-Propietario
//! `winext4svc` — servicio de Windows que monta al arrancar las unidades ext4 listadas en
//! `C:\ProgramData\winext4\montajes.txt`:
//!
//! ```text
//! # disco o imagen            letra   modo                    [offset] [dueno=cuenta]
//! \\.\PhysicalDrive2           Z:      lectura
//! D:\imagenes\datos.img        W:      escritura
//! \\.\PhysicalDrive3           X:      escritura-disco-fisico  dueno=juan
//! ```
//!
//! `dueno=cuenta`: los archivos aparecen como de esa cuenta (sin él, como de SYSTEM, que es
//! quien corre el servicio; git los rechaza por «dubious ownership»).
//!
//! Un DISCO FÍSICO solo se monta con escritura con la palabra `escritura-disco-fisico`
//! (`escritura` a secas se rechaza para un disco): el montador verifica el volumen antes y lo abre
//! en exclusiva.
//!
//! Un proceso `winext4.exe` POR MONTAJE (el fallo de uno no tumba a los demás; si muere solo, se
//! relanza hasta 3 veces). Al parar el servicio o apagar el equipo, cada unidad se desmonta
//! LIMPIO (`winext4 --desmontar`) antes de terminar. Corre como LocalSystem; las letras se ven
//! para todos los usuarios (gestor de montajes de Dokan).
//!
//! `winext4svc --consola` hace lo mismo en primer plano (para probar sin instalar).

#[cfg(not(windows))]
fn main() {
    eprintln!("winext4svc solo existe en Windows");
}

#[cfg(windows)]
fn main() {
    std::process::exit(svc::ejecutar());
}

#[cfg(any(windows, test))]
mod config {
    /// Un montaje de `montajes.txt`.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Montaje {
        pub dispositivo: String,
        pub letra: String,
        pub escritura: bool,
        pub offset: Option<String>,
        /// Cuenta dueña de los archivos (`dueno=cuenta`).
        pub dueno: Option<String>,
    }

    /// Interpreta `montajes.txt`. Líneas vacías y `#` se ignoran; una línea mal formada es un
    /// ERROR con su número (D20: no se monta «lo que se pueda» en silencio).
    ///
    /// # Errors
    /// Mensaje con la línea que no se entiende.
    pub fn leer(texto: &str) -> Result<Vec<Montaje>, String> {
        let mut v = Vec::new();
        for (i, l) in texto.lines().enumerate() {
            let l = l.trim();
            if l.is_empty() || l.starts_with('#') {
                continue;
            }
            let p: Vec<&str> = l.split_whitespace().collect();
            if p.len() < 3 || p.len() > 5 {
                return Err(format!("línea {}: se esperan «dispositivo letra modo [offset] [dueno=cuenta]»: {l}", i + 1));
            }
            let letra = p[1].trim_end_matches('\\').to_ascii_uppercase();
            let b = letra.as_bytes();
            if b.len() != 2 || !b[0].is_ascii_uppercase() || b[1] != b':' {
                return Err(format!("línea {}: letra de unidad inválida «{}»", i + 1, p[1]));
            }
            let fisico = p[0].to_ascii_lowercase().contains("physicaldrive");
            let escritura = match p[2].to_ascii_lowercase().as_str() {
                "lectura" | "ro" => false,
                "escritura" | "rw" if fisico => {
                    return Err(format!(
                        "línea {}: «{}» es un DISCO FÍSICO: para escribir en él el modo es «escritura-disco-fisico» \
                         (a propósito distinto: un disco con datos no se desbloquea por una palabra corriente)",
                        i + 1,
                        p[2]
                    ))
                }
                "escritura" | "rw" => true,
                "escritura-disco-fisico" if fisico => true,
                "escritura-disco-fisico" => return Err(format!("línea {}: «escritura-disco-fisico» es solo para \\\\.\\PhysicalDriveN; para una imagen, «escritura»", i + 1)),
                otro => return Err(format!("línea {}: modo «{otro}» (usa lectura, escritura o escritura-disco-fisico)", i + 1)),
            };
            if v.iter().any(|m: &Montaje| m.letra == letra) {
                return Err(format!("línea {}: la letra {letra} ya está usada", i + 1));
            }
            // El dispositivo va como ARGUMENTO al montador: nada que empiece por «-» (se tomaría
            // por una opción) y el offset solo numérico (decimal o 0xHEX).
            if p[0].starts_with('-') {
                return Err(format!("línea {}: un dispositivo no puede empezar por «-»", i + 1));
            }
            // Tras el modo: como mucho un offset y un `dueno=`, en cualquier orden.
            let (mut offset, mut dueno) = (None, None);
            for o in &p[3..] {
                if let Some(c) = o.strip_prefix("dueno=") {
                    // Va como ARGUMENTO al montador: nombre de cuenta corriente, nada que empiece
                    // por «-» ni lleve comillas o separadores de órdenes.
                    let valida = !c.is_empty() && !c.starts_with('-') && c.chars().all(|x| x.is_alphanumeric() || "._-\\$ ".contains(x));
                    if !valida || dueno.is_some() {
                        return Err(format!("línea {}: «{o}» no es un dueño válido (dueno=cuenta, una vez)", i + 1));
                    }
                    dueno = Some(c.to_string());
                    continue;
                }
                let ok = o.strip_prefix("0x").or_else(|| o.strip_prefix("0X")).map_or_else(|| o.parse::<u64>().is_ok(), |h| u64::from_str_radix(h, 16).is_ok());
                if !ok || offset.is_some() {
                    return Err(format!("línea {}: el offset «{o}» no es un número", i + 1));
                }
                offset = Some((*o).to_string());
            }
            v.push(Montaje { dispositivo: p[0].to_string(), letra, escritura, offset, dueno });
        }
        Ok(v)
    }
}

#[cfg(windows)]
mod svc {
    use std::ffi::c_void;
    use std::io::Write;
    use std::path::PathBuf;
    use std::process::{Child, Command};
    use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
    use std::time::{Duration, Instant};

    use super::config::{self, Montaje};

    const NOMBRE: &str = "winext4";
    const SERVICE_WIN32_OWN_PROCESS: u32 = 0x10;
    const SERVICE_STOPPED: u32 = 1;
    const SERVICE_START_PENDING: u32 = 2;
    const SERVICE_STOP_PENDING: u32 = 3;
    const SERVICE_RUNNING: u32 = 4;
    const SERVICE_ACCEPT_STOP: u32 = 0x1;
    const SERVICE_ACCEPT_SHUTDOWN: u32 = 0x4;
    const CONTROL_STOP: u32 = 1;
    const CONTROL_INTERROGATE: u32 = 4;
    const CONTROL_SHUTDOWN: u32 = 5;
    const NO_ERROR: u32 = 0;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    #[repr(C)]
    struct ServiceStatus {
        tipo: u32,
        estado: u32,
        controles: u32,
        salida_win32: u32,
        salida_servicio: u32,
        punto_control: u32,
        espera: u32,
    }

    #[repr(C)]
    struct EntradaTabla {
        nombre: *mut u16,
        proc_: Option<unsafe extern "system" fn(u32, *mut *mut u16)>,
    }

    #[link(name = "advapi32")]
    extern "system" {
        fn StartServiceCtrlDispatcherW(tabla: *const EntradaTabla) -> i32;
        fn RegisterServiceCtrlHandlerExW(
            nombre: *const u16,
            manejador: unsafe extern "system" fn(u32, u32, *mut c_void, *mut c_void) -> u32,
            ctx: *mut c_void,
        ) -> isize;
        fn SetServiceStatus(h: isize, st: *const ServiceStatus) -> i32;
    }

    static PARAR: AtomicBool = AtomicBool::new(false);
    static MANEJADOR: AtomicIsize = AtomicIsize::new(0);

    fn ancho(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// Configuración y registros viven en la carpeta DEL PROPIO SERVICIO (`C:\Program Files\
    /// winext4`), que solo los administradores pueden escribir. NO en `C:\ProgramData`: ahí
    /// cualquier usuario puede crear archivos, y un `montajes.txt` plantado haría que el servicio
    /// (SYSTEM) montara con escritura lo que ese usuario quisiera — escalada de privilegios.
    fn carpeta() -> PathBuf {
        std::env::current_exe().ok().and_then(|p| p.parent().map(std::path::Path::to_path_buf)).unwrap_or_else(|| PathBuf::from(r"C:\Program Files\winext4"))
    }

    // ---- Comprobación del DACL: nadie que no sea administrador puede escribir aquí ----
    const SE_FILE_OBJECT: u32 = 1;
    const DACL_SECURITY_INFORMATION: u32 = 0x4;
    const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
    /// Derechos que permiten cambiar el contenido o la seguridad de un archivo/carpeta.
    const DERECHOS_DE_ESCRITURA: u32 = 0x0002 | 0x0004 | 0x0010 | 0x0100 | 0x0001_0000 | 0x0004_0000 | 0x0008_0000 | 0x1000_0000 | 0x4000_0000;

    #[repr(C)]
    struct Acl {
        revision: u8,
        _sbz1: u8,
        tamano: u16,
        n_aces: u16,
        _sbz2: u16,
    }
    #[repr(C)]
    struct CabeceraAce {
        tipo: u8,
        banderas: u8,
        tamano: u16,
    }

    #[link(name = "advapi32")]
    extern "system" {
        fn GetNamedSecurityInfoW(nombre: *const u16, tipo: u32, info: u32, dueno: *mut *mut c_void, grupo: *mut *mut c_void, dacl: *mut *mut Acl, sacl: *mut *mut Acl, sd: *mut *mut c_void) -> u32;
        fn GetAce(acl: *const Acl, indice: u32, ace: *mut *mut c_void) -> i32;
        fn ConvertSidToStringSidW(sid: *const c_void, cadena: *mut *mut u16) -> i32;
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn LocalFree(p: *mut c_void) -> *mut c_void;
    }

    /// SIDs de cuentas NO administrativas: Everyone, Authenticated Users, Users, Interactive,
    /// INTERACTIVE/usuarios del dominio… (si cualquiera de ellos puede escribir, no es seguro).
    fn sid_no_admin(sid: &str) -> bool {
        matches!(sid, "S-1-1-0" | "S-1-5-11" | "S-1-5-32-545" | "S-1-5-4" | "S-1-5-32-546") || (sid.starts_with("S-1-5-21-") && !sid.ends_with("-500") && !sid.ends_with("-512"))
    }

    /// ¿Puede escribir `ruta` alguien que no sea SYSTEM/Administradores? `Err` con el motivo.
    fn comprobar_solo_admin(ruta: &std::path::Path) -> Result<(), String> {
        let nombre = ancho(&ruta.display().to_string());
        let mut dacl: *mut Acl = std::ptr::null_mut();
        let mut sd: *mut c_void = std::ptr::null_mut();
        let r = unsafe { GetNamedSecurityInfoW(nombre.as_ptr(), SE_FILE_OBJECT, DACL_SECURITY_INFORMATION, std::ptr::null_mut(), std::ptr::null_mut(), &mut dacl, std::ptr::null_mut(), &mut sd) };
        if r != 0 {
            return Err(format!("no se pudo leer la seguridad de {} (error {r})", ruta.display()));
        }
        let mut problema = None;
        if dacl.is_null() {
            problema = Some("no tiene DACL (cualquiera puede escribir)".to_string());
        } else {
            let n = unsafe { (*dacl).n_aces };
            for i in 0..u32::from(n) {
                let mut ace: *mut c_void = std::ptr::null_mut();
                if unsafe { GetAce(dacl, i, &mut ace) } == 0 {
                    continue;
                }
                let cab = unsafe { &*(ace as *const CabeceraAce) };
                if cab.tipo != ACCESS_ALLOWED_ACE_TYPE {
                    continue;
                }
                // ACCESS_ALLOWED_ACE: cabecera (4) + máscara (4) + SID.
                let mascara = unsafe { *((ace as *const u8).add(4) as *const u32) };
                let sid = unsafe { (ace as *const u8).add(8) as *const c_void };
                let mut cadena: *mut u16 = std::ptr::null_mut();
                if unsafe { ConvertSidToStringSidW(sid, &mut cadena) } == 0 {
                    continue;
                }
                let mut len = 0;
                while unsafe { *cadena.add(len) } != 0 {
                    len += 1;
                }
                let s = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(cadena, len) });
                unsafe { LocalFree(cadena as *mut c_void) };
                if sid_no_admin(&s) && mascara & DERECHOS_DE_ESCRITURA != 0 {
                    problema = Some(format!("la cuenta {s} tiene derechos de escritura ({mascara:#x})"));
                    break;
                }
            }
        }
        unsafe { LocalFree(sd) };
        problema.map_or(Ok(()), |p| Err(format!("{}: {p}", ruta.display())))
    }

    fn registro(msg: &str) {
        let _ = std::fs::create_dir_all(carpeta());
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(carpeta().join("servicio.log")) {
            let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
            let _ = writeln!(f, "[{t}] {msg}");
        }
        eprintln!("{msg}");
    }

    fn poner_estado(estado: u32, punto: u32, espera_ms: u32) {
        let h = MANEJADOR.load(Ordering::SeqCst);
        if h == 0 {
            return;
        }
        let st = ServiceStatus {
            tipo: SERVICE_WIN32_OWN_PROCESS,
            estado,
            controles: if estado == SERVICE_RUNNING { SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN } else { 0 },
            salida_win32: NO_ERROR,
            salida_servicio: 0,
            punto_control: punto,
            espera: espera_ms,
        };
        unsafe {
            SetServiceStatus(h, &st);
        }
    }

    unsafe extern "system" fn manejador(control: u32, _tipo: u32, _datos: *mut c_void, _ctx: *mut c_void) -> u32 {
        match control {
            CONTROL_STOP | CONTROL_SHUTDOWN => {
                PARAR.store(true, Ordering::SeqCst);
                poner_estado(SERVICE_STOP_PENDING, 1, 30_000);
                NO_ERROR
            }
            CONTROL_INTERROGATE => NO_ERROR,
            _ => 120, // ERROR_CALL_NOT_IMPLEMENTED
        }
    }

    /// `winext4.exe` junto a este ejecutable.
    fn montador() -> PathBuf {
        std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join("winext4.exe"))).unwrap_or_else(|| PathBuf::from("winext4.exe"))
    }

    fn lanzar(m: &Montaje) -> std::io::Result<Child> {
        let mut c = Command::new(montador());
        c.arg(&m.dispositivo).arg(format!("{}\\", m.letra));
        if m.escritura {
            c.arg("--escritura");
            if m.dispositivo.to_ascii_lowercase().contains("physicaldrive") {
                // `leer` solo deja llegar aquí un disco físico con «escritura-disco-fisico».
                c.arg("--autorizo-disco-fisico");
            }
        }
        if let Some(o) = &m.offset {
            c.arg("--offset").arg(o);
        }
        if let Some(d) = &m.dueno {
            c.arg("--dueno").arg(d);
        }
        // Salida del montador al registro del servicio, por montaje.
        let log = std::fs::OpenOptions::new().create(true).append(true).open(carpeta().join(format!("montaje-{}.log", &m.letra[..1])))?;
        c.stdout(log.try_clone()?).stderr(log);
        use std::os::windows::process::CommandExt;
        c.creation_flags(CREATE_NO_WINDOW);
        c.spawn()
    }

    /// El trabajo del servicio: montar, vigilar, y al parar desmontar limpio.
    fn trabajar() -> i32 {
        let ruta = carpeta().join("montajes.txt");
        // Antes de leer NADA: la carpeta y la configuración solo las escriben administradores.
        for p in [carpeta(), ruta.clone()] {
            if p.exists() {
                if let Err(e) = comprobar_solo_admin(&p) {
                    registro(&format!("SE NIEGA A ARRANCAR: {e}. Un usuario sin privilegios podría decidir qué monta el servicio (SYSTEM). Quita esos permisos o reinstala."));
                    return 1;
                }
            }
        }
        let texto = match std::fs::read_to_string(&ruta) {
            Ok(t) => t,
            Err(e) => {
                registro(&format!("no se pudo leer {}: {e} (sin montajes)", ruta.display()));
                String::new()
            }
        };
        let montajes = match config::leer(&texto) {
            Ok(v) => v,
            Err(e) => {
                registro(&format!("montajes.txt inválido: {e}"));
                return 1;
            }
        };
        registro(&format!("arranca: {} montajes", montajes.len()));
        let mut vivos: Vec<(Montaje, Option<Child>, u32)> = Vec::new();
        for m in montajes {
            let hijo = match lanzar(&m) {
                Ok(c) => {
                    registro(&format!("montando {} en {} ({})", m.dispositivo, m.letra, if m.escritura { "escritura" } else { "lectura" }));
                    Some(c)
                }
                Err(e) => {
                    registro(&format!("no se pudo lanzar el montaje de {}: {e}", m.letra));
                    None
                }
            };
            vivos.push((m, hijo, 0));
        }
        poner_estado(SERVICE_RUNNING, 0, 0);
        while !PARAR.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(500));
            for (m, hijo, reintentos) in &mut vivos {
                let murio = match hijo {
                    Some(c) => matches!(c.try_wait(), Ok(Some(_))),
                    None => false,
                };
                if murio && !PARAR.load(Ordering::SeqCst) {
                    if *reintentos < 3 {
                        *reintentos += 1;
                        registro(&format!("el montaje de {} terminó solo; relanzo ({}/3)", m.letra, reintentos));
                        std::thread::sleep(Duration::from_secs(2 * u64::from(*reintentos)));
                        *hijo = lanzar(m).ok();
                    } else {
                        registro(&format!("el montaje de {} falló 3 veces: se deja de intentar (mira montaje-{}.log)", m.letra, &m.letra[..1]));
                        *hijo = None;
                    }
                }
            }
        }
        // Parar: desmontaje LIMPIO de cada unidad, y esperar a que cada montador cierre.
        registro("parando: desmontaje limpio de cada unidad");
        for (i, (m, hijo, _)) in vivos.iter_mut().enumerate() {
            poner_estado(SERVICE_STOP_PENDING, 2 + i as u32, 30_000);
            if let Some(c) = hijo {
                let _ = Command::new(montador()).arg("--desmontar").arg(format!("{}\\", m.letra)).status();
                let fin = Instant::now() + Duration::from_secs(25);
                loop {
                    if matches!(c.try_wait(), Ok(Some(_))) {
                        registro(&format!("{} desmontado", m.letra));
                        break;
                    }
                    if Instant::now() > fin {
                        registro(&format!("{} no cerró a tiempo: se fuerza (el volumen pedirá recuperar el journal)", m.letra));
                        let _ = c.kill();
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(200));
                }
            }
        }
        registro("parado");
        0
    }

    unsafe extern "system" fn principal(_argc: u32, _argv: *mut *mut u16) {
        let nombre = ancho(NOMBRE);
        let h = RegisterServiceCtrlHandlerExW(nombre.as_ptr(), manejador, std::ptr::null_mut());
        MANEJADOR.store(h, Ordering::SeqCst);
        poner_estado(SERVICE_START_PENDING, 1, 10_000);
        let r = trabajar();
        let h = MANEJADOR.load(Ordering::SeqCst);
        let st = ServiceStatus { tipo: SERVICE_WIN32_OWN_PROCESS, estado: SERVICE_STOPPED, controles: 0, salida_win32: if r == 0 { NO_ERROR } else { 1066 }, salida_servicio: r as u32, punto_control: 0, espera: 0 };
        SetServiceStatus(h, &st);
    }

    pub fn ejecutar() -> i32 {
        if std::env::args().nth(1).as_deref() == Some("--consola") {
            // En primer plano: Ctrl+C no se intercepta aquí; para probar, crear el archivo PARAR.
            let marca = carpeta().join("PARAR");
            let _ = std::fs::remove_file(&marca);
            std::thread::spawn(move || loop {
                if marca.exists() {
                    PARAR.store(true, Ordering::SeqCst);
                    let _ = std::fs::remove_file(&marca);
                    break;
                }
                std::thread::sleep(Duration::from_millis(300));
            });
            return trabajar();
        }
        let mut nombre = ancho(NOMBRE);
        let tabla = [EntradaTabla { nombre: nombre.as_mut_ptr(), proc_: Some(principal) }, EntradaTabla { nombre: std::ptr::null_mut(), proc_: None }];
        if unsafe { StartServiceCtrlDispatcherW(tabla.as_ptr()) } == 0 {
            eprintln!("winext4svc es un servicio de Windows: instálalo con el instalador (o «sc create winext4 binPath= …»), o pruébalo con --consola");
            return 1;
        }
        0
    }
}

#[cfg(test)]
mod pruebas {
    use super::config::leer;

    #[test]
    fn lee_montajes_validos() {
        let v = leer("# comentario\n\\\\.\\PhysicalDrive2  z:\\  lectura\nD:\\a.img W: escritura 17408\n").unwrap();
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].letra, "Z:");
        assert!(!v[0].escritura);
        assert!(v[1].escritura && v[1].offset.as_deref() == Some("17408"));
    }

    #[test]
    fn rechaza_lo_mal_formado_con_su_linea() {
        assert!(leer("x Z:").unwrap_err().contains("línea 1"));
        assert!(leer("x ZZ: lectura").unwrap_err().contains("letra"));
        assert!(leer("x Z: talvez").unwrap_err().contains("modo"));
        assert!(leer("a Z: lectura\nb z: lectura").unwrap_err().contains("ya está usada"));
        assert!(leer("--escritura Z: lectura").unwrap_err().contains("«-»"), "inyección de opción");
        assert!(leer("x Z: lectura 12;calc").unwrap_err().contains("offset"));
        assert!(leer("x Z: lectura 0x4400").is_ok() && leer("x Z: lectura 17408").is_ok(), "pareja: offsets válidos");
        // Disco físico: «escritura» a secas se rechaza; hace falta la palabra explícita.
        assert!(leer("\\\\.\\PhysicalDrive2 Z: escritura").unwrap_err().contains("escritura-disco-fisico"));
        assert!(leer("\\\\.\\physicaldrive2 Z: rw").is_err(), "también en minúsculas y con rw");
        let v = leer("\\\\.\\PhysicalDrive2 Z: escritura-disco-fisico").unwrap();
        assert!(v[0].escritura, "pareja: con la palabra explícita, sí");
        assert!(leer("D:\\a.img W: escritura-disco-fisico").is_err(), "la palabra del disco no vale para una imagen");
        assert!(leer("D:\\a.img W: escritura").unwrap()[0].escritura, "pareja: imagen con escritura, sí");
        assert_eq!(leer("").unwrap(), vec![], "vacío = sin montajes (no es error)");
    }

    #[test]
    fn dueno_opcional_validado() {
        let v = leer("\\\\.\\PhysicalDrive2 Z: escritura-disco-fisico dueno=juan").unwrap();
        assert_eq!(v[0].dueno.as_deref(), Some("juan"));
        let v = leer("D:\\a.img W: escritura dueno=juan 17408").unwrap();
        assert_eq!((v[0].dueno.as_deref(), v[0].offset.as_deref()), (Some("juan"), Some("17408")), "en cualquier orden");
        assert_eq!(leer("D:\\a.img W: lectura").unwrap()[0].dueno, None, "pareja: sin dueño");
        assert!(leer("D:\\a.img W: lectura dueno=").is_err(), "vacío");
        assert!(leer("D:\\a.img W: lectura dueno=-x").is_err(), "inyección de opción");
        assert!(leer("D:\\a.img W: lectura dueno=a&calc").is_err(), "separador de órdenes");
        assert!(leer("D:\\a.img W: lectura dueno=a dueno=b").is_err(), "dos veces");
        assert!(leer("D:\\a.img W: lectura 1 2").is_err(), "dos offsets");
    }
}
