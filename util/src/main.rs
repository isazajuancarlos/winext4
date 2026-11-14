// SPDX-FileCopyrightText: 2026 Juan Carlos Isaza
// SPDX-License-Identifier: LicenseRef-Xiliux-Propietario
//! `winext4util` — mantenimiento y recuperación de volúmenes ext4 desde Windows (o Linux).
//!
//! ```text
//! winext4util info      <imagen|\\.\PhysicalDriveN> [--offset N]
//! winext4util verificar <imagen|disco> [--offset N]          (solo lectura; 0 = sano, 4 = errores)
//! winext4util recuperar <imagen> [--offset N]                (reproduce el journal pendiente)
//! winext4util rescatar  <imagen|disco> <ruta-en-ext4> <carpeta-destino> [--offset N]
//! ```
//!
//! `rescatar` copia un árbol del ext4 a una carpeta de Windows SIN escribir en el ext4,
//! siguiendo aunque haya partes dañadas, y deja un informe de lo que no pudo salvar: es la
//! herramienta para sacar los datos de un disco que no monta.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use winext4_nucleo::dispositivo::{self, DispositivoBloque};
use winext4_nucleo::verificar::Gravedad;
use winext4_nucleo::Ext4;

type Fs = Ext4<DispositivoBloque<std::fs::File>>;

fn uso() -> ExitCode {
    eprintln!(
        "winext4util — herramientas para ext4\n\n\
         uso:\n  \
         winext4util info      <imagen|\\\\.\\PhysicalDriveN> [--offset N]\n  \
         winext4util verificar <imagen|disco> [--offset N]\n  \
         winext4util recuperar <imagen> [--offset N]\n  \
         winext4util reparar   <imagen|disco> [--autorizo-disco-fisico --deshacer <archivo>] [--offset N]\n  \
         winext4util deshacer  <imagen|disco> <archivo> [--autorizo-disco-fisico] [--offset N]\n  \
         winext4util formatear <imagen> --tamano 512M [--etiqueta X] [--sobrescribir]\n  \
         winext4util formatear <disco> --autorizo-disco-fisico [--etiqueta X]   (solo su partición Linux)\n  \
         winext4util inodo     <imagen|disco> <número> [--offset N]\n  \
         winext4util rescatar  <imagen|disco> <ruta-en-ext4> <carpeta-destino> [--offset N]\n  \
         winext4util enlazar   <imagen> <destino> <ruta-nueva> [--simbolico]   (como ln / ln -s)\n\n\
         Un disco físico exige consola de ADMINISTRADOR. verificar sale con 0 si está sano y 4 si\n\
         encontró errores (como e2fsck)."
    );
    ExitCode::from(8)
}

fn abrir(ruta: &str, offset: Option<u64>, escritura: bool) -> Result<Fs, String> {
    let (d, base) = dispositivo::abrir(ruta, offset, escritura).map_err(|e| {
        let extra = if dispositivo::es_disco_fisico(ruta) { " (¿consola de administrador?)" } else { "" };
        format!("no se pudo abrir {ruta}: {e}{extra}")
    })?;
    Ext4::abrir(d).map_err(|e| format!("{ruta} (offset {base}) no es un ext4 legible: {e}"))
}

fn main() -> ExitCode {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut offset = None;
    if let Some(i) = args.iter().position(|a| a == "--offset") {
        match args.get(i + 1).and_then(|s| dispositivo::parse_offset(s)) {
            Some(o) => offset = Some(o),
            None => {
                eprintln!("--offset requiere un número de bytes (decimal o 0xHEX)");
                return ExitCode::from(8);
            }
        }
        args.drain(i..i + 2);
    }
    let r = match args.first().map(String::as_str) {
        Some("info") if args.len() == 2 => info(&args[1], offset),
        Some("verificar") if args.len() == 2 => verificar(&args[1], offset),
        Some("recuperar") if args.len() == 2 => recuperar(&args[1], offset),
        Some("reparar") if args.len() >= 2 => {
            let autorizado = args.iter().any(|a| a == "--autorizo-disco-fisico");
            let deshacer = args.iter().position(|a| a == "--deshacer").and_then(|i| args.get(i + 1)).map(PathBuf::from);
            reparar(&args[1], offset, autorizado, deshacer.as_deref())
        }
        Some("formatear") if args.len() >= 2 => {
            let bandera = |b: &str| args.iter().any(|a| a == b);
            let valor = |b: &str| args.iter().position(|a| a == b).and_then(|i| args.get(i + 1)).cloned();
            formatear(&args[1], valor("--tamano").as_deref(), valor("--etiqueta").as_deref().unwrap_or(""), bandera("--sobrescribir"), bandera("--autorizo-disco-fisico"))
        }
        Some("deshacer") if args.len() >= 3 => {
            let autorizado = args.iter().any(|a| a == "--autorizo-disco-fisico");
            deshacer(&args[1], Path::new(&args[2]), offset, autorizado)
        }
        Some("inodo") if args.len() == 3 => match args[2].parse::<u32>() {
            Ok(n) => inodo(&args[1], n, offset),
            Err(_) => return uso(),
        },
        Some("rescatar") if args.len() == 4 => rescatar(&args[1], &args[2], Path::new(&args[3]), offset),
        Some("enlazar") if args.len() == 4 || (args.len() == 5 && args.iter().any(|a| a == "--simbolico")) => {
            let simbolico = args.iter().any(|a| a == "--simbolico");
            let pos: Vec<&String> = args[1..].iter().filter(|a| *a != "--simbolico").collect();
            enlazar(pos[0], pos[1], pos[2], simbolico, offset)
        }
        _ => return uso(),
    };
    match r {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(8)
        }
    }
}

fn nombres(bits: u32, tabla: &[(u32, &str)]) -> String {
    let mut v: Vec<String> = tabla.iter().filter(|(b, _)| bits & b != 0).map(|(_, n)| (*n).to_string()).collect();
    let resto = bits & !tabla.iter().fold(0, |a, (b, _)| a | b);
    if resto != 0 {
        v.push(format!("desconocidas({resto:#x})"));
    }
    v.join(" ")
}

/// Fecha UTC legible de un epoch Unix (sin dependencias: algoritmo de días civiles).
fn fecha(seg: u32) -> String {
    let s = i64::from(seg);
    let (dias, resto) = (s.div_euclid(86_400), s.rem_euclid(86_400));
    let z = dias + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02} UTC", resto / 3600, (resto % 3600) / 60)
}

fn info(ruta: &str, offset: Option<u64>) -> Result<ExitCode, String> {
    let fs = abrir(ruta, offset, false)?;
    let sb = &fs.sb;
    let mib = |b: u64| b * u64::from(sb.tamano_bloque) / (1024 * 1024);
    let uuid: String = sb.uuid.iter().map(|b| format!("{b:02x}")).collect();
    println!("Volumen:            {}", if sb.etiqueta.is_empty() { "(sin etiqueta)" } else { &sb.etiqueta });
    println!("UUID:               {}-{}-{}-{}-{}", &uuid[0..8], &uuid[8..12], &uuid[12..16], &uuid[16..20], &uuid[20..32]);
    println!("Tamaño:             {} MiB ({} bloques de {} bytes)", mib(sb.bloques), sb.bloques, sb.tamano_bloque);
    println!("Libre:              {} MiB", mib(fs.bloques_libres()));
    println!("Inodos:             {} ({} por grupo, {} bytes)", sb.inodos, sb.inodos_por_grupo, sb.tamano_inodo);
    println!("Grupos:             {}", sb.n_grupos());
    println!("Estado:             {}{}", if sb.estado & 1 != 0 { "limpio" } else { "NO limpio" }, if sb.estado & 2 != 0 { ", con errores marcados" } else { "" });
    println!(
        "Journal pendiente:  {}",
        if sb.incompat & winext4_nucleo::superbloque::INCOMPAT_RECOVER != 0 { "SÍ (usa «winext4util recuperar»)" } else { "no" }
    );
    // El registro de errores que el KERNEL deja en el superbloque (s_error_count, primero y
    // último): cuándo, en qué inodo/bloque y en qué función. Ninguna herramienta de Windows lo
    // enseña, y es lo primero que hay que mirar ante «con errores marcados».
    let c = fs.superbloque_crudo();
    let u32_ = |o: usize| u32::from_le_bytes(c[o..o + 4].try_into().expect("4"));
    let n_err = u32_(0x194);
    if n_err > 0 || sb.estado & 2 != 0 {
        println!("Errores del kernel: {n_err}");
        // Offsets del formato: el «último» guarda la línea ANTES que el bloque (al revés que el primero).
        for (etq, t, ino, blq, func, linea) in [("primero", 0x198, 0x19C, 0x1A0, 0x1A8, 0x1C8), ("último ", 0x1CC, 0x1D0, 0x1D8, 0x1E0, 0x1D4)] {
            let cuando = u32_(t);
            if cuando == 0 {
                continue;
            }
            let f = String::from_utf8_lossy(&c[func..func + 32]).trim_end_matches('\0').to_string();
            println!("  {etq}: epoch {cuando} ({}), función {f}:{}, inodo {}, bloque {}", fecha(cuando), u32_(linea), u32_(ino), u32::from_le_bytes(c[blq..blq + 4].try_into().expect("4")));
        }
    }
    println!(
        "incompat:           {}",
        nombres(sb.incompat, &[(0x2, "filetype"), (0x4, "needs_recovery"), (0x10, "meta_bg"), (0x40, "extent"), (0x80, "64bit"), (0x100, "mmp"), (0x200, "flex_bg"), (0x400, "ea_inode"), (0x2000, "csum_seed"), (0x4000, "large_dir"), (0x8000, "inline_data"), (0x1_0000, "encrypt"), (0x2_0000, "casefold")])
    );
    println!(
        "ro_compat:          {}",
        nombres(sb.ro_compat, &[(0x1, "sparse_super"), (0x2, "large_file"), (0x8, "huge_file"), (0x10, "gdt_csum"), (0x20, "dir_nlink"), (0x40, "extra_isize"), (0x100, "quota"), (0x200, "bigalloc"), (0x400, "metadata_csum"), (0x1000, "readonly"), (0x2000, "project"), (0x8000, "verity"), (0x1_0000, "orphan_present")])
    );
    println!(
        "compat:             {}",
        nombres(sb.compat, &[(0x4, "has_journal"), (0x8, "ext_attr"), (0x10, "resize_inode"), (0x20, "dir_index"), (0x200, "sparse_super2"), (0x400, "fast_commit"), (0x1000, "orphan_file")])
    );
    Ok(ExitCode::SUCCESS)
}

fn verificar(ruta: &str, offset: Option<u64>) -> Result<ExitCode, String> {
    let mut fs = abrir(ruta, offset, false)?;
    eprintln!("verificando {ruta} (solo lectura)…");
    let t = std::time::Instant::now();
    let inf = fs.verificar().map_err(|e| format!("la verificación no pudo terminar: {e}"))?;
    for h in &inf.hallazgos {
        let g = match h.gravedad {
            Gravedad::Error => "ERROR",
            Gravedad::Aviso => "aviso",
        };
        println!("[{g}] {}: {}", h.clase, h.detalle);
    }
    println!(
        "\n{} grupos, {} inodos en uso, {} directorios, {} bloques en uso — {} errores, {} avisos ({:.1} s)",
        inf.grupos,
        inf.inodos,
        inf.directorios,
        inf.bloques_en_uso,
        inf.errores(),
        inf.hallazgos.len() - inf.errores(),
        t.elapsed().as_secs_f64()
    );
    Ok(if inf.errores() == 0 && inf.tiene("journal-pendiente") {
        // Lo verificado es el estado ANTERIOR al corte: el journal trae cambios sin aplicar.
        println!("SIN ERRORES ESTRUCTURALES, PERO CON JOURNAL PENDIENTE: recupéralo (montarlo en Linux, o \
                  «winext4util recuperar» sobre una imagen) y vuelve a verificar.");
        ExitCode::from(1)
    } else if inf.errores() == 0 {
        println!("VOLUMEN SANO.");
        ExitCode::SUCCESS
    } else {
        println!("HAY ERRORES: no escribas en este volumen hasta repararlo (e2fsck en Linux).");
        ExitCode::from(4)
    })
}

/// Diagnóstico de UN inodo (estilo `debugfs stat`): campos, suma, quién lo nombra, y su
/// bloque de atributos extendidos con su cabecera y su suma.
fn inodo(ruta: &str, n: u32, offset: Option<u64>) -> Result<ExitCode, String> {
    use winext4_nucleo::csum;
    let mut fs = abrir(ruta, offset, false)?;
    let c = fs.inodo_crudo(n).map_err(|e| format!("inodo {n}: {e}"))?;
    let i = fs.inodo(n).map_err(|e| e.to_string())?;
    let u16_ = |o: usize| u16::from_le_bytes([c[o], c[o + 1]]);
    let u32_ = |o: usize| u32::from_le_bytes(c[o..o + 4].try_into().expect("4"));
    let tipo = match i.modo & 0xF000 {
        0x4000 => "directorio",
        0x8000 => "archivo",
        0xA000 => "enlace simbólico",
        _ => "otro",
    };
    println!("Inodo {n}: {tipo}, modo {:o}, {} bytes, {} enlaces, uid {} gid {}", i.modo & 0o7777, i.tamano, u16_(0x1A), u16_(0x02), u16_(0x18));
    println!("  mtime {} · flags {:#x}", fecha(i.mtime), i.flags);
    if fs.sb.tiene_csum_metadatos() {
        let ok = csum::inodo(fs.semilla(), n, &c) == csum::inodo_guardada(&c);
        println!("  suma del inodo: {}", if ok { "correcta" } else { "INCORRECTA" });
    }
    let acl = u64::from(u32_(0x68)) | (u64::from(u16_(0x76)) << 32);
    if acl == 0 {
        println!("  atributos extendidos: dentro del inodo o ninguno (sin bloque aparte)");
    } else {
        println!("  bloque de atributos extendidos: {acl}");
        match fs.leer_bloque(acl) {
            Ok(b) => {
                let magia = u32::from_le_bytes(b[0..4].try_into().expect("4"));
                let refs = u32::from_le_bytes(b[4..8].try_into().expect("4"));
                let guardada = u32::from_le_bytes(b[0x10..0x14].try_into().expect("4"));
                println!("    magia {magia:#010x} ({}), compartido por {refs} inodos", if magia == 0xEA02_0000 { "válida" } else { "INVÁLIDA" });
                if fs.sb.tiene_csum_metadatos() {
                    // Suma del bloque xattr: semilla + nº de bloque (64 bits LE) + el bloque con la suma a cero.
                    let mut copia = b;
                    copia[0x10..0x14].fill(0);
                    let calc = csum::crc32c(csum::crc32c(fs.semilla(), &acl.to_le_bytes()), &copia);
                    println!("    suma del bloque: {}", if calc == guardada { "correcta" } else { "INCORRECTA" });
                }
            }
            Err(e) => println!("    ILEGIBLE: {e}"),
        }
    }
    // Quién lo nombra: búsqueda en anchura por los directorios (se corta en 200 000 dirs).
    let mut cola = vec![(winext4_nucleo::INODO_RAIZ, "/".to_string())];
    let mut vistos = 0u32;
    let mut nombres = Vec::new();
    while let Some((d, r)) = cola.pop() {
        vistos += 1;
        if vistos > 200_000 || nombres.len() >= 5 {
            break;
        }
        let Ok(di) = fs.inodo(d) else { continue };
        let Ok(es) = fs.listar(&di) else { continue };
        for e in es {
            if e.nombre == "." || e.nombre == ".." {
                continue;
            }
            let p = if r == "/" { format!("/{}", e.nombre) } else { format!("{r}/{}", e.nombre) };
            if e.ino == n {
                nombres.push(p.clone());
            }
            if e.tipo == 2 {
                cola.push((e.ino, p));
            }
        }
    }
    if nombres.is_empty() {
        println!("  nombrado por: (ningún directorio recorrido lo nombra; {vistos} directorios vistos)");
    } else {
        for p in nombres {
            println!("  nombrado por: {p}");
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Reparación SEGURA (por el journal; se niega ante lo grave). Sobre un disco físico exige la
/// bandera explícita `--autorizo-disco-fisico`: es la decisión del dueño del disco, no un
/// valor por defecto.
fn reparar(ruta: &str, offset: Option<u64>, autorizado: bool, deshacer: Option<&Path>) -> Result<ExitCode, String> {
    let fisico = dispositivo::es_disco_fisico(ruta);
    if fisico && (!autorizado || deshacer.is_none()) {
        return Err("reparar escribe en el disco. Sobre un disco físico hay que pedirlo expresamente Y dejar \
                    un archivo para deshacer EN OTRO DISCO:\n  winext4util reparar <disco> --autorizo-disco-fisico --deshacer D:\\deshacer.bin"
            .into());
    }
    let (d, base) = dispositivo::abrir(ruta, offset, true).map_err(|e| format!("no se pudo abrir {ruta}: {e}"))?;
    let inf = match deshacer {
        Some(p) => {
            let dd = dispositivo::ConDeshacer::nuevo(d, p).map_err(|e| format!("no se pudo crear el archivo para deshacer {}: {e}", p.display()))?;
            let mut fs = Ext4::abrir(dd).map_err(|e| format!("{ruta} (offset {base}): {e}"))?;
            let r = reparar_en(&mut fs, ruta)?;
            eprintln!("archivo para deshacer: {} ({} bloques originales guardados)", p.display(), fs_trozos(&fs));
            r
        }
        None => {
            let mut fs = Ext4::abrir(d).map_err(|e| format!("{ruta} (offset {base}): {e}"))?;
            reparar_en(&mut fs, ruta)?
        }
    };
    Ok(inf)
}

fn fs_trozos(fs: &Ext4<dispositivo::ConDeshacer<DispositivoBloque<std::fs::File>>>) -> usize {
    fs.dispositivo().trozos_guardados()
}

/// Tamaño legible: `64M`, `2G`, `1536K` o bytes.
fn parse_tamano(s: &str) -> Option<u64> {
    let s = s.trim();
    let (n, m) = match s.chars().last()? {
        'K' | 'k' => (&s[..s.len() - 1], 1u64 << 10),
        'M' | 'm' => (&s[..s.len() - 1], 1 << 20),
        'G' | 'g' => (&s[..s.len() - 1], 1 << 30),
        'T' | 't' => (&s[..s.len() - 1], 1 << 40),
        _ => (s, 1),
    };
    n.parse::<u64>().ok()?.checked_mul(m)
}

/// `mkwinext4`: formatea una imagen (la crea si no existe) o la partición Linux de un disco.
fn formatear(ruta: &str, tamano: Option<&str>, etiqueta: &str, sobrescribir: bool, autorizado: bool) -> Result<ExitCode, String> {
    let fisico = dispositivo::es_disco_fisico(ruta);
    let (mut d, bytes) = if fisico {
        if !autorizado {
            return Err("formatear BORRA TODO lo que haya en la partición. Sobre un disco físico hay que pedirlo \
                        expresamente: --autorizo-disco-fisico (solo se formatea la partición Linux de la GPT)."
                .into());
        }
        let f = std::fs::OpenOptions::new().read(true).write(true).open(ruta).map_err(|e| format!("no se pudo abrir {ruta}: {e} (¿consola de administrador?)"))?;
        let mut d = DispositivoBloque::nuevo(f, 0, 4096);
        let (off, tam) = dispositivo::particion_linux_con_tamano(&mut d)
            .map_err(|e| format!("no se pudo leer la GPT de {ruta}: {e}"))?
            .ok_or("el disco no tiene una partición de tipo Linux en su GPT: créala antes (y nunca se formatea el disco entero)")?;
        d.reposicionar(off);
        eprintln!("partición Linux en el offset {off}, {} MiB", tam >> 20);
        (d, tam)
    } else {
        let existe = Path::new(ruta).exists();
        if existe && !sobrescribir {
            return Err(format!("{ruta} ya existe: para BORRARLO y formatear de nuevo, añade --sobrescribir"));
        }
        let bytes = match (tamano.and_then(parse_tamano), existe) {
            (Some(b), _) => b,
            (None, true) => std::fs::metadata(ruta).map_err(|e| e.to_string())?.len(),
            (None, false) => return Err("una imagen nueva necesita --tamano (p. ej. --tamano 512M)".into()),
        };
        let f = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(ruta).map_err(|e| format!("no se pudo crear {ruta}: {e}"))?;
        f.set_len(bytes).map_err(|e| format!("no se pudo dimensionar {ruta}: {e}"))?;
        (DispositivoBloque::nuevo(f, 0, 1), bytes)
    };
    let inf = winext4_nucleo::formatear::formatear(&mut d, bytes, etiqueta).map_err(|e| format!("no se pudo formatear: {e}"))?;
    let u: String = inf.uuid.iter().map(|b| format!("{b:02x}")).collect();
    println!(
        "formateado {ruta}: ext4 de {} MiB, {} grupos, {} inodos, journal de {} MiB, UUID {}-{}-{}-{}-{}",
        (inf.bloques * 4096) >> 20,
        inf.grupos,
        inf.inodos,
        (inf.bloques_journal * 4096) >> 20,
        &u[0..8],
        &u[8..12],
        &u[12..16],
        &u[16..20],
        &u[20..32]
    );
    // Comprobación inmediata con el verificador propio.
    drop(d);
    if !fisico {
        let mut fs = abrir(ruta, None, false)?;
        let v = fs.verificar().map_err(|e| e.to_string())?;
        println!("verificación: {} errores.", v.errores());
    }
    Ok(ExitCode::SUCCESS)
}

fn deshacer(ruta: &str, archivo: &Path, offset: Option<u64>, autorizado: bool) -> Result<ExitCode, String> {
    if dispositivo::es_disco_fisico(ruta) && !autorizado {
        return Err("deshacer escribe en el disco: sobre un disco físico añade --autorizo-disco-fisico".into());
    }
    let (mut d, _) = dispositivo::abrir(ruta, offset, true).map_err(|e| format!("no se pudo abrir {ruta}: {e}"))?;
    let n = dispositivo::aplicar_deshacer(&mut d, archivo).map_err(|e| format!("no se pudo deshacer: {e}"))?;
    println!("restaurados {n} bloques originales desde {}.", archivo.display());
    Ok(ExitCode::SUCCESS)
}

fn reparar_en<R: std::io::Read + std::io::Write + std::io::Seek>(fs: &mut Ext4<R>, ruta: &str) -> Result<ExitCode, String> {
    if fs.journal_pendiente() {
        return Err("hay journal pendiente: recupéralo antes («winext4util recuperar» o montarlo en Linux)".into());
    }
    eprintln!("reparando {ruta}…");
    let inf = fs.reparar().map_err(|e| format!("la reparación no pudo hacerse: {e}"))?;
    if !inf.no_reparables.is_empty() {
        println!("NO SE TOCÓ NADA: hay daños que esta reparación no arregla con seguridad:");
        for h in &inf.no_reparables {
            println!("  [{}] {}", h.clase, h.detalle);
        }
        println!("Repáralo con e2fsck en Linux (sudo e2fsck -f <partición>).");
        return Ok(ExitCode::from(4));
    }
    if inf.hecho.is_empty() {
        println!("volumen sano: nada que reparar (no se escribió nada).");
        return Ok(ExitCode::SUCCESS);
    }
    for h in &inf.hecho {
        println!("  hecho: {h}");
    }
    if inf.errores_despues == 0 {
        println!("REPARADO: la verificación posterior da 0 errores.");
        Ok(ExitCode::SUCCESS)
    } else {
        println!("quedan {} errores tras reparar: pásale e2fsck en Linux.", inf.errores_despues);
        Ok(ExitCode::from(4))
    }
}

fn recuperar(ruta: &str, offset: Option<u64>) -> Result<ExitCode, String> {
    if dispositivo::es_disco_fisico(ruta) {
        return Err("recuperar escribe en el volumen: sobre un disco físico está bloqueado en esta versión. \
                    Hazlo en Linux (montar lo recupera) o sobre una imagen."
            .into());
    }
    let mut fs = abrir(ruta, offset, true)?;
    if !fs.journal_pendiente() {
        println!("no hay journal pendiente: nada que hacer.");
        return Ok(ExitCode::SUCCESS);
    }
    let inf = fs.recuperar_journal().map_err(|e| format!("no se pudo recuperar: {e}"))?;
    println!(
        "recuperado: {} transacciones, {} bloques reproducidos, {} revocados, {} descartados por suma inválida.",
        inf.transacciones, inf.bloques_reproducidos, inf.revocados_saltados, inf.sumas_invalidas
    );
    if inf.huerfanos_pendientes {
        println!("quedan inodos huérfanos pendientes: los procesa Linux al montar o e2fsck.");
    }
    Ok(ExitCode::SUCCESS)
}

/// Nombre válido en Windows: sin `\ / : * ? " < > |`, sin punto/espacio final, y sin nombres
/// reservados (CON, NUL, COM1…). Devuelve el nombre y si hubo que cambiarlo.
fn nombre_windows(n: &str) -> (String, bool) {
    let mut s: String = n.chars().map(|c| if "\\/:*?\"<>|".contains(c) || (c as u32) < 32 { '_' } else { c }).collect();
    while s.ends_with('.') || s.ends_with(' ') {
        s.pop();
        s.push('_');
    }
    let base = s.split('.').next().unwrap_or("").to_ascii_uppercase();
    let reservado = matches!(base.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (base.len() == 4 && (base.starts_with("COM") || base.starts_with("LPT")) && base.as_bytes()[3].is_ascii_digit());
    if reservado {
        s.insert(0, '_');
    }
    let cambio = s != n;
    (s, cambio)
}

#[derive(Default)]
struct Rescate {
    archivos: u64,
    bytes: u64,
    directorios: u64,
    fallos: Vec<String>,
    renombrados: Vec<String>,
    enlaces: Vec<String>,
    /// Directorios ya recorridos: en un volumen dañado un directorio puede aparecer en dos
    /// sitios (o contenerse); sin esto el recorrido crece exponencialmente hasta el tope de 256.
    dirs_vistos: std::collections::HashSet<u32>,
}

/// `ln destino nueva` / `ln -s destino nueva` dentro del ext4. Desde el Explorador o `mklink`
/// no se puede: Dokan no pasa `FSCTL_SET_REPARSE_POINT` ni `FileLinkInformation` (medido).
fn enlazar(ruta: &str, destino: &str, nueva: &str, simbolico: bool, offset: Option<u64>) -> Result<ExitCode, String> {
    if dispositivo::es_disco_fisico(ruta) {
        return Err("escribir en un DISCO FÍSICO sigue bloqueado en esta versión (las imágenes, no)".into());
    }
    let mut fs = abrir(ruta, offset, true)?;
    let nueva = nueva.trim_end_matches('/');
    let (padre, nombre) = match nueva.rfind('/') {
        Some(i) => (if i == 0 { "/" } else { &nueva[..i] }, &nueva[i + 1..]),
        None => ("/", nueva),
    };
    let dir = fs.buscar_siguiendo(padre, true).map_err(|e| format!("{padre}: {e}"))?;
    fs.abrir_escritura().map_err(|e| format!("no se puede escribir en {ruta}: {e}"))?;
    let r = if simbolico {
        fs.crear_enlace_simbolico(dir, nombre, destino).map(|_| ())
    } else {
        // Duro: el destino es una RUTA del volumen (sin seguir el último enlace, como `ln`).
        match fs.buscar_siguiendo(destino, false) {
            Ok(ino) => fs.crear_enlace_duro(dir, nombre, ino),
            Err(e) => Err(e),
        }
    };
    let cierre = fs.cerrar_escritura();
    r.map_err(|e| format!("{nueva}: {e}"))?;
    cierre.map_err(|e| format!("al cerrar: {e}"))?;
    println!("{} {nueva} -> {destino}", if simbolico { "enlace simbólico" } else { "enlace duro" });
    Ok(ExitCode::SUCCESS)
}

fn rescatar(ruta: &str, origen: &str, destino: &Path, offset: Option<u64>) -> Result<ExitCode, String> {
    let mut fs = abrir(ruta, offset, false)?;
    std::fs::create_dir_all(destino).map_err(|e| format!("no se pudo crear {}: {e}", destino.display()))?;
    let ini = fs.buscar_numero(origen).map_err(|e| format!("{origen}: {e}"))?;
    let mut r = Rescate::default();
    let t = std::time::Instant::now();
    rescatar_rec(&mut fs, ini, origen, destino.to_path_buf(), &mut r, 0);
    // Informe junto a lo rescatado.
    let informe = destino.join("_INFORME-RESCATE.txt");
    if let Ok(mut f) = std::fs::File::create(&informe) {
        let _ = writeln!(f, "Rescate de {origen} desde {ruta}\n");
        let _ = writeln!(f, "{} archivos ({} bytes), {} directorios, {} fallos\n", r.archivos, r.bytes, r.directorios, r.fallos.len());
        for (titulo, v) in [("NO SE PUDIERON SALVAR", &r.fallos), ("RENOMBRADOS (nombre inválido en Windows)", &r.renombrados), ("ENLACES SIMBÓLICOS (no se crean; destino)", &r.enlaces)] {
            if !v.is_empty() {
                let _ = writeln!(f, "== {titulo} ==");
                for l in v {
                    let _ = writeln!(f, "{l}");
                }
                let _ = writeln!(f);
            }
        }
    }
    println!(
        "rescatados {} archivos ({:.1} MiB) y {} directorios en {:.1} s; {} fallos, {} renombrados, {} enlaces anotados.\ninforme: {}",
        r.archivos,
        r.bytes as f64 / 1_048_576.0,
        r.directorios,
        t.elapsed().as_secs_f64(),
        r.fallos.len(),
        r.renombrados.len(),
        r.enlaces.len(),
        informe.display()
    );
    Ok(if r.fallos.is_empty() { ExitCode::SUCCESS } else { ExitCode::from(4) })
}

fn rescatar_rec(fs: &mut Fs, ino: u32, ruta_fs: &str, dest: PathBuf, r: &mut Rescate, prof: u32) {
    if prof > 256 {
        r.fallos.push(format!("{ruta_fs}: demasiada profundidad (¿bucle de directorios?)"));
        return;
    }
    let inodo = match fs.inodo(ino) {
        Ok(i) => i,
        Err(e) => {
            r.fallos.push(format!("{ruta_fs}: inodo ilegible: {e}"));
            return;
        }
    };
    if inodo.es_directorio() {
        if !r.dirs_vistos.insert(ino) {
            r.fallos.push(format!("{ruta_fs}: el directorio (inodo {ino}) ya se rescató por otra ruta — volumen dañado, no se recorre dos veces"));
            return;
        }
        if let Err(e) = std::fs::create_dir_all(&dest) {
            r.fallos.push(format!("{ruta_fs}: no se pudo crear {}: {e}", dest.display()));
            return;
        }
        r.directorios += 1;
        let entradas = match fs.listar(&inodo) {
            Ok(v) => v,
            Err(e) => {
                r.fallos.push(format!("{ruta_fs}: directorio ilegible: {e}"));
                return;
            }
        };
        for e in entradas {
            if e.nombre == "." || e.nombre == ".." {
                continue;
            }
            let hijo_fs = if ruta_fs.ends_with('/') { format!("{ruta_fs}{}", e.nombre) } else { format!("{ruta_fs}/{}", e.nombre) };
            let (n, cambio) = nombre_windows(&e.nombre);
            if cambio {
                r.renombrados.push(format!("{hijo_fs} -> {n}"));
            }
            rescatar_rec(fs, e.ino, &hijo_fs, dest.join(n), r, prof + 1);
        }
    } else if inodo.es_enlace() {
        match fs.leer_enlace(&inodo) {
            Ok(d) => r.enlaces.push(format!("{ruta_fs} -> {d}")),
            Err(e) => r.fallos.push(format!("{ruta_fs}: enlace ilegible: {e}")),
        }
    } else if inodo.es_regular() {
        // Por trozos: un archivo de varios GB no se carga entero en memoria.
        const TROZO: usize = 8 * 1024 * 1024;
        let res = (|| -> Result<u64, String> {
            let mut f = std::fs::File::create(&dest).map_err(|e| format!("crear: {e}"))?;
            let mut off = 0u64;
            while off < inodo.tamano {
                let d = fs.leer_rango(&inodo, off, TROZO).map_err(|e| format!("leer en {off}: {e}"))?;
                if d.is_empty() {
                    break;
                }
                f.write_all(&d).map_err(|e| format!("escribir: {e}"))?;
                off += d.len() as u64;
            }
            let mtime = std::time::UNIX_EPOCH + std::time::Duration::from_secs(u64::from(inodo.mtime));
            let _ = f.set_modified(mtime);
            Ok(off)
        })();
        match res {
            Ok(b) => {
                r.archivos += 1;
                r.bytes += b;
            }
            Err(e) => {
                r.fallos.push(format!("{ruta_fs}: {e}"));
                let _ = std::fs::remove_file(&dest); // nada de copias a medias que parezcan buenas
            }
        }
    }
}

#[cfg(test)]
mod pruebas {
    use super::nombre_windows;

    #[test]
    fn tamanos_legibles() {
        assert_eq!(super::parse_tamano("512M"), Some(512 << 20));
        assert_eq!(super::parse_tamano("2G"), Some(2 << 30));
        assert_eq!(super::parse_tamano("4096"), Some(4096));
        assert_eq!(super::parse_tamano("doce"), None);
        assert_eq!(super::parse_tamano("99999999999T"), None, "desbordamiento: None, no un tamaño inventado");
    }

    #[test]
    fn fecha_de_epoch() {
        assert_eq!(super::fecha(0), "1970-01-01 00:00 UTC");
        assert_eq!(super::fecha(1_762_329_600), "2025-11-05 08:00 UTC");
        assert_eq!(super::fecha(951_782_400), "2000-02-29 00:00 UTC", "año bisiesto");
    }

    #[test]
    fn nombres_invalidos_en_windows_se_sanean() {
        assert_eq!(nombre_windows("normal.txt"), ("normal.txt".into(), false));
        assert_eq!(nombre_windows("a:b*c?.txt"), ("a_b_c_.txt".into(), true));
        assert_eq!(nombre_windows("fin."), ("fin_".into(), true));
        assert_eq!(nombre_windows("CON"), ("_CON".into(), true));
        assert_eq!(nombre_windows("com1.log"), ("_com1.log".into(), true));
        assert_eq!(nombre_windows("console"), ("console".into(), false), "pareja: CONSOLE no es reservado");
        assert_eq!(nombre_windows("ñandú"), ("ñandú".into(), false));
    }
}
