use proptest::prelude::*;
use std::collections::{BTreeMap, BTreeSet};
use zerqon_inventory::*;

fn catalog() -> ItemCatalog {
    ItemCatalog::from_definitions(
        vec![
            ItemDef {
                id: ItemId(0),
                key: "minecraft:air".into(),
                max_stack: 64,
                max_damage: None,
            },
            ItemDef {
                id: ItemId(1),
                key: "minecraft:stone".into(),
                max_stack: 64,
                max_damage: None,
            },
        ],
        vec![
            ComponentDef {
                id: ComponentId(1),
                key: "minecraft:max_stack_size".into(),
            },
            ComponentDef {
                id: ComponentId(6),
                key: "minecraft:custom_name".into(),
            },
            ComponentDef {
                id: ComponentId(0),
                key: "minecraft:custom_data".into(),
            },
        ],
    )
    .unwrap()
}

#[test]
fn default_stacks_validate_count_and_unknown_identity() {
    let catalog = catalog();
    assert!(ItemStack::new(ItemId(1), 0, ValidatedPatch::default()).is_err());
    let valid = ItemStack::new(ItemId(1), 64, ValidatedPatch::default()).unwrap();
    catalog.validate_stack(&valid).unwrap();
    assert!(
        catalog
            .validate_stack(&valid.with_count(65).unwrap())
            .is_err()
    );
    assert!(
        catalog
            .validate_stack(&ItemStack::new(ItemId(999), 1, ValidatedPatch::default()).unwrap())
            .is_err()
    );
    assert!(
        catalog
            .validate_stack(&ItemStack::new(ItemId(0), 1, ValidatedPatch::default()).unwrap())
            .is_err()
    );
}

#[test]
fn actual_creative_bytes_decode_and_server_encoding_removes_component_delimiter() {
    let catalog = catalog();
    let wire = [0, 36, 1, 1, 1, 0, 6, 7, 8, 0, 4, b'S', b'a', b'f', b'e'];
    let packet = decode_creative_slot(&wire, &catalog, DecodeLimits::default()).unwrap();
    assert_eq!(packet.slot, 36);
    let stack = packet.stack.as_ref().unwrap();
    assert_eq!(
        stack.patch().added().get(&ComponentId(6)),
        Some(&ComponentValue::CustomName("Safe".into()))
    );
    let mut server = Vec::new();
    encode_server_stack(&packet.stack, &catalog, &mut server).unwrap();
    assert_eq!(server, [1, 1, 1, 0, 6, 8, 0, 4, b'S', b'a', b'f', b'e']);
}

#[test]
fn typed_max_stack_patch_changes_count_limit_and_unsupported_values_are_rejected() {
    let catalog = catalog();
    let patch = ValidatedPatch::new(
        BTreeMap::from([(ComponentId(1), ComponentValue::MaxStackSize(99))]),
        BTreeSet::new(),
        &catalog,
    )
    .unwrap();
    catalog
        .validate_stack(&ItemStack::new(ItemId(1), 99, patch).unwrap())
        .unwrap();
    assert!(
        ValidatedPatch::new(
            BTreeMap::from([(
                ComponentId(0),
                ComponentValue::CustomName("wrong-type".into())
            )]),
            BTreeSet::new(),
            &catalog
        )
        .is_err()
    );
    let unsupported = [0, 36, 1, 1, 1, 0, 0, 1, 0];
    assert!(matches!(
        decode_creative_slot(&unsupported, &catalog, DecodeLimits::default()),
        Err(ItemError::UnsupportedComponent(_))
    ));
}

#[test]
fn hashes_are_predictions_with_presence_and_fixed_width_values() {
    let catalog = catalog();
    let bytes = [
        0, 7, 0, 36, 0, 0, 1, 0, 36, 1, 1, 64, 1, 1, 0x12, 0x34, 0x56, 0x78, 1, 6, 0,
    ];
    let click = decode_click(&bytes, &catalog, DecodeLimits::default()).unwrap();
    assert_eq!(click.mode, ClickMode::Pickup);
    assert_eq!(click.carried, HashedStack::Empty);
    assert!(
        matches!(&click.changed_slots[0].1, HashedStack::Item { count: 64, added, .. } if added.get(&ComponentId(1)) == Some(&0x12345678))
    );
}

#[test]
fn removed_defaults_and_explicit_defaults_have_correct_stack_semantics() {
    let catalog = catalog();
    let ordinary = ItemStack::new(ItemId(1), 20, ValidatedPatch::default()).unwrap();
    let redundant = ValidatedPatch::new(
        BTreeMap::from([(ComponentId(1), ComponentValue::MaxStackSize(64))]),
        BTreeSet::new(),
        &catalog,
    )
    .unwrap();
    let redundant = ItemStack::new(ItemId(1), 40, redundant).unwrap();
    assert!(catalog.same_kind(&ordinary, &redundant).unwrap());
    let removed =
        ValidatedPatch::new(BTreeMap::new(), BTreeSet::from([ComponentId(1)]), &catalog).unwrap();
    let single = ItemStack::new(ItemId(1), 1, removed).unwrap();
    assert_eq!(catalog.effective_max_stack(&single).unwrap(), 1);
    assert!(!catalog.same_kind(&single, &ordinary).unwrap());
    assert!(
        catalog
            .validate_stack(&single.with_count(2).unwrap())
            .is_err()
    );
}

#[test]
fn malformed_or_unsupported_component_payloads_are_never_echoed() {
    let catalog = catalog();
    for bytes in [
        vec![0, 36, 1, 1, 1, 0, 6, 8, 8, 0, 4, b'S', b'a', b'f', b'e', 0],
        vec![0, 36, 1, 1, 1, 0, 6, 1, 10],
        vec![0, 36, 1, 1, 1, 0, 6, 4, 8, 0, 1, 0],
        vec![0, 36, 1, 1, 1, 0, 6, 6, 8, 0, 3, 0xed, 0xa0, 0x80],
        vec![0, 36, 1, 1, 2, 0, 1, 1, 64, 1, 1, 64],
        vec![0, 36, 1, 1, 1, 1, 1, 1, 64, 1],
        vec![0, 36, 1, 1, 0, 1, 0],
        vec![0, 36, 255, 255, 255, 255, 15],
    ] {
        assert!(
            decode_creative_slot(&bytes, &catalog, DecodeLimits::default()).is_err(),
            "{bytes:?}"
        );
    }
    let invalid = Some(ItemStack::new(ItemId(999), 1, ValidatedPatch::default()).unwrap());
    let mut output = vec![11, 22];
    assert!(encode_server_stack(&invalid, &catalog, &mut output).is_err());
    assert_eq!(output, [11, 22]);
}

#[test]
fn use_on_and_carried_packet_follow_the_official_fixture() {
    let body = hex("00000000bfffffd040013e8000003f0000003f400000000011");
    let request = decode_use_on(&body).unwrap();
    assert_eq!(request.block, BlockPosition { x: 2, y: 64, z: -3 });
    assert_eq!(request.hit, [0.25, 0.5, 0.75]);
    assert_eq!(request.face, Face::Up);
    assert_eq!(request.sequence, 17);
    assert_eq!(decode_carried_slot(&[0, 8]).unwrap(), 8);
    assert_eq!(encode_held_slot_body(8).unwrap(), [8]);
    assert!(decode_carried_slot(&[0, 9]).is_err());
    let mut nonfinite = body;
    nonfinite[10..14].copy_from_slice(&f32::NAN.to_be_bytes());
    assert!(decode_use_on(&nonfinite).is_err());
}

#[test]
fn catalog_loader_rejects_wrong_versions_paths_and_missing_or_oversized_defaults() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(
        directory
            .path()
            .join("generated/reports/minecraft/components/item"),
    )
    .unwrap();
    std::fs::write(
        directory.path().join("prepared.json"),
        r#"{"version":"26.3","protocol":777}"#,
    )
    .unwrap();
    let registries = directory.path().join("generated/reports/registries.json");
    std::fs::write(&registries, r#"{"minecraft:item":{"entries":{"minecraft:stone":{"protocol_id":1}}},"minecraft:data_component_type":{"entries":{"minecraft:max_stack_size":{"protocol_id":1}}}}"#).unwrap();
    let defaults = directory
        .path()
        .join("generated/reports/minecraft/components/item/stone.json");
    assert!(matches!(
        ItemCatalog::load(directory.path()),
        Err(ItemError::Io(_))
    ));
    std::fs::write(
        &defaults,
        r#"{"components":{"minecraft:max_stack_size":64}}"#,
    )
    .unwrap();
    assert_eq!(ItemCatalog::load(directory.path()).unwrap().item_count(), 1);
    std::fs::write(
        directory.path().join("prepared.json"),
        r#"{"version":"26.2","protocol":776}"#,
    )
    .unwrap();
    assert!(matches!(
        ItemCatalog::load(directory.path()),
        Err(ItemError::Invalid(_))
    ));
    std::fs::write(
        directory.path().join("prepared.json"),
        r#"{"version":"26.3","protocol":777}"#,
    )
    .unwrap();
    std::fs::write(&defaults, vec![b' '; 1_048_577]).unwrap();
    assert!(matches!(
        ItemCatalog::load(directory.path()),
        Err(ItemError::Limit(_))
    ));
    std::fs::write(&registries,r#"{"minecraft:item":{"entries":{"minecraft:../outside":{"protocol_id":1}}},"minecraft:data_component_type":{"entries":{}}}"#).unwrap();
    assert!(matches!(
        ItemCatalog::load(directory.path()),
        Err(ItemError::Invalid(_))
    ));
}

#[test]
fn decoder_budgets_are_checked_before_component_allocation() {
    let catalog = catalog();
    let valid = [0, 36, 1, 1, 1, 0, 6, 7, 8, 0, 4, b'S', b'a', b'f', b'e'];
    for limits in [
        DecodeLimits {
            max_packet_bytes: 4,
            ..DecodeLimits::default()
        },
        DecodeLimits {
            max_components: 0,
            ..DecodeLimits::default()
        },
        DecodeLimits {
            max_component_bytes: 6,
            ..DecodeLimits::default()
        },
        DecodeLimits {
            max_text_bytes: 3,
            ..DecodeLimits::default()
        },
    ] {
        assert!(decode_creative_slot(&valid, &catalog, limits).is_err());
    }
    assert!(
        decode_creative_slot(
            &[0, 36, 1, 1, 255, 255, 255, 255, 7],
            &catalog,
            DecodeLimits::default()
        )
        .is_err()
    );
}

#[test]
fn server_component_decoder_honors_the_same_payload_cap_as_client_decoder() {
    let catalog = catalog();
    let bytes = [1, 1, 1, 0, 6, 8, 0, 4, b'S', b'a', b'f', b'e'];
    assert!(matches!(
        decode_server_stack(
            &bytes,
            &catalog,
            DecodeLimits {
                max_component_bytes: 1,
                ..DecodeLimits::default()
            }
        ),
        Err(ItemError::Limit(_))
    ));
}

#[test]
fn catalog_namespace_cannot_escape_the_generated_reports_directory() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(directory.path().join("generated/reports")).unwrap();
    std::fs::create_dir_all(directory.path().join("generated/components/item")).unwrap();
    std::fs::write(
        directory.path().join("prepared.json"),
        r#"{"version":"26.3","protocol":777}"#,
    )
    .unwrap();
    std::fs::write(directory.path().join("generated/reports/registries.json"),r#"{"minecraft:item":{"entries":{"..:stone":{"protocol_id":1}}},"minecraft:data_component_type":{"entries":{"minecraft:max_stack_size":{"protocol_id":1}}}}"#).unwrap();
    std::fs::write(
        directory
            .path()
            .join("generated/components/item/stone.json"),
        r#"{"components":{"minecraft:max_stack_size":64}}"#,
    )
    .unwrap();
    assert!(matches!(
        ItemCatalog::load(directory.path()),
        Err(ItemError::Invalid(_))
    ));
}

fn hex(value: &str) -> Vec<u8> {
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|bytes| u8::from_str_radix(std::str::from_utf8(bytes).unwrap(), 16).unwrap())
        .collect()
}

proptest! {
    #[test]
    fn typed_unicode_stacks_roundtrip_in_both_distinct_wire_formats(count in 1_u8..=64, text in prop::collection::vec(any::<char>(),0..80)) {
        let catalog = catalog();
        let text: String = text.into_iter().collect();
        let patch = ValidatedPatch::new(BTreeMap::from([(ComponentId(6),ComponentValue::CustomName(text))]),BTreeSet::new(),&catalog).unwrap();
        let stack = Some(ItemStack::new(ItemId(1),count,patch).unwrap());
        let mut client = Vec::new(); encode_client_stack(&stack,&catalog,&mut client).unwrap();
        let mut server = Vec::new(); encode_server_stack(&stack,&catalog,&mut server).unwrap();
        prop_assert_eq!(decode_client_stack(&client,&catalog,DecodeLimits::default()).unwrap(),stack.clone());
        prop_assert_eq!(decode_server_stack(&server,&catalog,DecodeLimits::default()).unwrap(),stack.clone());
        let packet = encode_creative_slot_body(&CreativeSlotRequest {slot:36,stack},&catalog).unwrap();
        for end in 0..packet.len() { prop_assert!(decode_creative_slot(&packet[..end],&catalog,DecodeLimits::default()).is_err()); }
        let mut trailing = packet; trailing.push(0);
        prop_assert!(decode_creative_slot(&trailing,&catalog,DecodeLimits::default()).is_err());
    }

    #[test]
    fn arbitrary_bounded_inputs_never_panic_and_success_has_valid_authoritative_types(bytes in prop::collection::vec(any::<u8>(),0..512)) {
        let catalog = catalog();
        if let Ok(request) = decode_creative_slot(&bytes,&catalog,DecodeLimits::default()) {
            if let Some(stack) = &request.stack { prop_assert!(catalog.validate_stack(stack).is_ok()); }
            let canonical = encode_creative_slot_body(&request,&catalog).unwrap();
            prop_assert_eq!(decode_creative_slot(&canonical,&catalog,DecodeLimits::default()).unwrap(),request);
        }
        let _ = decode_click(&bytes,&catalog,DecodeLimits::default());
        let _ = decode_use_on(&bytes);
    }
}
