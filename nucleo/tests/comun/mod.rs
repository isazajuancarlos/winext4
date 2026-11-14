// SPDX-FileCopyrightText: 2026 Juan Carlos Isaza
// SPDX-License-Identifier: LicenseRef-Xiliux-Propietario
//! Acceso al ORÁCULO (`e2fsprogs`: `mkfs.ext4`, `debugfs`, `e2fsck`) desde las pruebas, en
//! Linux y en Windows. En Linux se llaman directo; en Windows no existen, así que se corren
//! dentro de WSL (`wsl.exe -d <distro> -e <bin>`, distro por `WINEXT4_WSL`, por defecto
//! `Debian`) y las rutas de Windows se traducen a `/mnt/<unidad>/...`.
//!
//! Por qué importa (D30): sin esto, en Windows TODAS las pruebas contra ext4 real se saltan
//! solas y el banco sale verde sin haber medido nada.

#![allow(dead_code)] // cada archivo de pruebas usa un subconjunto

use std::path::Path;
use std::process::Command;

/// Distro de WSL que lleva `e2fsprogs` (solo Windows).
#[cfg(windows)]
fn distro() -> String {
    std::env::var("WINEXT4_WSL").unwrap_or_else(|_| "Debian".to_string())
}

/// Un `Command` listo para lanzar la herramienta `bin` de e2fsprogs en esta plataforma.
#[cfg(not(windows))]
pub fn e2fs(bin: &str) -> Command {
    Command::new(bin)
}

/// Un `Command` listo para lanzar la herramienta `bin` de e2fsprogs en esta plataforma.
#[cfg(windows)]
pub fn e2fs(bin: &str) -> Command {
    // `-e` no pasa por un shell de login: el PATH no trae `/usr/sbin`, donde viven
    // `mkfs.ext4`/`debugfs`/`e2fsck` para un usuario no-root. Se fija explícito.
    let mut c = Command::new("wsl.exe");
    c.args(["-d", &distro(), "-e", "env", WSL_PATH, bin]);
    c
}

/// PATH completo dentro de WSL (incluye `sbin`).
#[cfg(windows)]
const WSL_PATH: &str = "PATH=/usr/sbin:/usr/bin:/sbin:/bin";

/// La ruta `p` tal como la ve la herramienta (en Windows, `C:\a\b` → `/mnt/c/a/b`).
#[cfg(not(windows))]
pub fn ruta(p: &Path) -> String {
    p.display().to_string()
}

/// La ruta `p` tal como la ve la herramienta (en Windows, `C:\a\b` → `/mnt/c/a/b`).
#[cfg(windows)]
pub fn ruta(p: &Path) -> String {
    let s = p.display().to_string();
    let b = s.as_bytes();
    assert!(
        b.len() >= 3 && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/'),
        "ruta no absoluta de unidad, WSL no la vería: {s}"
    );
    let unidad = (b[0] as char).to_ascii_lowercase();
    format!("/mnt/{unidad}/{}", s[3..].replace('\\', "/"))
}

/// ¿Está disponible la herramienta `bin`? (En Windows: WSL con la distro y el binario dentro.)
#[cfg(not(windows))]
pub fn hay(bin: &str) -> bool {
    Command::new(bin).arg("-V").output().is_ok()
}

/// ¿Está disponible la herramienta `bin`? (En Windows: WSL con la distro y el binario dentro.)
#[cfg(windows)]
pub fn hay(bin: &str) -> bool {
    Command::new("wsl.exe")
        .args(["-d", &distro(), "-e", "env", WSL_PATH, "sh", "-c", &format!("command -v {bin}")])
        .output()
        .is_ok_and(|o| o.status.success())
}

/// ¿Está el oráculo completo (`mkfs.ext4`, `debugfs`, `e2fsck`)?
pub fn hay_oraculo() -> bool {
    hay("mkfs.ext4") && hay("debugfs") && hay("e2fsck")
}

/// Corre `bin args`; `Some(true)` si salió 0, `Some(false)` si falló, `None` si no existe.
pub fn corre(bin: &str, args: &[&str]) -> Option<bool> {
    if !hay(bin) {
        return None;
    }
    let o = e2fs(bin).args(args).output().unwrap_or_else(|e| panic!("no se pudo ejecutar {bin}: {e}"));
    Some(o.status.success())
}

/// Corre un guion de `tests/guiones/` COMO ROOT (montar una imagen lo exige): en Windows dentro
/// de WSL; en Linux solo si ya somos root. `false` si no se pudo (la prueba se salta diciéndolo).
pub fn guion_root(nombre: &str, args: &[&str]) -> bool {
    let guion = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("guiones").join(nombre);
    #[cfg(windows)]
    let o = {
        let mut a = vec!["-d".to_string(), std::env::var("WINEXT4_WSL").unwrap_or_else(|_| "Debian".into()), "-u".into(), "root".into(), "--".into(), "sh".into(), ruta(&guion)];
        a.extend(args.iter().map(|s| (*s).to_string()));
        Command::new("wsl.exe").args(&a).output()
    };
    #[cfg(not(windows))]
    let o = Command::new("sh").arg(&guion).args(args).output();
    match o {
        Ok(o) if o.status.success() => true,
        Ok(o) => {
            eprintln!("guion {nombre} falló: {}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
            false
        }
        Err(e) => {
            eprintln!("no se pudo lanzar {nombre}: {e}");
            false
        }
    }
}

/// El contenido de `ruta_fs` dentro de la imagen, leído por `debugfs` (lector INDEPENDIENTE
/// del nuestro: si la escritura y nuestra lectura se equivocaran igual, aquí se nota).
pub fn volcar(img: &Path, ruta_fs: &str) -> Vec<u8> {
    let destino = img.with_extension(format!("dump{}", ruta_fs.len()));
    let guion = format!("dump {ruta_fs} {}", ruta(&destino));
    let o = e2fs("debugfs").args(["-R", &guion, &ruta(img)]).output().expect("lanzar debugfs");
    assert!(o.status.success(), "debugfs dump falló: {}", String::from_utf8_lossy(&o.stderr));
    let v = std::fs::read(&destino).unwrap_or_else(|e| panic!("debugfs no volcó {ruta_fs}: {e}"));
    let _ = std::fs::remove_file(&destino);
    v
}

/// `e2fsck -fn` sobre la imagen: `Ok(())` si el FS está LIMPIO, `Err(salida)` si no.
pub fn fsck(img: &Path) -> Result<(), String> {
    let o = e2fs("e2fsck").args(["-fn", &ruta(img)]).output().expect("lanzar e2fsck");
    if o.status.success() {
        Ok(())
    } else {
        Err(format!(
            "{}{}",
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr)
        ))
    }
}
