//! Registered Scarlett frame buffers and the bytes of completed decoder planes.
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex, OnceLock};

static REGISTER: OnceLock<usize> = OnceLock::new();
static WAIT: OnceLock<usize> = OnceLock::new();
static CLOSE: OnceLock<usize> = OnceLock::new();
static ACTIVE: AtomicBool = AtomicBool::new(false);
static MOVIES: LazyLock<Mutex<HashMap<usize, Movie>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Default)]
struct Movie {
    table: usize,
    planes: HashMap<(u32, usize), Plane>,
}
struct Plane {
    address: usize,
    backing: Option<(usize, u64)>,
    bytes: Arc<[u8]>,
}

pub fn export(name: &str, original: usize) -> usize {
    let (cell, hook) = match name {
        "BinkRegisterFrameBuffers" => (&REGISTER, register as *const () as usize),
        "BinkDoFrameAsyncWait" => (&WAIT, wait as *const () as usize),
        "BinkClose" => (&CLOSE, close as *const () as usize),
        _ => return original,
    };
    if *cell.get_or_init(|| original) == original {
        hook
    } else {
        original
    }
}

extern "system" fn register(handle: usize, table: usize) {
    let original: extern "system" fn(usize, usize) =
        unsafe { std::mem::transmute(*REGISTER.get().unwrap()) };
    original(handle, table);
    if handle != 0 && table != 0 {
        MOVIES.lock().unwrap_or_else(|e| e.into_inner()).insert(
            handle,
            Movie {
                table,
                ..Default::default()
            },
        );
        ACTIVE.store(true, Ordering::Release);
    }
}

extern "system" fn close(handle: usize) {
    let original: extern "system" fn(usize) = unsafe { std::mem::transmute(*CLOSE.get().unwrap()) };
    {
        let mut movies = MOVIES.lock().unwrap_or_else(|e| e.into_inner());
        movies.remove(&handle);
        ACTIVE.store(!movies.is_empty(), Ordering::Release);
    }
    original(handle);
}

fn physical(at: usize, len: usize) -> Option<(usize, u64)> {
    match crate::inplace::cover(at, len) {
        crate::inplace::Cover::Run(section, offset) => Some((section, offset)),
        _ => None,
    }
}

extern "system" fn wait(handle: usize, timeout: i32) -> i32 {
    let original: extern "system" fn(usize, i32) -> i32 =
        unsafe { std::mem::transmute(*WAIT.get().unwrap()) };
    let result = original(handle, timeout);
    if result == 0 {
        return result;
    }
    let table = MOVIES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&handle)
        .map(|m| m.table);
    if let Some(table) = table {
        completed(handle, table)
    }
    result
}

fn completed(handle: usize, table: usize) {
    // GetFrameBuffersInfo writes six header dwords, followed by five
    // (buffer pointer, allocate flag, pitch) records for each slot.
    let mut header = [0u8; 24];
    if !crate::host::read_into(table as *const u8, &mut header) {
        return;
    }
    let word = |i: usize| u32::from_le_bytes(header[i..i + 4].try_into().unwrap());
    let slot = word(20);
    if slot >= word(0) || slot >= 3 {
        return;
    }
    {
        let mut movies = MOVIES.lock().unwrap_or_else(|e| e.into_inner());
        let Some(movie) = movies.get_mut(&handle).filter(|m| m.table == table) else {
            return;
        };
        movie.planes.retain(|&(old, _), _| old != slot);
    }
    let mut records = [0u8; 80];
    if !crate::host::read_into((table + 24 + slot as usize * 80) as *const u8, &mut records) {
        return;
    }
    for (i, record) in records.chunks_exact(16).enumerate() {
        let at = usize::from_le_bytes(record[..8].try_into().unwrap());
        let pitch = u32::from_le_bytes(record[12..].try_into().unwrap()) as usize;
        let height = word(if matches!(i, 1 | 2) { 16 } else { 8 }) as usize;
        let Some(len) = pitch
            .checked_mul(height)
            .filter(|&n| n > 0 && n <= 128 << 20)
        else {
            continue;
        };
        if at == 0 {
            continue;
        }
        let _mapped = crate::watch::title_memory_reading();
        let mut bytes = vec![0; len];
        if !crate::host::read_into(at as *const u8, &mut bytes) {
            continue;
        }
        let backing = physical(at, len);
        drop(_mapped);
        crate::watch::title_memory_written(at, len, false);
        let mut movies = MOVIES.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(movie) = movies.get_mut(&handle).filter(|m| m.table == table) {
            movie.planes.insert(
                (slot, i),
                Plane {
                    address: at,
                    backing,
                    bytes: bytes.into(),
                },
            );
        }
    }
}

/// Same physical offset, even when the title samples a different view of the plane.
pub(crate) fn plane(at: usize, len: usize) -> Option<(Arc<[u8]>, usize)> {
    if !ACTIVE.load(Ordering::Acquire) {
        return None;
    }
    let identity = physical(at, len.min(1));
    let movies = MOVIES.lock().unwrap_or_else(|e| e.into_inner());
    movies
        .values()
        .flat_map(|m| m.planes.values())
        .find_map(|p| {
            let offset = match (p.backing, identity) {
                (Some((s, a)), Some((t, b))) if s == t => {
                    usize::try_from(b.checked_sub(a)?).ok()?
                }
                (None, None) => at.checked_sub(p.address)?,
                _ => return None,
            };
            (offset < p.bytes.len()).then(|| (p.bytes.clone(), offset))
        })
}

pub(crate) fn changed(at: usize, len: usize) {
    if !ACTIVE.load(Ordering::Acquire) || len == 0 {
        return;
    }
    let end = at.saturating_add(len);
    let backing = physical(at, len);
    for movie in MOVIES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .values_mut()
    {
        movie.planes.retain(|_, p| {
            if p.address < end && at < p.address.saturating_add(p.bytes.len()) { return false }
            !matches!((backing, p.backing), (Some((s,a)), Some((t,b))) if s == t && a < b.saturating_add(p.bytes.len() as u64) && b < a.saturating_add(len as u64))
        });
    }
}
