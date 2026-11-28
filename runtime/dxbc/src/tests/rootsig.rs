use super::*;

fn serialize(words: &[u32]) -> Vec<u8> {
    words.iter().flat_map(|value| value.to_le_bytes()).collect()
}

#[test]
fn texture_spans_resolve_append_and_keep_bindless_conservative() {
    let range = |range_type, num, offset_in_table| RootRange {
        range_type,
        num,
        offset_in_table,
        base_register: 0,
        space: 0,
    };
    let table = |ranges| RootParam {
        visibility: 0,
        kind: RootParamKind::Table(ranges),
    };
    let mut root = RootSig {
        version: 2,
        flags: 0,
        params: vec![
            table(vec![range(3, 1, 0)]),
            table(vec![range(2, 2, 4), range(0, 3, u32::MAX), range(1, 1, 16)]),
        ],
        num_static_samplers: 0,
        static_samplers: vec![],
    };
    assert_eq!(
        root.placed_texture_tables(),
        Some(vec![(1, 6..9), (1, 16..17)])
    );
    root.flags = 0x400;
    assert_eq!(root.placed_texture_tables(), None);
    root.flags = 0;
    root.params.push(table(vec![range(0, u32::MAX, 0)]));
    assert_eq!(root.placed_texture_tables(), None);
}

#[test]
fn resolves_offsets_and_preserves_register_spaces_and_sampler_bits() {
    // Parameter headers follow the static sampler. Descriptor rows and
    // constants appear in a different physical order from their headers.
    for version in [1, 2] {
        let mut data = vec![version, 3, 76, 1, 24, 0x41];
        let sampler = [17, 3, 2, 4, 0xff00_1122, 16, 8, 2, 0, 0x7f80_0000, 7, 5, 4];
        data.extend_from_slice(&sampler);
        data.extend([0, 5, 124, 4, 3, 132, 1, 0, 112]); // headers
        data.extend([9, 8, 6]); // constants
        let first_range = if version == 1 { 140 } else { 144 };
        data.extend([2, first_range]); // table
        data.extend([7, 11]); // root UAV
        if version == 2 {
            data.push(8);
        }
        data.extend([0, u32::MAX, 12, 15]);
        if version == 2 {
            data.push(2);
        }
        data.push(u32::MAX);
        data.extend([2, 7, 19, 23]);
        if version == 2 {
            data.push(4);
        }
        data.push(31);

        let root = parse_rts0(&serialize(&data)).unwrap();
        assert_eq!(
            (root.version, root.flags, root.num_static_samplers),
            (version, 0x41, 1)
        );
        assert_eq!(root.static_samplers, [sampler]);
        let [table, desc, constants] = root.params.as_slice() else {
            panic!("parameter count")
        };
        assert_eq!(table.visibility, 5);
        let RootParamKind::Table(ranges) = &table.kind else {
            panic!("descriptor table")
        };
        assert_eq!(ranges.len(), 2);
        assert_eq!(
            (ranges[0].num, ranges[0].offset_in_table),
            (u32::MAX, u32::MAX)
        );
        let range = &ranges[1];
        assert_eq!(
            (
                range.range_type,
                range.num,
                range.base_register,
                range.space,
                range.offset_in_table
            ),
            (2, 7, 19, 23, 31)
        );
        assert!(matches!(
            desc.kind,
            RootParamKind::Descriptor {
                kind: 4,
                register: 7,
                space: 11
            }
        ));
        assert!(matches!(
            constants.kind,
            RootParamKind::Constants {
                register: 9,
                space: 8,
                num32: 6
            }
        ));
    }
}

#[test]
fn counts_each_owned_copy_of_shared_ranges_toward_the_limit() {
    let mut words = vec![1, 2, 24, 1, 76, 0]; // one static sampler
    words.extend([0, 1, 48, 0, 5, 48]); // both headers point to the same table
    words.extend([1, 56, 0, 4, 7, 0, 0]); // one range at offset 56
    words.extend([0; 13]);
    let body = serialize(&words);
    let bytes = 2 * (std::mem::size_of::<RootParam>() + std::mem::size_of::<RootRange>())
        + std::mem::size_of::<[u32; 13]>();
    let root = parse_rts0_with_limit(&body, bytes).unwrap();
    assert_eq!(root.params.len(), 2);
    for param in root.params {
        let RootParamKind::Table(ranges) = param.kind else {
            panic!("expected descriptor table");
        };
        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0].base_register, 7);
    }
    assert!(parse_rts0_with_limit(&body, bytes - 1).is_err());
}

#[test]
fn rejects_invalid_offsets_counts_and_short_versioned_records() {
    let valid = [2, 1, 24, 0, 0, 0, 2, 5, 36, 4, 7, 2];
    let bytes = serialize(&valid);
    assert!(parse_rts0(&bytes).is_ok());
    for n in 0..bytes.len() {
        assert!(parse_rts0(&bytes[..n]).is_err(), "accepted prefix {n}");
    }
    for (index, bad) in [
        (0, 3),
        (1, u32::MAX),
        (2, 4),
        (2, u32::MAX),
        (3, u32::MAX),
        (4, u32::MAX),
        (6, 9),
        (8, 16),
        (8, 44),
    ] {
        let mut corrupt = valid;
        corrupt[index] = bad;
        assert!(parse_rts0(&serialize(&corrupt)).is_err(), "field {index}");
    }
    let incomplete_range = [2, 1, 24, 0, 0, 0, 0, 0, 36, 1, 44, 0, 1, 2, 3, 4];
    assert!(parse_rts0(&serialize(&incomplete_range)).is_err());
    assert!(
        parse_rts0(&serialize(&[1, 0, 0, 0, 0, 0]))
            .unwrap()
            .params
            .is_empty()
    );
}


#[test]
fn stride_table_variant_appends_volatile_srv_and_shifts_container() {
    // A minimal v1.1 root signature: one table parameter, one static sampler.
    let version = 2u32;
    let params_offset = 24u32;
    let table_offset = params_offset + 12; // after one header
    let ranges_offset = table_offset + 8;
    let samplers_start = ranges_offset + 24; // one v1.1 range (24 bytes)
    let sampler_size = 52usize;
    let mut body = Vec::new();
    body.extend_from_slice(&version.to_le_bytes());
    body.extend_from_slice(&1u32.to_le_bytes()); // num_params
    body.extend_from_slice(&params_offset.to_le_bytes());
    body.extend_from_slice(&1u32.to_le_bytes()); // num_static_samplers
    body.extend_from_slice(&samplers_start.to_le_bytes());
    body.extend_from_slice(&0x41u32.to_le_bytes()); // flags
    body.extend_from_slice(&0u32.to_le_bytes()); // tag: table
    body.extend_from_slice(&0u32.to_le_bytes()); // visibility
    body.extend_from_slice(&table_offset.to_le_bytes());
    body.extend_from_slice(&1u32.to_le_bytes()); // count
    body.extend_from_slice(&ranges_offset.to_le_bytes());
    // one v1.1 range: type, num, base, space, flags, offset_in_table
    body.extend_from_slice(&[0, 0, 0, 0]);
    body.extend_from_slice(&1u32.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes());
    body.extend_from_slice(&[0u8; 52]); // one zero sampler

    let rtso_tag = 32 + 8; // header (32) + two directory entries (2*4)
    let mut shader = Vec::new();
    shader.extend_from_slice(b"DXBC");
    shader.extend_from_slice(&[0u8; 16]); // digest
    shader.extend_from_slice(&1u32.to_le_bytes()); // container version
    shader.extend_from_slice(&0u32.to_le_bytes()); // total size placeholder
    shader.extend_from_slice(&2u32.to_le_bytes()); // two chunks
    shader.extend_from_slice(&(rtso_tag as u32).to_le_bytes()); // RTS0 directory entry
    shader.extend_from_slice(&((rtso_tag + 8 + body.len()) as u32).to_le_bytes()); // ACCC entry (placeholder)
    shader.extend_from_slice(b"RTS0");
    shader.extend_from_slice(&(body.len() as u32).to_le_bytes());
    shader.extend_from_slice(&body);
    shader.extend_from_slice(b"ACCC");
    shader.extend_from_slice(&24u32.to_le_bytes());
    shader.extend_from_slice(&[0u8; 24]);
    let second_tag = rtso_tag + 8 + body.len();
    // Patch the second directory entry (after the RTS0 tag) and the header's total size.
    shader[36..40].copy_from_slice(&(second_tag as u32).to_le_bytes());
    let total = shader.len();
    shader[24..28].copy_from_slice(&(total as u32).to_le_bytes());
    let original_size = shader.len();

    let variant = stride_table_variant(&shader).expect("variant builds");
    // One extra header (12) + one extra stride body (12 for v1.1).
    assert_eq!(variant.len(), original_size + 24);
    assert_eq!(u32::from_le_bytes(variant[24..28].try_into().unwrap()) as usize, variant.len());

    let container = crate::Dxbc::parse(&variant).expect("variant parses");
    let chunk = container.chunk(b"RTS0").expect("RTS0 survives");
    let body_bytes = &variant[chunk.offset..chunk.offset + chunk.size];
    let root = parse_rts0(body_bytes).expect("variant's RTS0 parses");
    assert_eq!(root.num_static_samplers, 1);
    assert_eq!(root.params.len(), 2);
    match &root.params[1].kind {
        RootParamKind::Descriptor { register, space, .. } => {
            assert_eq!(*register, 1);
            assert_eq!(*space, 32766);
        }
        other => panic!("last param is a descriptor, got {other:?}"),
    }
    match &root.params[0].kind {
        RootParamKind::Table(ranges) => assert_eq!(ranges.len(), 1),
        other => panic!("first param is a table, got {other:?}"),
    }
    let second = container.chunks.iter().find(|c| c.fourcc == *b"ACCC").unwrap();
    assert_eq!(second.offset, second_tag + 24 + 8); // tag+8, not tag
}
