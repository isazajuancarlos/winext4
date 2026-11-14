// SPDX-FileCopyrightText: 2026 Juan Carlos Isaza
// SPDX-License-Identifier: LicenseRef-Xiliux-Propietario
//! El **camino de LECTURA** del motor: sobre un dispositivo de bloques (hoy un archivo; mañana
//! un volumen de Windows), abre el superbloque, localiza la tabla de inodos vía los descriptores
//! de grupo, lee un inodo, mapea sus bloques (árboles de **extents** de ext4, o bloques
//! indirectos clásicos de ext2/3), lista directorios y lee el contenido de un archivo.
//!
//! Es SOLO lectura a propósito (la escritura es F3 y va detrás del banco `e2fsck`). Todo dato
//! que no cuadra —magia de extent, inodo fuera de rango, directorio htree aún no soportado— se
//! reporta como error (D20), nunca se adivina: un mapeo inventado leería bloques ajenos.

use std::io::{self, Read, Seek, SeekFrom, Write};

use crate::superbloque::{ErrorSuperbloque, Superbloque};

/// Qué puede salir mal al leer el sistema de archivos. Fail-closed (D20): el llamante decide.
#[derive(Debug)]
pub enum ErrorFs {
    /// Error de E/S del dispositivo subyacente.
    Io(io::Error),
    /// El superbloque no es legible (ver [`ErrorSuperbloque`]).
    Superbloque(ErrorSuperbloque),
    /// Número de inodo 0 o mayor que `s_inodes_count`: no existe.
    InodoFuera { ino: u32 },
    /// Un nodo de extents no empieza por su magia `0xF30A`: estructura corrupta o mal offset.
    ExtentMagia { vista: u16 },
    /// Se pidió listar algo que no es un directorio.
    NoEsDirectorio,
    /// Se pidió el destino de algo que no es un enlace simbólico.
    NoEsEnlace,
    /// Un componente de la ruta no existe en su directorio.
    NoEncontrado { nombre: String },
    /// La escritura EN SITIO se saldría del tamaño actual del archivo. Crecer exige asignar
    /// bloques (F3 completa), que aún no se hace: fail-closed, no se inventa espacio (D20).
    NoCreceAun { tamano: u64, pedido: u64 },
    /// La escritura cae en un bloque no asignado (archivo disperso): asignarlo es F3 completa.
    HuecoNoAsignable { bloque_logico: u64 },
    /// El volumen (o el inodo) usa algo que la escritura no sabe mantener: no se escribe.
    NoSoportado { que: &'static str },
    /// Una suma de metadatos guardada no coincide con la recalculada: no se escribe encima.
    SumaInvalida { que: &'static str },
    /// No queda un bloque libre en ningún grupo inicializado.
    SinEspacio,
    /// No queda un inodo libre en ningún grupo inicializado.
    SinInodos,
    /// El archivo necesitaría un árbol de extents de profundidad > 1 (más de 1360 fragmentos).
    ArbolExtentsLleno,
    /// Estructura en disco incoherente (detectada al leer): no se sigue, no se adivina.
    Corrupto { que: &'static str },
    /// Ya existe una entrada con ese nombre en el directorio.
    YaExiste { nombre: String },
    /// Nombre vacío, de más de 255 bytes, con `/` o NUL, o `.`/`..`.
    NombreInvalido,
    /// Se pidió borrar un directorio que tiene entradas.
    DirectorioNoVacio,
    /// Se pidió una operación de archivo sobre un directorio.
    EsDirectorio,
}

impl core::fmt::Display for ErrorFs {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "E/S: {e}"),
            Self::Superbloque(e) => write!(f, "superbloque: {e}"),
            Self::InodoFuera { ino } => write!(f, "inodo {ino} fuera de rango"),
            Self::ExtentMagia { vista } => write!(f, "nodo de extents con magia {vista:#06x}"),
            Self::NoEsDirectorio => write!(f, "no es un directorio"),
            Self::NoEsEnlace => write!(f, "no es un enlace simbólico"),
            Self::NoEncontrado { nombre } => write!(f, "no se encontró «{nombre}»"),
            Self::NoCreceAun { tamano, pedido } => {
                write!(f, "escritura hasta {pedido} excede el tamaño {tamano} (crecer es F3)")
            }
            Self::HuecoNoAsignable { bloque_logico } => {
                write!(f, "el bloque lógico {bloque_logico} es un hueco (asignar es F3)")
            }
            Self::NoSoportado { que } => write!(f, "escritura no soportada: {que}"),
            Self::SumaInvalida { que } => write!(f, "suma de metadatos inválida en {que}"),
            Self::SinEspacio => write!(f, "sin bloques libres"),
            Self::SinInodos => write!(f, "sin inodos libres"),
            Self::ArbolExtentsLleno => write!(f, "árbol de extents demasiado profundo (> 1360 fragmentos)"),
            Self::Corrupto { que } => write!(f, "estructura corrupta: {que}"),
            Self::YaExiste { nombre } => write!(f, "ya existe «{nombre}»"),
            Self::NombreInvalido => write!(f, "nombre de archivo inválido"),
            Self::DirectorioNoVacio => write!(f, "el directorio no está vacío"),
            Self::EsDirectorio => write!(f, "es un directorio"),
        }
    }
}

impl std::error::Error for ErrorFs {}

impl From<io::Error> for ErrorFs {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// Marca de un árbol de extents (`eh_magic`).
const EXTENT_MAGIA: u16 = 0xF30A;
/// `EXT4_EXTENTS_FL`: el inodo mapea datos con extents, no con bloques indirectos.
const FLAG_EXTENTS: u32 = 0x8_0000;

/// Un inodo leído, con solo lo que el camino de lectura necesita.
#[derive(Debug, Clone)]
pub struct Inodo {
    /// `i_mode`: tipo (nibble alto) + permisos.
    pub modo: u16,
    /// Tamaño en bytes (`i_size_lo` + `i_size_high` para archivos regulares).
    pub tamano: u64,
    /// `i_flags`.
    pub flags: u32,
    /// `i_atime`: último acceso, en segundos desde epoch Unix (bits bajos de 32).
    pub atime: u32,
    /// `i_ctime`: último cambio de inodo, en segundos desde epoch Unix.
    pub ctime: u32,
    /// `i_mtime`: última modificación de datos, en segundos desde epoch Unix.
    pub mtime: u32,
    /// `i_crtime`: creación (solo en inodos con zona extra ≥ 24; si no, 0).
    pub crtime: u32,
    /// Los 60 bytes de `i_block`: o 15 punteros clásicos, o la raíz del árbol de extents.
    pub bloque: [u8; 60],
    /// `i_links_count`: nombres que lo apuntan (0 = libre o huérfano).
    pub enlaces: u16,
    /// `i_generation`: cambia cada vez que el número de inodo se REUTILIZA; un handle que guarda
    /// (número, generación) detecta así que su archivo ya no es el que abrió.
    pub generacion: u32,
}

impl Inodo {
    pub(crate) fn leer(b: &[u8]) -> Self {
        let mut bloque = [0u8; 60];
        bloque.copy_from_slice(&b[0x28..0x28 + 60]);
        let tamano = u32le(b, 0x04) as u64 | ((u32le(b, 0x6C) as u64) << 32);
        // Segundos de 32 bits (válido hasta 2038; ext4 extiende con `*_extra`, aún no leído).
        Self {
            modo: u16le(b, 0x00),
            tamano,
            flags: u32le(b, 0x20),
            atime: u32le(b, 0x08),
            ctime: u32le(b, 0x0C),
            mtime: u32le(b, 0x10),
            crtime: if b.len() > 0x98 && u16le(b, 0x80) >= 24 { u32le(b, 0x90) } else { 0 },
            bloque,
            enlaces: u16le(b, 0x1A),
            generacion: u32le(b, 0x64),
        }
    }

    /// ¿Es un directorio? (`S_IFDIR`).
    #[must_use]
    pub fn es_directorio(&self) -> bool {
        self.modo & 0xF000 == 0x4000
    }

    /// ¿Es un archivo regular? (`S_IFREG`).
    #[must_use]
    pub fn es_regular(&self) -> bool {
        self.modo & 0xF000 == 0x8000
    }

    /// ¿Es un enlace simbólico? (`S_IFLNK`).
    #[must_use]
    pub fn es_enlace(&self) -> bool {
        self.modo & 0xF000 == 0xA000
    }

    fn usa_extents(&self) -> bool {
        self.flags & FLAG_EXTENTS != 0
    }
}

/// Una entrada de directorio.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entrada {
    /// Número de inodo al que apunta.
    pub ino: u32,
    /// Nombre del archivo/subdirectorio.
    pub nombre: String,
    /// `file_type` de la entrada (1=regular, 2=dir, 7=symlink…); 0 si el FS no lo guarda.
    pub tipo: u8,
}

/// Tope de bloques que `bloques_fisicos` mapea de una vez (8 GiB con bloques de 4 KiB).
const MAX_BLOQUES_MAPA: u64 = 1 << 21;
/// Tope de lo que `leer_datos` carga entero en memoria (directorios, enlaces): 1 GiB.
const MAX_LEER_DATOS: u64 = 1 << 30;

/// El inodo raíz de todo ext2/3/4 es siempre el 2.
pub const INODO_RAIZ: u32 = 2;

/// Un extent ya resuelto: `len` bloques lógicos desde `logico` viven en `fisico`..
struct Extent {
    logico: u32,
    fisico: u64,
    len: u32,
}

/// El sistema de archivos ext4 montado sobre un dispositivo `R` (lectura siempre; escritura
/// solo si `R: Write`, ver el bloque de escritura y `escritura.rs`).
pub struct Ext4<R> {
    pub(crate) disp: R,
    /// El superbloque, público para que el llamante consulte características/geometría.
    pub sb: Superbloque,
    /// Los 1024 bytes del superbloque tal cual están en disco: la escritura parte de ellos
    /// para no perder ningún campo que [`Superbloque`] no modele.
    pub(crate) sb_crudo: Vec<u8>,
    /// Semilla de las sumas de metadatos (0 si el FS no lleva `metadata_csum`).
    pub(crate) semilla: u32,
    /// Transacción abierta (modo journal): metadatos modificados y aún no confirmados. Las
    /// lecturas de bloques los ven (`leer_bloque`), el disco todavía no.
    pub(crate) tx: Option<crate::transaccion::Tx>,
    /// Escritor del journal: presente si se escribe A TRAVÉS del journal.
    pub(crate) diario: Option<crate::journal::Journal>,
    /// Política de [`Ext4::buscar_siguiendo`]: ¿se siguen los enlaces cuyo destino es una
    /// CARPETA? Sí por defecto (como Linux). El host de Windows lo apaga: con Dokan un enlace a
    /// carpeta no se puede anunciar como tal (la etiqueta de reparse no llega al listado), así
    /// que `Remove-Item -Recurse` ENTRABA en él y vaciaba el destino (medido el 2026-11-05).
    pub seguir_enlaces_a_carpetas: bool,
    /// Caché de directorios (inodo → sus entradas). Sin ella, CADA búsqueda de un nombre releía
    /// el directorio entero, y crear N archivos en una carpeta costaba N² (medido: de 4 a 13,5 ms
    /// por archivo al llegar a 1500). Es válida porque winext4 es quien escribe el volumen; la
    /// mantienen al día las operaciones de directorio (insertar/quitar la actualizan en el sitio)
    /// y se VACÍA entera ante cualquier operación fallida, transacción descartada, recuperación
    /// del journal o reparación.
    pub(crate) cache_dirs: std::collections::HashMap<u32, Vec<Entrada>>,
    /// Agrupado de transacciones (`None` = una por operación, lo de siempre).
    pub(crate) agrupado: Option<crate::transaccion::Agrupado>,
    /// Registro de LIBERACIONES (época + qué bloques): quien lee SIN el candado comprueba que
    /// ninguno de SUS bloques se liberó desde su plan (si no, pudo pasar a OTRO archivo).
    pub(crate) epoca: std::sync::Arc<RegistroLiberaciones>,
}

/// Cuántas liberaciones recientes se recuerdan. Si una lectura sin candado abarca más, no se
/// puede comprobar y se repite con el candado (prudente, nunca acepta a ciegas).
const MAX_REGISTRO: usize = 16_384;

/// Época de liberación y registro acotado de los tramos liberados en cada una.
///
/// Orden que lo hace seguro: al liberar, PRIMERO se anota el tramo, LUEGO sube la época y solo
/// DESPUÉS se liberan los bloques en el bitmap (y pueden reutilizarse). Quien lee sin candado
/// COPIA y DESPUÉS mira la época: si un bloque suyo se reutilizó antes de la copia, su
/// liberación ya estaba anotada y la época ya había subido cuando la mira.
pub struct RegistroLiberaciones {
    contador: std::sync::atomic::AtomicU64,
    tramos: Mutex<std::collections::VecDeque<(u64, u64, u64)>>,
}

use std::sync::Mutex;

impl RegistroLiberaciones {
    fn nuevo() -> Self {
        RegistroLiberaciones { contador: std::sync::atomic::AtomicU64::new(0), tramos: Mutex::new(std::collections::VecDeque::new()) }
    }

    /// La época actual.
    pub fn actual(&self) -> u64 {
        self.contador.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Anota que se van a liberar `n` bloques desde `inicio` y sube la época (ANTES de liberar).
    pub(crate) fn anotar(&self, inicio: u64, n: u64) {
        let mut t = self.tramos.lock().unwrap_or_else(|e| e.into_inner());
        let ep = self.contador.load(std::sync::atomic::Ordering::SeqCst) + 1;
        t.push_back((ep, inicio, n));
        if t.len() > MAX_REGISTRO {
            t.pop_front();
        }
        self.contador.store(ep, std::sync::atomic::Ordering::SeqCst);
    }

    /// ¿Ninguno de `fisicos` se liberó después de la época `e0`? `false` también si el
    /// registro ya no cubre todo lo ocurrido desde `e0` (no se puede saber: no se acepta).
    pub fn intactos(&self, e0: u64, fisicos: &[Option<u64>]) -> bool {
        let e1 = self.actual();
        if e1 == e0 {
            return true;
        }
        let t = self.tramos.lock().unwrap_or_else(|e| e.into_inner());
        // El registro debe tener TODAS las épocas de (e0, e1].
        if t.front().is_none_or(|&(ep, _, _)| ep > e0 + 1) {
            return false;
        }
        let tocados = |ini: u64, n: u64| fisicos.iter().flatten().any(|&p| p >= ini && p < ini + n);
        !t.iter().filter(|&&(ep, _, _)| ep > e0).any(|&(_, ini, n)| tocados(ini, n))
    }
}

/// Plan de una lectura, hecho con el candado del motor y ejecutable SIN él (ver
/// [`Ext4::plan_lectura`] y `cache::CacheCompartida::copiar_plan`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanLectura {
    /// El contenido ya está aquí (rango vacío, o enlace simbólico rápido).
    Directo(Vec<u8>),
    /// Bloques físicos del rango lógico (`None` = hueco: ceros), desde el byte `ini` del
    /// primero, `largo` bytes, con la época de liberación del momento del plan.
    Bloques { fisicos: Vec<Option<u64>>, bs: u64, ini: usize, largo: usize, epoca: u64 },
    /// Hay bloques pendientes de una transacción abierta: leer por el camino de siempre.
    Motor,
}

impl PlanLectura {
    /// Ejecuta el plan SIN el candado del motor: copia desde la caché compartida y solo da la
    /// lectura por buena si TODOS los bloques estaban y la época de liberación NO cambió desde
    /// el plan (si cambió, un bloque pudo pasar a otro archivo: sus datos no son de este).
    /// Devuelve los bytes escritos en `buf`, o `None` si hay que leer con el candado.
    pub fn ejecutar(&self, cache: &crate::cache::CacheCompartida, epoca: &RegistroLiberaciones, buf: &mut [u8]) -> Option<usize> {
        match self {
            PlanLectura::Directo(v) => {
                let n = v.len().min(buf.len());
                buf[..n].copy_from_slice(&v[..n]);
                Some(n)
            }
            PlanLectura::Bloques { fisicos, bs, ini, largo, epoca: e0 } => {
                let n = (*largo).min(buf.len());
                if !cache.copiar_plan(fisicos, *bs, *ini, &mut buf[..n]) {
                    return None;
                }
                // DESPUÉS de copiar: si alguno de ESTOS bloques se liberó entretanto, no vale.
                epoca.intactos(*e0, fisicos).then_some(n)
            }
            PlanLectura::Motor => None,
        }
    }
}

impl<R: Read + Seek> Ext4<R> {
    /// Abre el FS: lee el superbloque en el byte 1024 y lo valida.
    ///
    /// # Errors
    /// [`ErrorFs::Io`] o [`ErrorFs::Superbloque`] si el dispositivo no da un ext4 legible.
    pub fn abrir(mut disp: R) -> Result<Self, ErrorFs> {
        disp.seek(SeekFrom::Start(crate::superbloque::DESPLAZAMIENTO as u64))?;
        let mut buf = vec![0u8; crate::superbloque::TAMANO];
        disp.read_exact(&mut buf)?;
        let sb = Superbloque::leer(&buf).map_err(ErrorFs::Superbloque)?;
        let semilla = if sb.tiene_csum_metadatos() {
            crate::csum::semilla(&buf, sb.guarda_semilla(), &sb.uuid)
        } else {
            0
        };
        Ok(Self { disp, sb, sb_crudo: buf, semilla, tx: None, diario: None, seguir_enlaces_a_carpetas: true, cache_dirs: std::collections::HashMap::new(), agrupado: None, epoca: std::sync::Arc::new(RegistroLiberaciones::nuevo()) })
    }

    pub(crate) fn bs(&self) -> u64 {
        self.sb.tamano_bloque as u64
    }

    /// Lee el bloque físico `bloque` entero.
    ///
    /// # Errors
    /// [`ErrorFs::Io`] si falla el dispositivo.
    pub fn leer_bloque(&mut self, bloque: u64) -> Result<Vec<u8>, ErrorFs> {
        if let Some(b) = self.tx.as_ref().and_then(|t| t.bloques.get(&bloque)) {
            return Ok(b.clone()); // lo pendiente de la transacción abierta manda
        }
        let bs = self.bs();
        let mut buf = vec![0u8; bs as usize];
        self.disp.seek(SeekFrom::Start(bloque * bs))?;
        self.disp.read_exact(&mut buf)?;
        Ok(buf)
    }

    /// Los 1024 bytes crudos del superbloque, tal como se leyeron al abrir.
    #[must_use]
    pub fn superbloque_crudo(&self) -> &[u8] {
        &self.sb_crudo
    }

    /// Semilla de las sumas de metadatos (0 sin `metadata_csum`).
    #[must_use]
    pub fn semilla(&self) -> u32 {
        self.semilla
    }

    /// El dispositivo de debajo (para consultar capas como el registro para deshacer).
    #[must_use]
    pub fn dispositivo(&self) -> &R {
        &self.disp
    }

    /// Dónde vive el descriptor del grupo `grupo`: (bloque, offset dentro del bloque). La GDT
    /// sigue al bloque del superbloque (sin `META_BG`, que se rechaza al escribir).
    pub(crate) fn pos_descriptor(&self, grupo: u64) -> (u64, usize) {
        let bs = self.bs();
        let gdt_inicio = self.sb.primer_bloque_dato as u64 + 1;
        let off_abs = grupo * self.sb.tamano_desc as u64;
        (gdt_inicio + off_abs / bs, (off_abs % bs) as usize)
    }

    /// El descriptor crudo del grupo `grupo`.
    ///
    /// # Errors
    /// [`ErrorFs::Io`] si falla el dispositivo.
    pub fn descriptor(&mut self, grupo: u64) -> Result<crate::grupos::Descriptor, ErrorFs> {
        let (bloque, off) = self.pos_descriptor(grupo);
        let datos = self.leer_bloque(bloque)?;
        let n = self.sb.tamano_desc as usize;
        Ok(crate::grupos::Descriptor { crudo: datos[off..off + n].to_vec() })
    }

    /// Bloque donde empieza la tabla de inodos del grupo `grupo` (vía el descriptor de grupo).
    fn tabla_inodos(&mut self, grupo: u64) -> Result<u64, ErrorFs> {
        Ok(self.descriptor(grupo)?.tabla_inodos())
    }

    /// Byte absoluto donde vive el inodo `ino` en el dispositivo.
    pub(crate) fn byte_inodo(&mut self, ino: u32) -> Result<u64, ErrorFs> {
        if ino == 0 || ino > self.sb.inodos {
            return Err(ErrorFs::InodoFuera { ino });
        }
        let ipg = self.sb.inodos_por_grupo as u64;
        let idx = ino as u64 - 1;
        let tabla = self.tabla_inodos(idx / ipg)?;
        Ok(tabla * self.bs() + (idx % ipg) * self.sb.tamano_inodo as u64)
    }

    /// Los `s_inode_size` bytes crudos del inodo `ino`.
    ///
    /// # Errors
    /// [`ErrorFs::InodoFuera`] si está fuera de rango; [`ErrorFs::Io`] en fallo de disco.
    pub fn inodo_crudo(&mut self, ino: u32) -> Result<Vec<u8>, ErrorFs> {
        // Por el BLOQUE que lo contiene: así ve lo pendiente de una transacción abierta.
        let byte = self.byte_inodo(ino)?;
        let bs = self.bs();
        let blk = self.leer_bloque(byte / bs)?;
        let o = (byte % bs) as usize;
        Ok(blk[o..o + self.sb.tamano_inodo as usize].to_vec())
    }

    /// Lee el inodo número `ino` (1-based; la raíz es [`INODO_RAIZ`]).
    ///
    /// # Errors
    /// [`ErrorFs::InodoFuera`] si está fuera de rango; [`ErrorFs::Io`] en fallo de disco.
    pub fn inodo(&mut self, ino: u32) -> Result<Inodo, ErrorFs> {
        Ok(Inodo::leer(&self.inodo_crudo(ino)?))
    }

    /// El inodo raíz (`/`).
    ///
    /// # Errors
    /// Propaga [`ErrorFs`] de la lectura del inodo.
    pub fn raiz(&mut self) -> Result<Inodo, ErrorFs> {
        self.inodo(INODO_RAIZ)
    }

    /// Junta todos los extents de un inodo (recorre el árbol; `profundidad>0` lee nodos índice).
    fn recoger_extents(&mut self, nodo: &[u8], salida: &mut Vec<Extent>) -> Result<(), ErrorFs> {
        self.recoger_extents_prof(nodo, salida, None)
    }

    /// Como `recoger_extents`, exigiendo que la profundidad BAJE de uno en uno (un nodo que se
    /// apunte a sí mismo o a un antepasado no puede recursar sin fin) y que las entradas quepan
    /// en el nodo (si no, se leería fuera del búfer).
    fn recoger_extents_prof(&mut self, nodo: &[u8], salida: &mut Vec<Extent>, esperada: Option<u16>) -> Result<(), ErrorFs> {
        let magia = u16le(nodo, 0);
        if magia != EXTENT_MAGIA {
            return Err(ErrorFs::ExtentMagia { vista: magia });
        }
        let entradas = u16le(nodo, 2) as usize;
        let profundidad = u16le(nodo, 6);
        if profundidad > 5 || esperada.is_some_and(|p| p != profundidad) {
            return Err(ErrorFs::Corrupto { que: "profundidad incoherente en el árbol de extents" });
        }
        if entradas > (nodo.len() - 12) / 12 {
            return Err(ErrorFs::Corrupto { que: "nodo de extents con más entradas de las que caben" });
        }
        for i in 0..entradas {
            let e = 12 + i * 12; // tras la cabecera de 12 bytes
            if profundidad == 0 {
                let logico = u32le(nodo, e);
                let crudo = u16le(nodo, e + 4);
                // len > 32768 ⇒ extent no inicializado (preasignado); los bloques existen igual.
                let len = if crudo > 32768 { crudo - 32768 } else { crudo } as u32;
                let fisico = u32le(nodo, e + 8) as u64 | ((u16le(nodo, e + 6) as u64) << 32);
                salida.push(Extent { logico, fisico, len });
            } else {
                let hijo = u32le(nodo, e + 4) as u64 | ((u16le(nodo, e + 8) as u64) << 32);
                let datos = self.leer_bloque(hijo)?;
                self.recoger_extents_prof(&datos, salida, Some(profundidad - 1))?;
            }
        }
        Ok(())
    }

    /// Mapea el bloque lógico `l` de un inodo con bloques indirectos CLÁSICOS (ext2/3).
    /// `None` = hueco (bloque disperso) → se lee como ceros.
    fn fisico_clasico(&mut self, ino: &Inodo, l: u64) -> Result<Option<u64>, ErrorFs> {
        let pb = self.bs() / 4; // punteros por bloque
        let directo = |n: usize| -> u64 { u32le(&ino.bloque, n * 4) as u64 };
        // 12 directos
        if l < 12 {
            let p = directo(l as usize);
            return Ok((p != 0).then_some(p));
        }
        let mut l = l - 12;
        // indirecto simple (puntero 12)
        if l < pb {
            return self.indirecto(directo(12), l, 0);
        }
        l -= pb;
        // indirecto doble (puntero 13)
        if l < pb * pb {
            return self.indirecto(directo(13), l, 1);
        }
        l -= pb * pb;
        // indirecto triple (puntero 14); más allá no existe nada (tamaño dañado → error, no pánico)
        if l >= pb * pb * pb {
            return Err(ErrorFs::Corrupto { que: "bloque lógico fuera del alcance de los indirectos" });
        }
        self.indirecto(directo(14), l, 2)
    }

    /// Resuelve un índice dentro de un bloque de punteros con `nivel` niveles por debajo.
    fn indirecto(&mut self, bloque: u64, indice: u64, nivel: u32) -> Result<Option<u64>, ErrorFs> {
        if bloque == 0 {
            return Ok(None);
        }
        let pb = self.bs() / 4;
        let datos = self.leer_bloque(bloque)?;
        if nivel == 0 {
            let p = u32le(&datos, (indice as usize) * 4) as u64;
            return Ok((p != 0).then_some(p));
        }
        let por_rama = pb.pow(nivel); // cuántos bloques cubre cada puntero de este nivel
        let rama = (indice / por_rama) as usize;
        let resto = indice % por_rama;
        let hijo = u32le(&datos, rama * 4) as u64;
        self.indirecto(hijo, resto, nivel - 1)
    }

    /// Los bloques físicos de datos de `ino`, en orden lógico (`None` = hueco).
    ///
    /// # Errors
    /// [`ErrorFs`] si un nodo de extents es inválido o falla el disco.
    pub fn bloques_fisicos(&mut self, ino: &Inodo) -> Result<Vec<Option<u64>>, ErrorFs> {
        let n_bloques = ino.tamano.div_ceil(self.bs());
        // Tope: el tamaño viene del disco; un i_size dañado no puede pedir una reserva gigante.
        // (Directorios y journal reales caben de sobra; los archivos grandes van por leer_rango.)
        if n_bloques > MAX_BLOQUES_MAPA {
            return Err(ErrorFs::Corrupto { que: "inodo demasiado grande para mapearlo entero" });
        }
        let mut v = Vec::with_capacity(n_bloques as usize);
        if ino.usa_extents() {
            let mut exts = Vec::new();
            self.recoger_extents(&ino.bloque, &mut exts)?;
            for l in 0..n_bloques {
                v.push(exts.iter().find_map(|e| {
                    let fin = e.logico as u64 + e.len as u64;
                    (l >= e.logico as u64 && l < fin).then(|| e.fisico + (l - e.logico as u64))
                }));
            }
        } else {
            for l in 0..n_bloques {
                v.push(self.fisico_clasico(ino, l)?);
            }
        }
        Ok(v)
    }

    /// Los bloques INTERNOS del árbol de extents de `ino` (índices y hojas fuera de `i_block`).
    /// Vacío si el árbol cabe entero en el inodo (profundidad 0) o si no usa extents.
    ///
    /// # Errors
    /// [`ErrorFs`] si un nodo es inválido o falla el disco.
    pub fn bloques_de_arbol(&mut self, ino: &Inodo) -> Result<Vec<u64>, ErrorFs> {
        let mut v = Vec::new();
        if ino.usa_extents() {
            let raiz = ino.bloque;
            self.recoger_nodos(&raiz, &mut v)?;
        }
        Ok(v)
    }

    fn recoger_nodos(&mut self, nodo: &[u8], salida: &mut Vec<u64>) -> Result<(), ErrorFs> {
        let magia = u16le(nodo, 0);
        if magia != EXTENT_MAGIA {
            return Err(ErrorFs::ExtentMagia { vista: magia });
        }
        let prof = u16le(nodo, 6);
        if prof == 0 {
            return Ok(());
        }
        if prof > 5 || u16le(nodo, 2) as usize > (nodo.len() - 12) / 12 {
            return Err(ErrorFs::Corrupto { que: "nodo de extents incoherente" });
        }
        for i in 0..u16le(nodo, 2) as usize {
            let e = 12 + i * 12;
            let hijo = u32le(nodo, e + 4) as u64 | ((u16le(nodo, e + 8) as u64) << 32);
            salida.push(hijo);
            let datos = self.leer_bloque(hijo)?;
            // La profundidad debe bajar: si no, un nodo que se apunte a sí mismo recursa sin fin.
            if u16le(&datos, 0) == EXTENT_MAGIA && u16le(&datos, 6) != prof - 1 {
                return Err(ErrorFs::Corrupto { que: "profundidad incoherente en el árbol de extents" });
            }
            self.recoger_nodos(&datos, salida)?;
        }
        Ok(())
    }

    /// Lee `largo` bytes de `ino` desde `offset` (recortado al tamaño), leyendo SOLO los bloques
    /// del rango. Es lo que usa el host para cada petición de lectura: leer el archivo entero
    /// por cada trozo que pide Windows haría cuadrático copiar un archivo grande.
    ///
    /// # Errors
    /// [`ErrorFs`] si un nodo de extents es inválido o falla el disco.
    pub fn leer_rango(&mut self, ino: &Inodo, offset: u64, largo: usize) -> Result<Vec<u8>, ErrorFs> {
        if offset >= ino.tamano || largo == 0 {
            return Ok(Vec::new());
        }
        let fin = (offset + largo as u64).min(ino.tamano);
        // Enlace simbólico RÁPIDO: su «contenido» es el destino, guardado en i_block. Sin esto,
        // los bytes del texto se leían como punteros a bloques → error de E/S o, peor, el
        // contenido de un bloque cualquiera del disco (cazado el 2026-11-05 montando en W:).
        if ino.es_enlace() && ino.tamano < 60 {
            return Ok(ino.bloque[offset as usize..fin as usize].to_vec());
        }
        let bs = self.bs();
        let l0 = offset / bs;
        // Mapear cada bloque lógico a su físico y LEER EN TRAMOS: los bloques físicamente
        // contiguos van en UNA lectura. Bloque a bloque eran una llamada al disco por cada 4 KiB:
        // 45-50 MB/s desde Z: en un NVMe (medido el 2026-11-05).
        let fisicos = self.fisicos_de_rango(ino, offset, fin)?;
        let mut datos = vec![0u8; fisicos.len() * bs as usize]; // los huecos quedan a cero
        let mut k = 0usize;
        while k < fisicos.len() {
            let Some(p) = fisicos[k] else {
                k += 1;
                continue;
            };
            // Lo pendiente de una transacción abierta manda: ese bloque se lee de memoria.
            if self.tx.as_ref().is_some_and(|t| t.bloques.contains_key(&p)) {
                let b = self.leer_bloque(p)?;
                datos[k * bs as usize..(k + 1) * bs as usize].copy_from_slice(&b);
                k += 1;
                continue;
            }
            let mut n = 1usize;
            while k + n < fisicos.len()
                && fisicos[k + n] == Some(p + n as u64)
                && !self.tx.as_ref().is_some_and(|t| t.bloques.contains_key(&(p + n as u64)))
            {
                n += 1;
            }
            self.disp.seek(SeekFrom::Start(p * bs))?;
            self.disp.read_exact(&mut datos[k * bs as usize..(k + n) * bs as usize])?;
            k += n;
        }
        let ini = (offset - l0 * bs) as usize;
        Ok(datos[ini..ini + (fin - offset) as usize].to_vec())
    }

    /// Los bloques físicos de los bloques lógicos que cubren `[offset, fin)` (`None` = hueco).
    /// Lo comparten [`Self::leer_rango`] y [`Self::plan_lectura`]: los dos ven el MISMO mapa.
    fn fisicos_de_rango(&mut self, ino: &Inodo, offset: u64, fin: u64) -> Result<Vec<Option<u64>>, ErrorFs> {
        let bs = self.bs();
        let (l0, l1) = (offset / bs, (fin - 1) / bs);
        let exts = if ino.usa_extents() {
            let mut v = Vec::new();
            self.recoger_extents(&ino.bloque, &mut v)?;
            Some(v)
        } else {
            None
        };
        let mut fisicos: Vec<Option<u64>> = Vec::with_capacity((l1 - l0 + 1) as usize);
        for l in l0..=l1 {
            fisicos.push(match &exts {
                Some(v) => v.iter().find_map(|e| {
                    let f = e.logico as u64 + e.len as u64;
                    (l >= e.logico as u64 && l < f).then(|| e.fisico + (l - e.logico as u64))
                }),
                None => self.fisico_clasico(ino, l)?,
            });
        }
        Ok(fisicos)
    }

    /// El PLAN de leer `largo` bytes de `ino` desde `offset`: qué bloques físicos, con la
    /// época de liberación del momento. Se hace con el candado del motor (rápido: el mapa sale
    /// de la caché) y se EJECUTA sin él desde la caché compartida; tras copiar hay que comparar
    /// la época con [`Self::epoca`]: si cambió, la lectura se repite con [`Self::leer_rango`].
    ///
    /// # Errors
    /// [`ErrorFs`] si un nodo de extents es inválido o falla el disco.
    pub fn plan_lectura(&mut self, ino: &Inodo, offset: u64, largo: usize) -> Result<PlanLectura, ErrorFs> {
        if offset >= ino.tamano || largo == 0 {
            return Ok(PlanLectura::Directo(Vec::new()));
        }
        let fin = (offset + largo as u64).min(ino.tamano);
        if ino.es_enlace() && ino.tamano < 60 {
            return Ok(PlanLectura::Directo(ino.bloque[offset as usize..fin as usize].to_vec()));
        }
        let fisicos = self.fisicos_de_rango(ino, offset, fin)?;
        // Un bloque pendiente de la transacción abierta solo existe en ella: ese, por el motor.
        if let Some(t) = self.tx.as_ref() {
            if fisicos.iter().flatten().any(|p| t.bloques.contains_key(p)) {
                return Ok(PlanLectura::Motor);
            }
        }
        let bs = self.bs();
        Ok(PlanLectura::Bloques {
            fisicos,
            bs,
            ini: (offset % bs) as usize,
            largo: (fin - offset) as usize,
            epoca: self.epoca.actual(),
        })
    }

    /// El registro de liberaciones (compartido: se consulta sin el candado).
    #[must_use]
    pub fn epoca(&self) -> std::sync::Arc<RegistroLiberaciones> {
        std::sync::Arc::clone(&self.epoca)
    }

    /// Bloques libres del volumen según el superbloque.
    #[must_use]
    pub fn bloques_libres(&self) -> u64 {
        let lo = u32le(&self.sb_crudo, 0x0C) as u64;
        let hi = if self.sb.es_64bit() { u32le(&self.sb_crudo, 0x158) as u64 } else { 0 };
        lo | (hi << 32)
    }

    /// Lee TODO el contenido de un archivo (o los datos de un directorio), recortado a su tamaño.
    ///
    /// # Errors
    /// [`ErrorFs`] si un nodo de extents es inválido o falla el disco.
    pub fn leer_datos(&mut self, ino: &Inodo) -> Result<Vec<u8>, ErrorFs> {
        if ino.tamano > MAX_LEER_DATOS {
            return Err(ErrorFs::Corrupto { que: "demasiado grande para cargarlo entero (usa leer_rango)" });
        }
        let bs = self.bs();
        let n_bloques = ino.tamano.div_ceil(bs);
        let mut datos = Vec::with_capacity(ino.tamano as usize);

        if ino.usa_extents() {
            let mut exts = Vec::new();
            let raiz = ino.bloque; // la raíz del árbol vive en i_block
            self.recoger_extents(&raiz, &mut exts)?;
            for l in 0..n_bloques {
                let fis = exts.iter().find_map(|e| {
                    let fin = e.logico as u64 + e.len as u64;
                    (l >= e.logico as u64 && l < fin).then(|| e.fisico + (l - e.logico as u64))
                });
                match fis {
                    Some(p) => datos.extend_from_slice(&self.leer_bloque(p)?),
                    None => datos.resize(datos.len() + bs as usize, 0), // hueco
                }
            }
        } else {
            for l in 0..n_bloques {
                match self.fisico_clasico(ino, l)? {
                    Some(p) => datos.extend_from_slice(&self.leer_bloque(p)?),
                    None => datos.resize(datos.len() + bs as usize, 0),
                }
            }
        }

        datos.truncate(ino.tamano as usize);
        Ok(datos)
    }

    /// Lista las entradas de un directorio por recorrido lineal de sus bloques. **Funciona
    /// igual para directorios htree (indexados)**: un bloque de índice empieza por un registro
    /// cuyo `rec_len` abarca el bloque entero (y `inode`=0), así que el recorrido lo salta sin
    /// interpretarlo — las entradas reales viven en los bloques hoja, que son dirents normales.
    /// Incluye `.` y `..` (como hace `ls -a`/`debugfs ls`).
    ///
    /// # Errors
    /// [`ErrorFs::NoEsDirectorio`] si `ino` no es directorio; [`ErrorFs`] en fallo de disco/extents.
    pub fn listar(&mut self, ino: &Inodo) -> Result<Vec<Entrada>, ErrorFs> {
        if !ino.es_directorio() {
            return Err(ErrorFs::NoEsDirectorio);
        }
        let datos = self.leer_datos(ino)?;
        Ok(entradas_de(&datos))
    }

    /// Resuelve una ruta absoluta (`/a/b/c`) a su inodo, desde la raíz.
    ///
    /// # Errors
    /// [`ErrorFs::NoEncontrado`] si falta un componente; [`ErrorFs`] por el camino.
    pub fn buscar(&mut self, ruta: &str) -> Result<Inodo, ErrorFs> {
        let n = self.buscar_numero(ruta)?;
        self.inodo(n)
    }

    /// Como [`Self::buscar`], pero devuelve el NÚMERO de inodo (lo que la escritura necesita).
    ///
    /// # Errors
    /// [`ErrorFs::NoEncontrado`] si falta un componente; [`ErrorFs`] por el camino.
    pub fn buscar_numero(&mut self, ruta: &str) -> Result<u32, ErrorFs> {
        let mut n = INODO_RAIZ;
        for comp in ruta.split('/').filter(|c| !c.is_empty()) {
            n = self.buscar_en(n, comp)?.ok_or_else(|| ErrorFs::NoEncontrado { nombre: comp.to_string() })?.ino;
        }
        Ok(n)
    }

    /// Máximo de directorios en la caché; al pasarlo se vacía (recorrer un disco de 300 000
    /// carpetas no puede llenar la memoria).
    const MAX_DIRS_EN_CACHE: usize = 4096;

    /// Las entradas del directorio `n`, de la caché o del disco (y entonces se guardan).
    ///
    /// # Errors
    /// [`ErrorFs::NoEsDirectorio`] o E/S.
    pub fn entradas_dir(&mut self, n: u32) -> Result<Vec<Entrada>, ErrorFs> {
        self.asegurar_en_cache(n)?;
        Ok(self.cache_dirs.get(&n).cloned().unwrap_or_default())
    }

    /// La entrada `nombre` del directorio `n` (sin copiar el directorio entero).
    ///
    /// # Errors
    /// [`ErrorFs::NoEsDirectorio`] o E/S.
    pub fn buscar_en(&mut self, n: u32, nombre: &str) -> Result<Option<Entrada>, ErrorFs> {
        self.asegurar_en_cache(n)?;
        Ok(self.cache_dirs.get(&n).and_then(|v| v.iter().find(|e| e.nombre == nombre).cloned()))
    }

    fn asegurar_en_cache(&mut self, n: u32) -> Result<(), ErrorFs> {
        if !self.cache_dirs.contains_key(&n) {
            let d = self.inodo(n)?;
            let v = self.listar(&d)?;
            if self.cache_dirs.len() >= Self::MAX_DIRS_EN_CACHE {
                self.cache_dirs.clear();
            }
            self.cache_dirs.insert(n, v);
        }
        Ok(())
    }

    /// Resuelve `ruta` SIGUIENDO enlaces simbólicos, como el kernel: los de los componentes
    /// intermedios siempre; el del último solo si `seguir_final` (falso = `lstat`/`unlink`:
    /// borrar o renombrar actúa sobre el ENLACE, no sobre su destino).
    ///
    /// Solo se siguen destinos RELATIVOS que quedan dentro del volumen. Un destino absoluto
    /// (`/mnt/data/x` depende de dónde montó Linux el volumen: adivinarlo sería inventar, D20),
    /// uno que sube por encima de la raíz, uno roto o una cadena de más de 40 (`MAXSYMLINKS`)
    /// NO se siguen: si es el último componente, el resultado es el propio enlace (se ve como
    /// lo que es); si es intermedio, la ruta no existe ([`ErrorFs::NoEncontrado`]).
    ///
    /// # Errors
    /// [`ErrorFs::NoEncontrado`], [`ErrorFs`] por el camino.
    pub fn buscar_siguiendo(&mut self, ruta: &str, seguir_final: bool) -> Result<u32, ErrorFs> {
        let comps: Vec<String> = ruta.split('/').filter(|c| !c.is_empty()).map(str::to_string).collect();
        let mut saltos = 0u32;
        let camino = self.resolver(vec![INODO_RAIZ], &comps, seguir_final, &mut saltos, false)?;
        Ok(*camino.last().expect("el camino siempre tiene la raíz"))
    }

    /// ¿Se puede quedar la resolución de un enlace en `n`? No si es OTRO enlace (bucle o tope
    /// de saltos: el ELOOP del kernel), ni si es una carpeta con la política apagada.
    fn seguible(&mut self, n: u32) -> Result<bool, ErrorFs> {
        let i = self.inodo(n)?;
        Ok(!i.es_enlace() && (self.seguir_enlaces_a_carpetas || !i.es_directorio()))
    }

    /// Recorre `comps` desde `camino` (inodos desde la raíz, para «..»). `es_destino`: se
    /// está resolviendo el destino de un enlace, donde subir por encima de la raíz o
    /// atravesar algo que no es directorio hacen el enlace NO SEGUIBLE (no un error del
    /// llamante). `saltos` es global a toda la resolución: con 40 enlaces la recursión acaba.
    fn resolver(&mut self, mut camino: Vec<u32>, comps: &[String], seguir_final: bool, saltos: &mut u32, es_destino: bool) -> Result<Vec<u32>, ErrorFs> {
        const MAX_ENLACES: u32 = 40;
        for (k, comp) in comps.iter().enumerate() {
            match comp.as_str() {
                "." => continue,
                ".." => {
                    if camino.len() > 1 {
                        camino.pop();
                    } else if es_destino {
                        return Err(ErrorFs::NoEncontrado { nombre: "..".into() });
                    }
                    continue;
                }
                _ => {}
            }
            let actual = *camino.last().expect("el camino siempre tiene la raíz");
            let dir = self.inodo(actual)?;
            if !dir.es_directorio() {
                return Err(if es_destino { ErrorFs::NoEncontrado { nombre: comp.clone() } } else { ErrorFs::NoEsDirectorio });
            }
            let n = self.buscar_en(actual, comp)?.ok_or_else(|| ErrorFs::NoEncontrado { nombre: comp.clone() })?.ino;
            let ino = self.inodo(n)?;
            let es_ultimo = k + 1 == comps.len();
            if ino.es_enlace() && (!es_ultimo || seguir_final) && *saltos < MAX_ENLACES {
                let destino = self.leer_enlace(&ino)?;
                if !destino.is_empty() && !destino.starts_with('/') {
                    *saltos += 1;
                    let dc: Vec<String> = destino.split('/').filter(|c| !c.is_empty()).map(str::to_string).collect();
                    match self.resolver(camino.clone(), &dc, true, saltos, true) {
                        // Si el destino resuelto ACABA en un enlace, es que no se pudo seguir
                        // (bucle o tope de saltos: el ELOOP del kernel) → no seguible.
                        Ok(c) if self.seguible(*c.last().expect("camino no vacío"))? => {
                            camino = c;
                            continue;
                        }
                        Ok(_) => {}
                        Err(ErrorFs::NoEncontrado { .. } | ErrorFs::NoEsDirectorio) => {} // no seguible
                        Err(e) => return Err(e),
                    }
                }
            }
            if ino.es_enlace() && !es_ultimo {
                // Enlace intermedio que no se pudo seguir: por ahí no hay camino.
                return Err(ErrorFs::NoEncontrado { nombre: comp.clone() });
            }
            camino.push(n);
        }
        Ok(camino)
    }

    /// Destino de un enlace simbólico. Los enlaces **rápidos** (destino < 60 bytes) lo guardan
    /// EN LÍNEA en `i_block`; los largos, en bloques de datos como un archivo.
    ///
    /// # Errors
    /// [`ErrorFs::NoEsEnlace`] si `ino` no es un symlink; [`ErrorFs`] al leer un enlace largo.
    pub fn leer_enlace(&mut self, ino: &Inodo) -> Result<String, ErrorFs> {
        if !ino.es_enlace() {
            return Err(ErrorFs::NoEsEnlace);
        }
        // Enlace rápido: destino embebido en los 60 bytes de i_block (sin bloques de datos).
        if ino.tamano < 60 {
            let n = ino.tamano as usize;
            return Ok(String::from_utf8_lossy(&ino.bloque[..n]).into_owned());
        }
        let datos = self.leer_datos(ino)?;
        Ok(String::from_utf8_lossy(&datos).into_owned())
    }
}

/// Camino de ESCRITURA (F3, en construcción). Solo disponible cuando el dispositivo es de
/// lectura-escritura, así el host de solo lectura (que abre el disco RO) no puede ni compilar
/// una escritura por error.
impl<R: Read + Write + Seek> Ext4<R> {
    /// Escribe un bloque de METADATOS: con transacción abierta va a ella (y al journal al
    /// confirmar); sin journal, directo al disco.
    pub(crate) fn escribir_meta(&mut self, bloque: u64, bytes: &[u8]) -> Result<(), ErrorFs> {
        if let Some(t) = self.tx.as_mut() {
            t.anotar_deshacer(bloque);
            t.bloques.insert(bloque, bytes.to_vec());
            return Ok(());
        }
        self.escribir_bloque(bloque, bytes)
    }

    /// Escribe un bloque de DATOS de archivo: siempre directo (modo ordenado: llegan al disco
    /// antes del commit que los nombra). Si ese bloque tenía una copia de metadatos pendiente
    /// (se liberó y se reasignó como datos en la misma transacción), la copia se descarta:
    /// si no, el commit la escribiría encima de los datos.
    pub(crate) fn escribir_datos(&mut self, bloque: u64, bytes: &[u8]) -> Result<(), ErrorFs> {
        if let Some(t) = self.tx.as_mut() {
            t.anotar_deshacer(bloque);
            t.bloques.remove(&bloque);
        }
        // Un bloque con copia en el journal sin vaciar (fue METADATOS en una transacción ya
        // confirmada) que ahora se reutiliza para DATOS: si se cortara la luz, la recuperación
        // reescribiría encima la copia vieja. Se vacía el journal antes (jbd2: revocación).
        if self.diario.as_ref().is_some_and(|j| j.registrados.contains(&bloque)) {
            self.vaciar_journal()?;
        }
        self.escribir_bloque(bloque, bytes)
    }

    /// Como [`Self::escribir_datos`] para un TRAMO de bloques de datos contiguos (`bytes` es un
    /// múltiplo del bloque): las mismas comprobaciones por bloque (transacción, copia en el
    /// journal) y UNA sola escritura al disco.
    pub(crate) fn escribir_datos_tramo(&mut self, primero: u64, bytes: &[u8]) -> Result<(), ErrorFs> {
        let bs = self.bs();
        debug_assert!((bytes.len() as u64).is_multiple_of(bs));
        let n = bytes.len() as u64 / bs;
        if let Some(t) = self.tx.as_mut() {
            for b in primero..primero + n {
                t.anotar_deshacer(b);
                t.bloques.remove(&b);
            }
        }
        if self.diario.as_ref().is_some_and(|j| (primero..primero + n).any(|b| j.registrados.contains(&b))) {
            self.vaciar_journal()?;
        }
        self.escribir_bloque(primero, bytes)
    }

    pub(crate) fn escribir_bloque(&mut self, bloque: u64, bytes: &[u8]) -> Result<(), ErrorFs> {
        self.disp.seek(SeekFrom::Start(bloque * self.bs()))?;
        self.disp.write_all(bytes)?;
        Ok(())
    }

    /// Sobrescribe `datos` en `ino` desde `offset`, **sin cambiar el tamaño ni asignar bloques**
    /// (escritura EN SITIO). Es la escritura más segura: toca solo bloques de DATOS ya asignados,
    /// no metadatos, así que `e2fsck` sigue limpio. Crecer/crear/borrar/asignar es el resto de F3.
    ///
    /// # Errors
    /// [`ErrorFs::NoCreceAun`] si se saldría del tamaño; [`ErrorFs::HuecoNoAsignable`] si cae en un
    /// bloque no asignado; [`ErrorFs`] por E/S o extents.
    pub fn escribir_en_sitio(&mut self, ino: &Inodo, offset: u64, datos: &[u8]) -> Result<(), ErrorFs> {
        let fin = offset + datos.len() as u64;
        if fin > ino.tamano {
            return Err(ErrorFs::NoCreceAun { tamano: ino.tamano, pedido: fin });
        }
        let bs = self.bs();
        // Mapa de bloques: extents una vez; clásico por bloque.
        let exts = if ino.usa_extents() {
            let mut v = Vec::new();
            self.recoger_extents(&ino.bloque, &mut v)?;
            Some(v)
        } else {
            None
        };
        let mut hecho = 0usize;
        while hecho < datos.len() {
            let pos = offset + hecho as u64;
            let l = pos / bs;
            let dentro = (pos % bs) as usize;
            let n = core::cmp::min(bs as usize - dentro, datos.len() - hecho);
            let fis = match &exts {
                Some(v) => v.iter().find_map(|e| {
                    let f = e.logico as u64 + e.len as u64;
                    (l >= e.logico as u64 && l < f).then(|| e.fisico + (l - e.logico as u64))
                }),
                None => self.fisico_clasico(ino, l)?,
            };
            let p = fis.ok_or(ErrorFs::HuecoNoAsignable { bloque_logico: l })?;
            if dentro == 0 && n == bs as usize {
                self.escribir_bloque(p, &datos[hecho..hecho + n])?;
            } else {
                // Bloque parcial: leer-modificar-escribir para no pisar los bytes vecinos.
                let mut blk = self.leer_bloque(p)?;
                blk[dentro..dentro + n].copy_from_slice(&datos[hecho..hecho + n]);
                self.escribir_bloque(p, &blk)?;
            }
            hecho += n;
        }
        self.disp.flush()?;
        Ok(())
    }
}

/// Parsea los registros de directorio (`ext4_dir_entry_2`) de los bytes `datos` de un
/// directorio. Avanza por `rec_len`, que tesela cada bloque exactamente; por eso un registro
/// con `inode`=0 (relleno, cola de suma, o la cabecera de un bloque de índice htree cuyo
/// `rec_len` abarca el bloque) se **salta** sin interpretarlo. Función pura: así se prueba el
/// auto-salto del índice sin un FS entero (ver `pruebas`).
fn entradas_de(datos: &[u8]) -> Vec<Entrada> {
    let mut entradas = Vec::new();
    let mut i = 0usize;
    while i + 8 <= datos.len() {
        let eno = u32le(datos, i);
        let rec_len = u16le(datos, i + 4) as usize;
        let name_len = datos[i + 6] as usize;
        let tipo = datos[i + 7];
        if rec_len < 8 {
            break; // rec_len 0/absurdo: fin seguro (D20, no girar en falso)
        }
        if eno != 0 && name_len != 0 && i + 8 + name_len <= datos.len() {
            let nombre = String::from_utf8_lossy(&datos[i + 8..i + 8 + name_len]).into_owned();
            entradas.push(Entrada { ino: eno, nombre, tipo });
        }
        i += rec_len;
    }
    entradas
}

#[inline]
fn u16le(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([b[off], b[off + 1]])
}

#[inline]
fn u32le(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

#[cfg(test)]
mod pruebas {
    use super::*;
    use std::io::Cursor;

    /// Escribe un registro de directorio `ext4_dir_entry_2` y rellena hasta `rec_len`.
    fn rec(buf: &mut Vec<u8>, inode: u32, rec_len: u16, nombre: &[u8], ftype: u8) {
        let ini = buf.len();
        buf.extend_from_slice(&inode.to_le_bytes());
        buf.extend_from_slice(&rec_len.to_le_bytes());
        buf.push(nombre.len() as u8);
        buf.push(ftype);
        buf.extend_from_slice(nombre);
        while buf.len() < ini + rec_len as usize {
            buf.push(0);
        }
    }

    #[test]
    fn entradas_salta_el_bloque_de_indice_htree() {
        // "Bloques" de 64 bytes: [bloque0: . ..][bloque índice][bloque hoja].
        let mut d = Vec::new();
        rec(&mut d, 2, 12, b".", 2);
        rec(&mut d, 2, 52, b"..", 2); // su rec_len llena el bloque0 (12+52=64)

        // Bloque de índice: cabecera falsa inode=0, rec_len=64 (abarca el bloque). DENTRO, una
        // "trampa" que un parser ingenuo (que ignorara rec_len) leería como entrada.
        let ini_idx = d.len();
        d.extend_from_slice(&0u32.to_le_bytes()); // inode=0
        d.extend_from_slice(&64u16.to_le_bytes()); // rec_len=64
        d.push(0); // name_len=0
        d.push(0); // file_type=0
        rec(&mut d, 99, 16, b"TRAMPA", 1); // trampa, DENTRO del bloque índice
        while d.len() < ini_idx + 64 {
            d.push(0);
        }

        rec(&mut d, 11, 64, b"archivo", 1); // bloque hoja: una entrada real

        let e = entradas_de(&d);
        let nombres: Vec<&str> = e.iter().map(|x| x.nombre.as_str()).collect();
        assert_eq!(nombres, [".", "..", "archivo"], "debe saltar el índice y su trampa");
        assert!(!nombres.contains(&"TRAMPA"), "la trampa del bloque índice NO debe aparecer");
    }

    #[test]
    fn inodo_lee_las_marcas_de_tiempo() {
        // Buffer de inodo de 256 bytes con modo, tamaño y los tres tiempos en offsets conocidos.
        let mut b = vec![0u8; 256];
        b[0x00..0x02].copy_from_slice(&0x81A4u16.to_le_bytes()); // S_IFREG | 0644
        b[0x04..0x08].copy_from_slice(&1234u32.to_le_bytes()); // i_size_lo
        b[0x08..0x0C].copy_from_slice(&1_000_000_000u32.to_le_bytes()); // i_atime
        b[0x0C..0x10].copy_from_slice(&1_111_111_111u32.to_le_bytes()); // i_ctime
        b[0x10..0x14].copy_from_slice(&1_762_000_000u32.to_le_bytes()); // i_mtime
        let i = Inodo::leer(&b);
        assert!(i.es_regular());
        assert_eq!(i.tamano, 1234);
        assert_eq!(i.atime, 1_000_000_000);
        assert_eq!(i.ctime, 1_111_111_111);
        assert_eq!(i.mtime, 1_762_000_000);
    }

    /// Superbloque mínimo para pruebas en memoria: solo importa el tamaño de bloque.
    fn sb_bs(bs: u32) -> Superbloque {
        Superbloque {
            inodos: 0,
            bloques: 0,
            tamano_bloque: bs,
            inodos_por_grupo: 0,
            bloques_por_grupo: 0,
            primer_bloque_dato: 0,
            tamano_inodo: 256,
            tamano_desc: 32,
            estado: 1,
            compat: 0,
            incompat: 0,
            ro_compat: 0,
            uuid: [0; 16],
            etiqueta: String::new(),
        }
    }

    #[test]
    fn recoge_extents_de_profundidad_1() {
        // Árbol de extents con profundidad 1: la raíz (en i_block) es un nodo ÍNDICE que apunta
        // al bloque 2; ese bloque es un nodo HOJA con un extent (lógico 0, físico 3, 1 bloque);
        // el bloque 3 tiene los datos. Prueba la RECURSIÓN de recoger_extents sin un FS real.
        let bs = 1024usize;
        let mut dev = vec![0u8; bs * 4];

        // Nodo hoja en el bloque 2.
        let h = bs * 2;
        dev[h] = 0x0A;
        dev[h + 1] = 0xF3; // magia 0xF30A
        dev[h + 2] = 1; // entries=1
        dev[h + 6] = 0; // depth=0 (hoja)
        dev[h + 12 + 4] = 1; // ee_len=1
        dev[h + 12 + 8] = 3; // ee_start_lo=3 (bloque físico 3)

        // Datos conocidos en el bloque 3.
        let patron: Vec<u8> = (0..bs).map(|i| (i % 256) as u8).collect();
        dev[bs * 3..bs * 4].copy_from_slice(&patron);

        // Raíz (i_block): nodo índice depth=1 → apunta al bloque 2.
        let mut raiz = [0u8; 60];
        raiz[0] = 0x0A;
        raiz[1] = 0xF3;
        raiz[2] = 1; // entries=1
        raiz[6] = 1; // depth=1 (índice)
        raiz[16] = 2; // ei_leaf_lo=2

        let mut fs = Ext4 { disp: Cursor::new(dev), sb: sb_bs(bs as u32), sb_crudo: vec![0; 1024], semilla: 0, tx: None, diario: None, seguir_enlaces_a_carpetas: true, cache_dirs: std::collections::HashMap::new(), agrupado: None, epoca: std::sync::Arc::new(RegistroLiberaciones::nuevo()) };
        let ino = Inodo {
            modo: 0x8000,
            tamano: bs as u64,
            flags: FLAG_EXTENTS,
            atime: 0,
            ctime: 0,
            mtime: 0,
            crtime: 0,
            enlaces: 1,
            generacion: 0,
            bloque: raiz,
        };
        let datos = fs.leer_datos(&ino).expect("recorrer el árbol de profundidad 1");
        assert_eq!(datos, patron, "los datos deben venir del bloque físico 3 vía el índice");
    }
}
