use super::*;
use crate::gfx10::Valu;
use crate::ir::VCC;
use crate::{ResourceType, UsedResource};
use std::collections::BTreeMap;

pub(super) fn plain(inst: &Inst) -> Option<&Valu> {
    let Fields::Valu(f) = &inst.fields else {
        return None;
    };
    (f.sdwa.is_none() && f.dpp.is_none() && f.abs == 0 && f.neg == 0 && !f.clamp && f.omod == 0 && f.opsel == 0)
        .then_some(f)
}

#[derive(Default)]
struct Columns {
    coarse: Vec<BufferIndex>,
    packed: Option<Vec<PackedMaterialColumn>>,
    ranges: Option<Vec<ScalarRangeColumn>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wave32_exec_subset(words: &[u32]) -> bool {
        let code: Vec<_> = words.iter().flat_map(|word| word.to_le_bytes()).collect();
        let p = crate::ir::build(&code, false, 32).unwrap();
        let t = Tracer::new(&p, &[], HwStage::Cs, None).unwrap();
        let mask = |reg| p.blocks.iter().flat_map(|block| &block.insts)
            .find(|inst| inst.write(Reg::V(reg)).is_some()).unwrap().read(Reg::S(EXEC)).unwrap();
        t.resolve_exec_subset(mask(2), mask(1))
    }

    #[test]
    fn wave32_saved_exec_must_restore_the_mask_that_covered_the_producer() {
        let mut words = [
            0xbe80_3c01, // s_and_saveexec_b32 s0, s1: save the wider entry mask.
            0x7e02_0280, // v_mov_b32 v1, 0: mask under which the row is written.
            0xbe82_3c03, // s_and_saveexec_b32 s2, s3: save the producer's mask.
            0xbefe_0302, // Restore s2.
            0x7da4_0080, // v_cmpx_eq_u32 exec, 0, v0.
            0x7e04_0280, // v_mov_b32 v2, 0: mask under which the row is read.
            0xbf81_0000,
        ];
        assert!(wave32_exec_subset(&words));
        words[3] = 0xbefe_0300; // Restoring entry can re-enable lanes that skipped the row write.
        assert!(!wave32_exec_subset(&words));
    }

    #[test]
    fn wave32_exec_subset_checks_operand_roles_and_every_join_arm() {
        let check = |combine| wave32_exec_subset(&[
            0xbe80_3c01, 0x7e02_0280, // v1 records the initial narrowed EXEC.
            0x8704_027e, // s4 = exec & s2.
            0x8a05_037e, // s5 = exec & ~s3.
            combine,
            0x7e04_0280, 0xbf81_0000,
        ]);
        assert!(check(0x887e_0504)); // exec = s4 | s5.
        assert!(!check(0x887e_0604)); // exec = s4 | unrelated s6.
        assert!(!check(0x887e_0406)); // exec = unrelated s6 | s4.
        assert!(check(0x8a7e_0604)); // exec = s4 & ~s6.
        assert!(!check(0x8a7e_0406)); // exec = s6 & ~s4.
        assert!(!check(0xbe87_3f06)); // s_andn2_saveexec_b32 s7, s6 can widen EXEC.

        let join = |second_arm| wave32_exec_subset(&[
            0xbe80_3c01, 0x7e02_0280,
            0xbf84_0002, // s_cbranch_scc0 to the second arm.
            0x877e_027e, 0xbf82_0001, // Narrow EXEC and jump to the join.
            second_arm,
            0x7e04_0280, 0xbf81_0000,
        ]);
        assert!(join(0x8a7e_037e)); // Narrow EXEC on the other arm too.
        assert!(!join(0xbefe_0306)); // Replacing EXEC with unrelated s6 can widen it.
        assert!(!wave32_exec_subset(&[
            0xbe80_3c01,
            0x877e_027e, 0x7e02_0280, // Define the producer's EXEC inside a loop.
            0xbf85_fffd, // Re-enter the producer's defining block.
            0x877e_037e, 0x7e04_0280, 0xbf81_0000,
        ]));
    }

    #[test]
    fn wave64_column_requires_both_mask_halves_to_cover_the_entry() {
        let words = [
            0x7e0a_0280u32, // v_mov_b32 v5, 0 under the original EXEC.
            0xbe82_2400, // s_and_saveexec_b64 s[2:3], s[0:1].
            0x7e0c_0280, // v_mov_b32 v6, 0 under the narrower EXEC.
            0xbf81_0000,
        ];
        let code: Vec<_> = words.iter().flat_map(|word| word.to_le_bytes()).collect();
        let program = crate::ir::build(&code, true, 32).unwrap();
        let tracer = Tracer::new(&program, &[], HwStage::Cs, None).unwrap();
        let [wide, save, narrow, _] = program.blocks[0].insts.as_slice() else { panic!("unexpected mask program") };
        let exec = |inst: &Inst| [inst.read(Reg::S(EXEC)).unwrap(), inst.read(Reg::S(EXEC + 1)).unwrap()];
        for half in 0..2u8 {
            assert_eq!(tracer.saved_mask(save.write(Reg::S(2 + half)).unwrap(), &mut 64), Some(exec(wide)[half as usize]));
        }
        assert_eq!(tracer.wave64_write_covers_entry(exec(wide), wide), Some(true));
        assert_eq!(tracer.wave64_write_covers_entry(exec(narrow), wide), Some(true));
        assert_ne!(tracer.wave64_write_covers_entry(exec(wide), narrow), Some(true));
        assert_ne!(tracer.wave64_write_covers_entry([exec(narrow)[0], exec(wide)[1]], wide), Some(true));
    }

    #[test]
    fn masked_writes_union_all_reachable_structured_components() {
        let words = [
            0xbe84_0309,
            0xbe85_0380, // Table pointer in s[4:5].
            0xf408_0402,
            0xfa00_0000, // Load s[16:19] from table dword 0.
            0xe038_2028,
            0x8004_030f, // Load v[3:6] at record byte 40.
            0xbe9a_047e, // Save both halves of EXEC.
            0x7da4_1a80, // v_cmpx_eq_u32 exec, 0, v13
            0xe030_200c,
            0x8004_060f, // Overwrite selected v6 at byte 12.
            0xbefe_041a, // Restore both halves of EXEC.
            0x3424_0c85, // v_lshlrev_b32 v18, 5, v6
            0xbf81_0000,
        ];
        let prove = |words: &[u32], wave64| -> Option<Vec<u32>> {
            let code: Vec<_> = words.iter().flat_map(|x| x.to_le_bytes()).collect();
            let p = crate::ir::build(&code, wave64, 32).unwrap();
            let lanes = crate::structure::structure(&p).unwrap();
            let layout = [RootParam {
                root: RootType::Table,
                table_offset_dwords: 0,
                sgprs: 1,
                base_register: 0,
                user_data: 9,
                slot: 1,
            }];
            let meta = [UsedResource {
                root: RootType::Table,
                kind: ResourceType::Srv,
                table_offset_dwords: 0,
                lower_bound: 0,
                stride: 64,
                dwords: 4,
                user_data: 9,
                slot: 1,
            }];
            let resources = track(&p, &layout, HwStage::Ps, Some(&lanes)).unwrap();
            let t = Tracer::new(&p, &layout, HwStage::Ps, Some(&lanes)).unwrap();
            let shift = p
                .blocks
                .iter()
                .flat_map(|b| &b.insts)
                .find(|i| i.name() == "v_lshlrev_b32")
                .unwrap();
            let high = if wave64 { Some(shift.read(Reg::S(EXEC + 1))?) } else { None };
            let active = lanes.mask_bits(shift.read(Reg::S(EXEC))?, high)?;
            let mut out = Columns::default();
            t.selector_branches(
                shift.write(Reg::V(18))?,
                true,
                &vec![(active, true)],
                &resources,
                &meta,
                0,
                &mut out,
                &mut 128,
            )?;
            let mut fields: Vec<_> = out.coarse
                .iter()
                .map(|index| {
                    assert_eq!(index.word_step, 64);
                    index.word.bytes
                })
                .collect();
            fields.sort_unstable();
            Some(fields)
        };
        assert_eq!(prove(&words, true), Some(vec![12, 52]));
        let mut bad = words;
        bad[11] = 0x3424_0c84; // A different shift does not scale slots by 32.
        assert_eq!(prove(&bad, true), None);
        let mut bad = words;
        bad[8] = 0x7e0c_0303; // v_mov_b32 v6, v3 on a reachable masked arm.
        bad[9] = 0xbf80_0000;
        assert_eq!(prove(&bad, true), None);

        let mut wave32 = words;
        wave32[6] = 0xbe9a_037e; // Save EXEC_LO.
        wave32[10] = 0xbefe_031a; // Restore EXEC_LO.
        assert_eq!(prove(&wave32, false), Some(vec![12, 52]));
        wave32[8] = 0x7e0c_0280; // A reachable masked arm writes the default slot.
        wave32[9] = 0xbf80_0000;
        assert_eq!(prove(&wave32, false), Some(vec![52]));
        wave32[8] = 0x7e0c_0281; // Another literal is outside the column proof.
        assert_eq!(prove(&wave32, false), None);
    }

    #[test]
    fn packed_material_columns_require_the_bounded_parent_and_executed_loads() {
        let words = [
            0xbe84_0308, // Record table pointer.
            0xbe85_0380,
            0xf408_0402,
            0xfa00_05c0, // s[16:19] from record table dword 368.
            0xbe86_0309, // Parent table pointer.
            0xbe87_0380,
            0xf408_0503,
            0xfa00_0060, // s[20:23] from parent table dword 24.
            0xd548_000f,
            0x0221_1100, // v15 = v_bfe_u32(v0, 8, 8).
            0xe030_2008,
            0x8005_060f, // v6 = parent[v15].dword_at_8.
            0x2c0e_0c90, // v7 = v6 >> 16.
            0xe030_2024,
            0x8004_0607, // v6 = records[v7].dword_at_36.
            0x3424_0c85, // v18 = v6 << 5.
            0xbf81_0000,
        ];
        let prove = |words: &[u32], parent_stride| {
            let code: Vec<_> = words.iter().flat_map(|x| x.to_le_bytes()).collect();
            let p = crate::ir::build(&code, true, 32).unwrap();
            let lanes = crate::structure::structure(&p).unwrap();
            let layout = [0u8, 1].map(|slot| RootParam {
                root: RootType::Table,
                table_offset_dwords: 0,
                sgprs: 1,
                base_register: 0,
                user_data: 8 + slot,
                slot,
            });
            let used = [(0, 368, 64), (1, 24, parent_stride)].map(|(slot, table_offset_dwords, stride)| {
                UsedResource {
                    root: RootType::Table,
                    kind: ResourceType::Srv,
                    table_offset_dwords,
                    lower_bound: 0,
                    stride,
                    dwords: 4,
                    user_data: 8 + slot,
                    slot,
                }
            });
            let resources = track(&p, &layout, HwStage::Ps, Some(&lanes)).unwrap();
            let tracer = Tracer::new(&p, &layout, HwStage::Ps, Some(&lanes)).unwrap();
            let shift = p.blocks.last().unwrap().insts.iter().find(|inst| inst.name() == "v_lshlrev_b32")?;
            let active = lanes.mask_bits(shift.read(Reg::S(EXEC))?, shift.read(Reg::S(EXEC + 1)))?;
            let mut out = Columns { packed: Some(Vec::new()), ..Default::default() };
            tracer.selector_branches(shift.write(Reg::V(18))?, true, &vec![(active, true)],
                &resources, &used, 0, &mut out, &mut 128)?;
            out.packed
        };
        assert_eq!(prove(&words, 16), Some(vec![PackedMaterialColumn {
            parent: (1, 24),
            records: (0, 368),
            parent_count: 256,
            parent_stride: 16,
            packed_field: 8,
            shift: 16,
            index_mask: 0xffff,
            record_stride: 64,
            handle_field: 36,
            bytes: 0,
        }]));
        assert_eq!(prove(&words, 32), None);
        let mut wrong = words;
        wrong[9] = 0x0225_1100; // Nine bits do not fit the parent capture.
        assert_eq!(prove(&wrong, 16), None);
        let mut alternate = words.to_vec();
        alternate.splice(12..13, [
            0x360e_0cff, 0xffff, // v7 = v6 & 0xffff.
        ]);
        assert_eq!(prove(&alternate, 16).unwrap()[0].shift, 0);
    }

    #[test]
    fn scalar_range_columns_include_the_actual_zero_record_of_a_masked_address() {
        let track = |default: u32, sdwa: u32| {
            let words = [
                0xbe84_0308, 0xbe85_0380,
                0xf408_0402, 0xfa00_05c0, // s[16:19] from record table dword 368.
                0xbe86_0309, 0xbe87_0380,
                0xf408_0503, 0xfa00_0a00, // s[20:23] from base table dword 640.
                0xf420_008a, 0xfa00_03a8, // s2 from the base SRV's field at 936.
                0x7e60_0280 | default,    // Default address in v48.
                0xbe9a_047e,             // Save both halves of EXEC.
                0x7da4_1a80,             // v_cmpx_eq_u32 exec, 0, v13.
                0x4a60_6af9, sdwa,       // v48 = s2 + unsigned byte of v53.
                0xbf07_8002, 0x8582_807e, // Select the current EXEC when the base is nonzero.
                0xd501_0030, 0x000a_6080, // v48 selects zero or the computed record.
                0xbefe_041a,             // Restore both halves of EXEC.
                0xe030_2024, 0x8004_0630, // v6 = records[v48].dword_at_36.
                0x3424_0c85, 0xbf81_0000, // v18 = v6 << 5.
            ];
            let code: Vec<_> = words.iter().flat_map(|x| x.to_le_bytes()).collect();
            let p = crate::ir::build(&code, true, 64).unwrap();
            let lanes = crate::structure::structure(&p).unwrap();
            let layout = [0u8, 1].map(|slot| RootParam {
                root: RootType::Table, table_offset_dwords: 0, sgprs: 1, base_register: 0,
                user_data: 8 + slot, slot,
            });
            let used = [UsedResource {
                root: RootType::Table, kind: ResourceType::Srv, table_offset_dwords: 368,
                lower_bound: 0, stride: 64, dwords: 4, user_data: 8, slot: 0,
            }];
            let resources = track(&p, &layout, HwStage::Ps, Some(&lanes)).unwrap();
            let tracer = Tracer::new(&p, &layout, HwStage::Ps, Some(&lanes)).unwrap();
            let shift = p.blocks.last().unwrap().insts.iter().find(|inst| inst.name() == "v_lshlrev_b32")?;
            let active = lanes.mask_bits(shift.read(Reg::S(EXEC))?, shift.read(Reg::S(EXEC + 1)))?;
            let mut out = Columns { ranges: Some(Vec::new()), ..Default::default() };
            tracer.selector_branches(shift.write(Reg::V(18))?, true, &vec![(active, true)],
                &resources, &used, 0, &mut out, &mut 128)?;
            out.ranges
        };
        let [column] = track(0, 0x0386_0602).unwrap().try_into().unwrap();
        assert_eq!(column, ScalarRangeColumn {
            base: vec![ScalarWordOp::Constant(0), ScalarWordOp::Constant(936), ScalarWordOp::Add(0, 1),
                ScalarWordOp::Read { table: (1, 640), address: 2 }],
            records: (0, 368), count: 256, record_stride: 64, handle_field: 36, bytes: 0, zero_record: true,
        });
        assert_eq!(track(0, 0x0286_0602).unwrap(), [column]);
        assert!(track(0, 0x0b86_0602).is_none()); // Sign extension is not an unsigned byte range.
        assert!(track(1, 0x0386_0602).is_none()); // Only a zero record can accompany the byte range.
    }

    #[test]
    fn image_waterfall_proof_does_not_cover_a_scalar_descriptor_read() {
        let words = super::super::tests::WATERFALL_INDEXED_WAVE32;
        let accepts = |words: &[u32]| {
            let code: Vec<_> = words.iter().flat_map(|x| x.to_le_bytes()).collect();
            let p = crate::ir::build(&code, false, 256).unwrap();
            let lanes = crate::structure::structure(&p).unwrap();
            let layout = [(RootType::ResourceHeap, 0), (RootType::Table, 1)].map(|(root, slot)| RootParam {
                root,
                table_offset_dwords: 0,
                sgprs: 1,
                base_register: 0,
                user_data: slot.into(),
                slot,
            });
            let resources = track(&p, &layout, HwStage::Cs, Some(&lanes)).unwrap();
            let [load] = resources.waterfall.loads.as_slice() else { panic!("expected one waterfall"); };
            Tracer::new(&p, &layout, HwStage::Cs, Some(&lanes)).unwrap().image_only_in_waterfall(*load, &resources)
        };
        assert!(accepts(words));
        let mut mixed = words.to_vec();
        let retry = mixed.iter().position(|&w| w == 0xbe95_4715).unwrap();
        mixed[retry + 1] = 0xbf89_fff5; // The back edge still reaches the same instruction.
        mixed.insert(retry, 0xbe9c_0310); // s_mov_b32 s28, s16 reads a descriptor word.
        assert!(!accepts(&mixed));
    }

    #[test]
    fn bounded_record_loop_requires_the_unsigned_count_guard_and_sentinel() {
        let fixture = [
            0x7e16_02c1, // v_mov_b32 v11, -1
            0xbe89_0380, // s_mov_b32 s9, 0
            0xbf06_8007, // s_cmp_eq_u32 s7, 0
            0xbf85_0009, // skip loop when count is zero
            0xbe82_037e, // save exec
            0xbf82_0000, // jump to header
            0x8104_0609, // s4 = root first + i
            0x7e16_0204, // v11 = s4 in active lanes
            0xbf88_0003, // early exit
            0x8109_8109, // i += 1
            0xbf09_0709, // s_cmp_ge_u32 i, count
            0xbf84_fffa, // continue while i < count
            0xbefe_0302, // restore exec
            0x7daa_16c1, // v_cmpx_ne_u32 exec_lo, -1, v11
            0xe030_2050,
            0x8003_100b, // buffer_load_dword v16, v11 idxen offset:80
            0xbf81_0000,
        ];
        let prove = |words: &[u32]| {
            let code = words.iter().flat_map(|x| x.to_le_bytes()).collect::<Vec<_>>();
            let p = crate::ir::build(&code, false, 32).unwrap();
            let layout = [RootParam {
                root: RootType::Constants,
                table_offset_dwords: 0,
                sgprs: 2,
                base_register: 0,
                user_data: 6,
                slot: 2,
            }];
            let t = Tracer::new(&p, &layout, HwStage::Cs, None).unwrap();
            let load = p
                .blocks
                .iter()
                .flat_map(|b| &b.insts)
                .find(|i| i.name() == "buffer_load_dword")
                .unwrap();
            t.selected_records(load.read(Reg::V(11)).unwrap(), load.read(Reg::S(EXEC)).unwrap())
        };
        assert_eq!(prove(&fixture), Some(((2, 0), (2, 1))));
        for (word, bad) in [
            (2, 0xbf06_8107),
            (9, 0x8109_8209),
            (10, 0xbf08_0709),
            (13, 0x7daa_1680),
        ] {
            let mut changed = fixture;
            changed[word] = bad;
            assert_eq!(prove(&changed), None);
        }
    }

    #[test]
    fn typed_store_waterfall_reflects_only_the_masked_field_in_selected_records() {
        let words = [
            0xbe98_0301, // Table pointer in s[24:25].
            0xbe99_0380,
            0xbe9a_0300, // Heap pointer in s[26:27].
            0xbe9b_0380,
            0xf408_040c,
            0xfa00_0000, // Load the input view into s[16:19].
            0x7e16_02c1, // v11 = -1
            0xbe89_0380, // i = 0
            0xbf06_8007, // count == 0
            0xbf85_0009, // skip the loop
            0xbe82_037e, // save EXEC
            0xbf82_0000, // enter the loop
            0x8104_0609, // s4 = first + i
            0x7e16_0204, // v11 = s4 in active lanes
            0xbf88_0003, // exit if no lanes remain
            0x8109_8109, // i += 1
            0xbf09_0709, // i >= count
            0xbf84_fffa, // repeat while i < count
            0xbefe_0302, // restore EXEC
            0x7daa_16c1, // exclude lanes with v11 == -1
            0xbf88_0010, // skip the waterfall when no record was selected
            0xe034_2038,
            0x8004_100b, // load two dwords at record byte 56 into v[16:17]
            0x3624_22ff,
            0x00ff_ffff, // v18 = v17 & 0x00ffffff
            0x3424_2485, // v18 <<= 5
            0xbe97_037e, // save EXEC for restoration
            0xbe95_037e, // remaining lanes
            0x7e28_0512, // v_readfirstlane_b32 s20, v18
            0x7da4_2414, // select lanes with the same index
            0xf408_030d,
            0x2800_0000, // read the selected heap descriptor into s[12:15]
            0xe01c_2000,
            0x8003_0000, // typed four-channel store
            0xbe95_4715, // drop lanes already served
            0xbf89_fff8, // repeat for remaining lanes
            0xbefe_0317, // restore EXEC
            0xbf81_0000,
        ];
        let analyze = |words: &[u32]| {
            let code = words.iter().flat_map(|x| x.to_le_bytes()).collect::<Vec<_>>();
            let p = crate::ir::build(&code, false, 32).unwrap();
            let lanes = crate::structure::structure(&p).unwrap();
            let layout = [
                (RootType::ResourceHeap, 0, 0, 1),
                (RootType::Table, 1, 1, 1),
                (RootType::Constants, 2, 6, 2),
            ].map(|(root, slot, user_data, sgprs)| RootParam {
                root,
                slot,
                user_data,
                sgprs,
                base_register: 0,
                table_offset_dwords: 0,
            });
            let used = [UsedResource {
                root: RootType::Table,
                kind: ResourceType::Srv,
                table_offset_dwords: 0,
                lower_bound: 0,
                stride: 176,
                dwords: 4,
                user_data: 1,
                slot: 1,
            }];
            let resources = track_with_used(&p, &layout, HwStage::Cs, Some(&lanes), &used, &crate::native::StageIo::None).unwrap();
            let tracer = Tracer::new(&p, &layout, HwStage::Cs, Some(&lanes)).unwrap();
            let read = p.blocks.iter().flat_map(|block| &block.insts)
                .find(|inst| inst.name() == "buffer_load_dwordx2").unwrap();
            let selected = tracer.selected_records(read.read(Reg::V(11)).unwrap(), read.read(Reg::S(EXEC)).unwrap());
            let reachable_store = resources.waterfall.loads.iter().any(|&load| tracer.waterfall_shift(load).is_some());
            let indices = resources.buffer_uav_indices.into_iter().map(|(_, index)| index).collect::<Vec<_>>();
            (indices, selected, reachable_store)
        };
        let (indices, selected, reachable_store) = analyze(&words);
        assert_eq!(selected, Some(((2, 0), (2, 1))));
        assert!(reachable_store);
        assert_eq!(indices, [BufferIndex {
            table_slot: 1,
            table_dword: 0,
            word: HeapOffset { bytes: 60, constant: None },
            word_step: 0,
            bytes: 0,
            guard: None,
            records: Some(IndexRecords { first: (2, 0), count: (2, 1), stride: 176 }),
            value_mask: 0x00ff_ffff,
        }]);
        let mut wrong = words;
        wrong[24] = 0x0fff_ffff; // Not the required mask.
        assert!(analyze(&wrong).0.is_empty());

        // The counter's count-zero test can skip through VCC instead of SCC.
        // Empty EXEC can take the loop, but cannot select a lane's write index.
        let mut masked = words.to_vec();
        masked.splice(9..10, [
            0x8503_807e, // s3 = count == 0 ? EXEC : 0
            0x876a_037e, // VCC = EXEC & s3
            0xbf87_0009, // skip the loop when VCC is nonempty
        ]);
        let (masked_indices, scalar_bound, _) = analyze(&masked);
        assert_eq!(masked_indices, indices);
        assert_eq!(scalar_bound, None);
        masked[9] = 0x8503_7e80; // The zero-count arm has no active mask.
        assert!(analyze(&masked).0.is_empty());
        let mut wrong = words;
        wrong[16] = 0xbf08_0709; // The loop no longer proves i < count.
        assert!(analyze(&wrong).0.is_empty());
    }
}

impl Tracer<'_> {
    /// In a waterfall the first-lane read visits the active lanes of its
    /// preheader. Follow its exact EXEC mask. Phi edges and masked writes
    /// can yield several structured-buffer fields or the default slot.
    pub(super) fn structured_indices(
        &self,
        load: Site,
        resources: &Resources,
        used: &[UsedResource],
    ) -> Option<(Vec<BufferIndex>, Vec<PackedMaterialColumn>, Vec<ScalarRangeColumn>)> {
        if used.is_empty() {
            return None;
        }
        let lanes = self.lanes?;
        let offset = self.first_lane_offset(load)?;
        let reader = self.def(offset)?;
        let block = &self.p.blocks[load.0];
        if block.preds.len() != 2 || !block.preds.contains(&load.0) {
            return None;
        }
        let pre = block.preds.iter().position(|&b| b != load.0)?;
        let entry = |reg| {
            let (b, i) = self.phis[reader.read(Reg::S(reg))?.0 as usize]?;
            (b == load.0).then(|| block.phis[i].args[pre])
        };
        let low = entry(EXEC)?;
        let high = if self.p.wave64 { Some(entry(EXEC + 1)?) } else { None };
        let active = lanes.mask_bits(low, high)?;
        if !resources.waterfall.loads.contains(&load) {
            // The shader also reads image descriptor words as scalar numbers
            // (width and height). The retry instruction observes the selected
            // EXEC at the load even though SMEM has no explicit EXEC operand.
            // With an initially empty EXEC, readfirstlane could name lane 0.
            let retry = block.insts.len().checked_sub(2)?;
            if retry <= load.1
                || block.insts[retry].name() != "s_andn2_wrexec_b64"
                || block.insts[load.1..retry]
                    .iter()
                    .any(|i| i.write(Reg::S(EXEC)).is_some() || i.write(Reg::S(EXEC + 1)).is_some())
                || !self.nonempty_entry(block.preds[pre], [low, high?])
            {
                return None;
            }
            self.waterfall(load.0, &[retry], &[offset]).ok()?;
        }
        let known = vec![(lanes.reach[block.preds[pre]], true), (active, true)];
        let heap = resources.scalar.iter().find(|(s, _)| *s == load)?.1;
        if self.layout.get(heap.param)?.root != RootType::ResourceHeap {
            return None;
        }
        let shifted = self.first_lane_source(offset)?;
        let image = self.stage == HwStage::Ps && resources.waterfall.loads.contains(&load);
        let mut out = Columns {
            packed: image.then(Vec::new),
            ranges: image.then(Vec::new),
            ..Default::default()
        };
        let mut fuel = 128;
        self.selector_branches(
            shifted,
            true,
            &known,
            resources,
            used,
            heap.offset,
            &mut out,
            &mut fuel,
        )?;
        if out.coarse.is_empty() || !self.p.wave64 && !self.image_only_in_waterfall(load, resources) {
            return None;
        }
        let image_only = image && self.image_only_in_waterfall(load, resources);
        let packed = out.packed.filter(|_| image_only).unwrap_or_default();
        let ranges = out.ranges.filter(|_| image_only).unwrap_or_default();
        Some((out.coarse, packed, ranges))
    }

    /// The image proof only bounds lanes active at waterfall entry. A
    /// scalar descriptor use can run even when no lane is active.
    pub(super) fn image_only_in_waterfall(&self, load: Site, resources: &Resources) -> bool {
        self.descriptor_only_in_waterfall(load, resources, |inst| matches!(inst.fields, Fields::Image { .. }))
    }

    fn descriptor_only_in_waterfall(
        &self,
        load: Site,
        resources: &Resources,
        accepts: impl Fn(&Inst) -> bool,
    ) -> bool {
        let outputs = &self.inst(load).writes;
        let descriptor_word = |v| outputs.iter().any(|&(_, word)| word == v);
        for (b, block) in self.p.blocks.iter().enumerate() {
            if block.phis.iter().any(|phi| phi.args.iter().copied().any(descriptor_word)) {
                return false;
            }
            for (i, inst) in block.insts.iter().enumerate() {
                if !inst.reads.iter().any(|&(_, v)| descriptor_word(v)) {
                    continue;
                }
                if b != load.0 || !accepts(inst)
                    || !resources.descriptors.iter().any(|d| {
                        d.site == (b, i) && d.role == Role::Resource
                            && matches!(d.source, Source::Loaded { load: read, first: 0 } if read == load)
                    })
                {
                    return false;
                }
            }
        }
        true
    }

    fn nonempty_entry(&self, mut block: usize, pair: [Value; 2]) -> bool {
        let dom = self.dominators.get_or_init(|| {
            crate::ir::idoms(
                &self.p.blocks,
                &crate::ir::reverse_postorder(&self.p.blocks),
                &self.rpo,
            )
        });
        let mask = |v| self.saved_mask(v, &mut 64).unwrap_or(v);
        let pair = pair.map(mask);
        while block != 0 {
            let parent = dom[block];
            if parent == usize::MAX || parent == block {
                break;
            }
            let guard = &self.p.blocks[parent];
            if self.p.blocks[block].preds.as_slice() == [parent]
                && let Exit::Branch { taken, not_taken } = guard.exit
                && taken != not_taken
                && let Some(branch) = guard.insts.last()
                && ((branch.name() == "s_cbranch_execz" && not_taken == block)
                    || (branch.name() == "s_cbranch_execnz" && taken == block))
                && [EXEC, EXEC + 1].map(|r| branch.read(Reg::S(r)).map(mask)) == pair.map(Some)
            {
                return true;
            }
            block = parent;
        }
        false
    }

    fn selector_branches(
        &self,
        v: Value,
        needs_shift: bool,
        known: &Known,
        resources: &Resources,
        used: &[UsedResource],
        bias: u32,
        out: &mut Columns,
        fuel: &mut usize,
    ) -> Option<()> {
        self.selected_lane_defs(v, known, fuel, &mut |site, inst, component, here, fuel| {
            self.selector_definition(site, inst, component, needs_shift, here, resources, used, bias, out, fuel)
        })
    }

    pub(super) fn selected_lane_defs(
        &self,
        v: Value,
        known: &Known,
        fuel: &mut usize,
        visit: &mut impl FnMut(Site, &Inst, u8, &Known, &mut usize) -> Option<()>,
    ) -> Option<()> {
        *fuel = fuel.checked_sub(1)?;
        let lanes = self.lanes?;
        if let Some((b, i)) = self.phis[v.0 as usize] {
            let block = &self.p.blocks[b];
            if block.preds.iter().any(|&p| self.rpo[p] >= self.rpo[b]) {
                return None;
            }
            for (j, &p) in block.preds.iter().enumerate() {
                let mut edge = known.clone();
                edge.push((lanes.reach[p], true));
                if let LaneExit::Branch {
                    when,
                    taken,
                    not_taken,
                } = lanes.exits[p]
                    && taken != not_taken
                    && lanes.edge_fixes_lane_bit(p, taken == b)
                {
                    edge.push((when, taken == b));
                }
                if self.meet(lanes, &edge) {
                    self.selected_lane_defs(block.phis[i].args[j], &edge, fuel, visit)?;
                }
            }
            return Some(());
        }
        let site = self.defs[v.0 as usize]?;
        let inst = self.inst(site);
        let reg = inst.writes.iter().find(|(_, value)| *value == v)?.0;
        let Reg::V(component) = reg else {
            return None;
        };
        let mut here = known.clone();
        let low = inst.read(Reg::S(EXEC))?;
        let high = if self.p.wave64 { Some(inst.read(Reg::S(EXEC + 1))?) } else { None };
        let mask = lanes.mask_bits(low, high)?;
        let mut skipped = known.clone();
        skipped.push((mask, false));
        if self.meet(lanes, &skipped) {
            self.selected_lane_defs(inst.read(reg)?, &skipped, fuel, visit)?;
        }
        here.push((mask, true));
        if !self.meet(lanes, &here) {
            return Some(());
        }
        visit(site, inst, component, &here, fuel)
    }

    fn selector_definition(
        &self,
        site: Site,
        inst: &Inst,
        component: u8,
        needs_shift: bool,
        here: &Known,
        resources: &Resources,
        used: &[UsedResource],
        bias: u32,
        out: &mut Columns,
        fuel: &mut usize,
    ) -> Option<()> {
        if !self.p.wave64 && inst.name() == "v_mov_b32"
            && let [src] = plain(inst)?.src.as_slice()
            && self.term(inst, src)? == Term::Const(0)
        {
            // A reflected column scan includes its bias plus zero explicitly,
            // for both this default slot and an out-of-bounds buffer read.
            // If no column is reached, the caller keeps the access opaque.
            return Some(());
        }
        if needs_shift {
            if inst.name() != "v_lshlrev_b32" {
                return None;
            }
            let [bits, Operand::Vector { reg, .. }] = plain(inst)?.src.as_slice() else {
                return None;
            };
            if self.term(inst, bits)? != Term::Const(5) {
                return None;
            }
            return self.selector_branches(
                inst.read(Reg::V(*reg))?,
                false,
                here,
                resources,
                used,
                bias,
                out,
                fuel,
            );
        }
        let Fields::Buffer {
            data: Some(Operand::Vector { reg: dest, .. }),
            soffset,
            offset,
            offen: false,
            idxen: true,
            format: None,
            lds: false,
            tfe: false,
            ..
        } = &inst.fields
        else {
            return None;
        };
        let count = match inst.name() {
            "buffer_load_dword" => 1,
            "buffer_load_dwordx2" => 2,
            "buffer_load_dwordx3" => 3,
            "buffer_load_dwordx4" => 4,
            _ => return None,
        };
        let component = component.checked_sub(*dest)?;
        if component >= count || self.term(inst, soffset)? != Term::Const(0) {
            return None;
        }
        let (table_slot, table_dword) = self.buffer_index_table(site, resources)?;
        let field = u32::from(*offset) + 4 * u32::from(component);
        let stride = self.record_stride(table_slot, table_dword, field, used)?;
        let step = 1 << stride.trailing_zeros();
        let index = BufferIndex {
            table_slot,
            table_dword,
            word: HeapOffset {
                bytes: field % step,
                constant: None,
            },
            word_step: step,
            bytes: bias,
            guard: None,
            records: None,
            value_mask: u32::MAX,
        };
        if !out.coarse.contains(&index) {
            out.coarse.push(index);
        }
        if let Some(columns) = &mut out.packed {
            if let Some(column) = self.packed_material_column(inst, here, resources, used, index, field, stride) {
                if !columns.contains(&column) { columns.push(column); }
            } else {
                // We can replace the coarse scan only if every reachable
                // load has a bounded address chain.
                out.packed = None;
            }
        }
        if let Some(columns) = &mut out.ranges {
            if let Some(column) = self.scalar_range_column(inst, here, resources, index, field, stride) {
                if !columns.contains(&column) { columns.push(column); }
            } else {
                out.ranges = None;
            }
        }
        Some(())
    }

    pub(super) fn record_stride(&self, slot: u8, dword: u32, field: u32, used: &[UsedResource]) -> Option<u32> {
        let table = self
            .layout
            .iter()
            .find(|r| r.root == RootType::Table && r.slot == slot)?;
        let meta = used.iter().find(|u| {
            u.root == RootType::Table && u.user_data == table.user_data && u.table_offset_dwords == dword
        })?;
        (meta.kind == ResourceType::Srv
            && meta.stride.is_multiple_of(4)
            && field.is_multiple_of(4)
            && field.checked_add(4)? <= meta.stride)
            .then_some(meta.stride)
    }

    // A scalar move can restore a saved EXEC after divergent control flow.
    // saveexec's explicit SGPR destination receives the *previous* mask.
    fn saved_mask(&self, v: Value, remaining: &mut u8) -> Option<Value> {
        *remaining = remaining.checked_sub(1)?;
        if let Some((b, i)) = self.phis[v.0 as usize] {
            let args = &self.p.blocks[b].phis[i].args;
            let first = self.saved_mask(*args.first()?, remaining)?;
            return args[1..]
                .iter()
                .all(|&a| self.saved_mask(a, remaining) == Some(first))
                .then_some(first);
        }
        let Some(inst) = self.def(v) else { return Some(v) };
        if inst.name() == "s_mov_b64"
            && let Fields::Sop {
                src,
                dst: Some(Operand::Scalar { code: dest, .. }),
                ..
            } = &inst.fields
            && let [Operand::Scalar { code, .. }] = src.as_slice()
            && let Some(half) = (0..2).find(|k| inst.write(Reg::S(*dest + k)) == Some(v))
        {
            return self.saved_mask(inst.read(Reg::S(*code + half))?, remaining);
        }
        if inst.name() == "s_mov_b32"
            && let Fields::Sop { src, .. } = &inst.fields
            && let [Operand::Scalar { code, .. }] = src.as_slice()
        {
            return self.saved_mask(inst.read(Reg::S(*code))?, remaining);
        }
        if inst.name() == "s_and_saveexec_b32"
            && let Fields::Sop {
                dst: Some(Operand::Scalar { code, .. }),
                ..
            } = &inst.fields
            && *code != EXEC
            && inst.write(Reg::S(*code)) == Some(v)
        {
            return self.saved_mask(inst.read(Reg::S(EXEC))?, remaining);
        }
        if inst.name() == "s_and_saveexec_b64"
            && let Fields::Sop {
                dst: Some(Operand::Scalar { code, dwords: 2 }),
                ..
            } = &inst.fields
            && *code < EXEC - 1
            && let Some(half) = (0..2).find(|half| inst.write(Reg::S(*code + half)) == Some(v))
        {
            return self.saved_mask(inst.read(Reg::S(EXEC + half))?, remaining);
        }
        Some(v)
    }

    /// Prove inclusion in a single original Wave32 EXEC snapshot without
    /// interpreting the conditions that produced its component masks.
    fn resolve_exec_subset(&self, current: Value, producer: Value) -> bool {
        if self.p.wave64 { return false; }
        let i = producer.0 as usize;
        let definition = self.defs[i].or(self.phis[i]);
        if let Some((block, _)) = definition {
            // A loop may give the same SSA name a new dynamic mask.
            if self.reaches(block, block) { return false; }
        } else if self.inputs[i] != Some(Reg::S(EXEC)) {
            return false;
        }

        fn visit(t: &Tracer<'_>, v: Value, producer: Value, seen: &mut BTreeMap<Value, Option<bool>>) -> bool {
            if v == producer { return true; }
            if let Some(&known) = seen.get(&v) { return known.unwrap_or(false); }
            if seen.len() >= 128 { return false; }
            seen.insert(v, None);
            let result = if let Some((b, i)) = t.phis[v.0 as usize] {
                let args = &t.p.blocks[b].phis[i].args;
                !args.is_empty() && args.iter().all(|&arg| visit(t, arg, producer, seen))
            } else {
                t.def(v).is_some_and(|inst| {
                    let exec = Reg::S(EXEC);
                    let writes_exec = inst.write(exec) == Some(v);
                    let mut covered = |input: Option<Value>| input.is_some_and(|x| visit(t, x, producer, seen));
                    if writes_exec && inst.name().starts_with("v_cmpx_") {
                        return covered(inst.read(exec));
                    }
                    let Fields::Sop { src, dst: Some(Operand::Scalar { code, dwords: 1 }), .. } = &inst.fields else {
                        return false;
                    };
                    let writes_dst = inst.write(Reg::S(*code)) == Some(v);
                    let scalar = |op: &Operand| match *op {
                        Operand::Scalar { code, dwords: 1 } => inst.read(Reg::S(code)),
                        _ => None,
                    };
                    match (inst.name(), src.as_slice()) {
                        ("s_mov_b32", [a]) if writes_dst => covered(scalar(a)),
                        ("s_and_saveexec_b32", [a]) if *code != EXEC && (writes_dst || writes_exec) => {
                            covered(inst.read(exec)) || writes_exec && covered(scalar(a))
                        }
                        ("s_and_b32", [a, b]) if writes_dst => covered(scalar(a)) || covered(scalar(b)),
                        ("s_andn2_b32", [a, _]) if writes_dst => covered(scalar(a)),
                        ("s_or_b32", [a, b]) if writes_dst => covered(scalar(a)) && covered(scalar(b)),
                        _ => false,
                    }
                })
            };
            seen.insert(v, Some(result));
            result
        }

        visit(self, current, producer, &mut BTreeMap::new())
    }

    fn root_word(&self, v: Value) -> Option<(u8, u32)> {
        let (p, dword) = self.param_of(self.inputs[v.0 as usize]?)?;
        (self.layout[p].root == RootType::Constants).then_some((self.layout[p].slot, dword))
    }

    // i = phi(0, i+1); the only back edge tests i+1 < count. The unique
    // entry edge is guarded by count != 0, so every visit has i < count.
    fn counter_bound(&self, v: Value, lane: Option<Value>) -> Option<(u8, u32)> {
        self.root_word(self.counter_limit(v, false, lane)?)
    }

    pub(super) fn guarded_counter_limit(&self, v: Value, signed: bool) -> Option<Value> {
        self.counter_limit(v, signed, None)
    }

    fn counter_limit(&self, v: Value, signed: bool, lane: Option<Value>) -> Option<Value> {
        let (head, i) = self.phis[v.0 as usize]?;
        let block = &self.p.blocks[head];
        let phi = &block.phis[i];
        if phi.args.len() != 2 {
            return None;
        }
        let init = phi
            .args
            .iter()
            .position(|&a| matches!(self.origin(a), Term::Const(0)))?;
        let latch = block.preds[1 - init];
        let next = phi.args[1 - init];
        let (_, [Term::Value(old), Term::Const(1)] | [Term::Const(1), Term::Value(old)]) =
            self.sop2(next, &["s_add_i32", "s_add_u32"])?
        else {
            return None;
        };
        if old != v {
            return None;
        }
        let edge = &self.p.blocks[latch];
        if !matches!(edge.exit, Exit::Branch {taken, not_taken} if taken == head && not_taken != head) {
            return None;
        }
        let branch = edge.insts.last()?;
        if branch.name() != "s_cbranch_scc0" {
            return None;
        }
        let cmp = self.def(branch.read(Reg::Scc)?)?;
        let Fields::Sop { src, .. } = &cmp.fields else {
            return None;
        };
        if cmp.name() != if signed { "s_cmp_ge_i32" } else { "s_cmp_ge_u32" }
            || self.term(cmp, &src[0])? != Term::Value(next) {
            return None;
        }
        let Term::Value(count) = self.term(cmp, &src[1])? else {
            return None;
        };
        let mut entry = block.preds[init];
        // Walk straight-line preheaders, checking that the zero arm cannot
        // enter this loop without going back through the guarding branch.
        while let [parent] = self.p.blocks[entry].preds.as_slice() {
            if matches!(self.p.blocks[*parent].exit, Exit::Jump(dst) if dst == entry) {
                if self.rpo[*parent] >= self.rpo[entry] {
                    return None;
                }
                entry = *parent;
                continue;
            }
            let guard = &self.p.blocks[*parent];
            if !matches!(guard.exit, Exit::Branch { taken, not_taken } if not_taken == entry && taken != entry) {
                return None;
            }
            let skip = guard.insts.last()?;
            match skip.name() {
                "s_cbranch_scc1" => {
                    return self.counter_zero_test(skip.read(Reg::Scc)?, count, signed)?.then_some(count);
                }
                "s_cbranch_vccnz" if !signed => {
                    return self.lane_counter_zero_test(skip, count, lane?).then_some(count);
                }
                _ => return None,
            }
        }
        None
    }

    fn counter_zero_test(&self, scc: Value, count: Value, signed: bool) -> Option<bool> {
        let test = self.def(scc)?;
        let Fields::Sop { src, .. } = &test.fields else { return None };
        let [left, right] = src.as_slice() else { return None };
        let a = self.term(test, left)?;
        let b = self.term(test, right)?;
        Some(if signed {
            test.name() == "s_cmp_lt_i32" && (a, b) == (Term::Value(count), Term::Const(1))
        } else {
            test.name() == "s_cmp_eq_u32" && ((a, b) == (Term::Value(count), Term::Const(0))
                || (b, a) == (Term::Value(count), Term::Const(0)))
        })
    }

    /// A zero count skips the counter for any lane the later record selection
    /// could use. The other arm can still run scalar code with EXEC empty.
    fn lane_counter_zero_test(&self, branch: &Inst, count: Value, lane: Value) -> bool {
        let Some((_, [Term::Value(a), Term::Value(b)])) = branch.read(Reg::S(VCC))
            .and_then(|vcc| self.sop2(vcc, &["s_and_b32"]))
        else { return false };
        let covers = |choice, mask| {
            let (scc, Term::Value(when_zero), Term::Const(0)) = self.picked(choice)? else { return None };
            if !self.counter_zero_test(scc, count, false)? { return None; }
            let lanes = self.lanes?;
            let active = lanes.mask_bits(lane, None)?;
            let left = lanes.mask_bits(when_zero, None)?;
            let right = lanes.mask_bits(mask, None)?;
            (!self.meet(lanes, &[(active, true), (left, false)])
                && !self.meet(lanes, &[(active, true), (right, false)])).then_some(())
        };
        covers(a, b).or_else(|| covers(b, a)).is_some()
    }

    fn selected_records(&self, index: Value, mask: Value) -> Option<((u8, u32), (u8, u32))> {
        self.record_range(index, mask, false)
    }

    fn record_range(&self, index: Value, mask: Value, lane_guard: bool) -> Option<((u8, u32), (u8, u32))> {
        let test = self.def(self.saved_mask(mask, &mut 64)?)?;
        let [sentinel, Operand::Vector { reg, .. }] = plain(test)?.src.as_slice() else {
            return None;
        };
        if test.name() != "v_cmpx_ne_u32"
            || self.term(test, sentinel)? != Term::Const(u32::MAX)
            || test.read(Reg::V(*reg))? != index
        {
            return None;
        }
        let active = self.saved_mask(test.read(Reg::S(EXEC))?, &mut 64)?;
        let lane = lane_guard.then_some(active);
        let mut seen = std::collections::BTreeSet::new();
        let mut pending = vec![index];
        let mut bounds = None;
        while let Some(v) = pending.pop() {
            if !seen.insert(v) {
                continue;
            }
            if seen.len() > MAX_COPIES {
                return None;
            }
            if let Some((b, i)) = self.phis[v.0 as usize] {
                pending.extend(&self.p.blocks[b].phis[i].args);
                continue;
            }
            let mov = self.def(v)?;
            if mov.name() != "v_mov_b32" || mov.write(Reg::V(*reg)) != Some(v) {
                return None;
            }
            match self.term(mov, plain(mov)?.src.first()?)? {
                Term::Const(u32::MAX) => {}
                Term::Value(uniform) => {
                    let (_, [Term::Value(a), Term::Value(b)]) =
                        self.sop2(uniform, &["s_add_i32", "s_add_u32"])?
                    else {
                        return None;
                    };
                    let range = self
                        .root_word(a)
                        .zip(self.counter_bound(b, lane))
                        .or_else(|| self.root_word(b).zip(self.counter_bound(a, lane)))?;
                    if bounds.is_some_and(|old| old != range) {
                        return None;
                    }
                    bounds = Some(range);
                }
                _ => return None,
            }
            // Masked moves preserve the old VGPR in lanes they didn't cover.
            if self.saved_mask(mov.read(Reg::S(EXEC))?, &mut 64) != Some(active) {
                pending.push(mov.read(Reg::V(*reg))?);
            }
        }
        bounds
    }

    /// A typed store can only address slots named in the live input records.
    /// The waterfall's selected lanes must have executed the load and both
    /// integer operations. An inactive lane can retain older VGPR values.
    pub(super) fn typed_buffer_store_index(
        &self,
        load: Site,
        resources: &Resources,
        used: &[UsedResource],
    ) -> Option<BufferIndex> {
        if self.p.wave64 || !resources.waterfall.loads.contains(&load) {
            return None;
        }
        let lanes = self.lanes?;
        let (shift, input, [mask, _], preheader) = self.waterfall_shift(load)?;
        let masked = self.def(input)?;
        if masked.name() != "v_and_b32" {
            return None;
        }
        let [a, b] = plain(masked)?.src.as_slice() else { return None; };
        let (Operand::Vector { reg, .. }, constant) =
            (if matches!(a, Operand::Vector { .. }) { (a, b) } else { (b, a) })
        else { return None; };
        let Term::Const(value_mask @ 0x00ff_ffff) = self.term(masked, constant)? else { return None; };
        let word = masked.read(Reg::V(*reg))?;
        let read_site = self.defs[word.0 as usize]?;
        let read = self.inst(read_site);
        let Fields::Buffer {
            data: Some(Operand::Vector { reg: dest, .. }),
            addr: Operand::Vector { reg: idx, .. },
            soffset,
            offset,
            offen: false,
            idxen: true,
            format: None,
            lds: false,
            tfe: false,
            ..
        } = &read.fields else { return None; };
        let count = match read.name() {
            "buffer_load_dword" => 1,
            "buffer_load_dwordx2" => 2,
            "buffer_load_dwordx3" => 3,
            "buffer_load_dwordx4" => 4,
            _ => return None,
        };
        let component = reg.checked_sub(*dest)?;
        if component >= count || read.write(Reg::V(*reg)) != Some(word)
            || self.term(read, soffset)? != Term::Const(0)
        {
            return None;
        }
        let active = lanes.mask_bits(mask, None)?;
        for inst in [read, masked, shift] {
            let written = lanes.mask_bits(inst.read(Reg::S(EXEC))?, None)?;
            if self.meet(lanes, &[(lanes.reach[preheader], true), (active, true), (written, false)]) {
                return None;
            }
        }
        let (table_slot, table_dword) = self.buffer_index_table(read_site, resources)?;
        let field = u32::from(*offset) + 4 * u32::from(component);
        let stride = self.record_stride(table_slot, table_dword, field, used)?;
        let (first, count) = self.record_range(read.read(Reg::V(*idx))?, read.read(Reg::S(EXEC))?, true)?;
        let heap = resources.scalar.iter().find(|(s, _)| *s == load)?.1;
        if self.layout.get(heap.param)?.root != RootType::ResourceHeap
            || !self.descriptor_only_in_waterfall(load, resources, |inst|
                inst.name() == "buffer_store_format_xyzw")
        {
            return None;
        }
        Some(BufferIndex {
            table_slot,
            table_dword,
            word: HeapOffset { bytes: field, constant: None },
            word_step: 0,
            bytes: heap.offset,
            guard: None,
            records: Some(IndexRecords { first, count, stride }),
            value_mask,
        })
    }

    fn waterfall_shift(&self, load: Site) -> Option<(&Inst, Value, [Value; 2], usize)> {
        let offset = self.first_lane_offset(load)?;
        let shifted = self.first_lane_source(offset)?;
        let shift = self.def(shifted)?;
        let [bits, Operand::Vector { reg, .. }] = plain(shift)?.src.as_slice() else {
            return None;
        };
        if shift.name() != "v_lshlrev_b32" || self.term(shift, bits)? != Term::Const(5) {
            return None;
        }
        let reader = self.def(offset)?;
        let block = &self.p.blocks[load.0];
        if block.preds.len() != 2 || !block.preds.contains(&load.0) {
            return None;
        }
        let pre = block.preds.iter().position(|&b| b != load.0)?;
        let entry = |reg| {
            let (b, phi) = self.phis[reader.read(Reg::S(reg))?.0 as usize]?;
            (b == load.0).then(|| block.phis[phi].args[pre])
        };
        let low = entry(EXEC)?;
        let high = if self.p.wave64 { entry(EXEC + 1)? } else { low };
        Some((shift, shift.read(Reg::V(*reg))?, [low, high], block.preds[pre]))
    }

    /// Check the native EXEC values directly so a wave-wide scalar branch
    /// cannot introduce an unknown masked-off VGPR predecessor.
    fn wave64_write_covers_entry(&self, entry: [Value; 2], inst: &Inst) -> Option<bool> {
        let saved = |[lo, hi]: [Value; 2]| Some([self.saved_mask(lo, &mut 64)?, self.saved_mask(hi, &mut 64)?]);
        let read_exec = |inst: &Inst| Some([inst.read(Reg::S(EXEC))?, inst.read(Reg::S(EXEC + 1))?]);
        let written = read_exec(inst)?;
        if entry == written {
            return Some(true);
        }
        let entry = saved(entry)?;
        let written = saved(written)?;
        if entry == written {
            return Some(true);
        }
        let restrict = self.def(entry[0])?;
        if restrict.name() != "s_and_saveexec_b64"
            || [restrict.write(Reg::S(EXEC))?, restrict.write(Reg::S(EXEC + 1))?] != entry
        {
            return Some(false);
        }
        // The implicit result of AND_SAVEEXEC is a subset of its incoming EXEC.
        Some(saved(read_exec(restrict)?)? == written)
    }

    /// A split Wave64 can retain the original retry loop even when the
    /// per-lane analysis cannot eliminate it. Bound only its sampled slots;
    /// do not turn it into per-lane descriptor accesses.
    pub(super) fn native_retry_buffer_index(
        &self,
        load: Site,
        resources: &Resources,
        used: &[UsedResource],
    ) -> Option<BufferIndex> {
        if !self.p.wave64 || self.stage != HwStage::Cs || load.1 != 2 {
            return None;
        }
        let block = &self.p.blocks[load.0];
        if block.preds.len() != 2 || !matches!(block.exit, Exit::Branch { taken, not_taken }
            if taken == load.0 && not_taken != load.0)
        {
            return None;
        }
        let back = block.preds.iter().position(|&pred| pred == load.0)?;
        let [reader, select, descriptor, body @ .., retry, repeat] = block.insts.as_slice() else {
            return None;
        };
        let sample = match body {
            [sample] => sample,
            [setup, sample] if setup.name() == "v_mov_b32" => sample,
            _ => return None,
        };
        if reader.name() != "v_readfirstlane_b32" || select.name() != "v_cmpx_eq_u32"
            || descriptor.name() != "s_load_dwordx8" || !matches!(sample.name(), "image_sample" | "image_sample_l")
            || retry.name() != "s_andn2_wrexec_b64" || repeat.name() != "s_cbranch_execnz"
            || !std::ptr::eq(reader, self.def(self.first_lane_offset(load)?)?)
        {
            return None;
        }
        let Fields::Sop { dst: Some(Operand::Scalar { code, dwords: 2 }), src, .. } = &retry.fields else {
            return None;
        };
        if *code >= EXEC - 1 || !matches!(src.as_slice(), [Operand::Scalar { code: from, dwords: 2 }] if from == code) {
            return None;
        }
        let entry = |v: Value, reg, next| {
            let (b, i) = self.phis[v.0 as usize]?;
            if b != load.0 { return None; }
            let phi = &block.phis[i];
            (phi.reg == reg && phi.args[back] == next).then(|| phi.args[1 - back])
        };
        for half in 0..2 {
            let exec = Reg::S(EXEC + half);
            let remain = Reg::S(*code + half);
            let reader_exec = reader.read(exec)?;
            let after_retry = retry.write(exec)?;
            if select.read(exec)? != reader_exec || retry.read(exec)? != select.write(exec)?
                || repeat.read(exec)? != after_retry
            {
                return None;
            }
            // CMPX intersects EXEC. The only image still uses that mask.
            // WREXEC subtracts it from the saved remaining mask, which must
            // start equal to EXEC and take the same retry result thereafter.
            let start = entry(reader_exec, exec, after_retry)?;
            let saved = entry(retry.read(remain)?, remain, retry.write(remain)?)?;
            let save = self.def(saved)?;
            if save.name() != "s_mov_b64" || save.read(exec) != Some(start) || save.write(remain) != Some(saved) {
                return None;
            }
        }
        self.structured_buffer_index(load, resources, used)
    }

    pub(super) fn structured_buffer_index(
        &self,
        load: Site,
        resources: &Resources,
        used: &[UsedResource],
    ) -> Option<BufferIndex> {
        if used.is_empty() || self.p.wave64
            && (self.stage != HwStage::Cs || !self.image_only_in_waterfall(load, resources))
        {
            return None;
        }
        let (shift, input, entry, _) = self.waterfall_shift(load)?;
        let cvt = self.def(input).filter(|i| i.name() == "v_cvt_u32_f32");
        if self.p.wave64 && cvt.is_some() {
            return None;
        }
        let word = if let Some(cvt) = cvt {
            let [Operand::Vector { reg, .. }] = plain(cvt)?.src.as_slice() else {
                return None;
            };
            cvt.read(Reg::V(*reg))?
        } else {
            input
        };
        let read_site = self.defs[word.0 as usize]?;
        let read = self.inst(read_site);
        let Fields::Buffer {
            data: Some(Operand::Vector { reg: dest, .. }),
            addr: Operand::Vector { reg: idx, .. },
            soffset,
            offset: field,
            offen: false,
            idxen: true,
            format: None,
            lds: false,
            tfe: false,
            ..
        } = &read.fields
        else {
            return None;
        };
        if read.name() != "buffer_load_dword"
            || read.write(Reg::V(*dest)) != Some(word)
            || self.term(read, soffset)? != Term::Const(0)
        {
            return None;
        }
        let mask = if self.p.wave64 { entry[0] } else { self.saved_mask(entry[0], &mut 64)? };
        for inst in [Some(read), cvt, Some(shift)].into_iter().flatten() {
            let covered = if self.p.wave64 {
                self.wave64_write_covers_entry(entry, inst)?
            } else {
                self.saved_mask(inst.read(Reg::S(EXEC))?, &mut 64)? == mask
            };
            if !covered {
                return None;
            }
        }
        let (table_slot, table_dword) = self.buffer_index_table(read_site, resources)?;
        let stride = self.record_stride(table_slot, table_dword, u32::from(*field), used)?;
        let heap = resources.scalar.iter().find(|(site, _)| *site == load)?.1;
        if self.layout.get(heap.param)?.root != RootType::ResourceHeap {
            return None;
        }
        let (records, word_step, offset) = if cvt.is_some() {
            let (first, count) = self.selected_records(read.read(Reg::V(*idx))?, mask)?;
            (Some(IndexRecords { first, count, stride }), 0, u32::from(*field))
        } else {
            // The raw shader address is element*stride + field in wrapping
            // 32-bit arithmetic. Only its power-of-two divisor is guaranteed.
            let step = 1u32 << stride.trailing_zeros();
            (None, step, u32::from(*field) % step)
        };
        Some(BufferIndex {
            table_slot,
            table_dword,
            word: HeapOffset {
                bytes: offset,
                constant: None,
            },
            word_step,
            bytes: heap.offset,
            guard: None,
            records,
            value_mask: u32::MAX,
        })
    }

    /// The vector fallback loads one texture handle per hit record, then
    /// retries over the exact lanes sharing that handle. Numeric uses of
    /// the descriptor also require a nonempty native first iteration.
    pub(super) fn resolve_record_column(
        &self, load: Site, resources: &Resources, used: &[UsedResource], cs: &crate::native::ComputeIo,
    ) -> Option<(Site, FixedRecordColumn)> {
        if self.p.wave64 || cs.threads != [32, 1, 1] || load.1 != 2 {
            return None;
        }
        let block = &self.p.blocks[load.0];
        let [reader, select, descriptor, body @ .., retry, repeat] = block.insts.as_slice() else { return None; };
        if reader.name() != "v_readfirstlane_b32" || select.name() != "v_cmpx_eq_u32"
            || descriptor.name() != "s_load_dwordx8"
            || retry.name() != "s_andn2_wrexec_b32" || repeat.name() != "s_cbranch_execnz"
        {
            return None;
        }
        let image = |inst: &Inst, dim| matches!(inst.fields, Fields::Image { dim: d, .. } if d == dim);
        let (consumer, kind) = match body {
            [inst] if inst.name() == "image_load" && image(inst, 0) => ((load.0, 3), 1),
            [inst] if inst.name() == "image_sample_lz" && image(inst, 1) => ((load.0, 3), 2),
            [setup, inst] if setup.name() == "s_load_dwordx4" && inst.name() == "image_sample_lz" && image(inst, 1) => {
                ((load.0, 4), 2)
            }
            [inst] if self.resolve_dimensions_word(inst, descriptor) => (load, 1),
            _ => return None,
        };
        let (shift, word, [entry, _], preheader) = self.waterfall_shift(load)?;
        self.resolve_retry_lanes(load, reader, select, retry, repeat, entry)?;
        if consumer == load {
            // The descriptor calculation is scalar. Absent a nonempty first
            // iteration, readfirstlane may take an inactive lane's old word.
            let entry = self.saved_mask(entry, &mut 64)?;
            if !self.exec_is_nonempty((preheader, 0), entry, false) { return None; }
        } else {
            if self.inst(consumer).read(Reg::S(EXEC)) != select.write(Reg::S(EXEC))
                || !resources.descriptors.iter().any(|d| d.site == consumer && d.role == Role::Resource
                    && matches!(d.source, Source::Loaded { load: from, first: 0 } if from == load))
            {
                return None;
            }
        }
        let read_site = self.defs[word.0 as usize]?;
        let read = self.inst(read_site);
        let Fields::Buffer {
            data: Some(Operand::Vector { reg: dest, dwords: 1 }),
            addr: Operand::Vector { reg: idx, dwords: 1 }, soffset, offset,
            offen: false, idxen: true, format: None, lds: false, tfe: false, ..
        } = &read.fields else { return None; };
        let field = u32::from(*offset);
        if !matches!((field, kind), (60, 1) | (52 | 108, 2))
            || read.name() != "buffer_load_dword" || read.write(Reg::V(*dest)) != Some(word)
            || self.term(read, soffset)? != Term::Const(0) || self.reaches(load.0, read_site.0)
        {
            return None;
        }
        let exec = |inst: &Inst| self.saved_mask(inst.read(Reg::S(EXEC))?, &mut 64);
        let entry = self.saved_mask(entry, &mut 64)?;
        if exec(read)? != entry || exec(shift)? != entry {
            return None;
        }

        let index = read.read(Reg::V(*idx))?;
        let mask_site = self.defs[index.0 as usize]?;
        let mask = self.inst(mask_site);
        let [a, b] = plain(mask)?.src.as_slice() else { return None; };
        let producer_mask = mask.read(Reg::S(EXEC))?;
        if mask.name() != "v_and_b32"
            || !(self.raw(mask, a)? == Term::Const(0x3fff) || self.raw(mask, b)? == Term::Const(0x3fff))
            || mask_site.0 != read_site.0 && self.reaches(read_site.0, mask_site.0)
            || !(self.active_implies_exec(read_site, producer_mask)
                || self.resolve_exec_subset(read.read(Reg::S(EXEC))?, producer_mask))
        {
            return None;
        }
        let table = self.buffer_index_table(read_site, resources)?;
        let stride = self.record_stride(table.0, table.1, field, used)?;
        let heap = resources.scalar.iter().find(|(site, _)| *site == load)?.1;
        if table != (0, 288) || stride != 116 || heap.offset != 0
            || self.layout.get(heap.param)?.root != RootType::ResourceHeap
        {
            return None;
        }
        Some((consumer, FixedRecordColumn {
            table, field, stride, count: 16384, bytes: heap.offset, value_mask: u32::MAX,
        }))
    }

    fn resolve_retry_lanes(
        &self, load: Site, reader: &Inst, select: &Inst, retry: &Inst, repeat: &Inst, entry: Value,
    ) -> Option<()> {
        let block = &self.p.blocks[load.0];
        let Exit::Branch { taken, not_taken } = block.exit else { return None; };
        if taken != load.0 || not_taken == load.0 { return None; }
        let back = block.preds.iter().position(|&pred| pred == load.0)?;
        let exec = Reg::S(EXEC);
        let offset = self.first_lane_offset(load)?;
        let shifted = self.first_lane_source(offset)?;
        let [a, b] = plain(select)?.src.as_slice() else { return None; };
        let same = |sgpr, lane| self.term(select, sgpr) == Some(Term::Value(offset))
            && self.raw(select, lane) == Some(Term::Value(shifted));
        if reader.writes.first()?.1 != offset || !(same(a, b) || same(b, a)) {
            return None;
        }
        if select.read(exec)? != reader.read(exec)? || retry.read(exec)? != select.write(exec)?
            || repeat.read(exec)? != retry.write(exec)?
        {
            return None;
        }
        let Fields::Sop { dst: Some(Operand::Scalar { code, dwords: 1 }), src, .. } = &retry.fields else {
            return None;
        };
        if *code >= EXEC || !matches!(src.as_slice(), [Operand::Scalar { code: from, dwords: 1 }] if from == code) {
            return None;
        }
        let saved = Reg::S(*code);
        let prior = |value: Value, reg, next| {
            let (b, i) = self.phis[value.0 as usize]?;
            let p = self.p.blocks[b].phis.get(i)?;
            (b == load.0 && p.reg == reg && p.args.get(back) == Some(&next)).then(|| p.args[1 - back])
        };
        let first = prior(reader.read(exec)?, exec, retry.write(exec)?)?;
        let saved = prior(retry.read(saved)?, saved, retry.write(saved)?)?;
        (first == entry && self.saved_mask(saved, &mut 64)? == self.saved_mask(entry, &mut 64)?).then_some(())
    }

    fn resolve_dimensions_word(&self, inst: &Inst, descriptor: &Inst) -> bool {
        let Fields::Smem { data: Some(Operand::Scalar { code, dwords: 8 }), .. } = descriptor.fields else {
            return false;
        };
        let word = |operand, field| descriptor.write(Reg::S(code + field))
            .is_some_and(|value| self.term(inst, operand) == Some(Term::Value(value)));
        match inst.name() {
            "v_alignbit_b32" => {
                let Some([high, low, shift]) = plain(inst).map(|f| f.src.as_slice()) else { return false; };
                word(high, 2) && word(low, 1) && self.term(inst, shift) == Some(Term::Const(30))
            }
            "s_bfe_u32" => {
                let Fields::Sop { src, .. } = &inst.fields else { return false; };
                let [value, ctrl] = src.as_slice() else { return false; };
                word(value, 3) && self.term(inst, ctrl) == Some(Term::Const(0x0004_000c))
            }
            _ => false,
        }
    }
}
