use crate::player_store::{StoreError, StoredStack, decode_stack, encode_stack, serialize_payload};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use uuid::Uuid;
use zerqon_inventory::{ItemCatalog, ItemStack};
use zerqon_world::{ChunkKey, DropData, DropKey};

// Matches the opaque drop limit in zerqon-world's transaction domain.
const MAX_DROP_RECORD_BYTES: usize = 2 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum DropRecordError {
    #[error("invalid item-drop record: {0}")]
    Invalid(&'static str),
    #[error("item-drop record exceeds 2 MiB")]
    Limit,
    #[error(transparent)]
    Stack(StoreError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

impl From<StoreError> for DropRecordError {
    fn from(error: StoreError) -> Self {
        match error {
            StoreError::Limit => Self::Limit,
            error => Self::Stack(error),
        }
    }
}

/// Semantic after-image for one persistent dropped item. Runtime entity IDs,
/// random sampling and simulation work budgets are deliberately separate.
#[derive(Clone, Debug, PartialEq)]
pub struct DropRecord {
    pub stack: ItemStack,
    pub position: [f64; 3],
    pub velocity: [f64; 3],
    /// Original base Entity rotation: yaw then pitch, in degrees.
    pub rotation: [f32; 2],
    // Original ItemEntity and base Entity persistence uses signed NBT shorts.
    // Negative values and the lifetime/pickup sentinels must survive restart.
    pub age: i16,
    pub pickup_delay: i16,
    pub health: i16,
    pub fire_ticks: i16,
    pub no_gravity: bool,
    pub on_ground: bool,
    pub invulnerable: bool,
    pub silent: bool,
    /// Attribution only; this does not restrict who may pick the item up.
    pub thrower: Option<Uuid>,
    /// Original ItemEntity Owner/target, independently restricting pickup.
    pub owner: Option<Uuid>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredDropRecord {
    schema: u32,
    minecraft_version: String,
    protocol: u32,
    world: Uuid,
    drop: Uuid,
    stack: StoredStack,
    position_bits: [u64; 3],
    velocity_bits: [u64; 3],
    rotation_bits: [u32; 2],
    age: i16,
    pickup_delay: i16,
    health: i16,
    fire_ticks: i16,
    no_gravity: bool,
    on_ground: bool,
    invulnerable: bool,
    silent: bool,
    thrower: Option<Uuid>,
    owner: Option<Uuid>,
}

impl DropRecord {
    /// The caller chooses position, motion and rotation once per operation.
    /// Q/loot-specific delays and attribution are set before encoding.
    pub fn new(
        stack: ItemStack,
        position: [f64; 3],
        velocity: [f64; 3],
        rotation: [f32; 2],
    ) -> Self {
        Self {
            stack,
            position,
            velocity,
            rotation,
            age: 0,
            pickup_delay: 0,
            health: 5,
            fire_ticks: 0,
            no_gravity: false,
            on_ground: false,
            invulnerable: false,
            silent: false,
            thrower: None,
            owner: None,
        }
    }

    pub fn encode(
        &self,
        world: Uuid,
        key: DropKey,
        catalog: &ItemCatalog,
    ) -> Result<DropData, DropRecordError> {
        validate_identity(world, key)?;
        let chunk = self.chunk()?;
        let stack = encode_stack(&Some(self.stack.clone()), catalog)?
            .ok_or(DropRecordError::Invalid("drop stack must be nonempty"))?;
        let stored = StoredDropRecord {
            schema: 1,
            minecraft_version: "26.3".into(),
            protocol: 777,
            world,
            drop: key.uuid,
            stack,
            position_bits: self.position.map(f64::to_bits),
            velocity_bits: self.velocity.map(f64::to_bits),
            rotation_bits: self.rotation.map(f32::to_bits),
            age: self.age,
            pickup_delay: self.pickup_delay,
            health: self.health,
            fire_ticks: self.fire_ticks,
            no_gravity: self.no_gravity,
            on_ground: self.on_ground,
            invulnerable: self.invulnerable,
            silent: self.silent,
            thrower: self.thrower,
            owner: self.owner,
        };
        let data = serialize_payload(&stored, MAX_DROP_RECORD_BYTES)?;
        Ok(DropData {
            chunk,
            data: Arc::from(data),
        })
    }

    pub fn decode(
        data: &DropData,
        world: Uuid,
        key: DropKey,
        catalog: &ItemCatalog,
    ) -> Result<Self, DropRecordError> {
        if data.data.len() > MAX_DROP_RECORD_BYTES {
            return Err(DropRecordError::Limit);
        }
        validate_identity(world, key)?;
        let stored: StoredDropRecord = serde_json::from_slice(&data.data)?;
        if stored.schema != 1
            || stored.minecraft_version != "26.3"
            || stored.protocol != 777
            || stored.world != world
            || stored.drop != key.uuid
        {
            return Err(DropRecordError::Invalid(
                "drop identity, schema or Minecraft version differs",
            ));
        }
        let stack = decode_stack(Some(stored.stack), catalog)?
            .ok_or(DropRecordError::Invalid("drop stack must be nonempty"))?;
        let record = Self {
            stack,
            position: stored.position_bits.map(f64::from_bits),
            velocity: stored.velocity_bits.map(f64::from_bits),
            rotation: stored.rotation_bits.map(f32::from_bits),
            age: stored.age,
            pickup_delay: stored.pickup_delay,
            health: stored.health,
            fire_ticks: stored.fire_ticks,
            no_gravity: stored.no_gravity,
            on_ground: stored.on_ground,
            invulnerable: stored.invulnerable,
            silent: stored.silent,
            thrower: stored.thrower,
            owner: stored.owner,
        };
        if record.chunk()? != data.chunk {
            return Err(DropRecordError::Invalid(
                "drop position differs from its chunk index",
            ));
        }
        Ok(record)
    }

    fn chunk(&self) -> Result<ChunkKey, DropRecordError> {
        if self.position.iter().any(|value| !value.is_finite())
            || self.position[0].abs() > 30_000_000.0
            || self.position[1].abs() > 20_000_000.0
            || self.position[2].abs() > 30_000_000.0
        {
            return Err(DropRecordError::Invalid(
                "drop position is outside core world bounds",
            ));
        }
        // This durable codec preserves finite motion exactly. It does not apply
        // Entity.load's NBT normalization (>10 per axis becomes zero).
        if self.velocity.iter().any(|value| !value.is_finite())
            || self.rotation.iter().any(|value| !value.is_finite())
        {
            return Err(DropRecordError::Invalid(
                "drop velocity or rotation is nonfinite",
            ));
        }
        // Floor before division: dividing a negative subnormal first can
        // underflow to -0 and incorrectly move the indexed entity into chunk 0.
        Ok(ChunkKey::new(
            (self.position[0].floor() as i32).div_euclid(16),
            (self.position[2].floor() as i32).div_euclid(16),
        ))
    }
}

fn validate_identity(world: Uuid, key: DropKey) -> Result<(), DropRecordError> {
    if world.is_nil() || key.uuid.is_nil() {
        return Err(DropRecordError::Invalid("nil world or drop UUID"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use std::{
        collections::{BTreeMap, BTreeSet},
        sync::Arc,
    };
    use uuid::Uuid;
    use zerqon_inventory::{
        ComponentDef, ComponentId, ComponentValue, ItemCatalog, ItemDef, ItemId, ItemStack,
        ValidatedPatch,
    };
    use zerqon_world::{ChunkKey, DropData, DropKey};

    fn catalog(offset: u32) -> ItemCatalog {
        ItemCatalog::from_definitions(
            vec![ItemDef {
                id: ItemId(offset + 1),
                key: "minecraft:stone".into(),
                max_stack: 64,
                max_damage: None,
            }],
            [
                (1, "minecraft:custom_name"),
                (2, "minecraft:item_name"),
                (3, "minecraft:custom_data"),
            ]
            .into_iter()
            .map(|(id, key)| ComponentDef {
                id: ComponentId(offset + id),
                key: key.into(),
            })
            .collect(),
        )
        .unwrap()
    }

    fn stack(catalog: &ItemCatalog) -> ItemStack {
        let patch = ValidatedPatch::new(
            BTreeMap::from([(
                catalog.component_id("minecraft:custom_name").unwrap(),
                ComponentValue::CustomName("Привет 🪨".into()),
            )]),
            BTreeSet::from([catalog.component_id("minecraft:item_name").unwrap()]),
            catalog,
        )
        .unwrap();
        ItemStack::new(catalog.item_id("minecraft:stone").unwrap(), 7, patch).unwrap()
    }

    fn identities() -> (Uuid, DropKey) {
        (Uuid::from_u128(1), DropKey::new(Uuid::from_u128(2)))
    }

    fn record(catalog: &ItemCatalog) -> DropRecord {
        DropRecord::new(
            stack(catalog),
            [-0.25, 64.0, 16.0],
            [0.1, 0.2, -0.05],
            [17.5, -8.25],
        )
    }

    fn changed(data: &DropData, update: impl FnOnce(&mut Value)) -> DropData {
        let mut value: Value = serde_json::from_slice(&data.data).unwrap();
        update(&mut value);
        DropData {
            chunk: data.chunk,
            data: Arc::from(serde_json::to_vec(&value).unwrap()),
        }
    }

    #[test]
    fn exact_ieee_bits_and_named_stack_survive_all_persisted_fields() {
        let catalog = catalog(0);
        let (world, key) = identities();
        let mut original = record(&catalog);
        assert_eq!(
            (
                original.health,
                original.age,
                original.pickup_delay,
                original.fire_ticks
            ),
            (5, 0, 0, 0)
        );
        assert!(
            !original.no_gravity
                && !original.on_ground
                && !original.invulnerable
                && !original.silent
        );
        assert!(original.owner.is_none() && original.thrower.is_none());
        original.position = [
            -f64::from_bits(1),
            f64::from_bits(0x404f_e666_6666_6667),
            -0.0,
        ];
        original.velocity = [f64::from_bits(0x3fb9_9999_b333_3334), -0.0, f64::MAX];
        original.rotation = [f32::from_bits(0x3f80_0001), -0.0];
        original.age = -6000;
        original.pickup_delay = 32767;
        original.health = -17;
        original.fire_ticks = -20;
        original.no_gravity = true;
        original.on_ground = true;
        original.invulnerable = true;
        original.silent = true;
        original.owner = Some(Uuid::nil());
        original.thrower = Some(Uuid::from_u128(99));
        let encoded = original.encode(world, key, &catalog).unwrap();
        assert_eq!(encoded.chunk, ChunkKey::new(-1, 0));
        let restored = DropRecord::decode(&encoded, world, key, &catalog).unwrap();
        assert_eq!(
            restored.position.map(f64::to_bits),
            original.position.map(f64::to_bits)
        );
        assert_eq!(
            restored.velocity.map(f64::to_bits),
            original.velocity.map(f64::to_bits)
        );
        assert_eq!(
            restored.rotation.map(f32::to_bits),
            original.rotation.map(f32::to_bits)
        );
        assert_eq!(restored.stack, original.stack);
        assert_eq!(
            (
                restored.age,
                restored.pickup_delay,
                restored.health,
                restored.fire_ticks
            ),
            (-6000, 32767, -17, -20)
        );
        assert!(
            restored.no_gravity && restored.on_ground && restored.invulnerable && restored.silent
        );
        assert_eq!(restored.owner, Some(Uuid::nil()));
        assert_eq!(restored.thrower, Some(Uuid::from_u128(99)));
    }

    #[test]
    fn named_stack_components_remap_to_the_current_catalog_without_losing_removals() {
        let original = catalog(0);
        let current = catalog(100);
        let (world, key) = identities();
        let encoded = record(&original).encode(world, key, &original).unwrap();
        let raw: Value = serde_json::from_slice(&encoded.data).unwrap();
        assert_eq!(raw["stack"]["item"], "minecraft:stone");
        assert_eq!(
            raw["stack"]["added"][0]["component"],
            "minecraft:custom_name"
        );
        assert_eq!(raw["stack"]["removed"], json!(["minecraft:item_name"]));
        let restored = DropRecord::decode(&encoded, world, key, &current).unwrap();
        assert_eq!(restored.stack, stack(&current));
        assert_eq!(restored.stack.item(), ItemId(101));
    }

    #[test]
    fn signed_short_domains_and_owner_thrower_presence_are_preserved() {
        let catalog = catalog(0);
        let (world, key) = identities();
        for value in [i16::MIN, -6000, -1, 0, 5999, 6000, i16::MAX] {
            for owner in [None, Some(Uuid::nil()), Some(Uuid::from_u128(99))] {
                let mut original = record(&catalog);
                original.age = value;
                original.pickup_delay = value;
                original.health = value;
                original.fire_ticks = value;
                original.owner = owner;
                original.thrower = Some(Uuid::nil());
                let encoded = original.encode(world, key, &catalog).unwrap();
                let restored = DropRecord::decode(&encoded, world, key, &catalog).unwrap();
                assert_eq!(
                    (
                        restored.age,
                        restored.pickup_delay,
                        restored.health,
                        restored.fire_ticks
                    ),
                    (value, value, value, value)
                );
                assert_eq!(restored.owner, owner);
                assert_eq!(restored.thrower, Some(Uuid::nil()));
            }
        }
    }

    #[test]
    fn chunk_index_uses_floor_and_rejects_foreign_world_key_or_index() {
        let catalog = catalog(0);
        let (world, key) = identities();
        for (x, z, expected) in [
            (0.0, 0.0, (0, 0)),
            (-f64::from_bits(1), 0.0, (-1, 0)),
            (15.999, 16.0, (0, 1)),
            (-16.0, -16.0, (-1, -1)),
            (f64::from_bits((-16.0_f64).to_bits() + 1), -0.01, (-2, -1)),
            (30_000_000.0, -30_000_000.0, (1_875_000, -1_875_000)),
        ] {
            let mut original = record(&catalog);
            original.position = [x, 64.0, z];
            let encoded = original.encode(world, key, &catalog).unwrap();
            assert_eq!(encoded.chunk, ChunkKey::new(expected.0, expected.1));
            assert!(DropRecord::decode(&encoded, Uuid::from_u128(3), key, &catalog).is_err());
            assert!(
                DropRecord::decode(&encoded, world, DropKey::new(Uuid::from_u128(3)), &catalog)
                    .is_err()
            );
            let wrong = DropData {
                chunk: ChunkKey::new(expected.0 + 1, expected.1),
                data: encoded.data,
            };
            assert!(DropRecord::decode(&wrong, world, key, &catalog).is_err());
        }
        assert!(record(&catalog).encode(Uuid::nil(), key, &catalog).is_err());
        assert!(
            record(&catalog)
                .encode(world, DropKey::new(Uuid::nil()), &catalog)
                .is_err()
        );
    }

    #[test]
    fn nonfinite_vectors_rotations_and_positions_outside_core_bounds_are_rejected() {
        let catalog = catalog(0);
        let (world, key) = identities();
        for invalid in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            for field in ["position", "velocity"] {
                for axis in 0..3 {
                    let mut original = record(&catalog);
                    if field == "position" {
                        original.position[axis] = invalid;
                    } else {
                        original.velocity[axis] = invalid;
                    }
                    assert!(original.encode(world, key, &catalog).is_err());
                }
            }
        }
        for invalid in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let mut original = record(&catalog);
            original.rotation[1] = invalid;
            assert!(original.encode(world, key, &catalog).is_err());
        }
        for position in [
            [30_000_001.0, 0.0, 0.0],
            [0.0, -20_000_001.0, 0.0],
            [0.0, 0.0, -30_000_001.0],
        ] {
            let mut original = record(&catalog);
            original.position = position;
            assert!(original.encode(world, key, &catalog).is_err());
        }
        let encoded = record(&catalog).encode(world, key, &catalog).unwrap();
        for (pointer, value) in [
            ("/position_bits/0", json!(f64::NAN.to_bits())),
            ("/velocity_bits/2", json!(f64::INFINITY.to_bits())),
            ("/rotation_bits/0", json!(f32::NEG_INFINITY.to_bits())),
        ] {
            let invalid = changed(&encoded, |data| *data.pointer_mut(pointer).unwrap() = value);
            assert!(DropRecord::decode(&invalid, world, key, &catalog).is_err());
        }
    }

    #[test]
    fn malformed_identity_unknown_fields_and_invalid_stacks_fail_closed() {
        let catalog = catalog(0);
        let (world, key) = identities();
        let encoded = record(&catalog).encode(world, key, &catalog).unwrap();
        for (pointer, value) in [
            ("/schema", json!(2)),
            ("/minecraft_version", json!("26.2")),
            ("/protocol", json!(776)),
            ("/age", json!(32768)),
            ("/pickup_delay", json!(-32769)),
            ("/health", json!(32768)),
            ("/fire_ticks", json!(-32769)),
            ("/stack", Value::Null),
            ("/stack/count", json!(0)),
            ("/stack/count", json!(65)),
            ("/stack/item", json!("minecraft:missing")),
            ("/stack/added/0/component", json!("minecraft:missing")),
            ("/stack/added/0/component", json!("minecraft:custom_data")),
            ("/stack/removed", json!(["minecraft:custom_data"])),
        ] {
            let invalid = changed(&encoded, |data| *data.pointer_mut(pointer).unwrap() = value);
            assert!(
                DropRecord::decode(&invalid, world, key, &catalog).is_err(),
                "{pointer}"
            );
        }
        for update in ["root", "stack"] {
            let invalid = changed(&encoded, |data| {
                if update == "root" {
                    data["unexpected"] = json!(true);
                } else {
                    data["stack"]["unexpected"] = json!(true);
                }
            });
            assert!(DropRecord::decode(&invalid, world, key, &catalog).is_err());
        }
    }

    #[test]
    fn opaque_size_limit_and_complete_json_are_checked_before_accepting_a_record() {
        let catalog = catalog(0);
        let (world, key) = identities();
        let encoded = record(&catalog).encode(world, key, &catalog).unwrap();
        let mut bytes = encoded.data.to_vec();
        bytes.resize(2 * 1024 * 1024, b' ');
        let bounded = DropData {
            chunk: encoded.chunk,
            data: Arc::from(bytes.clone()),
        };
        DropRecord::decode(&bounded, world, key, &catalog).unwrap();
        bytes.push(b' ');
        let oversized = DropData {
            chunk: encoded.chunk,
            data: Arc::from(bytes),
        };
        assert!(matches!(
            DropRecord::decode(&oversized, world, key, &catalog),
            Err(DropRecordError::Limit)
        ));
        for bytes in [
            encoded.data[..encoded.data.len() - 1].to_vec(),
            [encoded.data.as_ref(), b"x"].concat(),
        ] {
            let invalid = DropData {
                chunk: encoded.chunk,
                data: Arc::from(bytes),
            };
            assert!(DropRecord::decode(&invalid, world, key, &catalog).is_err());
        }
    }
}
