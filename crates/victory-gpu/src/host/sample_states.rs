//! SRV state at actual native shader commands. State changes between two
//! commands share an ordinal, so only their final result affects that read.
use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;
use crate::descriptor::index_words::SampleWords;

type Reader = (usize, u32, u8, Option<usize>);
const UAV: u8 = 8;
const SHADER_STATES: u32 = 0xc0 | UAV as u32;
use windows::Win32::Graphics::Direct3D12::*;

/// Reflected shader-view kinds an SRV can select for this native image.
pub(super) fn native_kinds(desc: D3D12_RESOURCE_DESC) -> u32 {
    let single = desc.DepthOrArraySize == 1;
    let bit = |kind| 1u32 << kind;
    match desc.Dimension {
        D3D12_RESOURCE_DIMENSION_TEXTURE1D => bit(6) | if single { bit(1) } else { 0 },
        D3D12_RESOURCE_DIMENSION_TEXTURE2D if desc.SampleDesc.Count > 1 => bit(8) | if single { bit(3) } else { 0 },
        D3D12_RESOURCE_DIMENSION_TEXTURE2D => {
            let cube = desc.DepthOrArraySize >= 6 && desc.Width == u64::from(desc.Height);
            bit(7) | if single { bit(2) } else { 0 } | if cube { bit(5) | bit(9) } else { 0 }
        }
        D3D12_RESOURCE_DIMENSION_TEXTURE3D => bit(4),
        _ => 0,
    }
}

#[derive(Default)]
pub(super) struct Uses {
    command: u32,
    pending: Vec<Reader>,
    sources: Vec<Arc<[SampleWords]>>,
    source_ids: HashMap<Arc<[SampleWords]>, usize>,
    readers: HashMap<Reader, Vec<Range<u32>>>,
    transitions: HashMap<u64, Vec<(u32, u8)>>,
    // Native mips used by all OM binds and clears in the closed list.
    // None means an unknown view or a whole-image transition.
    color_mips: HashMap<usize, Option<Vec<u32>>>,
    unspecified_alias: bool,
    #[cfg(feature = "diagnostics")]
    pipelines: Vec<(u32, usize)>,
    #[cfg(feature = "diagnostics")]
    targets: Vec<(u32, Vec<(usize, bool, [u32; 7])>)>,
}

impl Uses {
    pub(super) fn render_view(&mut self, native: usize, depth: bool, words: [u32; 7]) {
        let mip = match (depth, words[0], words[2], words[4]) {
            (false, 0, _, _) => Some(0),
            (false, _, 4, 0) => Some(words[3]), // TEXTURE2D, plane zero
            _ => None,
        };
        let known = self.color_mips.entry(native).or_insert_with(|| Some(Vec::new()));
        if let Some(mip) = mip {
            if let Some(mips) = known && !mips.contains(&mip) { mips.push(mip); }
        } else { *known = None; }
    }

    pub(super) fn whole_color_access(&mut self, native: usize) {
        self.color_mips.insert(native, None);
    }

    pub(super) fn alias_barrier(&mut self, before: usize, after: usize) {
        if before == 0 || after == 0 { self.unspecified_alias = true; }
        if before != 0 { self.whole_color_access(before); }
        if after != 0 { self.whole_color_access(after); }
    }

    pub(super) fn disjoint_color_read(&self, native: usize, mips: Range<u32>) -> bool {
        !self.unspecified_alias && !mips.is_empty() && self.color_mips.get(&native).and_then(Option::as_ref)
            .is_some_and(|writes| !writes.is_empty() && writes.iter().all(|mip| !mips.contains(mip)))
    }

    #[cfg(feature = "diagnostics")]
    pub(super) fn color_uses(&self, native: usize) -> (bool, Option<&Option<Vec<u32>>>) {
        (self.unspecified_alias, self.color_mips.get(&native))
    }

    #[cfg(feature = "diagnostics")]
    pub(super) fn targets(&mut self, views: Vec<(usize, bool, [u32; 7])>) {
        if self.targets.last().map_or(&[][..], |(_, v)| v.as_slice()) == views { return; }
        if self.targets.last().is_some_and(|(at, _)| *at == self.command) { self.targets.pop(); }
        self.targets.push((self.command, views));
    }

    #[cfg(feature = "diagnostics")]
    pub(super) fn target_view(&self, command: u32, native: usize) -> Vec<(bool, [u32; 7])> {
        let before = self.targets.partition_point(|(at, _)| *at <= command);
        self.targets[..before].last().into_iter().flat_map(|(_, v)| v.iter())
            .filter(|(p, _, _)| *p == native).map(|(_, depth, view)| (*depth, *view)).collect()
    }

    pub(super) fn earlier_read_states(&self, kinds: impl FnOnce() -> u32) -> u32 {
        if self.readers.is_empty() { return 0; }
        let kinds = kinds();
        if kinds == 0 { return 0; }
        self.readers.keys().filter(|(_,used,state,_)| *state != UAV && (*used == 0 || used & kinds != 0))
            .fold(0, |states, &(_,_,stage,_)| states | u32::from(stage))
    }

    pub(super) fn begin(&mut self) {
        self.pending.clear();
    }

    pub(super) fn sources(&self) -> &[Arc<[SampleWords]>] { &self.sources }

    pub(super) fn readers_for(&self, heap: usize, uav: bool) -> Vec<(u32, Option<usize>)> {
        self.readers.keys().filter(|&&(owner, _, state, _)| owner == heap && (state == UAV) == uav)
            .map(|&(_, kinds, _, scope)| (kinds, scope)).collect()
    }

    pub(super) fn scopes_for(&self, heap: usize, uav: bool) -> Vec<(usize, Arc<[SampleWords]>)> {
        let ids: std::collections::BTreeSet<_> = self.readers.keys()
            .filter(|&&(owner, _, state, _)| owner == heap && (state == UAV) == uav)
            .filter_map(|&(_, _, _, scope)| scope).collect();
        ids.into_iter().map(|id| (id, self.sources[id].clone())).collect()
    }

    pub(super) fn observes(&mut self, heap: usize, kinds: u32, stage: u8, words: Option<&[SampleWords]>) {
        let scope = words.map(|words| {
            if let Some(id) = self.source_ids.get(words) { return *id; }
            let id = self.sources.len();
            let sources: Arc<[SampleWords]> = Arc::from(words);
            self.source_ids.insert(sources.clone(), id);
            self.sources.push(sources);
            id
        });
        let key = (heap, kinds, stage, scope);
        if !self.pending.contains(&key) {
            self.pending.push(key);
        }
    }

    pub(super) fn issued(&mut self, pipeline: usize) {
        #[cfg(feature = "diagnostics")]
        if self.pipelines.last().is_none_or(|&(_, last)| last != pipeline) {
            self.pipelines.push((self.command, pipeline));
        }
        #[cfg(not(feature = "diagnostics"))]
        let _ = pipeline;
        let next = self.command.checked_add(1).expect("native command ordinal");
        for key in self.pending.drain(..) {
            let runs = self.readers.entry(key).or_default();
            if let Some(last) = runs.last_mut().filter(|r| r.end == self.command) {
                last.end = next;
            } else {
                runs.push(self.command..next);
            }
        }
        self.command = next;
    }

    pub(super) fn transition(&mut self, serial: u64, state: u32) {
        let changes = self.transitions.entry(serial).or_default();
        let state = (state & SHADER_STATES) as u8;
        if let Some(last) = changes.last_mut().filter(|c| c.0 == self.command) {
            last.1 = state;
        } else {
            changes.push((self.command, state));
        }
    }

    #[cfg(feature = "diagnostics")]
    pub(super) fn pipeline(&self, ordinal: u32) -> Option<usize> {
        let next = self.pipelines.partition_point(|&(at, _)| at <= ordinal);
        next.checked_sub(1).map(|i| self.pipelines[i].1)
    }

    #[cfg(feature = "diagnostics")]
    pub(super) fn shader_states(&self, serial: u64) -> (u32, &[(u32, u8)]) {
        (self.command, self.transitions.get(&serial).map_or(&[], Vec::as_slice))
    }

    /// The ordinal, required and actual shader-read states for the first
    /// potential reader lacking its stage's bit. The native list prelude
    /// establishes its first tracked state before command zero.
    pub(super) fn conflict(&self, serial: u64, initial: u32, heap: usize, kind: u32, uav: bool, mut may_select: impl FnMut(usize) -> bool) -> Option<(u32, u8, u8, Option<usize>)> {
        let changes = self.transitions.get(&serial).map_or(&[][..], Vec::as_slice);
        'reader: for (&(owner, kinds, required, scope), runs) in &self.readers {
            if owner != heap || (required == UAV) != uav || (kinds != 0 && kind != 0 && kinds & kind == 0) {
                continue;
            }
            for run in runs {
                let start = changes.partition_point(|&(at, _)| at <= run.start);
                let state = if start == 0 {
                    (initial & SHADER_STATES) as u8
                } else {
                    changes[start - 1].1
                };
                if state & required != required {
                    if scope.is_some_and(|id| !may_select(id)) { continue 'reader; }
                    return Some((run.start, required, state, scope));
                }
                for &(at, state) in &changes[start..] {
                    if at >= run.end {
                        break;
                    }
                    if state & required != required {
                        if scope.is_some_and(|id| !may_select(id)) { continue 'reader; }
                        return Some((at, required, state, scope));
                    }
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sampled_state_is_required_only_at_actual_matching_draws() {
        let mut uses = Uses::default();
        uses.issued(1); // shader store with no opaque SRV
        uses.transition(19, 0x80);
        uses.observes(7, 4, 0x80, None);
        uses.issued(2);
        uses.transition(19, 0x4);
        uses.observes(7, 16, 0x40, None);
        uses.issued(3);
        assert_eq!(uses.conflict(19, 0x8, 7, 4, false, |_| true), None);
        assert_eq!(uses.conflict(19, 0x8, 7, 16, false, |_| true), Some((2, 0x40, 0, None)));
        uses.transition(19, 0x40);
        uses.transition(19, 0x8);
        uses.observes(7, 4, 0x40, None);
        uses.issued(4);
        assert_eq!(uses.conflict(19, 0x8, 7, 4, false, |_| true), Some((3, 0x40, UAV, None)));
    }
}

#[cfg(test)]
#[test]
fn disjoint_mip_reads_require_known_render_views_for_the_entire_list() {
    let mut uses = Uses::default();
    let texture = 42;
    let mut view = [1, 28, 4, 3, 0, 0, 0];
    assert!(!uses.disjoint_color_read(texture, 11..12));
    uses.render_view(texture, false, view);
    assert!(uses.disjoint_color_read(texture, 11..12));
    assert!(!uses.disjoint_color_read(texture, 2..4));
    view[3] = 11;
    uses.render_view(texture, false, view);
    assert!(!uses.disjoint_color_read(texture, 11..12));
    uses.whole_color_access(texture);
    uses.render_view(texture, false, view);
    assert!(!uses.disjoint_color_read(texture, 1..2));
}

#[cfg(test)]
#[test]
fn stage_materials_do_not_spread_between_draws_or_stages() {
    let mut uses = Uses::default();
    // A compact selector at command zero excludes this 2D depth slot.
    // The broad non-pixel stage sees only cubemaps on this command.
    uses.observes(7, 4, 0x80, Some(&[]));
    uses.observes(7, 32, 0x40, None);
    uses.issued(1);
    uses.transition(19, 0x40);
    uses.observes(7, 4, 0x40, None);
    uses.issued(2);
    assert_eq!(uses.conflict(19, 0x10, 7, 4, false, |_| false), None);
    assert_eq!(uses.conflict(19, 0x10, 7, 4, false, |_| true), Some((0, 0x80, 0, Some(0))));
    assert_eq!(uses.conflict(19, 0x10, 7, 32, false, |_| false), Some((0, 0x40, 0, None)));
    uses.transition(19, 0x10);
    uses.observes(7, 4, 0x80, Some(&[]));
    uses.issued(3);
    assert_eq!(uses.sources().len(), 1);
}

#[cfg(test)]
#[test]
fn uav_and_srv_proofs_follow_their_own_commands_and_resource_states() {
    let mut uses = Uses::default();
    uses.observes(7, 4, 0x80, None);
    uses.issued(101);
    uses.observes(7, 4, UAV, Some(&[]));
    uses.issued(102);
    assert_eq!(uses.readers_for(7, true), [(4, Some(0))]);
    assert_eq!(uses.readers_for(7, false), [(4, None)]);
    assert_eq!(uses.scopes_for(7, true).len(), 1);
    assert_eq!(uses.earlier_read_states(|| 4), 0x80);
    assert_eq!(uses.conflict(19, 0x80, 7, 4, true, |_| false), None);
    assert_eq!(uses.conflict(19, 0x80, 7, 4, true, |_| true), Some((1, UAV, 0x80, Some(0))));
    assert_eq!(uses.conflict(19, 0x80, 7, 4, false, |_| true), None);
    #[cfg(feature = "diagnostics")]
    assert_eq!(uses.pipeline(1), Some(102));
}

#[cfg(test)]
#[test]
fn omitting_an_entire_srv_pins_every_matching_stage() {
    use super::{SampleSelection, StageSelections};
    use std::collections::BTreeSet;
    use std::sync::Arc;

    let proof = |slots: &[u32]| (SampleSelection {
        words: Arc::from([]), slots: BTreeSet::from_iter(slots.iter().copied()), heap_count: 100,
    }, false.into());
    let mut scopes = StageSelections {
        readers: vec![(4, Some(0)), (4 | 16, Some(1)), (32, None), (64, Some(2))],
        entries: [(0, proof(&[3,5])), (1, proof(&[7]))].into(),
        ..Default::default()
    };
    assert!(!scopes.excludes(4, 3));
    assert!(!scopes.excludes(4, 7));
    assert!(scopes.entries.values().all(|(_, used)| !used.get()));
    assert!(scopes.excludes(4, 11));
    assert!(scopes.entries.values().all(|(_, used)| used.get()));
    assert!(!scopes.excludes(32, 11)); // broad, matching cube stage
    assert!(!scopes.excludes(64, 11)); // bounded reflection, unavailable buffer
    assert!(!scopes.excludes(0, 11));  // implicit view could match both broad stages
    assert!(scopes.excludes(128, 11)); // no issued stage can select this view
    assert!(StageSelections::default().excludes(4, 11));
    scopes.readers.push((0, None));
    scopes.coverage.get_mut().clear();
    assert!(!scopes.excludes(4, 11));  // unbounded shape
}
