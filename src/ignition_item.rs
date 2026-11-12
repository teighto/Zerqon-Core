use zerqon_inventory::{ComponentValue, ItemCatalog, ItemError, ItemStack, Slot, ValidatedPatch};

pub fn is_igniter(catalog: &ItemCatalog, stack: &ItemStack) -> bool {
    catalog.item(stack.item()).is_ok_and(|item| {
        matches!(
            item.key.as_str(),
            "minecraft:flint_and_steel" | "minecraft:fire_charge"
        )
    })
}

pub fn after_ignition(
    catalog: &ItemCatalog,
    stack: &ItemStack,
    creative: bool,
) -> Result<Slot, ItemError> {
    catalog.validate_stack(stack)?;
    if !is_igniter(catalog, stack) {
        return Err(ItemError::Invalid("item cannot ignite TNT"));
    }
    if creative {
        return Ok(Some(stack.clone()));
    }
    if catalog.item(stack.item())?.key == "minecraft:fire_charge" {
        return if stack.count() == 1 {
            Ok(None)
        } else {
            Ok(Some(stack.with_count(stack.count() - 1)?))
        };
    }
    wear_tool(catalog, stack)
}

pub fn wear_tool(catalog: &ItemCatalog, stack: &ItemStack) -> Result<Slot, ItemError> {
    let maximum = catalog.effective_component(stack, "minecraft:max_damage")?;
    let damage = catalog.effective_component(stack, "minecraft:damage")?;
    let unbreakable = catalog.effective_component(stack, "minecraft:unbreakable")?;
    let (Some(maximum), Some(damage), None) = (maximum, damage, unbreakable) else {
        return Ok(Some(stack.clone()));
    };
    let maximum = maximum
        .as_u64()
        .filter(|value| *value > 0)
        .ok_or(ItemError::Invalid("invalid item maximum damage"))?;
    let damage = damage
        .as_u64()
        .ok_or(ItemError::Invalid("invalid item damage"))?;
    let next = damage
        .checked_add(1)
        .ok_or(ItemError::Invalid("item damage overflow"))?;
    if next >= maximum {
        return Ok(None);
    }
    let id = catalog
        .component_id("minecraft:damage")
        .ok_or(ItemError::Invalid("missing item damage component"))?;
    let mut added = stack.patch().added().clone();
    let mut removed = stack.patch().removed().clone();
    removed.remove(&id);
    added.insert(
        id,
        ComponentValue::Damage(
            u32::try_from(next).map_err(|_| ItemError::Invalid("item damage overflow"))?,
        ),
    );
    let result = ItemStack::new(
        stack.item(),
        stack.count(),
        ValidatedPatch::new(added, removed, catalog)?,
    )?;
    catalog.validate_stack(&result)?;
    Ok(Some(result))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};
    use zerqon_inventory::{ComponentDef, ComponentId, ItemDef, ItemId};

    fn catalog() -> ItemCatalog {
        ItemCatalog::from_definitions(
            vec![
                ItemDef {
                    id: ItemId(1),
                    key: "minecraft:flint_and_steel".into(),
                    max_stack: 1,
                    max_damage: Some(64),
                },
                ItemDef {
                    id: ItemId(2),
                    key: "minecraft:fire_charge".into(),
                    max_stack: 64,
                    max_damage: None,
                },
            ],
            [
                "minecraft:max_damage",
                "minecraft:damage",
                "minecraft:unbreakable",
                "minecraft:custom_name",
            ]
            .into_iter()
            .enumerate()
            .map(|(id, key)| ComponentDef {
                id: ComponentId(id as u32),
                key: key.into(),
            })
            .collect(),
        )
        .unwrap()
    }

    fn tool(catalog: &ItemCatalog, damage: u32) -> ItemStack {
        ItemStack::new(
            ItemId(1),
            1,
            ValidatedPatch::new(
                BTreeMap::from([
                    (ComponentId(1), ComponentValue::Damage(damage)),
                    (ComponentId(3), ComponentValue::CustomName("teighto".into())),
                ]),
                BTreeSet::new(),
                catalog,
            )
            .unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn tool_wear_preserves_components_breaks_at_limit_and_honors_immunity() {
        let catalog = catalog();
        let fresh = tool(&catalog, 0);
        let used = after_ignition(&catalog, &fresh, false).unwrap().unwrap();
        assert_eq!(
            used.patch().added()[&ComponentId(1)],
            ComponentValue::Damage(1)
        );
        assert_eq!(
            used.patch().added()[&ComponentId(3)],
            ComponentValue::CustomName("teighto".into())
        );
        assert_eq!(
            after_ignition(&catalog, &fresh, true).unwrap(),
            Some(fresh.clone())
        );
        assert!(
            after_ignition(&catalog, &tool(&catalog, 63), false)
                .unwrap()
                .is_none()
        );
        let mut added = fresh.patch().added().clone();
        added.insert(ComponentId(2), ComponentValue::Unbreakable);
        let immune = ItemStack::new(
            ItemId(1),
            1,
            ValidatedPatch::new(added, BTreeSet::new(), &catalog).unwrap(),
        )
        .unwrap();
        assert_eq!(
            after_ignition(&catalog, &immune, false).unwrap(),
            Some(immune)
        );
        let no_durability = ItemStack::new(
            ItemId(1),
            1,
            ValidatedPatch::new(
                fresh.patch().added().clone(),
                BTreeSet::from([ComponentId(0)]),
                &catalog,
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            after_ignition(&catalog, &no_durability, false).unwrap(),
            Some(no_durability)
        );
    }

    #[test]
    fn charges_consume_one_unless_creative_and_damage_component_removal_disables_wear() {
        let catalog = catalog();
        let charges = ItemStack::new(ItemId(2), 2, ValidatedPatch::default()).unwrap();
        let remaining = after_ignition(&catalog, &charges, false).unwrap().unwrap();
        assert_eq!(remaining.count(), 1);
        assert!(
            after_ignition(&catalog, &remaining, false)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            after_ignition(&catalog, &charges, true).unwrap(),
            Some(charges)
        );
        let no_damage = ItemStack::new(
            ItemId(1),
            1,
            ValidatedPatch::new(BTreeMap::new(), BTreeSet::from([ComponentId(1)]), &catalog)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            after_ignition(&catalog, &no_damage, false).unwrap(),
            Some(no_damage)
        );
    }
}
