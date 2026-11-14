// SPDX-FileCopyrightText: 2026 Juan Carlos Isaza
// SPDX-License-Identifier: LicenseRef-Xiliux-Propietario
//! `winext4-fuse <imagen-o-dispositivo> <punto-de-montaje>` — monta un ext4 como FS de solo
//! lectura vía FUSE. Desmontar: `fusermount3 -u <punto>` o Ctrl-C.

use std::process::ExitCode;

use fuser::MountOption;
use winext4_fuse::Ext4Fuse;
use winext4_nucleo::Ext4;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 {
        eprintln!("uso: {} <imagen-o-dispositivo> <punto-de-montaje>", args[0]);
        return ExitCode::FAILURE;
    }
    let (imagen, punto) = (&args[1], &args[2]);

    let archivo = match std::fs::File::open(imagen) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("no se pudo abrir {imagen}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let motor = match Ext4::abrir(archivo) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("{imagen} no es un ext4 legible: {e}");
            return ExitCode::FAILURE;
        }
    };

    let opciones = vec![
        MountOption::RO,
        MountOption::FSName("winext4".to_string()),
        MountOption::Subtype("ext4".to_string()),
    ];
    eprintln!("montado {imagen} en {punto} (solo lectura). Ctrl-C o `fusermount3 -u {punto}` para salir.");
    if let Err(e) = fuser::mount2(Ext4Fuse::nuevo(motor), punto, &opciones) {
        eprintln!("fallo al montar: {e}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}
