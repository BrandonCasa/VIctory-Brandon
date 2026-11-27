//! Command queues. Submitted lists run on the host GPU and are waited for,
//! so fence signals complete at once.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

#[cfg(windows)]
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::object::{self, E_INVALIDARG, HRESULT, Iid, Object, QUEUE, S_OK, Table, This, guid};
use crate::resource::PAGEABLE_IIDS;

#[cfg(windows)]
mod behind;

const QUEUE_IIDS: [Iid; 4] = [
    guid(
        0x0ec870a6,
        0x5d7e,
        0x4c22,
        [0x8c, 0xfc, 0x5b, 0xaa, 0xe0, 0x76, 0x16, 0xed],
    ),
    PAGEABLE_IIDS[0],
    PAGEABLE_IIDS[1],
    PAGEABLE_IIDS[2],
];

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct QueueDesc {
    kind: u32,
    priority: i32,
    flags: u32,
    nodes: u32,
}

/// The title reads the list type right after the header.
#[repr(C)]
struct Queue {
    object: Object,
    kind: u32,
    desc: QueueDesc,
    /// What the queue was given after a wait for a fence that had not been
    /// reached, in order; it runs when the fence gets there.
    #[cfg(windows)]
    held: std::sync::Mutex<Vec<Held>>,
    /// Submission identity survives release and reuse of the title object address.
    #[cfg(windows)]
    id: usize,
}
const _: () = assert!(std::mem::offset_of!(Queue, kind) == 0x10);

/// One thing a waiting queue holds back.
#[cfg(windows)]
enum Held {
    /// The queue waits for the fence (its address) to reach the value.
    Wait(crate::fence::Point),
    /// A list of the next submission waits for a memory value.
    Value(crate::list::ValueWait),
    /// A submission: the lists' host sides, taken from the title's lists
    /// (which it may reset meanwhile), and the order they run in.
    Run(Vec<crate::list::Detached>, Vec<usize>),
    /// A signal of the queue's, due after what it holds before it.
    Signal(crate::fence::Point),
    Mappings(crate::resource::PageMappings),
    /// Previous work on this queue has reached a pending presentation.
    PresentBoundary(Arc<AtomicU64>),
    Present(Arc<AtomicU64>, Presentation),
}

/// The queues that hold something. Each entry owns a queue reference.
#[cfg(windows)]
static WAITING: std::sync::Mutex<Vec<usize>> = std::sync::Mutex::new(Vec::new());

#[cfg(windows)]
fn lock<T>(mutex: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// Submit a generation in parts separated by its recorded memory waits.
/// The lists before the first one that waits run at once if the queue
/// holds nothing; the rest goes behind what the queue holds, part by part
/// (`crate::list::detach_parts`): the waits of a part, then its host
/// lists. [`release_held`] runs the queue's line as far as the values are
/// there, now and whenever a submission has run.
#[cfg(windows)]
fn submit_cut(this: This, lists: &[This]) {
    let Some(host) = crate::host::host() else {
        return;
    };
    let first = if lock(&queue(this).held).is_empty() {
        lists
            .iter()
            .position(|&l| crate::list::cut_list(l))
            .unwrap_or(lists.len())
    } else {
        0
    };
    #[cfg(feature = "diagnostics")]
    crate::capture::op(this as usize, || {
        let named: Vec<String> = lists
            .iter()
            .map(|&l| match crate::list::host_list(l) {
                Some(h) => format!("{:#x}", std::ptr::from_ref(h) as usize),
                None => format!("title {l:?}"),
            })
            .collect();
        format!(
            "SUBMIT queue kind {:?} held {} lists {named:?} (the first {first} run at once, the rest part by part)",
            Some(queue(this).kind),
            first < lists.len()
        )
    });
    if first > 0 && crate::host::late() {
        // Sent to the card and not waited for: their host sides are taken
        // from the title's lists (which it may reset before they have run)
        // and come back, with their tables' slots, when they are settled.
        let (taken, order) = crate::list::detach(&lists[..first]);
        host.execute_late(taken, order);
    } else if first > 0 {
        let now: Vec<_> = lists[..first]
            .iter()
            .filter_map(|&l| crate::list::host_list(l))
            .collect();
        host.execute(&now);
    }
    if first < lists.len() {
        let parts = crate::list::detach_parts(&lists[first..]);
        static SEEN: AtomicU64 = AtomicU64::new(0);
        if SEEN.fetch_add(1, Ordering::Relaxed) < 8 {
            tracing::info!(
                kind = queue(this).kind,
                lists = lists.len(),
                run_at_once = first,
                parts = parts.len(),
                waits = parts.iter().map(|p| p.0.len()).sum::<usize>(),
                "a submission is given to its queue part by part (its lists wait for memory values)"
            );
        }
        {
            let mut held = lock(&queue(this).held);
            for (waits, taken, order) in parts {
                held.extend(waits.into_iter().map(Held::Value));
                held.push(Held::Run(taken, order));
            }
            note_held(this);
        }
    }
    // What ran may be what another queue waits for, and what was just
    // given to the queue may be free to run already.
    release_held();
}

/// Adds `what()` to what the queue holds, if it holds anything (it waits
/// for a fence): whether it was added.
#[cfg(windows)]
fn hold(this: This, what: impl FnOnce() -> Held) -> bool {
    if this.is_null() {
        return false;
    }
    let mut held = lock(&queue(this).held);
    if held.is_empty() {
        return false;
    }
    held.push(what());
    true
}

#[cfg(windows)]
fn note_held(this: This) {
    let mut waiting = lock(&WAITING);
    if !waiting.contains(&(this as usize)) {
        object::add_ref(this);
        waiting.push(this as usize);
    }
}

#[cfg(windows)]
static RELEASING: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Runs waiting queues as far as their fences allow, each in submission order. Called
/// where a fence may have moved: after a signal and after a submission (a
/// list's bottom-of-pipe write may be into a fence), never from inside
/// one.
#[cfg(windows)]
pub(crate) fn release_held() {
    if behind::on() {
        // What queues stand at is looked at by the thread that runs them.
        behind::wake();
        return;
    }
    // One caller at a time, so a queue's submissions keep their order.
    if lock(&WAITING).is_empty() {
        return;
    }
    let _looking = crate::timeline::span("looking at what queues hold");
    let _one = lock(&RELEASING);
    loop {
        let queues = lock(&WAITING).clone();
        let mut moved = false;
        for q in queues {
            // SAFETY: a live queue: one released is taken off the list.
            let held = unsafe { &(*(q as *const Queue)).held };
            let mut held = lock(held);
            loop {
                match held.first() {
                    None => break,
                    Some(Held::Wait(point))
                        if crate::fence::completed(point.0 as This) < point.1 =>
                    {
                        break;
                    }
                    Some(Held::Value(wait)) if !value_met(wait) => break,
                    Some(Held::Present(boundary, _)) if boundary.load(Ordering::Acquire) != 0 => break,
                    Some(_) => {}
                }
                match held.remove(0) {
                    Held::Wait(..) | Held::Value(..) => {}
                    Held::Mappings(mappings) => { apply_mappings(mappings); }
                    Held::PresentBoundary(boundary) => {
                        boundary.fetch_sub(1, Ordering::Release);
                    }
                    Held::Present(_, present) => {
                        if !present.show() {
                            tracing::error!(frame = present.shown.frame, "held presentation failed");
                        }
                    }
                    Held::Run(lists, order) => {
                        #[cfg(feature = "diagnostics")]
                        crate::capture::op(q, || {
                            format!(
                                "RUN held submission of {} lists",
                                order.len()
                            )
                        });
                        // Diagnostics: the part behind this one stands at
                        // waits that are met already, so the two could have
                        // been one host submission (one wait for the card,
                        // each range shaders write compared once).
                        #[cfg(feature = "diagnostics")]
                        {
                            let waits = held.iter().take_while(|h| matches!(h, Held::Value(_))).count();
                            if waits > 0
                                && matches!(held.get(waits), Some(Held::Run(..)))
                                && held[..waits].iter().all(|h| matches!(h, Held::Value(w) if w.met()))
                            {
                                crate::copystat::add(crate::copystat::C::PartsTogether, 1);
                            }
                        }
                        if crate::host::late()
                            && let Some(host) = crate::host::host()
                        {
                            // Not waited for: the host keeps the lists
                            // until they are settled.
                            host.execute_late(lists, order);
                        } else {
                            let ran = {
                                let runs: Vec<&crate::host::HostList> =
                                    order.iter().map(|&i| lists[i].host()).collect();
                                match crate::host::host() {
                                    Some(host) => {
                                        host.execute(&runs);
                                        true
                                    }
                                    None => false,
                                }
                            };
                            // They have run: their host lists are used again.
                            if ran {
                                crate::list::recycle(lists, &order);
                            }
                        }
                    }
                    // Behind what was sent to the card and not waited
                    // for, if anything was.
                    Held::Signal(point) => {
                        if !crate::host::late_signal(point.0, point.1) {
                            point.publish();
                        }
                    }
                }
                moved = true;
            }
            if held.is_empty() {
                lock(&WAITING).retain(|&w| w != q);
                drop(held);
                object::release_boxed::<Queue>(q as This);
            }
        }
        if !moved {
            break;
        }
    }
}

/// Before a submission of ours that is waited for, or a wait for flights by
/// a thread of the title's: what the title gave its queues before goes to the
/// card first ([`behind`]).
#[cfg(windows)]
pub(crate) fn before_sync() {
    if behind::on() {
        behind::drain();
    }
}

/// Whether submissions go into the line of a thread of ours ([`behind`]).
#[cfg(windows)]
pub(crate) fn line_on() -> bool {
    behind::on()
}

/// Lists in the line may have been translated ([`crate::list::thin`]).
#[cfg(windows)]
pub(crate) fn line_moved() {
    behind::wake();
}

/// This thread does work the line waits for: it does not wait for the line.
#[cfg(windows)]
pub(crate) fn no_drain_here() {
    behind::no_drain_here();
}

/// Submissions the title made that the host has not begun ([`behind`]).
#[cfg(windows)]
pub(crate) fn queued() -> u64 {
    behind::queued()
}

/// For the frame-rate line.
#[cfg(windows)]
pub(crate) fn behind_line(frames: f64) {
    if behind::on() {
        crate::list::thin::line(frames);
        behind::line(frames);
    }
}

/// A conditional wait reads title memory after overlapping GPU writes land.
#[cfg(windows)]
fn value_met(wait: &crate::list::ValueWait) -> bool {
    if crate::host::late() { crate::host::late_met(wait) } else { wait.met() }
}

fn queue<'a>(this: This) -> &'a Queue {
    // SAFETY: called from queue methods only.
    unsafe { &*this.cast::<Queue>() }
}

pub(crate) extern "system" fn create(
    _: This,
    desc: *const QueueDesc,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    // SAFETY: the title passes a description (null is tolerated).
    let Some(&desc) = (unsafe { desc.as_ref() }) else {
        return E_INVALIDARG;
    };
    tracing::debug!(?desc, "command queue");
    #[cfg(windows)]
    static NEXT_QUEUE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(1);
    let queue = Box::new(Queue {
        object: if crate::device::newer() {
            Object::new(&NEWER_FUNCTIONS)
        } else {
            Object::new(&FUNCTIONS)
        },
        kind: desc.kind,
        desc,
        #[cfg(windows)]
        held: Default::default(),
        #[cfg(windows)]
        id: NEXT_QUEUE.fetch_add(1, Ordering::Relaxed),
    });
    object::hand_out(QUEUE, queue, iid, &QUEUE_IIDS, out)
}

/// The console's queue description: an engine or pipe and a queue on it
/// in place of PC's priority and node mask.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct QueueDescX {
    kind: u32,
    flags: u32,
    engine_or_pipe: u32,
    queue: u32,
}

/// Device method `CreateCommandQueueX`. The host picks engines itself, so
/// the queue is created as on PC.
pub(crate) extern "system" fn create_x(
    this: This,
    desc: *const QueueDescX,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    // SAFETY: the title passes a description (null is tolerated).
    let Some(&x) = (unsafe { desc.as_ref() }) else {
        return E_INVALIDARG;
    };
    tracing::debug!(desc = ?x, "console command queue");
    let desc = QueueDesc {
        kind: x.kind,
        priority: 0,
        flags: x.flags,
        nodes: 1,
    };
    create(this, &desc, iid, out)
}

extern "system" fn query(this: This, iid: *const Iid, out: *mut This) -> HRESULT {
    object::answer(
        QUEUE,
        iid,
        object::is(iid, &QUEUE_IIDS).then_some(this),
        out,
    )
}
extern "system" fn release(this: This) -> u32 {
    object::release_boxed::<Queue>(this)
}

/// Runs the lists' host lists and waits for them (decision 0007). A queue
/// that waits for a fence keeps them until the fence is reached.
extern "system" fn execute(this: This, count: u32, lists: *const This) {
    if lists.is_null() {
        return;
    }
    let _called = crate::timeline::span("the title's ExecuteCommandLists");
    #[cfg(all(windows, feature = "diagnostics"))]
    crate::timeline::title_thread();
    #[cfg(windows)]
    if let Some(host) = crate::host::host() {
        // SAFETY: the title passes `count` lists.
        let lists = unsafe { std::slice::from_raw_parts(lists, count as usize) };
        if behind::on() && !this.is_null() {
            behind::submit(this, lists);
            return;
        }
        let recordings = crate::list::thin::submitted(lists);
        let native;
        let lists = if recordings.is_empty() {
            lists
        } else {
            for r in recordings.iter() {
                r.wait();
            }
            native = recordings.iter().map(|r| r.back()).filter(|l| !l.is_null()).collect::<Vec<_>>();
            &native[..]
        };
        if !this.is_null() {
            submit_cut(this, lists);
            return;
        }
        let host_lists: Vec<_> = lists
            .iter()
            .filter_map(|&l| crate::list::host_list(l))
            .collect();
        host.execute(&host_lists);
        // What the lists wrote at the bottom of the pipe may be a fence
        // another queue waits for.
        release_held();
        return;
    }
    let _ = this;
    static ONCE: AtomicBool = AtomicBool::new(false);
    if !ONCE.swap(true, Ordering::Relaxed) {
        tracing::warn!(count, "command lists submitted; no host GPU runs them");
    }
}

/// The start of a present plane: frame token, then the plane's resources.
#[derive(Clone, Copy)]
#[repr(C)]
pub(crate) struct Plane {
    token: u64,
    count: u32,
    resources: *const This,
}
/// Retail PresentX adds source-rectangle and destination pointers, a color
/// space and seven option words after the common prefix. Older callers pass
/// only the prefix for single-plane presents.
#[cfg(windows)]
#[derive(Clone, Copy)]
#[repr(C)]
struct RetailPlane {
    prefix: Plane,
    source_rects: *const (),
    destination: *const (),
    color_space: u32,
    options: [u32; 7],
}
#[cfg(windows)]
const _: () = assert!(size_of::<RetailPlane>() == 72);

/// Copy the caller-owned record and both rectangles while PresentX owns them.
#[cfg(windows)]
fn display_plane(
    parameters: *const Plane,
    index: usize,
    retail: bool,
) -> Option<(This, crate::compose::Geometry)> {
    fn read<T: Copy>(src: *const T) -> Option<T> {
        if src.is_null() {
            return None;
        }
        let mut out = std::mem::MaybeUninit::<T>::uninit();
        // SAFETY: read_into fills the complete plain-data ABI value before use.
        unsafe {
            let bytes = std::slice::from_raw_parts_mut(out.as_mut_ptr().cast(), size_of::<T>());
            crate::host::read_into(src.cast(), bytes).then(|| out.assume_init())
        }
    }
    let (prefix, mut geometry) = if retail {
        let plane = read(parameters.cast::<RetailPlane>().wrapping_add(index))?;
        let mut g = crate::compose::Geometry::default();
        if !plane.source_rects.is_null() {
            g.source = Some(read(plane.source_rects.cast())?);
        }
        if !plane.destination.is_null() {
            g.destination = read(plane.destination.cast())?;
        }
        g.color = plane.color_space;
        (plane.prefix, g)
    } else {
        (read(parameters.wrapping_add(index))?, Default::default())
    };
    if prefix.count == 0 {
        return None;
    }
    let resource = read(prefix.resources)?;
    if !crate::resource::is_resource(resource) {
        return None;
    }
    let copy = crate::resource::host_copy(resource)?;
    geometry.serial = copy.serial;
    Some((resource, geometry))
}
/// Submission tagged with the frame it belongs to `(count, lists, frame
/// token, flags)`; frames are paced elsewhere, so it runs as a plain one.
extern "system" fn execute_x(this: This, count: u32, lists: *const This, _token: u64, _flags: u32) {
    execute(this, count, lists);
}
/// Signal with sync settings for the top and bottom of the pipe; our
/// submissions are complete when they return, so it signals at once.
extern "system" fn signal2_x(
    this: This,
    fence: This,
    value: u64,
    _bottom: *const u8,
    _top: *const u8,
) -> HRESULT {
    signal(this, fence, value)
}
/// Markers and events only matter to profilers.
extern "system" fn marker(_: This) {}
extern "system" fn signal(this: This, fence: This, value: u64) -> HRESULT {
    if fence.is_null() {
        return E_INVALIDARG;
    }
    let _called = crate::timeline::span("the title's queue Signal");
    #[cfg(windows)]
    if behind::on() && !this.is_null() {
        behind::signal(this, fence, value);
        return S_OK;
    }
    // A queue that waits signals after what it holds.
    #[cfg(windows)]
    if hold(this, || Held::Signal(crate::fence::Point::new(fence, value))) {
        return S_OK;
    }
    // Behind the submissions sent to the card and not waited for: the
    // fence is set when they are settled.
    #[cfg(windows)]
    if crate::host::late_signal(fence as usize, value) {
        return S_OK;
    }
    let _ = this;
    crate::fence::signal(fence, value)
}
/// From here on the queue runs nothing it is given until `fence` reaches
/// `value`: how the console orders work between queues (the title's
/// compute queue waits for the depth the graphics queue draws). Our
/// submissions run when they are made, so a wait for a value already
/// reached is nothing; otherwise what the queue is given next is held
/// ([`Held`]) until a signal or a list's bottom-of-pipe write gets the
/// fence there.
extern "system" fn wait(this: This, fence: This, value: u64) -> HRESULT {
    if fence.is_null() {
        return E_INVALIDARG;
    }
    let _called = crate::timeline::span("the title's queue Wait");
    #[cfg(windows)]
    if !this.is_null() {
        let now = crate::fence::completed(fence);
        // Whether the title uses this at all (the first calls).
        {
            static CALLS: AtomicU64 = AtomicU64::new(0);
            if CALLS.fetch_add(1, Ordering::Relaxed) < 8 {
                tracing::info!(kind = queue(this).kind, now, value, "a queue is told to wait for a fence");
            }
        }
        #[cfg(feature = "diagnostics")]
        crate::capture::op(this as usize, || format!("WAIT queue kind {} for fence {fence:?} value {value} (now {now})", queue(this).kind));
        if behind::on() {
            behind::wait(this, fence, value);
            return S_OK;
        }
        let behind = {
            let mut held = lock(&queue(this).held);
            let behind = !held.is_empty() || now < value;
            if behind {
                held.push(Held::Wait(crate::fence::Point::new(fence, value)));
                note_held(this);
            }
            behind
        };
        if behind {
            static SEEN: AtomicU64 = AtomicU64::new(0);
            if SEEN.fetch_add(1, Ordering::Relaxed) < 8 {
                tracing::info!(
                    kind = queue(this).kind,
                    now,
                    value,
                    "a queue waits for a fence not reached yet"
                );
            }
            // The fence may have got there meanwhile.
            release_held();
        }
    }
    let _ = (this, value);
    S_OK
}
/// Ticks per second of GPU timestamps (see [`Written::GpuTimestamp`]).
extern "system" fn timestamp_frequency(_: This, out: *mut u64) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: checked non-null.
    unsafe { out.write(TIMESTAMP_FREQUENCY) };
    S_OK
}
const TIMESTAMP_FREQUENCY: u64 = 100_000_000;

/// What a list's bottom-of-pipe write stores when the list has run.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Written {
    Value(u64),
    /// The GPU clock: [`TIMESTAMP_FREQUENCY`] ticks per second since the
    /// process started (our GPU work is done on the CPU's timeline).
    GpuTimestamp,
    /// The CPU's performance counter.
    SystemTimestamp,
}

impl Written {
    /// Stores the low `bytes` (4 or 8) of the value at `at`.
    pub(crate) fn write(self, at: *mut u8, bytes: usize) {
        let value = match self {
            Written::Value(v) => v,
            Written::GpuTimestamp => {
                static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
                let elapsed = START.get_or_init(std::time::Instant::now).elapsed();
                let now = (elapsed.as_nanos() / (1_000_000_000 / u128::from(TIMESTAMP_FREQUENCY))) as u64;
                #[cfg(all(windows, feature = "diagnostics"))]
                crate::host::spans::stamped(at as usize, now);
                as_loaded(now, at as usize, gpu_load())
            }
            Written::SystemTimestamp => system_timestamp(),
        };
        #[cfg(all(windows, feature = "diagnostics"))]
        crate::timeline::told("written when its list had run", at as usize, value);
        // SAFETY: the title names writable GPU memory (unified with ours).
        unsafe {
            if bytes == 4 {
                at.cast::<u32>().write_unaligned(value as u32);
            } else {
                at.cast::<u64>().write_unaligned(value);
            }
        }
        #[cfg(all(windows, feature = "diagnostics"))]
        crate::capture::value_event("write retired", at as u64, value);
    }
}

/// What the title's GPU time is made to look like, in ticks
/// (`VICTORY_GPU_LOAD_MS`; unset: as it is).
///
/// The title asks for four GPU timestamps a frame, in pairs of 16 bytes, the
/// start in the second eight and the end in the first. All of them are written
/// when the frame's last submissions land, within microseconds, so every pair
/// says the card did nothing.
fn gpu_load() -> u64 {
    static LOAD: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *LOAD.get_or_init(|| {
        let ms = std::env::var("VICTORY_GPU_LOAD_MS").ok().and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(0);
        ms * (TIMESTAMP_FREQUENCY / 1000)
    })
}

/// The timestamp written at `at` when the clock says `now`, if a pair's start
/// is to lie `load` ticks before its end.
fn as_loaded(now: u64, at: usize, load: u64) -> u64 {
    if at & 8 != 0 {
        now.saturating_sub(load).max(1)
    } else {
        now
    }
}

fn system_timestamp() -> u64 {
    #[cfg(windows)]
    {
        let mut count = 0;
        // SAFETY: a valid out pointer.
        unsafe { windows_sys::Win32::System::Performance::QueryPerformanceCounter(&mut count) };
        count as u64
    }
    #[cfg(not(windows))]
    0
}

/// Present requests received, including frames retained behind a wait.
static FRAMES: AtomicU64 = AtomicU64::new(0);

#[cfg(feature = "diagnostics")]
pub(crate) fn presented() -> u64 {
    FRAMES.load(Ordering::Relaxed)
}

/// Shows the first plane's first resource in our window.
///
/// shortcut: one plane, no scaling or placement, the source expected in
/// the common state.
extern "system" fn present(
    this: This,
    planes: u32,
    parameters: *const Plane,
    _: *const u8,
) -> HRESULT {
    present_with(this, planes, parameters, false)
}

extern "system" fn present_retail(
    this: This,
    planes: u32,
    parameters: *const Plane,
    _: *const u8,
) -> HRESULT {
    present_with(this, planes, parameters, true)
}

fn present_with(
    this: This,
    planes: u32,
    plane_parameters: *const Plane,
    retail: bool,
) -> HRESULT {
    #[cfg(not(windows))]
    let _ = this;
    #[cfg(all(windows, feature = "diagnostics"))]
    crate::copystat::present_begins();
    let called = crate::timeline::span("the title's Present");
    #[cfg(all(windows, feature = "diagnostics"))]
    crate::timeline::title_thread();
    // Submissions sent to the card and not waited for are settled first,
    // and what that frees runs in its own order, before the display reads their outputs.
    #[cfg(windows)]
    if behind::on() {
        // The present goes into the line of what the queues were given.
    } else if crate::host::late() && !crate::host::late_free_present() {
        crate::host::settle_for_present();
        release_held();
        crate::host::settle_for_present();
    } else if crate::host::late() {
        // The present is sent ahead too: what waits for a value a flight
        // will write runs behind that flight, and nothing is settled here.
        release_held();
    }
    let frame = FRAMES.fetch_add(1, Ordering::Relaxed) + 1;
    if frame.is_power_of_two() {
        let (draws, dispatches) = crate::list::take_counts();
        #[cfg(windows)]
        let (drawn, dispatched) = {
            static DRAWN: AtomicU64 = AtomicU64::new(0);
            static DISPATCHED: AtomicU64 = AtomicU64::new(0);
            let since = |last: &AtomicU64, now: u64| now - last.swap(now, Ordering::Relaxed);
            (
                since(&DRAWN, crate::draw::drawn()),
                since(&DISPATCHED, crate::draw::dispatched()),
            )
        };
        #[cfg(not(windows))]
        let (drawn, dispatched) = (0, 0);
        tracing::info!(
            frame,
            draws,
            drawn,
            dispatches,
            dispatched,
            "frame requested (since the last line: draws and dispatches, and those run on the host)"
        );
        #[cfg(windows)]
        if frame >= 256 {
            let (hits, misses) = crate::shader_cache::counts();
            tracing::info!(hits, misses, "shaders from the cache and translated so far");
        }
    }
    #[cfg(all(windows, feature = "diagnostics"))]
    {
        crate::capture::frame_drew(frame, crate::draw::drawn());
        if crate::capture::listed(frame) {
            before_sync();
            crate::capture::dump_targets(frame);
        }
    }
    #[cfg(windows)]
    if planes != 0 && crate::host::host().is_some() {
        let Some((resource, base)) = display_plane(plane_parameters, 0, retail) else {
            return E_INVALIDARG;
        };
        let second = if planes > 1 && crate::compose::on() {
            let Some(over) = display_plane(plane_parameters, 1, retail) else {
                return E_INVALIDARG;
            };
            Some(over)
        } else {
            None
        };
        let Some(texture) = crate::resource::host_copy(resource) else {
            return E_INVALIDARG;
        };
        let overlay = if let Some((resource, _)) = second {
            let Some(overlay) = crate::resource::host_copy(resource) else { return E_INVALIDARG };
            Some(overlay)
        } else {
            None
        };
        let geometry = [base, second.map(|p| p.1).unwrap_or_default()];
        let shown = Shown {
            frame,
            resource: resource as usize,
            geometry,
        };
        let present = Presentation { texture, overlay, shown };
        if behind::on() && !this.is_null() {
            behind::present(this, present);
        } else if !show_or_hold(this, present) {
            return 0x80004005u32 as i32;
        }
    }
    #[cfg(not(windows))]
    let _ = (planes, plane_parameters, retail);
    called.end();
    #[cfg(all(windows, feature = "diagnostics"))]
    frame_ends();
    S_OK
}

/// Diagnostics: the title's present is over.
#[cfg(all(windows, feature = "diagnostics"))]
fn frame_ends() {
    crate::timeline::presented();
    crate::faultbook::frame();
    crate::ab::presented(|switch, was, will| {
        if switch == "VICTORY_ASYNC_SUBMIT" && was && !will {
            behind::leave();
        }
    });
    if let Some(on) = crate::ab::pick("VICTORY_LANE_CORE") {
        static WAS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if WAS.swap(on, std::sync::atomic::Ordering::Relaxed) != on {
            crate::cores::settle();
        }
    }
    if let Some(on) = crate::ab::pick("VICTORY_CACHE_CORES") {
        static WAS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if WAS.swap(on, std::sync::atomic::Ordering::Relaxed) != on {
            crate::cores::settle();
        }
    }
}

/// The frame a present shows, and the title's resource it shows.
#[cfg(windows)]
#[derive(Clone, Copy)]
struct Shown {
    geometry: [crate::compose::Geometry; 2],
    frame: u64,
    resource: usize,
}

/// Own the display inputs until their queue reaches the present. The state
/// used by the compositor is the state after the preceding submissions.
#[cfg(windows)]
struct Presentation {
    texture: Arc<crate::resource::HostTexture>,
    overlay: Option<Arc<crate::resource::HostTexture>>,
    shown: Shown,
}

#[cfg(windows)]
impl Presentation {
    fn show(&self) -> bool {
        let state = self.texture.state.load(Ordering::Relaxed);
        let overlay = self.overlay.as_ref().map(|o| (&o.texture, o.state.load(Ordering::Relaxed)));
        show_planes(&self.texture.texture, state, overlay, self.shown)
    }
}

#[cfg(windows)]
fn show_or_hold(this: This, present: Presentation) -> bool {
    let _one = lock(&RELEASING);
    let queues = lock(&WAITING).clone();
    if queues.is_empty() {
        drop(_one);
        return present.show();
    }
    if this.is_null() {
        return false;
    }
    // Snapshot each prior queue tail. A later signal on an independent queue
    // can satisfy an earlier wait without itself becoming a present dependency.
    let boundary = Arc::new(AtomicU64::new(queues.len() as u64));
    for q in queues {
        lock(&queue(q as This).held).push(Held::PresentBoundary(boundary.clone()));
    }
    lock(&queue(this).held).push(Held::Present(boundary, present));
    note_held(this);
    drop(_one);
    release_held();
    true
}

/// Shows `texture` (with optional `overlay` plane blended over it) in our window.
#[cfg(windows)]
fn show_planes(
    texture: &windows::Win32::Graphics::Direct3D12::ID3D12Resource,
    state: u32,
    overlay: Option<(&windows::Win32::Graphics::Direct3D12::ID3D12Resource, u32)>,
    what: Shown,
) -> bool {
    let Some(host) = crate::host::host() else {
        return false;
    };
    if crate::host::take_failed_frame() {
        return false;
    }
    let shown = host.present_planes(texture, state, overlay, what.geometry, what.frame);
    #[cfg(feature = "diagnostics")]
    blinks(what.frame, what.resource as This, state, shown);
    #[cfg(not(feature = "diagnostics"))]
    let _ = what.resource;
    shown.is_some()
}

/// Diagnostics: logs a presented frame that is black after one that was
/// not, with what was presented and how many draws ran since.
#[cfg(all(windows, feature = "diagnostics"))]
fn blinks(frame: u64, resource: This, state: u32, shown: Option<bool>) {
    static LAST: std::sync::Mutex<(bool, u64, u64)> = std::sync::Mutex::new((false, 0, 0));
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    let drawn = crate::draw::drawn();
    let black = shown.unwrap_or(false);
    let draws = crate::draw::take_frame();
    if black && !last.0 {
        last.2 += 1;
        tracing::warn!(
            frame,
            resource = ?resource,
            state = format_args!("{state:#x}"),
            draws = drawn - last.1,
            copied = shown.is_some(),
            black_frames = last.2,
            levels = ?crate::capture::recent_levels(),
            "a black frame after a frame that was not"
        );
        for d in draws {
            tracing::warn!("black frame {frame}: {d}");
        }
        AFTER.store(frame + 6, Ordering::Relaxed);
    }
    // Six frames on: the levels around it (a fade or a single drop).
    static AFTER: AtomicU64 = AtomicU64::new(0);
    if AFTER.load(Ordering::Relaxed) == frame {
        tracing::warn!(frame, levels = ?crate::capture::recent_levels(), "levels after the black frame");
    }
    *last = (black, drawn, last.2);
}

/// A run of destination pages: how they are mapped (0 pool pages from
/// `start` on, 1 pool page `start` each, 2 unmapped, 3 left as they are)
/// and how many.
#[repr(C)]
pub(crate) struct PageRange {
    kind: u32,
    start: u32,
    count: u32,
}

/// The ranges mapped from a destination address on.
#[repr(C)]
pub(crate) struct PageBatch {
    destination: u64,
    count: u32,
    ranges: *const PageRange,
}

/// Bytes per mapped page.
const PAGE: u64 = 64 << 10;

/// `CopyPageMappingsBatchX(count, batches, pool, pool pages, flags)`: maps
/// pages of a pool (memory the title mapped physical pages into) at the
/// batches' addresses. The title streams textures into pool pages
/// and maps them into its tiled heap this way (title+0xf2c470, ranges of
/// one constant page or of unmapped pages). Capture the pool pages before
/// returning; publish the destination mappings at this point in queue order.
extern "system" fn copy_page_mappings(
    this: This,
    count: u32,
    batches: *const PageBatch,
    pool: u64,
    pool_pages: u32,
    _flags: u32,
) -> HRESULT {
    if batches.is_null() && count > 0 {
        return E_INVALIDARG;
    }
    let _called = crate::timeline::span("the title's CopyPageMappings");
    let batches = (count > 0).then(|| {
        // SAFETY: the title passes `count` batches.
        unsafe { std::slice::from_raw_parts(batches, count as usize) }
    });
    let mut gathered: Vec<(u64, u64, u64)> = Vec::new();
    for b in batches.into_iter().flatten() {
        if b.ranges.is_null() {
            continue;
        }
        // SAFETY: each batch holds `count` ranges.
        let ranges = unsafe { std::slice::from_raw_parts(b.ranges, b.count as usize) };
        let mut at = b.destination;
        for r in ranges {
            let len = u64::from(r.count) * PAGE;
            let inside = |pages: u32| r.start.checked_add(pages).is_some_and(|e| e <= pool_pages);
            let Some(end) = at.checked_add(len) else { return E_INVALIDARG };
            match r.kind {
                0 | 1 if inside(if r.kind == 1 { 1 } else { r.count }) => {
                    let Some(source) = pool.checked_add(u64::from(r.start) * PAGE) else { return E_INVALIDARG };
                    if r.kind == 0 {
                        gathered.push((at, source, len));
                    } else {
                        gathered.extend((at..end).step_by(PAGE as usize).map(|dest| (dest, source, PAGE)));
                    }
                }
                2 => gathered.push((at, 0, len)),
                3 => {}
                _ => return E_INVALIDARG,
            }
            at = end;
        }
    }
    if gathered.is_empty() { return S_OK; }
    let Some(apply) = crate::resource::capture_page_mappings(&gathered) else { return E_INVALIDARG };
    let mappings: crate::resource::PageMappings = Box::new(move || {
        #[cfg(windows)]
        crate::host::before_page_mappings(&gathered);
        apply()
    });
    #[cfg(windows)]
    if !this.is_null() {
        if behind::on() {
            behind::map_pages(this, mappings);
            return S_OK;
        }
        let mut held = lock(&queue(this).held);
        if !held.is_empty() {
            held.push(Held::Mappings(mappings));
            return S_OK;
        }
    }
    if apply_mappings(mappings) { S_OK } else { E_INVALIDARG }
}

fn apply_mappings(mappings: crate::resource::PageMappings) -> bool {
    let ok = mappings();
    if !ok {
        tracing::error!("a queued page mapping failed");
        #[cfg(windows)]
        crate::host::refuse_frame(format_args!("a queued page mapping failed"));
    }
    ok
}

extern "system" fn desc(this: This, out: *mut QueueDesc) {
    if !out.is_null() {
        // SAFETY: checked non-null; the title passes room for a description.
        unsafe { out.write(queue(this).desc) };
    }
}

/// The queue methods by their slot in older titles' queues (the 2020-era
/// layout): IUnknown 3, object 4, device child 1, queue 26 (12 as on PC,
/// then console ones).
const METHODS: [(usize, *const ()); 15] = [
    (0, query as *const ()),
    (2, release as *const ()),
    (7, object::parent_device as *const ()), // 0x38
    (10, execute as *const ()),              // 0x50
    (11, marker as *const ()),               // 0x58 set marker
    (12, marker as *const ()),               // 0x60 begin event
    (13, marker as *const ()),               // 0x68 end event
    (14, signal as *const ()),               // 0x70
    (15, wait as *const ()),                 // 0x78
    (16, timestamp_frequency as *const ()),  // 0x80
    (26, desc as *const ()),                 // 0xd0
    (29, copy_page_mappings as *const ()),   // 0xe8 page mapping batch
    (30, present as *const ()),              // 0xf0
    (33, execute_x as *const ()),            // 0x108
    (32, signal2_x as *const ()),            // 0x100
];

/// Newer titles' queues have two more methods before Signal, which moved
/// from 0x70 to 0x80: every slot from 0x70 on is two later (taken here as
/// starting exactly at 0x70, the methods before it kept in place).
/// Inferred from the title's calls: Signal 0x80 `(fence,
/// value)`, timestamp frequency 0x90 (title+0x24841a6), suspend and resume
/// 0xb8/0xc0, PresentX 0x100, the frame-token submission 0x118 `(count,
/// lists, token, flags)` (title+0x24821dd).
const fn newer_slot(slot: usize) -> usize {
    if slot >= 14 { slot + 2 } else { slot }
}

const fn queue_table(newer: bool) -> [*const (); 36] {
    let mut f = object::d3d12_object(object::missing_table::<QUEUE, 36>());
    let mut i = 0;
    while i < METHODS.len() {
        let (slot, method) = METHODS[i];
        f[if newer { newer_slot(slot) } else { slot }] = if newer && slot == 30 {
            present_retail as *const ()
        } else {
            method
        };
        i += 1;
    }
    f
}

static FUNCTIONS: Table<36> = Table(queue_table(false));
static NEWER_FUNCTIONS: Table<36> = Table(queue_table(true));

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::slot;

    /// Where a queue's signal is: newer layouts have it two slots later.
    /// Another test may switch layouts while this one runs, so the queue's
    /// own table decides.
    fn signal_slot(q: This) -> usize {
        // SAFETY: one of our queues; its first field is the table.
        let table = unsafe { *q.cast::<*const *const ()>() };
        if table == NEWER_FUNCTIONS.0.as_ptr() {
            0x80
        } else {
            0x70
        }
    }

    #[test]
    fn queue_signals_complete_at_once() {
        let null = std::ptr::null();
        let desc = QueueDesc {
            kind: 0,
            priority: 0,
            flags: 0,
            nodes: 0,
        };
        let mut q = null;
        assert_eq!(create(null, &desc, &QUEUE_IIDS[0], &mut q), S_OK);
        // SAFETY: the title reads the type at +0x10.
        assert_eq!(unsafe { q.cast::<u8>().add(0x10).cast::<u32>().read() }, 0);
        let mut fence = null;
        let fence_iid = &crate::fence::FENCE_IIDS[0];
        assert_eq!(
            crate::fence::create(null, 0, 0, fence_iid, &mut fence),
            S_OK
        );
        let signal = slot::<extern "system" fn(This, This, u64) -> HRESULT>(q, signal_slot(q));
        assert_eq!(signal(q, fence, 7), S_OK);
        assert_eq!(
            slot::<extern "system" fn(This) -> u64>(fence, 0x40)(fence),
            7
        );
        assert_eq!(slot::<extern "system" fn(This) -> u32>(q, 0x10)(q), 0);
    }

    /// The title's compute queue: `{COMPUTE, 0, pipe 1, queue 0}`.
    #[test]
    fn console_queues_are_created_as_on_pc() {
        let null = std::ptr::null();
        let desc = QueueDescX {
            kind: 2,
            flags: 0,
            engine_or_pipe: 1,
            queue: 0,
        };
        let mut q = null;
        assert_eq!(create_x(null, &desc, &QUEUE_IIDS[0], &mut q), S_OK);
        assert_eq!(queue(q).desc.kind, 2);
        let none = std::ptr::null();
        assert_eq!(create_x(null, none, &QUEUE_IIDS[0], &mut q), E_INVALIDARG);
    }

    #[test]
    fn newer_queues_have_their_methods_two_slots_later() {
        let (old, new) = (queue_table(false), queue_table(true));
        assert_eq!(old[10], new[10]); // submission stays at 0x50
        assert_eq!(old[14], new[16]); // signal moves from 0x70 to 0x80
        assert_eq!(old[30], present as *const ());
        assert_eq!(new[32], present_retail as *const ());
        assert_eq!(new[35], execute_x as *const ()); // 0x118
    }

    #[test]
    fn a_pair_of_timestamps_shows_the_load_it_is_given() {
        let (start, end) = (0xd_0419_6698, 0xd_0419_6690);
        assert_eq!(as_loaded(5_000_000, end, 4_000_000) - as_loaded(5_000_000, start, 4_000_000), 4_000_000);
        assert_eq!(as_loaded(5_000_000, start, 0), 5_000_000);
        assert_eq!(as_loaded(5, start, 100), 1);
    }

    #[test]
    fn timestamps_and_values_are_written() {
        let mut at = [0u64; 2];
        Written::Value(0x1_0000_0002).write(at.as_mut_ptr().cast(), 4);
        assert_eq!(at[0], 2);
        Written::GpuTimestamp.write(at[1..].as_mut_ptr().cast(), 8);
        let first = at[1];
        std::thread::sleep(std::time::Duration::from_millis(2));
        Written::GpuTimestamp.write(at[1..].as_mut_ptr().cast(), 8);
        assert!(at[1] > first);
    }
}
