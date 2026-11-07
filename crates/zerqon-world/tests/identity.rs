use zerqon_world::WorldStore;

#[test]
fn world_uuid_is_persisted_and_distinct_from_another_world() {
    let directory = tempfile::tempdir().unwrap();
    let world_path = directory.path().join("first");
    let first = WorldStore::open(&world_path).unwrap();
    let identity = first.uuid().unwrap();
    assert!(!identity.is_nil());
    let marker: serde_json::Value =
        serde_json::from_slice(&std::fs::read(world_path.join("storage-format.json")).unwrap())
            .unwrap();
    assert_eq!(marker["world"], identity.to_string());
    drop(first);
    let reopened = WorldStore::open(&world_path).unwrap();
    assert_eq!(reopened.uuid().unwrap(), identity);
    let other = WorldStore::open(directory.path().join("second")).unwrap();
    assert_ne!(other.uuid().unwrap(), identity);
}
