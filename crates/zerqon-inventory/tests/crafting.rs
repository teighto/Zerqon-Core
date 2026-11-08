use proptest::prelude::*;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use zerqon_inventory::*;

fn catalog() -> ItemCatalog {
    ItemCatalog::from_definitions(
        [
            "air",
            "oak_planks",
            "birch_planks",
            "stick",
            "stone",
            "coal",
            "charcoal",
            "torch",
        ]
        .iter()
        .enumerate()
        .map(|(id, name)| ItemDef {
            id: ItemId(id as u32),
            key: format!("minecraft:{name}"),
            max_stack: 64,
            max_damage: None,
        })
        .collect(),
        vec![ComponentDef {
            id: ComponentId(0),
            key: "minecraft:custom_name".into(),
        }],
    )
    .unwrap()
}

#[test]
fn shaped_trims_empty_borders_and_mirrors_horizontally_but_not_vertically() {
    let catalog = catalog();
    let recipes = recipes(
        &catalog,
        json!({
            "type":"minecraft:crafting_shaped", "key":{"A":"minecraft:coal", "B":"minecraft:stick"},
            "pattern":["   ", " A ", "B  "], "result":{"id":"minecraft:torch", "count":4}
        }),
    );
    let mirrored = [
        stack(&catalog, "coal", 1),
        None,
        None,
        stack(&catalog, "stick", 1),
    ];
    assert!(recipes.find_match(&mirrored, &catalog).unwrap().is_some());
    let rotated = [
        stack(&catalog, "stick", 1),
        None,
        None,
        stack(&catalog, "coal", 1),
    ];
    assert!(recipes.find_match(&rotated, &catalog).unwrap().is_none());
}

#[test]
fn shaped_alternatives_accept_both_fuels_and_reject_extra_inputs() {
    let catalog = catalog();
    let recipes = recipes(
        &catalog,
        json!({
            "type":"minecraft:crafting_shaped", "key":{"#":"minecraft:stick", "X":["minecraft:coal","minecraft:charcoal"]},
            "pattern":["X", "#"], "result":{"id":"minecraft:torch", "count":4}
        }),
    );
    for fuel in ["coal", "charcoal"] {
        let mut grid = [
            stack(&catalog, fuel, 1),
            None,
            stack(&catalog, "stick", 1),
            None,
        ];
        assert_eq!(
            recipes
                .find_match(&grid, &catalog)
                .unwrap()
                .unwrap()
                .consumed_slots,
            [0, 2]
        );
        grid[3] = stack(&catalog, "stone", 1);
        assert!(recipes.find_match(&grid, &catalog).unwrap().is_none());
    }
}

#[test]
fn shapeless_backtracks_for_overlapping_alternatives() {
    let catalog = catalog();
    let recipes = recipes(
        &catalog,
        json!({
            "type":"minecraft:crafting_shapeless",
            "ingredients":[["minecraft:coal","minecraft:charcoal"],"minecraft:coal"],
            "result":{"id":"minecraft:torch", "count":4}
        }),
    );
    let grid = [
        stack(&catalog, "coal", 1),
        stack(&catalog, "charcoal", 1),
        None,
        None,
    ];
    assert_eq!(
        recipes
            .find_match(&grid, &catalog)
            .unwrap()
            .unwrap()
            .consumed_slots,
        [0, 1]
    );
}

#[test]
fn shapeless_repeated_ingredients_require_distinct_slots() {
    let catalog = catalog();
    let recipes = recipes(
        &catalog,
        json!({
            "type":"minecraft:crafting_shapeless", "ingredients":["#minecraft:planks","#minecraft:planks"],
            "result":{"id":"minecraft:stick", "count":4}
        }),
    );
    let grid = [stack(&catalog, "oak_planks", 64), None, None, None];
    assert!(recipes.find_match(&grid, &catalog).unwrap().is_none());
    let grid = [
        stack(&catalog, "oak_planks", 1),
        None,
        None,
        stack(&catalog, "oak_planks", 1),
    ];
    assert_eq!(
        recipes
            .find_match(&grid, &catalog)
            .unwrap()
            .unwrap()
            .consumed_slots,
        [0, 3]
    );
}

#[test]
fn ingredient_matching_uses_item_identity_and_does_not_copy_input_patch() {
    let catalog = catalog();
    let recipes = recipes(
        &catalog,
        json!({
            "type":"minecraft:crafting_shapeless", "ingredients":["minecraft:coal"],
            "result":{"id":"minecraft:torch", "count":4}
        }),
    );
    let patch = ValidatedPatch::new(
        BTreeMap::from([(ComponentId(0), ComponentValue::CustomName("Fuel".into()))]),
        BTreeSet::new(),
        &catalog,
    )
    .unwrap();
    let named = ItemStack::new(catalog.item_id("minecraft:coal").unwrap(), 1, patch).unwrap();
    let result = recipes
        .find_match(&[None, None, Some(named), None], &catalog)
        .unwrap()
        .unwrap();
    assert_eq!(result.output, stack(&catalog, "torch", 4).unwrap());
}

#[test]
fn shorthand_result_creates_one_plain_item_for_both_recipe_types() {
    let catalog = catalog();
    for recipe in [
        json!({"type":"minecraft:crafting_shapeless","ingredients":["minecraft:coal"],"result":"minecraft:torch"}),
        json!({"type":"minecraft:crafting_shaped","key":{"A":"minecraft:coal"},"pattern":["A"],"result":"minecraft:torch"}),
    ] {
        let book = recipes(&catalog, recipe);
        let matched = book
            .find_match(&[None, None, None, stack(&catalog, "coal", 1)], &catalog)
            .unwrap()
            .unwrap();
        assert_eq!(matched.output, stack(&catalog, "torch", 1).unwrap());
        assert_eq!(matched.consumed_slots, [3]);
    }
}

#[test]
fn null_optional_output_fields_use_the_original_defaults() {
    let catalog = catalog();
    for result in [
        json!({"id":"minecraft:torch","count":null}),
        json!({"id":"minecraft:torch","components":null}),
    ] {
        let book = recipes(
            &catalog,
            json!({"type":"minecraft:crafting_shapeless","ingredients":["minecraft:coal"],"result":result}),
        );
        assert_eq!(
            book.find_match(&[stack(&catalog, "coal", 1), None, None, None], &catalog)
                .unwrap()
                .unwrap()
                .output,
            stack(&catalog, "torch", 1).unwrap()
        );
    }
}

#[test]
fn shapeless_show_notification_false_preserves_ordinary_match() {
    let catalog = catalog();
    let book = recipes(
        &catalog,
        json!({"type":"minecraft:crafting_shapeless","ingredients":["minecraft:coal"],"result":{"id":"minecraft:torch"},"show_notification":false}),
    );
    assert!(book.report().excluded.is_empty());
    assert_eq!(
        book.find_match(&[stack(&catalog, "coal", 1), None, None, None], &catalog)
            .unwrap()
            .unwrap()
            .output,
        stack(&catalog, "torch", 1).unwrap()
    );
}

#[test]
fn malformed_known_metadata_is_rejected_for_both_recipe_types() {
    let catalog = catalog();
    for base in [
        json!({"type":"minecraft:crafting_shapeless","ingredients":["minecraft:coal"],"result":{"id":"minecraft:torch"}}),
        json!({"type":"minecraft:crafting_shaped","key":{"A":"minecraft:coal"},"pattern":["A"],"result":{"id":"minecraft:torch"}}),
    ] {
        for (field, value) in [
            ("group", json!({})),
            ("category", json!("bad")),
            ("show_notification", json!({})),
        ] {
            let mut recipe = base.clone();
            recipe[field] = value;
            assert!(
                CraftingRecipes::from_json(
                    &catalog,
                    &BTreeMap::from([("minecraft:test".into(), recipe.clone())]),
                    &BTreeMap::new()
                )
                .is_err(),
                "accepted {recipe}"
            );
        }
    }
}

#[test]
fn nested_tags_resolve_optional_missing_entries_without_relaxing_required_ones() {
    let catalog = catalog();
    let raw = BTreeMap::from([(
        "minecraft:test".into(),
        json!({
            "type":"minecraft:crafting_shapeless", "ingredients":["#minecraft:outer"],
            "result":{"id":"minecraft:stick"}
        }),
    )]);
    let mut tags = BTreeMap::from([
        (
            "minecraft:outer".into(),
            json!({"values":["#minecraft:inner",{"id":"minecraft:missing","required":false},{"id":"#minecraft:missing","required":false}]}),
        ),
        (
            "minecraft:inner".into(),
            json!({"values":["minecraft:oak_planks"]}),
        ),
    ]);
    let recipes = CraftingRecipes::from_json(&catalog, &raw, &tags).unwrap();
    assert!(
        recipes
            .find_match(
                &[None, None, None, stack(&catalog, "oak_planks", 1)],
                &catalog
            )
            .unwrap()
            .is_some()
    );
    tags.insert(
        "minecraft:inner".into(),
        json!({"values":["minecraft:missing"]}),
    );
    assert!(CraftingRecipes::from_json(&catalog, &raw, &tags).is_err());
}

#[test]
fn cyclic_tags_and_missing_recipe_tags_are_errors() {
    let catalog = catalog();
    let raw = BTreeMap::from([(
        "minecraft:test".into(),
        json!({
            "type":"minecraft:crafting_shapeless", "ingredients":["#minecraft:a"],"result":{"id":"minecraft:stick"}
        }),
    )]);
    for tags in [
        BTreeMap::new(),
        BTreeMap::from([
            ("minecraft:a".into(), json!({"values":["#minecraft:b"]})),
            ("minecraft:b".into(), json!({"values":["#minecraft:a"]})),
        ]),
    ] {
        assert!(CraftingRecipes::from_json(&catalog, &raw, &tags).is_err());
    }
}

#[test]
fn unsupported_types_component_results_and_large_recipes_never_match() {
    let catalog = catalog();
    for recipe in [
        json!({"type":"minecraft:crafting_special_repairitem", "ingredients":["minecraft:coal"], "result":{"id":"minecraft:torch"}}),
        json!({"type":"minecraft:crafting_shapeless", "ingredients":["minecraft:coal"], "result":{"id":"minecraft:torch", "components":{"minecraft:custom_name":"unsupported result patch"}}}),
        json!({"type":"minecraft:crafting_shaped", "key":{"A":"minecraft:coal"}, "pattern":["AAA"], "result":{"id":"minecraft:torch"}}),
        json!({"type":"minecraft:crafting_shapeless", "ingredients":["minecraft:coal","minecraft:coal","minecraft:coal","minecraft:coal","minecraft:coal"], "result":{"id":"minecraft:torch"}}),
    ] {
        let recipes = recipes(&catalog, recipe);
        assert_eq!(recipes.report().excluded.len(), 1);
        assert!(recipes.report().supported.is_empty());
        assert!(
            recipes
                .find_match(&[stack(&catalog, "coal", 5), None, None, None], &catalog)
                .unwrap()
                .is_none()
        );
    }
}

#[test]
fn excluded_recipes_report_the_specific_reason_and_supported_priority_is_stable() {
    let catalog = catalog();
    let ordinary = json!({"type":"minecraft:crafting_shapeless","ingredients":["minecraft:coal"],"result":{"id":"minecraft:torch"}});
    let raw = BTreeMap::from([
        ("minecraft:z".into(), ordinary.clone()),
        ("minecraft:a".into(), ordinary),
        (
            "minecraft:component".into(),
            json!({"type":"minecraft:crafting_shapeless","ingredients":["minecraft:coal"],"result":{"id":"minecraft:torch","components":{"minecraft:suspicious_stew_effects":[]}}}),
        ),
        (
            "minecraft:hook".into(),
            json!({"type":"minecraft:crafting_shapeless","ingredients":["minecraft:coal"],"result":{"id":"minecraft:torch"},"custom_hook":"unknown"}),
        ),
        (
            "minecraft:special".into(),
            json!({"type":"minecraft:crafting_transmute"}),
        ),
    ]);
    let recipes = CraftingRecipes::from_json(&catalog, &raw, &BTreeMap::new()).unwrap();
    assert_eq!(recipes.report().supported, ["minecraft:a", "minecraft:z"]);
    assert_eq!(
        recipes.report().excluded,
        vec![
            CraftingExclusion {
                recipe_id: "minecraft:component".into(),
                reason: CraftingExclusionReason::OutputComponents(vec![
                    "minecraft:suspicious_stew_effects".into()
                ])
            },
            CraftingExclusion {
                recipe_id: "minecraft:hook".into(),
                reason: CraftingExclusionReason::UnsupportedFields(vec!["custom_hook".into()])
            },
            CraftingExclusion {
                recipe_id: "minecraft:special".into(),
                reason: CraftingExclusionReason::UnsupportedType(
                    "minecraft:crafting_transmute".into()
                )
            },
        ]
    );
    assert_eq!(
        recipes
            .find_match(&[stack(&catalog, "coal", 1), None, None, None], &catalog)
            .unwrap()
            .unwrap()
            .recipe_id,
        "minecraft:a"
    );
}

#[test]
fn loader_rejects_unbounded_counts_depth_and_alternatives() {
    let catalog = catalog();
    let recipes = (0..8193)
        .map(|index| {
            (
                format!("minecraft:recipe_{index}"),
                json!({"type":"minecraft:smelting"}),
            )
        })
        .collect();
    assert!(matches!(
        CraftingRecipes::from_json(&catalog, &recipes, &BTreeMap::new()),
        Err(CraftingError::Data(ItemError::Limit(_)))
    ));
    let mut tags = BTreeMap::new();
    for index in 0..34 {
        tags.insert(
            format!("minecraft:tag_{index:02}"),
            json!({"values":[format!("#minecraft:tag_{:02}",index+1)]}),
        );
    }
    tags.insert(
        "minecraft:tag_34".into(),
        json!({"values":["minecraft:coal"]}),
    );
    assert!(matches!(
        CraftingRecipes::from_json(&catalog, &BTreeMap::new(), &tags),
        Err(CraftingError::Tag {
            source: ItemError::Limit(_),
            ..
        })
    ));
    let alternatives = vec!["minecraft:coal"; 65_537];
    let recipes = BTreeMap::from([(
        "minecraft:wide".into(),
        json!({"type":"minecraft:crafting_shapeless","ingredients":[alternatives],"result":{"id":"minecraft:torch"}}),
    )]);
    assert!(matches!(
        CraftingRecipes::from_json(&catalog, &recipes, &BTreeMap::new()),
        Err(CraftingError::Recipe {
            source: ItemError::Limit(_),
            ..
        })
    ));
}

#[test]
fn tag_depth_limit_is_independent_of_memoization_order() {
    let catalog = catalog();
    // Leaves sort first, so a recursion-stack-only limit sees depth one at each
    // root even though the final dependency chain is too deep.
    let mut tags = BTreeMap::from([(
        "minecraft:tag_00".into(),
        json!({"values":["minecraft:coal"]}),
    )]);
    for index in 1..34 {
        tags.insert(
            format!("minecraft:tag_{index:02}"),
            json!({"values":[format!("#minecraft:tag_{:02}",index-1)]}),
        );
    }
    assert!(matches!(
        CraftingRecipes::from_json(&catalog, &BTreeMap::new(), &tags),
        Err(CraftingError::Tag {
            source: ItemError::Limit(_),
            ..
        })
    ));
}

#[test]
fn item_tag_arrays_are_not_misread_as_alternatives_and_empty_tags_do_not_match() {
    let catalog = catalog();
    let mut recipe = json!({"type":"minecraft:crafting_shapeless","ingredients":[["#minecraft:empty","minecraft:coal"]],"result":{"id":"minecraft:torch"}});
    let tags = BTreeMap::from([("minecraft:empty".into(), json!({"values":[]}))]);
    assert!(
        CraftingRecipes::from_json(
            &catalog,
            &BTreeMap::from([("minecraft:test".into(), recipe.clone())]),
            &tags
        )
        .is_err()
    );
    recipe["ingredients"] = json!(["#minecraft:empty"]);
    let recipes = CraftingRecipes::from_json(
        &catalog,
        &BTreeMap::from([("minecraft:test".into(), recipe)]),
        &tags,
    )
    .unwrap();
    assert!(
        recipes
            .find_match(&[stack(&catalog, "coal", 1), None, None, None], &catalog)
            .unwrap()
            .is_none()
    );
    assert!(
        recipes
            .find_match(&[None, None, None, None], &catalog)
            .unwrap()
            .is_none()
    );
}

#[test]
fn catalog_item_hooks_exclude_bundle_output() {
    let directory = tempfile::tempdir().unwrap();
    let reports = directory.path().join("generated/reports");
    let defaults = reports.join("minecraft/components/item");
    std::fs::create_dir_all(&defaults).unwrap();
    std::fs::write(
        directory.path().join("prepared.json"),
        br#"{"version":"26.3","protocol":777}"#,
    )
    .unwrap();
    std::fs::write(reports.join("registries.json"),serde_json::to_vec(&json!({
        "minecraft:item":{"entries":{"minecraft:coal":{"protocol_id":1},"minecraft:bundle":{"protocol_id":2}}},
        "minecraft:data_component_type":{"entries":{"minecraft:max_stack_size":{"protocol_id":0},"minecraft:bundle_contents":{"protocol_id":1}}}
    })).unwrap()).unwrap();
    std::fs::write(
        defaults.join("coal.json"),
        br#"{"components":{"minecraft:max_stack_size":64}}"#,
    )
    .unwrap();
    std::fs::write(
        defaults.join("bundle.json"),
        br#"{"components":{"minecraft:max_stack_size":1,"minecraft:bundle_contents":[]}}"#,
    )
    .unwrap();
    let catalog = ItemCatalog::load(directory.path()).unwrap();
    for result in [json!({"id":"minecraft:bundle"}), json!("minecraft:bundle")] {
        let book = CraftingRecipes::from_json(&catalog,&BTreeMap::from([("minecraft:test".into(),json!({"type":"minecraft:crafting_shapeless","ingredients":["minecraft:coal"],"result":result}))]),&BTreeMap::new()).unwrap();
        assert_eq!(
            book.report().excluded[0].reason,
            CraftingExclusionReason::OutputItemHook
        );
        assert!(
            book.find_match(&[stack(&catalog, "coal", 1), None, None, None], &catalog)
                .unwrap()
                .is_none()
        );
    }
}

#[test]
fn invalid_recipe_outputs_and_shape_definitions_are_rejected() {
    let catalog = catalog();
    for recipe in [
        json!({"type":"minecraft:crafting_shapeless","ingredients":["minecraft:coal"],"result":{"id":"minecraft:torch","count":65}}),
        json!({"type":"minecraft:crafting_shapeless","ingredients":[],"result":{"id":"minecraft:torch"}}),
        json!({"type":"minecraft:crafting_shapeless","ingredients":[[]],"result":{"id":"minecraft:torch"}}),
        json!({"type":"minecraft:crafting_shapeless","ingredients":["minecraft:air"],"result":{"id":"minecraft:torch"}}),
        json!({"type":"minecraft:crafting_shapeless","ingredients":["minecraft:coal"],"result":{"id":"minecraft:missing"}}),
        json!({"type":"minecraft:crafting_shaped","key":{"A":"minecraft:coal"},"pattern":["A","AA"],"result":{"id":"minecraft:torch"}}),
        json!({"type":"minecraft:crafting_shaped","key":{"A":"minecraft:coal"},"pattern":["B"],"result":{"id":"minecraft:torch"}}),
        json!({"type":"minecraft:crafting_shaped","key":{"A":"minecraft:coal","B":"minecraft:stone"},"pattern":["A"],"result":{"id":"minecraft:torch"}}),
        json!({"type":"minecraft:crafting_shaped","key":{" ":"minecraft:coal"},"pattern":[" "],"result":{"id":"minecraft:torch"}}),
    ] {
        assert!(
            CraftingRecipes::from_json(
                &catalog,
                &BTreeMap::from([("minecraft:test".into(), recipe.clone())]),
                &BTreeMap::new()
            )
            .is_err(),
            "accepted {recipe}"
        );
    }
}

#[test]
fn invalid_input_stack_is_rejected_even_when_no_recipe_matches() {
    let catalog = catalog();
    let recipes = CraftingRecipes::from_json(&catalog, &BTreeMap::new(), &BTreeMap::new()).unwrap();
    assert!(
        recipes
            .find_match(&[stack(&catalog, "coal", 65), None, None, None], &catalog)
            .is_err()
    );
    assert!(
        recipes
            .find_match(
                &[
                    Some(ItemStack::new(ItemId(999), 1, ValidatedPatch::default()).unwrap()),
                    None,
                    None,
                    None
                ],
                &catalog
            )
            .is_err()
    );
}

proptest! {
    #[test]
    fn shapeless_matches_exactly_the_required_item_multiset(
        slots in prop::array::uniform4((0u8..5,1u8..=64)),
    ) {
        let catalog = catalog();
        let recipes = recipes(&catalog,json!({"type":"minecraft:crafting_shapeless", "ingredients":[["minecraft:coal","minecraft:charcoal"],"minecraft:coal","minecraft:stick"],"result":{"id":"minecraft:torch","count":4}}));
        let grid = slots.map(|(kind,count)|match kind {
            0 => None,
            1 => stack(&catalog,"coal",count),
            2 => stack(&catalog,"charcoal",count),
            3 => stack(&catalog,"stick",count),
            _ => stack(&catalog,"stone",count),
        });
        let frequency = |kind| slots.iter().filter(|(item,_)|*item==kind).count();
        let expected = frequency(0)==1 && frequency(3)==1 && frequency(1)>=1 && frequency(1)+frequency(2)==2;
        prop_assert_eq!(recipes.find_match(&grid,&catalog).unwrap().is_some(),expected);
    }

    #[test]
    fn shapeless_matching_is_invariant_under_permutation_and_stack_counts(
        counts in prop::array::uniform3(1u8..=64),
        order in prop::sample::select(vec![[0,1,2,3],[0,2,3,1],[1,3,0,2],[2,0,1,3],[3,2,1,0],[3,1,0,2]]),
    ) {
        let catalog = catalog();
        let recipes = recipes(&catalog,json!({"type":"minecraft:crafting_shapeless", "ingredients":[["minecraft:coal","minecraft:charcoal"],"minecraft:coal","minecraft:stick"],"result":{"id":"minecraft:torch","count":4}}));
        let original = [stack(&catalog,"coal",counts[0]),stack(&catalog,"charcoal",counts[1]),stack(&catalog,"stick",counts[2]),None];
        let grid = order.map(|index|original[index].clone());
        let before = grid.clone();
        let matched = recipes.find_match(&grid,&catalog).unwrap().unwrap();
        prop_assert_eq!(matched.output,stack(&catalog,"torch",4).unwrap());
        prop_assert_eq!(matched.consumed_slots.len(),3);
        prop_assert_eq!(matched.consumed_slots.iter().copied().collect::<BTreeSet<_>>().len(),3);
        let mut remaining = grid.clone();
        for slot in matched.consumed_slots {
            let input = remaining[slot].take().unwrap();
            remaining[slot] = (input.count() > 1).then(||input.with_count(input.count()-1).unwrap());
        }
        let count = |slots:&[Slot;4]|slots.iter().flatten().map(|stack|u16::from(stack.count())).sum::<u16>();
        prop_assert_eq!(count(&grid)-count(&remaining),3);
        prop_assert_eq!(grid,before);
    }
}

fn root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_owned()
}

fn prepared_definitions(directory: &std::path::Path) -> BTreeMap<String, Value> {
    fn visit(
        root: &std::path::Path,
        directory: &std::path::Path,
        definitions: &mut BTreeMap<String, Value>,
    ) {
        for entry in std::fs::read_dir(directory).unwrap() {
            let entry = entry.unwrap();
            let kind = entry.file_type().unwrap();
            let path = entry.path();
            if kind.is_dir() {
                visit(root, &path, definitions);
            } else if kind.is_file()
                && path
                    .extension()
                    .is_some_and(|extension| extension == "json")
            {
                let relative = path.strip_prefix(root).unwrap().with_extension("");
                let name = relative
                    .components()
                    .map(|part| part.as_os_str().to_str().unwrap())
                    .collect::<Vec<_>>()
                    .join("/");
                let value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
                assert!(
                    definitions
                        .insert(format!("minecraft:{name}"), value)
                        .is_none()
                );
            }
        }
    }
    let mut definitions = BTreeMap::new();
    visit(directory, directory, &mut definitions);
    definitions
}

#[test]
fn original_java_matching_and_assembly_oracle_agree_for_all_cases() {
    let oracle: Value =
        serde_json::from_str(include_str!("../../../tools/crafting-data/oracle.json")).unwrap();
    assert_eq!(oracle["minecraft"], "26.3");
    assert_eq!(oracle["protocol"], 777);
    let catalog = oracle_catalog(&oracle);
    let tags = oracle["tags"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(key, values)| (key.clone(), json!({"values":values})))
        .collect();
    for case in oracle["cases"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let id = case["recipe"].as_str().unwrap();
        let recipes = CraftingRecipes::from_json(
            &catalog,
            &BTreeMap::from([(id.into(), oracle["recipes"][id].clone())]),
            &tags,
        )
        .unwrap();
        assert_eq!(recipes.report().supported, [id], "{name}");
        let input = case["input"].as_array().unwrap();
        assert_eq!(input.len(), 4, "{name}");
        let grid: [Slot; 4] = std::array::from_fn(|index| oracle_stack(&input[index], &catalog));
        let before = grid.clone();
        let matched = recipes.find_match(&grid, &catalog).unwrap();
        assert_eq!(
            matched.is_some(),
            case["expected"]["matches"].as_bool().unwrap(),
            "{name}"
        );
        if let Some(matched) = matched {
            assert_eq!(
                Some(matched.output),
                oracle_stack(&case["expected"]["result"], &catalog),
                "{name}"
            );
            assert_eq!(
                matched.consumed_slots,
                grid.iter()
                    .enumerate()
                    .filter_map(|(index, slot)| slot.as_ref().map(|_| index))
                    .collect::<Vec<_>>(),
                "{name}"
            );
        }
        assert_eq!(grid, before, "{name}");
    }
}

#[test]
fn original_java_codec_oracle_agrees_for_metadata_and_result_forms() {
    let oracle: Value =
        serde_json::from_str(include_str!("../../../tools/crafting-data/oracle.json")).unwrap();
    let catalog = oracle_catalog(&oracle);
    let tags = oracle["tags"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(key, values)| (key.clone(), json!({"values":values})))
        .collect();
    let cases = oracle["codec_cases"].as_array().unwrap();
    assert!(!cases.is_empty());
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let book = CraftingRecipes::from_json(
            &catalog,
            &BTreeMap::from([("minecraft:codec_case".into(), case["recipe"].clone())]),
            &tags,
        );
        if !case["expected"]["accepted"].as_bool().unwrap() {
            assert!(
                book.is_err(),
                "accepted invalid original codec fixture {name}"
            );
            continue;
        }
        let book = book.unwrap_or_else(|error| panic!("{name}: {error}"));
        assert_eq!(book.report().supported, ["minecraft:codec_case"], "{name}");
        let input = case["input"].as_array().unwrap();
        assert_eq!(input.len(), 4, "{name}");
        let grid = std::array::from_fn(|index| oracle_stack(&input[index], &catalog));
        let before = grid.clone();
        let matched = book.find_match(&grid, &catalog).unwrap();
        assert_eq!(
            matched.is_some(),
            case["expected"]["matches"].as_bool().unwrap(),
            "{name}"
        );
        if let Some(matched) = matched {
            assert_eq!(
                Some(matched.output),
                oracle_stack(&case["expected"]["result"], &catalog),
                "{name}"
            );
        }
        assert_eq!(grid, before, "{name}");
    }
}

#[test]
fn original_java_float_counts_are_intentionally_rejected_by_rust() {
    let oracle: Value =
        serde_json::from_str(include_str!("../../../tools/crafting-data/oracle.json")).unwrap();
    let catalog = oracle_catalog(&oracle);
    let tags = oracle["tags"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(key, values)| (key.clone(), json!({"values":values})))
        .collect();
    let cases = oracle["strict_rejections"].as_array().unwrap();
    assert_eq!(cases.len(), 4);
    for case in cases {
        let name = case["name"].as_str().unwrap();
        assert_eq!(case["rust_reject"], true, "{name}");
        assert_eq!(case["expected"]["accepted"], true, "{name}");
        assert_eq!(case["expected"]["result"]["count"], 1, "{name}");
        let result = CraftingRecipes::from_json(
            &catalog,
            &BTreeMap::from([("minecraft:codec_case".into(), case["recipe"].clone())]),
            &tags,
        );
        assert!(
            matches!(
                result,
                Err(CraftingError::Recipe {
                    source: ItemError::Invalid(_),
                    ..
                })
            ),
            "expected intentional rejection of Java numeric coercion: {name}"
        );
    }
}

fn oracle_catalog(oracle: &Value) -> ItemCatalog {
    let items = oracle["items"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(key, value)| ItemDef {
            id: ItemId(u32::try_from(value["id"].as_u64().unwrap()).unwrap()),
            key: key.clone(),
            max_stack: u8::try_from(value["max_stack"].as_u64().unwrap()).unwrap(),
            max_damage: value["max_damage"]
                .as_u64()
                .map(|damage| u32::try_from(damage).unwrap()),
        })
        .collect();
    let components = oracle["components"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(key, value)| ComponentDef {
            id: ComponentId(u32::try_from(value.as_u64().unwrap()).unwrap()),
            key: key.clone(),
        })
        .collect();
    ItemCatalog::from_definitions(items, components).unwrap()
}

fn oracle_stack(value: &Value, catalog: &ItemCatalog) -> Slot {
    if value.is_null() {
        return None;
    }
    let components = value["components"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(key, value)| {
            // Only this component occurs in the original fixtures. Any new component
            // must be represented explicitly rather than discarded by the adapter.
            assert_eq!(key, "minecraft:custom_name");
            let name = value.as_str().or_else(|| value["text"].as_str()).unwrap();
            (
                catalog.component_id(key).unwrap(),
                ComponentValue::CustomName(name.into()),
            )
        })
        .collect();
    let patch = ValidatedPatch::new(components, BTreeSet::new(), catalog).unwrap();
    let stack = ItemStack::new(
        catalog.item_id(value["item"].as_str().unwrap()).unwrap(),
        u8::try_from(value["count"].as_u64().unwrap()).unwrap(),
        patch,
    )
    .unwrap();
    catalog.validate_stack(&stack).unwrap();
    Some(stack)
}

/// Uses official prepared data, including nested item-tag resource names.
#[test]
#[ignore = "requires prepared official Minecraft 26.3 recipes, item tags and item reports"]
fn original_builtin_corpus_loads_and_reports_all_ambiguous_grid_witnesses() {
    let root = root();
    let prepared = root.join(".cache/vanilla/26.3");
    let catalog = ItemCatalog::load(&prepared).unwrap();
    let data = prepared.join("data/minecraft");
    let raw = prepared_definitions(&data.join("recipe"));
    let tags = prepared_definitions(&data.join("tags/item"));
    let book = CraftingRecipes::from_json(&catalog, &raw, &tags).unwrap();
    let report = book.report();
    assert_eq!(raw.len(), 2042);
    assert_eq!(report.supported.len() + report.excluded.len(), raw.len());
    assert_eq!(
        report
            .excluded
            .iter()
            .filter(|entry| matches!(entry.reason, CraftingExclusionReason::OutputComponents(_)))
            .count(),
        17
    );
    assert!(
        report
            .excluded
            .iter()
            .any(|entry| entry.recipe_id == "minecraft:bundle"
                && entry.reason == CraftingExclusionReason::OutputItemHook)
    );
    let mut resolved = BTreeMap::new();
    for tag in tags.keys() {
        resolve_corpus_tag(tag, &tags, &mut resolved);
    }
    let mut domains = BTreeMap::<u8, Vec<(String, [BTreeSet<String>; 4])>>::new();
    for id in &report.supported {
        for domain in recipe_domains(&raw[id], &resolved) {
            let mask = domain.iter().enumerate().fold(0, |mask, (index, items)| {
                mask | (u8::from(!items.is_empty()) << index)
            });
            domains.entry(mask).or_default().push((id.clone(), domain));
        }
    }
    let mut witnesses = BTreeMap::new();
    for group in domains.values() {
        for (index, (left, one)) in group.iter().enumerate() {
            for (right, two) in &group[index + 1..] {
                if left == right || witnesses.contains_key(&(left.clone(), right.clone())) {
                    continue;
                }
                let mut grid: [Option<String>; 4] = std::array::from_fn(|_| None);
                let mut compatible = true;
                for slot in 0..4 {
                    if one[slot].is_empty() {
                        continue;
                    }
                    let Some(common) = one[slot].intersection(&two[slot]).next() else {
                        compatible = false;
                        break;
                    };
                    grid[slot] = Some(common.clone());
                }
                if !compatible {
                    continue;
                }
                let typed = grid.each_ref().map(|slot| {
                    slot.as_ref().map(|name| {
                        ItemStack::new(catalog.item_id(name).unwrap(), 1, ValidatedPatch::default())
                            .unwrap()
                    })
                });
                for recipe in [left, right] {
                    let single = CraftingRecipes::from_json(
                        &catalog,
                        &BTreeMap::from([(recipe.clone(), raw[recipe].clone())]),
                        &tags,
                    )
                    .unwrap();
                    assert!(
                        single.find_match(&typed, &catalog).unwrap().is_some(),
                        "invalid overlap witness {left} / {right}: {grid:?}"
                    );
                }
                witnesses.insert((left.clone(), right.clone()), grid);
            }
        }
    }
    let overlaps: Vec<_> = witnesses
        .into_iter()
        .map(|((left, right), grid)| json!({"recipes":[left,right],"input":grid}))
        .collect();
    let summary = json!({"supported":report.supported,"excluded":report.excluded.iter().map(|entry|json!({"recipe":entry.recipe_id,"reason":format!("{:?}",entry.reason)})).collect::<Vec<_>>(),"overlaps":overlaps});
    std::fs::create_dir_all(root.join(".local")).unwrap();
    std::fs::write(
        root.join(".local/crafting-corpus-report.json"),
        serde_json::to_vec_pretty(&summary).unwrap(),
    )
    .unwrap();
    eprintln!(
        "Original builtin corpus: {} supported, {} excluded, {} ambiguous recipe pairs",
        report.supported.len(),
        report.excluded.len(),
        overlaps.len()
    );
    assert!(
        overlaps.is_empty(),
        "ambiguous recipes require original Java verification; see .local/crafting-corpus-report.json"
    );
}

fn resolve_corpus_tag(
    id: &str,
    tags: &BTreeMap<String, Value>,
    resolved: &mut BTreeMap<String, BTreeSet<String>>,
) {
    if resolved.contains_key(id) {
        return;
    }
    let mut items = BTreeSet::new();
    for entry in tags[id]["values"].as_array().unwrap() {
        let name = entry
            .as_str()
            .unwrap_or_else(|| entry["id"].as_str().unwrap());
        if let Some(tag) = name.strip_prefix('#') {
            if entry["required"] == false && !tags.contains_key(tag) {
                continue;
            }
            resolve_corpus_tag(tag, tags, resolved);
            items.extend(resolved[tag].iter().cloned());
        } else {
            items.insert(name.into());
        }
    }
    resolved.insert(id.into(), items);
}

fn ingredient_domain(value: &Value, tags: &BTreeMap<String, BTreeSet<String>>) -> BTreeSet<String> {
    if let Some(name) = value.as_str() {
        if let Some(tag) = name.strip_prefix('#') {
            return tags[tag].clone();
        }
        return BTreeSet::from([name.into()]);
    }
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_str().unwrap().into())
        .collect()
}

fn recipe_domains(
    recipe: &Value,
    tags: &BTreeMap<String, BTreeSet<String>>,
) -> BTreeSet<[BTreeSet<String>; 4]> {
    let empty = || std::array::from_fn(|_| BTreeSet::new());
    let mut domains = BTreeSet::new();
    if recipe["type"] == "minecraft:crafting_shapeless" {
        let ingredients: Vec<_> = recipe["ingredients"]
            .as_array()
            .unwrap()
            .iter()
            .map(|ingredient| ingredient_domain(ingredient, tags))
            .collect();
        fn place(
            ingredients: &[BTreeSet<String>],
            slots: [BTreeSet<String>; 4],
            domains: &mut BTreeSet<[BTreeSet<String>; 4]>,
        ) {
            let Some((first, rest)) = ingredients.split_first() else {
                domains.insert(slots);
                return;
            };
            for index in 0..4 {
                if slots[index].is_empty() {
                    let mut candidate = slots.clone();
                    candidate[index] = first.clone();
                    place(rest, candidate, domains);
                }
            }
        }
        place(&ingredients, empty(), &mut domains);
    } else {
        let rows: Vec<Vec<char>> = recipe["pattern"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row.as_str().unwrap().chars().collect())
            .collect();
        let points: Vec<_> = rows
            .iter()
            .enumerate()
            .flat_map(|(y, row)| {
                row.iter()
                    .enumerate()
                    .filter_map(move |(x, symbol)| (*symbol != ' ').then_some((x, y, *symbol)))
            })
            .collect();
        let min_x = points.iter().map(|point| point.0).min().unwrap();
        let min_y = points.iter().map(|point| point.1).min().unwrap();
        let width = points.iter().map(|point| point.0).max().unwrap() - min_x + 1;
        let height = points.iter().map(|point| point.1).max().unwrap() - min_y + 1;
        for offset_y in 0..=2 - height {
            for offset_x in 0..=2 - width {
                for mirror in [false, true] {
                    let mut slots = empty();
                    for (x, y, symbol) in &points {
                        let x = x - min_x;
                        let x = if mirror { width - 1 - x } else { x };
                        let slot = (offset_y + y - min_y) * 2 + offset_x + x;
                        slots[slot] = ingredient_domain(&recipe["key"][symbol.to_string()], tags);
                    }
                    domains.insert(slots);
                }
            }
        }
    }
    domains
}

fn stack(catalog: &ItemCatalog, name: &str, count: u8) -> Slot {
    Some(
        ItemStack::new(
            catalog.item_id(&format!("minecraft:{name}")).unwrap(),
            count,
            ValidatedPatch::default(),
        )
        .unwrap(),
    )
}

fn recipes(catalog: &ItemCatalog, value: Value) -> CraftingRecipes {
    CraftingRecipes::from_json(
        catalog,
        &BTreeMap::from([("minecraft:test".into(), value)]),
        &BTreeMap::from([(
            "minecraft:planks".into(),
            json!({"values":["minecraft:oak_planks", "minecraft:birch_planks"]}),
        )]),
    )
    .unwrap()
}

#[test]
fn shaped_tag_recipe_matches_right_offset_without_mutating_inputs() {
    let catalog = catalog();
    let recipes = recipes(
        &catalog,
        json!({
            "type":"minecraft:crafting_shaped", "key":{"#":"#minecraft:planks"},
            "pattern":["#", "#"], "result":{"id":"minecraft:stick", "count":4}
        }),
    );
    let grid = [
        None,
        stack(&catalog, "oak_planks", 5),
        None,
        stack(&catalog, "birch_planks", 9),
    ];
    let before = grid.clone();
    let matched = recipes
        .find_match(&grid, &catalog)
        .unwrap()
        .expect("stick recipe must match");
    assert_eq!(matched.output, stack(&catalog, "stick", 4).unwrap());
    assert_eq!(matched.consumed_slots, vec![1, 3]);
    assert_eq!(grid, before);
}
