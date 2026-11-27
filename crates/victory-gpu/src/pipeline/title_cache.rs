//! Cache identities compared by the retail producer and by its ordinary fallback.
use super::*;
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::sync::LazyLock;

type Match = (u64, u64, [u8; 32]);
type Seed = [u8; 32];
static BUILD: OnceLock<String> = OnceLock::new();
static STATE: LazyLock<Mutex<Index>> = LazyLock::new(Default::default);
#[derive(Default)]
struct Index {
    seeds: HashMap<Match, Seed>,
    ambiguous: HashSet<Match>,
    recipes: HashMap<Seed, [u8; 16]>,
    live: HashMap<[u8; 16], std::sync::Weak<make::Pending>>,
    loaded: bool,
}

fn path() -> Option<std::path::PathBuf> {
    Some(library::folder()?.join(format!("graphics-seeds-v2-{}.bin", BUILD.get()?)))
}

impl Index {
    fn load(&mut self) {
        if self.loaded {
            return;
        }
        self.loaded = true;
        if let Some(bytes) = path().and_then(|p| std::fs::read(p).ok()) {
            for record in bytes.chunks_exact(48) {
                self.recipes.insert(
                    record[..32].try_into().unwrap(),
                    record[32..].try_into().unwrap(),
                );
            }
        }
    }

    fn remember(&mut self, seed: Seed, recipe: [u8; 16]) {
        self.load();
        if self.recipes.get(&seed) == Some(&recipe) {
            return;
        }
        let recipe = if self.recipes.contains_key(&seed) {
            [0; 16]
        } else {
            recipe
        };
        self.recipes.insert(seed, recipe);
        if let Some(mut file) = path().and_then(|p| {
            std::fs::OpenOptions::new()
                .append(true)
                .create(true)
                .open(p)
                .ok()
        }) {
            let mut bytes = [0u8; 48];
            bytes[..32].copy_from_slice(&seed);
            bytes[32..].copy_from_slice(&recipe);
            let _ = file.write_all(&bytes);
        }
    }
}

pub fn wrappers(build: &str) -> (usize, usize) {
    BUILD.get_or_init(|| build.to_owned());
    (seed as *const () as usize, stream as *const () as usize)
}

thread_local! {
    static PACKED: std::cell::Cell<*const u8> = const {std::cell::Cell::new(std::ptr::null())};
}

// Only the identified producer supplies the variant prefix preceding packed.
extern "system" fn seed(
    device: This,
    packed: *const u8,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    PACKED.with(|slot| {
        let previous = slot.replace(packed);
        let result = create_cached_graphics_x(device, packed, iid, out);
        slot.set(previous);
        result
    })
}

pub(crate) extern "system" fn create_cached_graphics_x(
    _: This,
    packed: *const u8,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    unsafe { *out = std::ptr::null() };
    if packed.is_null() || !object::is(iid, &PIPELINE_IIDS) {
        return E_INVALIDARG;
    }
    if !PACKED.with(|slot| slot.get() == packed) {
        return object::NOT_FOUND;
    }
    let mut root_ptr = [0u8; 8];
    let mut prefix = [0u8; 40];
    if !crate::host::read_into(packed, &mut root_ptr)
        || !crate::host::read_into(packed.wrapping_sub(40), &mut prefix)
    {
        return object::NOT_FOUND;
    }
    let Some(root) = crate::root::digest(usize::from_le_bytes(root_ptr) as This) else {
        return object::NOT_FOUND;
    };
    let key = u64::from_le_bytes(prefix[..8].try_into().unwrap());
    let variant = u64::from_le_bytes(prefix[8..16].try_into().unwrap());
    let mut hash = blake3::Hasher::new();
    hash.update(&prefix);
    hash.update(root.as_bytes());
    // The retail seed has six packet ranges after its root. Extend the
    // identity with their bytes and length, independently of their addresses.
    let mut packet = [0u8; 4096];
    for i in 0..6 {
        let mut pair = [0u8; 16];
        if !crate::host::read_into(packed.wrapping_add(8 + i * 16), &mut pair) {
            return object::NOT_FOUND;
        }
        let address = usize::from_le_bytes(pair[..8].try_into().unwrap());
        let len = usize::from_le_bytes(pair[8..].try_into().unwrap());
        if len > 16 << 20 || (address == 0) != (len == 0) || address.checked_add(len).is_none() {
            return object::NOT_FOUND;
        }
        hash.update(&pair[8..]);
        for off in (0..len).step_by(packet.len()) {
            let n = (len - off).min(packet.len());
            if !crate::host::read_into((address + off) as *const u8, &mut packet[..n]) {
                return object::NOT_FOUND;
            }
            hash.update(&packet[..n]);
        }
    }

    let seed = *hash.finalize().as_bytes();
    let id = (key, variant, *root.as_bytes());
    let (recipe, live) = {
        let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
        state.load();
        if state.seeds.insert(id, seed).is_some_and(|old| old != seed) {
            state.ambiguous.insert(id);
        }
        if state.ambiguous.contains(&id) {
            return object::NOT_FOUND;
        }
        let recipe = state.recipes.get(&seed).copied().filter(|r| *r != [0; 16]);
        (recipe, recipe.and_then(|id| state.live.get(&id)?.upgrade()))
    };
    if let Some(p) = live
        && let Some(g) = &p.graphics
    {
        let mut bytes = Vec::new();
        g.put(&mut bytes);
        if let Some(g) = host::GraphicsState::get(&bytes) {
            return hand_out(p.shaders.clone(), Some(Box::new(g)), iid, out);
        }
    }
    let Some((shaders, Some(state))) = recipe
        .filter(|r| *r != [0; 16])
        .and_then(|r| known::restored(&r))
    else {
        return object::NOT_FOUND;
    };
    hand_out(shaders, Some(state), iid, out)
}

extern "system" fn stream(
    device: This,
    desc: *const StreamDesc,
    iid: *const Iid,
    out: *mut This,
    request: *const u8,
) -> HRESULT {
    let _ = device;
    let Some(recipe) = stream_recipe(desc) else {
        return E_INVALIDARG;
    };
    let root = recipe.cache_root;
    let hr = recipe.create(iid, out);
    if hr < 0 || request.is_null() || out.is_null() || unsafe { (*out).is_null() } {
        return hr;
    }
    let Some(root) = root else { return hr };
    let mut fields = [0u8; 16];
    if !crate::host::read_into(request.wrapping_add(0x28), &mut fields) {
        return hr;
    }
    let variant = u64::from_le_bytes(fields[..8].try_into().unwrap());
    let key = u64::from_le_bytes(fields[8..].try_into().unwrap());
    let id = (key, variant, *root.as_bytes());
    let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
    if !state.ambiguous.contains(&id)
        && let Some(seed) = state.seeds.get(&id).copied()
    {
        let p = pipeline(unsafe { *out });
        known::note(p.host.key(), &p.shaders, p.host.graphics.as_deref());
        let mut recipe = [0u8; 16];
        for (i, b) in recipe.iter_mut().enumerate() {
            *b = u8::from_str_radix(&p.host.key()[i * 2..i * 2 + 2], 16).unwrap();
        }
        state.remember(seed, recipe);
        state.live.insert(recipe, Arc::downgrade(&p.host));
    }
    hr
}

pub(crate) extern "system" fn variant(
    _: This,
    base: This,
    count: u32,
    records: *const u8,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    unsafe { *out = std::ptr::null() };
    if base.is_null() || count > 8 || (count > 0 && records.is_null()) {
        return E_INVALIDARG;
    }
    let p = pipeline(base);
    let Some(g) = p.host.graphics.as_ref() else {
        return E_INVALIDARG;
    };
    let mut bytes = Vec::new();
    g.put(&mut bytes);
    let Some(mut g) = host::GraphicsState::get(&bytes) else {
        return E_INVALIDARG;
    };
    for i in 0..count as usize {
        let mut record = [0u8; 0x14c];
        if !crate::host::read_into(records.wrapping_add(i * record.len()), &mut record) {
            return E_INVALIDARG;
        }
        let data = record.as_ptr();
        let kind = match unsafe { data.cast::<u32>().read_unaligned() } {
            0 => 11,
            1 => 10,
            2 => 8,
            _ => return object::NOT_FOUND,
        };
        if !unsafe { g.read(kind, data) } {
            return E_INVALIDARG;
        }
    }
    hand_out(p.shaders.clone(), Some(Box::new(g)), iid, out)
}
