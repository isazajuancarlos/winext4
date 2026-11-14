// SPDX-FileCopyrightText: 2026 Juan Carlos Isaza
// SPDX-License-Identifier: LicenseRef-Xiliux-Propietario
//! A1: recuperación del journal, por DIFERENCIAL contra e2fsprogs.
//!
//! Dos copias de la misma imagen sucia: A la reproduce `e2fsck -E journal_only` (e2fsprogs),
//! B la reproduce winext4. Deben quedar IDÉNTICAS bloque a bloque, salvo campos de tiempo del
//! superbloque. Dos fuentes de journals, porque cada una cubre lo que la otra no:
//! - el KERNEL de Linux (montar, escribir, `EXT4_IOC_SHUTDOWN`): formato real del mundo —
//!   pero el kernel ya había llevado casi todo a su sitio, así que SOLO no discrimina (lo cazó
//!   el control: 1 bloque cambiado, el del propio journal);
//! - `debugfs` (journal_write): contenido que NO está en su sitio, sumas v3 y REVOCACIÓN — aquí
//!   la reproducción cambia bloques de verdad y el revocado debe quedarse como estaba.

mod comun;

use comun::ruta;
use winext4_nucleo::{Ext4, InformeJournal};

const BS: usize = 4096;

fn abrir_rw(img: &std::path::Path) -> Ext4<std::fs::File> {
    let f = std::fs::OpenOptions::new().read(true).write(true).open(img).unwrap();
    Ext4::abrir(f).unwrap()
}

/// Recupera la imagen sucia con e2fsprogs (A) y con winext4 (B), exige que queden iguales y
/// devuelve (A, B, informe de winext4, bloques que la recuperación cambió).
fn diferencial(sucia: &std::path::Path) -> (Vec<u8>, Vec<u8>, InformeJournal, usize) {
    let a = sucia.with_extension("a.img");
    let b = sucia.with_extension("b.img");
    std::fs::copy(sucia, &a).unwrap();
    std::fs::copy(sucia, &b).unwrap();
    let o = comun::e2fs("e2fsck").args(["-E", "journal_only", "-y", &ruta(&a)]).output().unwrap();
    assert!(matches!(o.status.code(), Some(0 | 1)), "e2fsck journal_only: {}", String::from_utf8_lossy(&o.stdout));
    let inf = {
        let mut fs = abrir_rw(&b);
        assert!(fs.journal_pendiente(), "control D30: la imagen debe venir con needs_recovery");
        let inf = fs.recuperar_journal().unwrap();
        assert!(!fs.journal_pendiente(), "tras recuperar, needs_recovery desaparece");
        inf
    };
    let (va, vb, vs) = (std::fs::read(&a).unwrap(), std::fs::read(&b).unwrap(), std::fs::read(sucia).unwrap());
    let tolerados: &[std::ops::Range<usize>] = &[
        1024 + 0x2C..1024 + 0x38,   // s_mtime, s_wtime, s_mnt_count, s_max_mnt_count
        1024 + 0x40..1024 + 0x44,   // s_lastcheck
        1024 + 0x174..1024 + 0x180, // s_kbytes_written
        1024 + 0x3FC..1024 + 0x400, // s_checksum
    ];
    for k in 0..va.len() / BS {
        let (x, y) = (&va[k * BS..(k + 1) * BS], &vb[k * BS..(k + 1) * BS]);
        if x == y {
            continue;
        }
        for i in 0..BS {
            if x[i] != y[i] && !(k == 0 && tolerados.iter().any(|r| r.contains(&i))) {
                panic!("bloque {k} distinto en el byte {i:#x}: e2fsck={:#04x} winext4={:#04x}", x[i], y[i]);
            }
        }
    }
    let cambiados = (1..va.len() / BS).filter(|&k| va[k * BS..(k + 1) * BS] != vs[k * BS..(k + 1) * BS]).count();
    comun::fsck(&b).unwrap_or_else(|s| panic!("e2fsck tras recuperar con winext4:\n{s}"));
    // Pareja (D33): recuperar otra vez una imagen ya limpia no toca nada.
    let antes = std::fs::read(&b).unwrap();
    assert_eq!(abrir_rw(&b).recuperar_journal().unwrap(), InformeJournal::default());
    assert_eq!(std::fs::read(&b).unwrap(), antes, "sin journal pendiente, no se escribe nada");
    let _ = std::fs::remove_file(&a);
    let _ = std::fs::remove_file(&b);
    (va, vb, inf, cambiados)
}

#[test]
fn journal_de_debugfs_con_revocacion_igual_que_e2fsck() {
    if !comun::hay_oraculo() {
        eprintln!("SALTADA: faltan e2fsprogs");
        return;
    }
    let sucia = std::env::temp_dir().join(format!("winext4-jdbg-{}.img", std::process::id()));
    if !comun::guion_root("journal-debugfs.sh", &[&ruta(&sucia)]) {
        eprintln!("SALTADA: no se pudo fabricar el journal con debugfs");
        return;
    }
    let (_, vb, inf, cambiados) = diferencial(&sucia);
    eprintln!("debugfs: {inf:?}; bloques cambiados por la recuperación: {cambiados}");
    // T1 escribe 5000 y 5001; T2 reescribe 5001; T3 revoca 5000.
    assert_eq!(inf.transacciones, 3);
    assert_eq!(inf.revocados_saltados, 1, "el 5000 de T1 está revocado por T3");
    assert_eq!(inf.bloques_reproducidos, 2, "5001 dos veces (X y luego Y)");
    assert!(vb[5000 * BS..5001 * BS].iter().all(|&x| x == 0), "el revocado se queda como estaba");
    assert!(vb[5001 * BS..5002 * BS].iter().all(|&x| x == b'Y'), "5001 = la última escritura confirmada");
    assert!(cambiados >= 2, "control D30: la recuperación cambió bloques de verdad ({cambiados})");
    let _ = std::fs::remove_file(&sucia);
}

#[test]
fn journal_del_kernel_igual_que_e2fsck() {
    if !comun::hay_oraculo() {
        eprintln!("SALTADA: faltan e2fsprogs");
        return;
    }
    let sucia = std::env::temp_dir().join(format!("winext4-jker-{}.img", std::process::id()));
    if !comun::guion_root("journal-kernel.sh", &[&ruta(&sucia)]) {
        eprintln!("SALTADA: no se pudo fabricar el journal con el kernel (hace falta root/WSL)");
        return;
    }
    let (_, _, inf, cambiados) = diferencial(&sucia);
    eprintln!("kernel: {inf:?}; bloques cambiados: {cambiados}");
    assert!(inf.transacciones >= 1 && inf.bloques_reproducidos >= 1, "control D30: había transacciones del kernel");
    assert!(!inf.huerfanos_pendientes, "el archivo de huérfanos estaba vacío: orphan_present se quita");
    let _ = std::fs::remove_file(&sucia);
}
