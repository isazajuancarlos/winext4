// SPDX-FileCopyrightText: 2026 Juan Carlos Isaza
// SPDX-License-Identifier: LicenseRef-Xiliux-Propietario
//! Monta el motor vía FUSE y lo consume por el **VFS real del kernel** (`std::fs`): el oráculo
//! más fuerte posible (D28) — no comparamos contra `debugfs`, sino contra lo que el kernel
//! mismo lee por nuestro driver. Es exactamente el binding que WinFsp conducirá en Windows.
//!
//! Se salta si no hay `/dev/fuse` o faltan `mkfs.ext4`/`debugfs`.

use std::io::Write;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use winext4_fuse::Ext4Fuse;
use winext4_nucleo::Ext4;

static SEQ: AtomicU64 = AtomicU64::new(0);

fn hay(bin: &str) -> bool {
    Command::new(bin).arg("--help").output().map(|_| true).unwrap_or(false)
}

#[test]
fn el_kernel_monta_y_lee_winext4_por_fuse() {
    if !std::path::Path::new("/dev/fuse").exists() {
        eprintln!("SALTADA: no hay /dev/fuse");
        return;
    }
    if !hay("mkfs.ext4") || !hay("debugfs") {
        eprintln!("SALTADA: faltan e2fsprogs");
        return;
    }

    let id = format!("{}-{}", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed));
    let base = std::env::temp_dir().join(format!("winext4-fuse-{id}"));
    let img = base.with_extension("img");
    let hola_src = base.with_extension("hola");
    let datos_src = base.with_extension("datos");
    let mnt = base.with_extension("mnt");

    // Imagen con contenido conocido.
    const HOLA: &[u8] = b"hola winext4\n";
    const DESTINO: &str = "/un/destino/de/prueba";
    let datos: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
    {
        let f = std::fs::File::create(&img).unwrap();
        f.set_len(16 * 1024 * 1024).unwrap();
    }
    std::fs::File::create(&hola_src).unwrap().write_all(HOLA).unwrap();
    std::fs::File::create(&datos_src).unwrap().write_all(&datos).unwrap();
    assert!(Command::new("mkfs.ext4").args(["-F", "-q", "-b", "4096", img.to_str().unwrap()]).status().unwrap().success());
    let guion = format!(
        "write {} hola.txt\nwrite {} datos.bin\nsymlink /ln {DESTINO}\nmkdir /sub\nwrite {} /sub/dentro.txt\n",
        hola_src.display(),
        datos_src.display(),
        hola_src.display(),
    );
    let cmds = base.with_extension("cmds");
    std::fs::write(&cmds, guion).unwrap();
    assert!(Command::new("debugfs").args(["-w", "-f", cmds.to_str().unwrap(), img.to_str().unwrap()]).status().unwrap().success());

    std::fs::create_dir_all(&mnt).unwrap();
    let motor = Ext4::abrir(std::fs::File::open(&img).unwrap()).expect("abrir ext4");
    // Montaje en un hilo de fondo; el guard desmonta al soltarse.
    let sesion = match fuser::spawn_mount2(Ext4Fuse::nuevo(motor), &mnt, &[fuser::MountOption::RO]) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("SALTADA: FUSE no disponible en este entorno ({e})");
            return;
        }
    };

    // Esperar a que el montaje esté listo (lo hace verdadero el hilo de la sesión; tope 10 s).
    let fin = Instant::now() + Duration::from_secs(10);
    while std::fs::read_dir(&mnt).map(|mut d| d.next().is_none()).unwrap_or(true) {
        if Instant::now() > fin {
            drop(sesion);
            panic!("el montaje no estuvo listo a tiempo");
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    // Consumir por el kernel real (D28): listar, leer, symlink, subdirectorio.
    let nombres: std::collections::HashSet<String> = std::fs::read_dir(&mnt)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    for n in ["hola.txt", "datos.bin", "ln", "sub"] {
        assert!(nombres.contains(n), "falta {n} en {nombres:?}");
    }
    assert_eq!(std::fs::read(mnt.join("hola.txt")).unwrap(), HOLA);
    assert_eq!(std::fs::read(mnt.join("datos.bin")).unwrap(), datos, "100k bytes por el kernel");
    assert_eq!(std::fs::read_link(mnt.join("ln")).unwrap().to_str().unwrap(), DESTINO);
    assert_eq!(std::fs::read(mnt.join("sub/dentro.txt")).unwrap(), HOLA);

    drop(sesion); // desmonta
    // Limpieza best-effort.
    for p in [&img, &hola_src, &datos_src, &cmds] {
        let _ = std::fs::remove_file(p);
    }
    let _ = std::fs::remove_dir(&mnt);
}
