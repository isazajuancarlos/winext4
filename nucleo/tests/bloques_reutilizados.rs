// SPDX-FileCopyrightText: 2026 Juan Carlos Isaza
// SPDX-License-Identifier: LicenseRef-Xiliux-Propietario
//! Un bloque LIBERADO que se reutiliza no puede dejar ver lo que tenía. Al escribir en un
//! bloque recién asignado, la parte que la escritura NO cubre tiene que leerse a cero (si no,
//! un archivo nuevo filtraría el contenido de uno borrado: fuga de datos).
//!
//! Se escribe un archivo con un patrón «secreto», se borra, y otro archivo reutiliza esos
//! bloques con escrituras PARCIALES (unos bytes dentro de un bloque) y un tramo que cruza
//! bloques (primero y último parciales, los del medio enteros). Control D30: B reutiliza de
//! verdad bloques de A. Sin oráculo externo: corre siempre.

use winext4_nucleo::Ext4;

const SECRETO: u8 = 0xAA;
const BS: u64 = 4096;

fn imagen(nombre: &str) -> std::path::PathBuf {
    let img = std::env::temp_dir().join(format!("winext4-reutil-{nombre}-{}.img", std::process::id()));
    let mut f = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&img).unwrap();
    f.set_len(64 * 1024 * 1024).unwrap();
    winext4_nucleo::formatear::formatear(&mut f, 64 * 1024 * 1024, "REUTIL").unwrap();
    img
}

#[test]
fn un_bloque_reutilizado_no_deja_ver_lo_que_tenia() {
    let img = imagen("parcial");
    let f = std::fs::OpenOptions::new().read(true).write(true).open(&img).unwrap();
    let mut fs = Ext4::abrir(f).unwrap();
    fs.abrir_escritura().unwrap();

    // A: 1 MiB de «secreto», luego borrado.
    let a = fs.crear_archivo(2, "a", 0o644).unwrap();
    fs.escribir(a, 0, &vec![SECRETO; (BS * 256) as usize]).unwrap();
    let ia = fs.inodo(a).unwrap();
    let bloques_a: std::collections::HashSet<u64> = fs.bloques_fisicos(&ia).unwrap().into_iter().flatten().collect();
    assert_eq!(bloques_a.len(), 256, "control: A ocupa 256 bloques");
    fs.borrar(2, "a").unwrap();

    // B: escrituras PARCIALES en bloques separados (cada una asigna un bloque nuevo) y un tramo
    // que cruza bloques con extremos parciales.
    let b = fs.crear_archivo(2, "b", 0o644).unwrap();
    let mut esperado = Vec::new();
    let poner = |fs: &mut Ext4<std::fs::File>, off: u64, datos: &[u8], esperado: &mut Vec<u8>| {
        fs.escribir(b, off, datos).unwrap();
        let fin = off as usize + datos.len();
        if esperado.len() < fin {
            esperado.resize(fin, 0);
        }
        esperado[off as usize..fin].copy_from_slice(datos);
    };
    for k in 0..40u64 {
        poner(&mut fs, k * BS * 2 + 100, b"dato", &mut esperado);
    }
    // Valores 1..=100: NUNCA el secreto (0xAA = 170), o la prueba encontraría su propia aguja.
    let tramo: Vec<u8> = (0..(BS * 3) as usize).map(|i| (i % 100) as u8 + 1).collect();
    poner(&mut fs, BS * 100 + 10, &tramo, &mut esperado);

    let ib = fs.inodo(b).unwrap();
    let reusados = fs.bloques_fisicos(&ib).unwrap().into_iter().flatten().filter(|x| bloques_a.contains(x)).count();
    assert!(reusados > 0, "control D30: B tiene que reutilizar bloques que fueron de A");

    let leido = fs.leer_datos(&ib).unwrap();
    assert_eq!(leido.len(), esperado.len());
    let fugas = leido.iter().filter(|&&x| x == SECRETO).count();
    assert_eq!(fugas, 0, "{fugas} bytes del archivo BORRADO se ven en el nuevo ({reusados} bloques reutilizados)");
    assert_eq!(leido, esperado, "el contenido de B es exactamente lo escrito, con ceros en lo no escrito");
    fs.cerrar_escritura().unwrap();
    drop(fs);
    let _ = std::fs::remove_file(&img);
}
