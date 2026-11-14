// SPDX-FileCopyrightText: 2026 Juan Carlos Isaza
// SPDX-License-Identifier: LicenseRef-Xiliux-Propietario
//! **Dispositivo de bloques**: el adaptador entre el motor (que cree leer un volumen que empieza
//! en el byte 0) y lo real — una imagen en archivo, o un disco físico entero
//! (`\\.\PhysicalDriveN` en Windows, `/dev/sdX` en Linux) donde el ext4 empieza en el offset de
//! SU partición y la E/S debe ir alineada al sector.
//!
//! Lo comparten el montador (Dokan) y las herramientas (`winext4util`): un solo sitio que sabe
//! de GPT, offsets y alineación (D9).

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};

/// Traduce posiciones del motor (relativas a `base`) a E/S alineada sobre el dispositivo.
pub struct DispositivoBloque<R> {
    disp: R,
    base: u64,
    sec: u64,
    pos: u64,
}

impl<R: Read + Seek> DispositivoBloque<R> {
    /// `base`: offset en bytes de la partición; `sec`: alineación exigida (1 = ninguna).
    pub fn nuevo(disp: R, base: u64, sec: u64) -> Self {
        Self { disp, base, sec: sec.max(1), pos: 0 }
    }

    /// Cambia el offset de la partición (tras detectarla en la GPT).
    pub fn reposicionar(&mut self, base: u64) {
        self.base = base;
        self.pos = 0;
    }

    fn leer_alineado(&mut self, ini: u64, largo: usize) -> io::Result<(Vec<u8>, usize)> {
        self.disp.seek(SeekFrom::Start(ini))?;
        let mut tmp = vec![0u8; largo];
        let mut leidos = 0usize;
        while leidos < tmp.len() {
            match self.disp.read(&mut tmp[leidos..]) {
                Ok(0) => break,
                Ok(n) => leidos += n,
                Err(ref e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok((tmp, leidos))
    }
}

impl<R: Read + Seek> Read for DispositivoBloque<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let abs = self.base + self.pos;
        let ini = abs - abs % self.sec;
        let fin = (abs + buf.len() as u64).div_ceil(self.sec) * self.sec;
        let (tmp, leidos) = self.leer_alineado(ini, (fin - ini) as usize)?;
        let desfase = (abs - ini) as usize;
        if desfase >= leidos {
            return Ok(0);
        }
        let n = buf.len().min(leidos - desfase);
        buf[..n].copy_from_slice(&tmp[desfase..desfase + n]);
        self.pos += n as u64;
        Ok(n)
    }
}

/// Escritura alineada (leer-parchear-reescribir el tramo si hay sectores). `flush` llega al
/// DISCO (`sync_data`), no solo a la caché del sistema: la pérdida de datos por caché al
/// desenchufar es el fallo nº 1 documentado de Ext4Fsd (#82).
impl Write for DispositivoBloque<File> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let abs = self.base + self.pos;
        if self.sec == 1 || (abs.is_multiple_of(self.sec) && (buf.len() as u64).is_multiple_of(self.sec)) {
            // Sin sectores, o la escritura ya llega ALINEADA y completa (el caso de un bloque
            // de 4 KiB en un disco de 4 KiB): directa, sin leer antes lo que se va a pisar.
            self.disp.seek(SeekFrom::Start(abs))?;
            self.disp.write_all(buf)?;
        } else {
            let ini = abs - abs % self.sec;
            let fin = (abs + buf.len() as u64).div_ceil(self.sec) * self.sec;
            let (mut tmp, _) = self.leer_alineado(ini, (fin - ini) as usize)?;
            let d = (abs - ini) as usize;
            tmp[d..d + buf.len()].copy_from_slice(buf);
            self.disp.seek(SeekFrom::Start(ini))?;
            self.disp.write_all(&tmp)?;
        }
        self.pos += buf.len() as u64;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.disp.flush()?;
        self.disp.sync_data()
    }
}

impl<R: Read + Seek> Seek for DispositivoBloque<R> {
    fn seek(&mut self, de: SeekFrom) -> io::Result<u64> {
        self.pos = match de {
            SeekFrom::Start(n) => n,
            SeekFrom::Current(d) => self.pos.saturating_add_signed(d),
            SeekFrom::End(_) => {
                return Err(io::Error::new(io::ErrorKind::Unsupported, "DispositivoBloque: SeekFrom::End no soportado"));
            }
        };
        Ok(self.pos)
    }
}

/// GUID de tipo de partición **Linux filesystem data** (`0FC63DAF-8483-4772-8E79-3D69D8477DE4`)
/// en su representación en disco (campos 1–3 little-endian, 4–5 big-endian).
pub const GUID_LINUX_FS: [u8; 16] = [
    0xAF, 0x3D, 0xC6, 0x0F, 0x83, 0x84, 0x72, 0x47, 0x8E, 0x79, 0x3D, 0x69, 0xD8, 0x47, 0x7D, 0xE4,
];

/// Offset en bytes de la primera partición Linux-fs de la GPT de `disp` (sector lógico de 512).
/// `None` si no hay GPT válida o ninguna partición Linux.
///
/// # Errors
/// Errores de E/S del dispositivo.
pub fn offset_particion_ext4<R: Read + Seek>(disp: &mut R) -> io::Result<Option<u64>> {
    Ok(particiones_linux(disp)?.first().copied())
}

/// (offset, tamaño en bytes) de la primera partición Linux-fs de la GPT (sector de 512).
///
/// # Errors
/// Errores de E/S del dispositivo.
pub fn particion_linux_con_tamano<R: Read + Seek>(disp: &mut R) -> io::Result<Option<(u64, u64)>> {
    const SEC: u64 = 512;
    disp.seek(SeekFrom::Start(SEC))?;
    let mut cab = [0u8; 92];
    if disp.read_exact(&mut cab).is_err() || &cab[0..8] != b"EFI PART" {
        return Ok(None);
    }
    let lba = u64::from_le_bytes(cab[72..80].try_into().expect("8"));
    let n = u32::from_le_bytes(cab[80..84].try_into().expect("4"));
    let tam = u32::from_le_bytes(cab[84..88].try_into().expect("4")) as usize;
    if tam < 128 || n == 0 || n > 1024 {
        return Ok(None);
    }
    disp.seek(SeekFrom::Start(lba * SEC))?;
    let mut e = vec![0u8; tam];
    for _ in 0..n {
        if disp.read_exact(&mut e).is_err() {
            break;
        }
        if e[0..16] == GUID_LINUX_FS {
            let primero = u64::from_le_bytes(e[32..40].try_into().expect("8"));
            let ultimo = u64::from_le_bytes(e[40..48].try_into().expect("8"));
            return Ok(Some((primero * SEC, (ultimo + 1 - primero) * SEC)));
        }
    }
    Ok(None)
}

/// Offsets de TODAS las particiones Linux-fs de la GPT (para montar o listar varias).
///
/// # Errors
/// Errores de E/S del dispositivo.
pub fn particiones_linux<R: Read + Seek>(disp: &mut R) -> io::Result<Vec<u64>> {
    const SEC: u64 = 512;
    disp.seek(SeekFrom::Start(SEC))?;
    let mut cab = [0u8; 92];
    if disp.read_exact(&mut cab).is_err() || &cab[0..8] != b"EFI PART" {
        return Ok(Vec::new());
    }
    let lba = u64::from_le_bytes(cab[72..80].try_into().expect("8"));
    let n = u32::from_le_bytes(cab[80..84].try_into().expect("4"));
    let tam = u32::from_le_bytes(cab[84..88].try_into().expect("4")) as usize;
    if tam < 128 || n == 0 || n > 1024 {
        return Ok(Vec::new());
    }
    disp.seek(SeekFrom::Start(lba * SEC))?;
    let mut e = vec![0u8; tam];
    let mut v = Vec::new();
    for _ in 0..n {
        if disp.read_exact(&mut e).is_err() {
            break;
        }
        if e[0..16] == GUID_LINUX_FS {
            v.push(u64::from_le_bytes(e[32..40].try_into().expect("8")) * SEC);
        }
    }
    Ok(v)
}

/// Interpreta un offset: decimal (`17408`) o hexadecimal (`0x4400`).
#[must_use]
pub fn parse_offset(s: &str) -> Option<u64> {
    let s = s.trim();
    match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(h) => u64::from_str_radix(h, 16).ok(),
        None => s.parse::<u64>().ok(),
    }
}

/// ¿Es `ruta` un disco físico crudo (exige alineación y, para escribir, otra política)?
#[must_use]
pub fn es_disco_fisico(ruta: &str) -> bool {
    let r = ruta.to_ascii_lowercase();
    r.contains("physicaldrive") || r.starts_with("/dev/")
}

/// Abre `ruta` (imagen o disco físico) como dispositivo listo para el motor: detecta la
/// partición ext4 en la GPT si es un disco y no se dio `offset`. `escritura` abre R/W.
///
/// # Errors
/// E/S, o `NotFound` si un disco no tiene partición Linux y no se dio offset.
pub fn abrir(ruta: &str, offset: Option<u64>, escritura: bool) -> io::Result<(DispositivoBloque<File>, u64)> {
    let fisico = es_disco_fisico(ruta);
    let f = std::fs::OpenOptions::new().read(true).write(escritura).open(ruta)?;
    let mut d = DispositivoBloque::nuevo(f, 0, if fisico { 4096 } else { 1 });
    let base = match offset {
        Some(o) => o,
        None if fisico => offset_particion_ext4(&mut d)?.ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "no hay partición Linux/ext4 en la GPT (da el offset con --offset)")
        })?,
        None => 0,
    };
    d.reposicionar(base);
    Ok((d, base))
}

/// Cabecera del archivo para deshacer.
const DESHACER_MAGIA: &[u8; 20] = b"WINEXT4-DESHACER-v1\n";
/// Granularidad de lo que se guarda (un bloque de 4 KiB del volumen).
const DESHACER_TROZO: u64 = 4096;

/// **Registro para deshacer** (como `e2undo`): ANTES de cada escritura guarda en `undo` el
/// contenido ORIGINAL de cada trozo de 4 KiB que se va a pisar (cada trozo una sola vez, la
/// primera), y lo vacía al disco del archivo; solo después deja pasar la escritura. Con
/// [`aplicar_deshacer`] se restauran los originales.
///
/// Formato: cabecera + `u64` tamaño de trozo + registros `[u64 offset][bytes del trozo]`.
pub struct ConDeshacer<D> {
    disp: D,
    undo: File,
    pos: u64,
    guardados: std::collections::HashSet<u64>,
}

impl<D: Read + Write + Seek> ConDeshacer<D> {
    /// Envuelve `disp`, creando el archivo para deshacer en `ruta` (no debe existir: no se
    /// pisa uno anterior, que podría ser el único camino de vuelta de otra reparación).
    ///
    /// # Errors
    /// E/S, o `AlreadyExists` si `ruta` ya existe.
    pub fn nuevo(disp: D, ruta: &std::path::Path) -> io::Result<Self> {
        let mut undo = std::fs::OpenOptions::new().write(true).create_new(true).open(ruta)?;
        undo.write_all(DESHACER_MAGIA)?;
        undo.write_all(&DESHACER_TROZO.to_le_bytes())?;
        undo.sync_all()?;
        Ok(Self { disp, undo, pos: 0, guardados: std::collections::HashSet::new() })
    }

    /// Trozos guardados hasta ahora.
    #[must_use]
    pub fn trozos_guardados(&self) -> usize {
        self.guardados.len()
    }

    fn guardar_originales(&mut self, desde: u64, largo: usize) -> io::Result<()> {
        let ini = desde - desde % DESHACER_TROZO;
        let fin = desde + largo as u64;
        let mut t = ini;
        let mut nuevos = false;
        while t < fin {
            if self.guardados.insert(t) {
                let mut orig = vec![0u8; DESHACER_TROZO as usize];
                self.disp.seek(SeekFrom::Start(t))?;
                let mut leidos = 0;
                while leidos < orig.len() {
                    match self.disp.read(&mut orig[leidos..])? {
                        0 => break,
                        n => leidos += n,
                    }
                }
                self.undo.write_all(&t.to_le_bytes())?;
                self.undo.write_all(&orig)?;
                nuevos = true;
            }
            t += DESHACER_TROZO;
        }
        if nuevos {
            // El original llega al disco ANTES de pisarlo: si se corta aquí, se puede deshacer.
            self.undo.sync_data()?;
        }
        Ok(())
    }
}

impl<D: Read + Write + Seek> Read for ConDeshacer<D> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.disp.seek(SeekFrom::Start(self.pos))?;
        let n = self.disp.read(buf)?;
        self.pos += n as u64;
        Ok(n)
    }
}

impl<D: Read + Write + Seek> Write for ConDeshacer<D> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let p = self.pos;
        self.guardar_originales(p, buf.len())?;
        self.disp.seek(SeekFrom::Start(p))?;
        let n = self.disp.write(buf)?;
        self.pos = p + n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.disp.flush()
    }
}

impl<D: Read + Write + Seek> Seek for ConDeshacer<D> {
    fn seek(&mut self, de: SeekFrom) -> io::Result<u64> {
        self.pos = match de {
            SeekFrom::Start(n) => n,
            SeekFrom::Current(d) => self.pos.saturating_add_signed(d),
            SeekFrom::End(_) => return Err(io::Error::new(io::ErrorKind::Unsupported, "ConDeshacer: SeekFrom::End no soportado")),
        };
        Ok(self.pos)
    }
}

/// Restaura sobre `disp` los originales guardados en el archivo para deshacer `ruta`.
/// Devuelve cuántos trozos restauró.
///
/// # Errors
/// E/S, o `InvalidData` si el archivo no es un registro de winext4 válido.
pub fn aplicar_deshacer<D: Read + Write + Seek>(disp: &mut D, ruta: &std::path::Path) -> io::Result<usize> {
    let datos = std::fs::read(ruta)?;
    let malo = |m: &str| io::Error::new(io::ErrorKind::InvalidData, m.to_string());
    if datos.len() < 28 || &datos[..20] != DESHACER_MAGIA {
        return Err(malo("no es un archivo para deshacer de winext4"));
    }
    let trozo = u64::from_le_bytes(datos[20..28].try_into().expect("8")) as usize;
    if trozo == 0 || !(datos.len() - 28).is_multiple_of(8 + trozo) {
        return Err(malo("archivo para deshacer truncado o corrupto"));
    }
    let mut n = 0;
    for r in datos[28..].chunks(8 + trozo) {
        let off = u64::from_le_bytes(r[..8].try_into().expect("8"));
        disp.seek(SeekFrom::Start(off))?;
        disp.write_all(&r[8..])?;
        n += 1;
    }
    disp.flush()?;
    Ok(n)
}

#[cfg(test)]
mod pruebas {
    use super::*;
    use std::io::Cursor;

    fn disco_gpt(primer_lba: u64, guid: [u8; 16]) -> Vec<u8> {
        let mut d = vec![0u8; 2 * 1024 * 1024];
        d[512..520].copy_from_slice(b"EFI PART");
        d[512 + 72..512 + 80].copy_from_slice(&2u64.to_le_bytes());
        d[512 + 80..512 + 84].copy_from_slice(&4u32.to_le_bytes());
        d[512 + 84..512 + 88].copy_from_slice(&128u32.to_le_bytes());
        let e = 2 * 512;
        d[e..e + 16].copy_from_slice(&guid);
        d[e + 32..e + 40].copy_from_slice(&primer_lba.to_le_bytes());
        d
    }

    #[test]
    fn gpt_detecta_particion_linux() {
        let mut dev = DispositivoBloque::nuevo(Cursor::new(disco_gpt(2048, GUID_LINUX_FS)), 0, 4096);
        assert_eq!(offset_particion_ext4(&mut dev).unwrap(), Some(1024 * 1024));
    }

    #[test]
    fn gpt_ignora_particion_no_linux() {
        let mut dev = DispositivoBloque::nuevo(Cursor::new(disco_gpt(2048, [0x11u8; 16])), 0, 4096);
        assert_eq!(offset_particion_ext4(&mut dev).unwrap(), None);
    }

    #[test]
    fn sin_gpt_devuelve_none() {
        let mut dev = DispositivoBloque::nuevo(Cursor::new(vec![0u8; 1024 * 1024]), 0, 4096);
        assert_eq!(offset_particion_ext4(&mut dev).unwrap(), None);
    }

    #[test]
    fn adaptador_traduce_offset_y_alinea() {
        let base = 17408u64;
        let patron = [0xDE, 0xAD, 0xBE, 0xEF, 0x53, 0xEF];
        let mut d = vec![0u8; base as usize + 4096];
        let p = base as usize + 1024;
        d[p..p + patron.len()].copy_from_slice(&patron);
        let mut dev = DispositivoBloque::nuevo(Cursor::new(d), base, 4096);
        dev.seek(SeekFrom::Start(1024)).unwrap();
        let mut buf = [0u8; 6];
        dev.read_exact(&mut buf).unwrap();
        assert_eq!(buf, patron);
        let mut dev2 = DispositivoBloque::nuevo(Cursor::new(vec![0u8; 4096 * 8]), 0, 4096);
        dev2.seek(SeekFrom::Start(1024)).unwrap();
        let mut buf2 = [0u8; 6];
        dev2.read_exact(&mut buf2).unwrap();
        assert_ne!(buf2, patron);
    }

    #[test]
    fn escritura_alineada_parchea_sin_pisar_vecinos() {
        // Sobre un archivo real (la escritura es para File): base no alineada, sector 4096.
        let ruta = std::env::temp_dir().join(format!("winext4-disp-{}.bin", std::process::id()));
        std::fs::write(&ruta, vec![0xAAu8; 32 * 1024]).unwrap();
        {
            let f = std::fs::OpenOptions::new().read(true).write(true).open(&ruta).unwrap();
            let mut dev = DispositivoBloque::nuevo(f, 17408 - 16384, 4096);
            dev.seek(SeekFrom::Start(100)).unwrap();
            dev.write_all(b"HOLA").unwrap();
            dev.flush().unwrap();
        }
        let v = std::fs::read(&ruta).unwrap();
        let p = (17408 - 16384) + 100;
        assert_eq!(&v[p..p + 4], b"HOLA");
        assert!(v[..p].iter().all(|&b| b == 0xAA) && v[p + 4..].iter().all(|&b| b == 0xAA), "vecinos intactos");
        let _ = std::fs::remove_file(&ruta);
    }

    /// Escritura ALINEADA y completa (el camino sin lectura previa) en un disco de sectores:
    /// escribe exactamente lo suyo y deja intactos los sectores vecinos.
    #[test]
    fn escritura_alineada_completa_sin_lectura_previa() {
        let ruta = std::env::temp_dir().join(format!("winext4-disp-al-{}.bin", std::process::id()));
        std::fs::write(&ruta, vec![0xAAu8; 64 * 1024]).unwrap();
        {
            let f = std::fs::OpenOptions::new().read(true).write(true).open(&ruta).unwrap();
            let mut dev = DispositivoBloque::nuevo(f, 8192, 4096);
            dev.seek(SeekFrom::Start(4096)).unwrap();
            dev.write_all(&[0x11u8; 8192]).unwrap();
            dev.flush().unwrap();
        }
        let v = std::fs::read(&ruta).unwrap();
        assert!(v[12288..20480].iter().all(|&b| b == 0x11), "los dos sectores escritos");
        assert!(v[..12288].iter().all(|&b| b == 0xAA) && v[20480..].iter().all(|&b| b == 0xAA), "vecinos intactos");
        let _ = std::fs::remove_file(&ruta);
    }

    #[test]
    fn deshacer_restaura_exactamente_lo_original() {
        let dir = std::env::temp_dir();
        let disco = dir.join(format!("winext4-dsh-{}.bin", std::process::id()));
        let undo = dir.join(format!("winext4-dsh-{}.undo", std::process::id()));
        let _ = std::fs::remove_file(&undo);
        let original: Vec<u8> = (0..64 * 1024).map(|i| (i % 251) as u8).collect();
        std::fs::write(&disco, &original).unwrap();
        {
            let f = std::fs::OpenOptions::new().read(true).write(true).open(&disco).unwrap();
            let mut d = ConDeshacer::nuevo(f, &undo).unwrap();
            d.seek(SeekFrom::Start(5000)).unwrap();
            d.write_all(&[0xEE; 9000]).unwrap(); // cruza 3 trozos
            d.seek(SeekFrom::Start(6000)).unwrap();
            d.write_all(&[0xDD; 10]).unwrap(); // mismo trozo: no se guarda dos veces
            assert_eq!(d.trozos_guardados(), 3);
        }
        assert_ne!(std::fs::read(&disco).unwrap(), original, "control: se escribió");
        let mut f = std::fs::OpenOptions::new().read(true).write(true).open(&disco).unwrap();
        assert_eq!(aplicar_deshacer(&mut f, &undo).unwrap(), 3);
        assert_eq!(std::fs::read(&disco).unwrap(), original, "deshacer devuelve el original exacto");
        // Pareja: no se pisa un archivo para deshacer que ya existe.
        let f = std::fs::OpenOptions::new().read(true).write(true).open(&disco).unwrap();
        assert!(ConDeshacer::nuevo(f, &undo).is_err(), "un undo existente no se sobrescribe");
        let _ = std::fs::remove_file(&disco);
        let _ = std::fs::remove_file(&undo);
    }

    #[test]
    fn parse_offset_dec_y_hex() {
        assert_eq!(parse_offset("17408"), Some(17408));
        assert_eq!(parse_offset("0x4400"), Some(0x4400));
        assert_eq!(parse_offset(" 0X4400 "), Some(0x4400));
        assert_eq!(parse_offset("abc"), None);
    }

    fn guid_a_disco(txt: &str) -> [u8; 16] {
        let h: Vec<u8> = txt
            .bytes()
            .filter(|b| *b != b'-')
            .collect::<Vec<_>>()
            .chunks(2)
            .map(|c| u8::from_str_radix(std::str::from_utf8(c).unwrap(), 16).unwrap())
            .collect();
        let mut o = [0u8; 16];
        o[0] = h[3];
        o[1] = h[2];
        o[2] = h[1];
        o[3] = h[0];
        o[4] = h[5];
        o[5] = h[4];
        o[6] = h[7];
        o[7] = h[6];
        o[8..16].copy_from_slice(&h[8..16]);
        o
    }

    #[test]
    fn guid_linux_fs_coincide_con_el_textual() {
        assert_eq!(guid_a_disco("0fc63daf-8483-4772-8e79-3d69d8477de4"), GUID_LINUX_FS);
    }
}
