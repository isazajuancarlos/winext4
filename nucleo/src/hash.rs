// SPDX-FileCopyrightText: 2026 Juan Carlos Isaza
// SPDX-License-Identifier: LicenseRef-Xiliux-Propietario
//! Hash de nombres de los directorios indexados (htree / `dir_index`): decide en qué hoja
//! vive cada nombre. Si se calcula distinto que el kernel, un archivo creado aquí existe en
//! disco pero el kernel no lo encuentra (y `e2fsck` lo marca en la hoja equivocada).
//!
//! Variantes del formato: `half_md4` (la de `mkfs.ext4` por defecto) y `tea`, cada una con
//! lectura de bytes CON signo o SIN signo según `s_flags` del superbloque. `legacy` no se
//! implementa (volúmenes de antes de 2002): se rechaza al escribir.
//!
//! Validado contra directorios indexados REALES fabricados por `e2fsck -D`
//! (`tests/directorios.rs`): cada nombre debe caer en la hoja donde e2fsprogs lo puso.

/// Versión de hash de `dx_root_info.hash_version` / `s_def_hash_version`.
pub const HASH_LEGACY: u8 = 0;
/// `half_md4`.
pub const HASH_HALF_MD4: u8 = 1;
/// `tea`.
pub const HASH_TEA: u8 = 2;

/// Fin de los hashes de 32 bits (`EXT4_HTREE_EOF_32BIT`).
const EOF_32: u32 = 0x7FFF_FFFF;

/// Empaqueta `msg` en `num` palabras de 32 bits, con el relleno que define el formato
/// (la longitud repetida en cada byte). `con_signo`: los bytes se leen como `i8`.
fn a_palabras(msg: &[u8], num: usize, con_signo: bool) -> Vec<u32> {
    let len = msg.len() as u32;
    let mut pad = len | (len << 8);
    pad |= pad << 16;
    let mut out = Vec::with_capacity(num);
    let mut val = pad;
    let mut restantes = num as i32;
    let n = msg.len().min(num * 4);
    for (i, &b) in msg[..n].iter().enumerate() {
        let c = if con_signo { b as i8 as i32 as u32 } else { b as u32 };
        val = c.wrapping_add(val << 8);
        if i % 4 == 3 {
            out.push(val);
            val = pad;
            restantes -= 1;
        }
    }
    restantes -= 1;
    if restantes >= 0 {
        out.push(val);
    }
    while out.len() < num {
        out.push(pad);
    }
    out
}

fn half_md4(buf: &mut [u32; 4], x: &[u32]) -> u32 {
    let f = |x: u32, y: u32, z: u32| z ^ (x & (y ^ z));
    let g = |x: u32, y: u32, z: u32| (x & y).wrapping_add((x ^ y) & z);
    let h = |x: u32, y: u32, z: u32| x ^ y ^ z;
    let (mut a, mut b, mut c, mut d) = (buf[0], buf[1], buf[2], buf[3]);
    macro_rules! r {
        ($f:expr, $a:ident, $b:ident, $c:ident, $d:ident, $x:expr, $s:expr) => {
            $a = $a.wrapping_add($f($b, $c, $d)).wrapping_add($x).rotate_left($s);
        };
    }
    const K2: u32 = 0x5A82_7999;
    const K3: u32 = 0x6ED9_EBA1;
    r!(f, a, b, c, d, x[0], 3);
    r!(f, d, a, b, c, x[1], 7);
    r!(f, c, d, a, b, x[2], 11);
    r!(f, b, c, d, a, x[3], 19);
    r!(f, a, b, c, d, x[4], 3);
    r!(f, d, a, b, c, x[5], 7);
    r!(f, c, d, a, b, x[6], 11);
    r!(f, b, c, d, a, x[7], 19);
    r!(g, a, b, c, d, x[1].wrapping_add(K2), 3);
    r!(g, d, a, b, c, x[3].wrapping_add(K2), 5);
    r!(g, c, d, a, b, x[5].wrapping_add(K2), 9);
    r!(g, b, c, d, a, x[7].wrapping_add(K2), 13);
    r!(g, a, b, c, d, x[0].wrapping_add(K2), 3);
    r!(g, d, a, b, c, x[2].wrapping_add(K2), 5);
    r!(g, c, d, a, b, x[4].wrapping_add(K2), 9);
    r!(g, b, c, d, a, x[6].wrapping_add(K2), 13);
    r!(h, a, b, c, d, x[3].wrapping_add(K3), 3);
    r!(h, d, a, b, c, x[7].wrapping_add(K3), 9);
    r!(h, c, d, a, b, x[2].wrapping_add(K3), 11);
    r!(h, b, c, d, a, x[6].wrapping_add(K3), 15);
    r!(h, a, b, c, d, x[1].wrapping_add(K3), 3);
    r!(h, d, a, b, c, x[5].wrapping_add(K3), 9);
    r!(h, c, d, a, b, x[0].wrapping_add(K3), 11);
    r!(h, b, c, d, a, x[4].wrapping_add(K3), 15);
    buf[0] = buf[0].wrapping_add(a);
    buf[1] = buf[1].wrapping_add(b);
    buf[2] = buf[2].wrapping_add(c);
    buf[3] = buf[3].wrapping_add(d);
    buf[1]
}

fn tea(buf: &mut [u32; 4], x: &[u32]) {
    let (mut b0, mut b1) = (buf[0], buf[1]);
    let (a, b, c, d) = (x[0], x[1], x[2], x[3]);
    let mut suma: u32 = 0;
    for _ in 0..16 {
        suma = suma.wrapping_add(0x9E37_79B9);
        b0 = b0.wrapping_add(
            (b1 << 4).wrapping_add(a) ^ b1.wrapping_add(suma) ^ (b1 >> 5).wrapping_add(b),
        );
        b1 = b1.wrapping_add(
            (b0 << 4).wrapping_add(c) ^ b0.wrapping_add(suma) ^ (b0 >> 5).wrapping_add(d),
        );
    }
    buf[0] = buf[0].wrapping_add(b0);
    buf[1] = buf[1].wrapping_add(b1);
}

/// Hash mayor de `nombre` (el que ordena el índice), o `None` si la versión no está
/// soportada. `semilla` = `s_hash_seed`; `con_signo` sale de `s_flags`.
#[must_use]
pub fn hash_nombre(nombre: &[u8], version: u8, semilla: [u32; 4], con_signo: bool) -> Option<u32> {
    let mut buf = [0x6745_2301, 0xEFCD_AB89, 0x98BA_DCFE, 0x1032_5476];
    if semilla.iter().any(|&w| w != 0) {
        buf = semilla;
    }
    let hash = match version {
        HASH_HALF_MD4 => {
            let mut p = nombre;
            loop {
                let palabras = a_palabras(p, 8, con_signo);
                half_md4(&mut buf, &palabras);
                if p.len() <= 32 {
                    break;
                }
                p = &p[32..];
            }
            buf[1]
        }
        HASH_TEA => {
            let mut p = nombre;
            loop {
                let palabras = a_palabras(p, 4, con_signo);
                tea(&mut buf, &palabras);
                if p.len() <= 16 {
                    break;
                }
                p = &p[16..];
            }
            buf[0]
        }
        _ => return None,
    };
    let mut h = hash & !1;
    if h == EOF_32 << 1 {
        h = (EOF_32 - 1) << 1;
    }
    Some(h)
}

#[cfg(test)]
mod pruebas {
    use super::*;

    #[test]
    fn el_relleno_repite_la_longitud() {
        // 2 bytes «ab», 8 palabras: la primera lleva los bytes sobre el relleno, el resto es
        // relleno puro (0x02020202).
        let w = a_palabras(b"ab", 8, false);
        assert_eq!(w.len(), 8);
        assert_eq!(w[1..], [0x0202_0202; 7]);
        assert_eq!(w[0], (0x0202_0202u32 << 16).wrapping_add((b'a' as u32) << 8).wrapping_add(b'b' as u32));
    }

    #[test]
    fn con_y_sin_signo_solo_difieren_en_bytes_altos() {
        let s = [1, 2, 3, 4];
        assert_eq!(hash_nombre(b"abc", 1, s, true), hash_nombre(b"abc", 1, s, false));
        assert_ne!(hash_nombre("ñandú".as_bytes(), 1, s, true), hash_nombre("ñandú".as_bytes(), 1, s, false));
    }

    #[test]
    fn el_hash_es_par_y_legacy_no_se_soporta() {
        let h = hash_nombre(b"archivo.txt", HASH_HALF_MD4, [9, 9, 9, 9], true).unwrap();
        assert_eq!(h & 1, 0, "el bit bajo es la marca de colisión del índice, siempre a 0");
        assert_eq!(hash_nombre(b"x", HASH_LEGACY, [0; 4], true), None);
    }
}
