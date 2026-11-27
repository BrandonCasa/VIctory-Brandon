// XboxBvhBuilder's triangle kernel, mode 4 (shader 15b7932704e73dd6).
// All reads are supplied by the ordered GPU memory view at submission.
pub(crate) fn packed_index_extent(
    base: u64,
    indices: u64,
    mut read: impl FnMut(&[u64]) -> Option<Vec<u64>>,
) -> Option<u64> {
    if base == 0 || base >= 1 << 40 || base % 4 != 0 || indices == 0 || indices % 3 != 0 {
        return None;
    }
    let header = dwords(base, 2, &mut read)?;
    let meshlets = header[0] & 0x1ffff;
    let triangles = indices / 3;
    if triangles > u64::from(header[1]) {
        return None;
    }
    let face_table = base + 12 + u64::from(meshlets) * 16;
    if face_table + triangles * 4 > 1 << 40 || face_table - base + triangles * 4 > 1 << 32 {
        return None;
    }
    let remap_table = (face_table - base + u64::from(header[1]) * 4) & 0x7fff_ffff;
    // A triangle names one of 2048 meshlets and three local remap entries.
    let mut local_indices = vec![0u128; meshlets.min(2048) as usize];
    for first in (0..triangles).step_by(2048) {
        let count = (triangles - first).min(2048) as usize;
        for face in dwords(face_table + first * 4, count, &mut read)? {
            let slot = local_indices.get_mut((face >> 21) as usize)?;
            *slot |= (1u128 << (face & 127))
                | (1u128 << (face >> 7 & 127))
                | (1u128 << (face >> 14 & 127));
        }
    }
    // Batch the pair of dwords from each referenced 16-byte meshlet record.
    let meshlets: Vec<_> = local_indices
        .into_iter()
        .enumerate()
        .filter_map(|(i, local)| (local != 0).then_some((base + 12 + i as u64 * 16, local)))
        .collect();
    let mut addresses = Vec::new();
    for &(at, _) in &meshlets {
        addresses.push(at & !7);
        if at & 4 != 0 {
            addresses.push(at + 4);
        }
    }
    let data = read(&addresses)?;
    let mut data = data.into_iter();
    let mut end = face_table + triangles * 4;
    for (at, local) in meshlets {
        let pair = data.next()?;
        let (a, b) = if at & 4 == 0 {
            (pair as u32, (pair >> 32) as u32)
        } else {
            ((pair >> 32) as u32, data.next()? as u32)
        };
        let first_vertex = ((a << 2) & 0x10000) | (b & 0xffff);
        // The native shader clears bit 31 and the low two bits before its
        // 32-bit fetch, then extracts the selected 16-bit vertex index.
        let first = (remap_table + u64::from(first_vertex) * 2) & 0x7fff_ffff;
        let highest = 127 - local.leading_zeros();
        let offset = |i: u32| (first + u64::from(i) * 2) & 0x7fff_fffc;
        let mut last = offset(highest);
        if first + u64::from(highest) * 2 >= 1 << 31 {
            let mut remaining = local;
            while remaining != 0 {
                last = last.max(offset(remaining.trailing_zeros()));
                remaining &= remaining - 1;
            }
        }
        end = end.max(base + last + 4);
    }
    (end <= 1 << 40).then_some(end - base)
}

fn dwords(
    at: u64,
    count: usize,
    read: &mut impl FnMut(&[u64]) -> Option<Vec<u64>>,
) -> Option<Vec<u32>> {
    let first = at & !7;
    let end = at + count as u64 * 4;
    let addresses: Vec<_> = (first..end).step_by(8).collect();
    let values = read(&addresses)?;
    (0..count)
        .map(|i| {
            let offset = i + ((at - first) / 4) as usize;
            Some((values.get(offset / 2)? >> ((offset % 2) * 32)) as u32)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packed_remap_near_bit_31_counts_fetches_on_both_sides() {
        let mut data = [0u32; 8];
        data[0] = 1;
        data[1] = (0x7fff_fffcu32 - 28) / 4;
        data[7] = 3 << 14;
        let result = packed_index_extent(0x4000, 3, |addresses| {
            addresses
                .iter()
                .map(|&at| {
                    let i = ((at - 0x4000) / 4) as usize;
                    Some(u64::from(data[i]) | (u64::from(data[i + 1]) << 32))
                })
                .collect()
        });
        assert_eq!(result, Some(0x8000_0000));
    }

    #[test]
    fn format_53_follows_each_triangles_meshlet_and_last_remap_dword() {
        for base in [0x2000u64, 0x2004] {
            let mut bytes = [0u8; 80];
            let mut put = |off: usize, value: u32| {
                let i = (base as usize & 7) + off;
                bytes[i..i + 4].copy_from_slice(&value.to_le_bytes());
            };
            put(0, 2);
            put(4, 2);
            put(12, 1 << 14);
            put(16, 11);
            put(32, 25);
            put(44, 3 << 14);
            put(48, (1 << 21) | (6 << 7));
            let result = packed_index_extent(base, 6, |addresses| {
                addresses
                    .iter()
                    .map(|&at| {
                        let offset = (at - (base & !7)) as usize;
                        Some(u64::from_le_bytes(
                            bytes.get(offset..offset + 8)?.try_into().ok()?,
                        ))
                    })
                    .collect()
            });
            assert_eq!(result, Some(52 + (65536 + 11 + 3) * 2 + 4));
        }
    }
}
