//! Compact device-local descriptor indices observed between the occurrence's
//! ordinary buffer uploads and its first native consumer.
use super::*;
use crate::descriptor::index_words::{DependentWords, DeviceBytes, DeviceRanges, SELECTOR_READ_BUDGET};

#[cfg(feature = "diagnostics")]
pub(super) static PROBE_FENCES: [std::sync::atomic::AtomicU64; 2] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 2];

// These private fences sit behind the actual selector copy/upload on the
// direct queue, separately from the native title-list clock.
fn wait_for_probe(queue: &ID3D12CommandQueue, fence: &ID3D12Fence) -> windows::core::Result<()> {
    // SAFETY: both COM interfaces are live through this synchronous wait.
    unsafe {
        queue.Signal(fence, 1)?;
        #[cfg(feature = "diagnostics")]
        PROBE_FENCES[0].fetch_add(1, Ordering::Relaxed);
        fence.SetEventOnCompletion(1, HANDLE::default())?;
        #[cfg(feature = "diagnostics")]
        PROBE_FENCES[1].fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

pub(super) struct Plan {
    pub(super) ordinary: DeviceRanges,
    chains: Vec<DependentWords>,
    writes: Vec<(usize, usize)>,
    pub(super) refresh_sources: std::collections::BTreeSet<u64>,
}

pub(super) fn ranges(list: &HostList) -> Option<Plan> {
    let d = list.deferred();
    // A broad mesh stage can share an occurrence with a pixel stage whose
    // exact words still prove that a depth/RTV slot was never sampled there.
    let stages = list.sample_uses.lock().unwrap_or_else(|e| e.into_inner()).sources().to_vec();
    let inputs: std::collections::HashSet<_> = d.indexed_heaps.iter().flat_map(|(_, _, s)| {
        s.words.iter().filter(|_| s.words_only).chain(s.uav_words.iter().filter(|_| s.uav_words_only))
    }).chain(stages.iter().flat_map(|s| s.iter())).filter(|w| w.on_device()).collect();
    if inputs.is_empty() { return None; }
    // An explicit copy, callback, or GPU buffer store can change the source
    // between two draws. Those require the existing per-draw broad preparation.
    let unsupported = d.copies.iter().find(|op| {
        !matches!(
            op,
            Step::Refresh(..) | Step::GpuWrite(..) | Step::Texture(..) | Step::PrivateNote(_) | Step::Fill(..) | Step::Copy(..)
        )
    });
    if d.unbounded_guest_write || d.descriptor_writes || unsupported.is_some() {
        #[cfg(feature = "diagnostics")]
        {
            static SAID: std::sync::Once = std::sync::Once::new();
            SAID.call_once(|| {
                tracing::info!(unknown=d.unbounded_guest_write, descriptors=d.descriptor_writes,
                declarations=d.declarations.len(), first_step=?unsupported.map(std::mem::discriminant),
                "device selector shares an occurrence with other producers")
            });
        }
        return None;
    }
    let mut ranges = DeviceRanges::new();
    let mut chains = Vec::new();
    for source in inputs {
        source.device_ranges(&mut ranges);
        if let Some(chain) = source.chain.as_deref().filter(|c| c.on_device()) {
            chains.push(chain.clone());
        }
    }
    // Bringing an input up to date cannot cross a declaration for the same
    // native buffer. The later normal prepare preserves all other writes,
    // shadow snapshots, and upload ordering.
    let mut declared = std::collections::BTreeSet::new();
    let mut ordered_refresh = std::collections::BTreeSet::new();
    let mut copied = Vec::new();
    for step in &d.copies {
        match step {
            Step::Copy(dst, _, bytes) => {
                let aliases = crate::inplace::aliases_of_bytes(*dst, *bytes);
                if aliases.iter().any(|&(lo, hi)| crate::descriptor::in_shader_heap(lo, hi - lo)) {
                    return None;
                }
                join_ranges(&mut copied, aliases);
            }
            Step::GpuWrite(shadow, ..) => {
                declared.insert(shadow.id());
            }
            Step::Refresh(shadow, ..) if declared.contains(&shadow.id()) => { ordered_refresh.insert(shadow.id()); }
            _ => {}
        }
    }
    // The selector pass moves this shadow's ordinary refreshes ahead of
    // the recording. Preserve normal copy/refresh order for any shadow
    // that can receive bytes from a copy, even outside the selector word.
    if !copied.is_empty() {
        for step in &d.copies {
            if let Step::Refresh(shadow, at, bytes) = step
                && crate::inplace::aliases_of_bytes(*at, *bytes).iter().any(|&(lo, hi)| touches(&copied, lo, hi)) {
                ordered_refresh.insert(shadow.id());
            }
        }
    }
    drop(d);
    if crate::picked::may_write(list) {
        return None;
    }
    if !ordered_refresh.is_empty() {
        ranges.retain(|&(id,_,_),_| !ordered_refresh.contains(&id));
        chains.retain(|chain| chain.device_sources().all(|id| !ordered_refresh.contains(&id)));
    }
    let writes = list.indexed_write_ranges()?;
    if !writes.is_empty() {
        // A source overlapping an actual producer stays unresolved. A
        // separate draw can still select from its own independent table.
        let overlaps = |at, n| crate::inplace::aliases_of_bytes(at,n).iter().any(|&(lo,hi)| touches(&writes,lo,hi));
        ranges.retain(|&(_,at,n),_| !overlaps(at,n));
    }
    if ranges.is_empty() && chains.is_empty() { return None; }
    let refresh_sources = ranges.keys().map(|&(id,_,_)| id)
        .chain(chains.iter().flat_map(DependentWords::device_sources)).collect();
    Some(Plan {
        ordinary: ranges,
        chains,
        writes,
        refresh_sources,
    })
}

/// Keep the ordinary producer behind its original queue fence. Surrounding
/// parts can still fly normally. A replay that cannot split retains broad preparation.
pub(super) fn cut_for(runs: &[&HostList]) -> Option<usize> {
    if runs.len() < 2 {
        return None;
    }
    runs.iter().position(|list| ranges(list).is_some()).map(|i| i.max(1))
}

pub(super) fn read(host: &Host, list: &HostList, plan: &Plan) -> Option<DeviceBytes> {
    let upload_fence: ID3D12Fence = unsafe { host.device.CreateFence(0, D3D12_FENCE_FLAG_NONE) }.ok()?;
    // Gather uploads only once and preserve later dirty bytes until the next
    // occurrence. Every level runs on this direct queue behind those uploads.
    let mut batch = Batch::default();
    for step in &list.deferred().copies {
        if let Step::Refresh(shadow, at, n) = step
            && plan.refresh_sources.contains(&shadow.id())
        {
            batch.add(shadow, *at, *n);
        }
    }
    batch.flush();
    let uploads = host.card_before(&[], &Default::default(), false, None);
    let mut bytes = DeviceBytes::new();
    let (mut ranges, bulk): (DeviceRanges,DeviceRanges) = plan.ordinary.iter()
        .map(|(&key,shadow)| (key,shadow.clone())).partition(|((_,_,n),_)| *n <= 256);
    let mut bulk = Some(bulk);
    let mut retired = false;
    // Indirect X count, group table, record pointers, then material handles. Each round
    // deduplicates all paths and descriptors before issuing the readback.
    for round in 0..4 {
        for chain in &plan.chains {
            chain.evaluate(&plan.writes, 0, &mut Default::default(), Some(&bytes), &mut ranges);
        }
        // Prioritize precise dependent words before a whole table. All
        // accepted bytes belong to this same ordered occurrence.
        if round == 3 || ranges.is_empty() || plan.chains.is_empty() {
            let mut room = SELECTOR_READ_BUDGET;
            for (key,shadow) in bulk.take().unwrap_or_default() {
                if key.2 <= room {
                    room -= key.2;
                    ranges.insert(key,shadow);
                }
            }
        }
        if ranges.is_empty() { break; }
        // The readback allocation is bounded per copy batch. Many exact
        // draws can share an occurrence without losing their last operands.
        let mut batch = DeviceRanges::new();
        let mut size = 0;
        for (key,shadow) in std::mem::take(&mut ranges) {
            if size != 0 && size + key.2 > SELECTOR_READ_BUDGET {
                if let Some(fetched) = read_ranges(host,&batch) { retired = true; bytes.extend(fetched); }
                batch.clear();
                size = 0;
            }
            size += key.2;
            batch.insert(key,shadow);
        }
        if !batch.is_empty() && let Some(fetched) = read_ranges(host,&batch) {
            retired = true;
            bytes.extend(fetched);
        }
    }
    if let Some(plan) = uploads {
        // Even an unavailable chain can leave an ordinary upload on the
        // queue. Its allocator and source memory retire after that work.
        if !retired {
            wait_for_probe(&host.queue, &upload_fence)
                .unwrap_or_else(|error| fail(format_args!("could not retire the material uploads ({error})")));
        }
        host.card_done(plan);
    }
    Some(bytes)
}

fn read_ranges(host: &Host, ranges: &DeviceRanges) -> Option<DeviceBytes> {
    let total: usize = ranges.keys().map(|(_, _, n)| *n).sum();
    let readback = host.read_back_buffer(total as u64)?;
    let (allocator, commands) = host.make_prelude()?;
    // Create all command resources before taking the occurrence's dirty uploads.
    let fence: ID3D12Fence = unsafe { host.device.CreateFence(0, D3D12_FENCE_FLAG_NONE) }.ok()?;
    unsafe {
        allocator.Reset().ok()?;
        commands.Reset(&allocator, None).ok()?;
    }
    let mut offset = 0;
    let mut entries = ranges.iter().peekable();
    while let Some((&(id,address,n),shadow)) = entries.next() {
        let mut bytes = n;
        while let Some((&(next_id,next,n),_)) = entries.peek().copied() {
            if next_id != id || address.checked_add(bytes) != Some(next) { break; }
            bytes += n;
            entries.next();
        }
        let from = shadow.offset(address as u64);
        unsafe {
            commands.CopyBufferRegion(&readback, offset, &shadow.buffer, from, bytes as u64);
        }
        offset += bytes as u64;
    }
    unsafe {
        commands.Close().ok()?;
    }
    let command: ID3D12CommandList = commands.cast().ok()?;
    // Separate ExecuteCommandLists calls give buffers their usual COMMON decay.
    unsafe {
        host.queue.ExecuteCommandLists(&[Some(command)]);
        wait_for_probe(&host.queue, &fence)
            .unwrap_or_else(|error| {
                fail(format_args!(
                    "could not complete the material selector readback ({error})"
                ))
            });
    }
    let mut bytes = DeviceBytes::new();
    let mut data = std::ptr::null_mut();
    let region = D3D12_RANGE { Begin: 0, End: total };
    // An empty snapshot takes the broad path while preserving the completed
    // buffer-upload phase. Map after the fence for CPU visibility.
    if unsafe { readback.Map(0, Some(&region), Some(&mut data)) }.is_err() {
        return Some(bytes);
    }
    if data.is_null() {
        unsafe {
            readback.Unmap(0, None);
        }
        return Some(bytes);
    }
    let mut offset = 0;
    for &key @ (_, _, n) in ranges.keys() {
        // The exact readback range is mapped and this queue's copy completed.
        let part = unsafe { std::slice::from_raw_parts(data.cast::<u8>().add(offset), n) };
        bytes.insert(key, part.to_vec());
        offset += n;
    }
    unsafe {
        readback.Unmap(0, Some(&D3D12_RANGE { Begin: 0, End: 0 }));
    }
    #[cfg(feature = "diagnostics")]
    {
        static READS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let uses = READS.fetch_add(1, Ordering::Relaxed) + 1;
        if uses.is_power_of_two() {
            tracing::info!(
                uses,
                bindings = ranges.len(),
                bytes = total,
                "sampled device-local material indices after buffer uploads"
            );
        }
    }
    Some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selector_reads_follow_uploads_and_preserve_untouched_gpu_words() {
        let Some(host) = host() else {
            return;
        };
        let title = [11u32, 22u32];
        let at = title.as_ptr() as usize;
        let Some(shadow) = crate::shadow::by_address(at as u64, 8) else {
            return;
        };
        let (seed, data) = host.upload_buffer(8).unwrap();
        unsafe {
            std::ptr::copy_nonoverlapping([6u32, 7].as_ptr().cast::<u8>(), data, 8);
        }
        host.copy_preserved_buffers(&[(shadow.buffer.clone(), 0, seed, 0, 8)])
            .unwrap();
        let list = host.new_list().unwrap();
        list.defer_refresh(&shadow, at, 4);
        let first = (shadow.id(), at, 4);
        let second = (shadow.id(), at + 4, 4);
        let ranges = DeviceRanges::from([(first, shadow.clone()), (second, shadow)]);
        let snapshot = read(
            host,
            &list,
            &Plan {
                refresh_sources: [first.0].into(),
                ordinary: ranges,
                chains: vec![],
                writes: vec![],
            },
        )
        .unwrap();
        assert_eq!(snapshot[&first], 11u32.to_le_bytes());
        assert_eq!(snapshot[&second], 7u32.to_le_bytes());
    }
}
