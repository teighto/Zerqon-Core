include!("../src/operator_store.rs");

#[cfg(test)]
mod tests {
    use super::*;
    fn first() -> Uuid {
        Uuid::from_u128(1)
    }
    fn second() -> Uuid {
        Uuid::from_u128(2)
    }

    #[test]
    fn grants_persist_authoritative_uuid_and_name_reuse_never_grants_rights() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ops.json");
        let mut store = OperatorStore::load(&path).unwrap();
        assert!(store.grant(first(), "OldName", 4).unwrap());
        assert!(store.is_operator(first()));
        assert!(!store.is_operator(second()));
        drop(store);
        let mut store = OperatorStore::load(&path).unwrap();
        assert_eq!(store.level(first()), 4);
        assert_eq!(store.level(second()), 0);
        assert!(store.grant(first(), "Renamed", 4).unwrap());
        assert!(store.is_operator(first()));
        assert!(store.lookup_by_name("OldName").is_none());
        assert_eq!(store.lookup_by_name("renamed").unwrap().uuid, first());
        assert!(store.grant(second(), "OldName", 1).unwrap());
        assert_eq!(store.level(second()), 1);
        assert!(store.revoke(second()).unwrap());
        assert!(!store.is_operator(second()));
        assert!(store.is_operator(first()));
        let stored: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(stored[0]["uuid"], first().to_string());
        assert_eq!(stored[0]["name"], "Renamed");
        assert_eq!(stored[0]["bypassesPlayerLimit"], false);
    }

    #[test]
    fn name_only_legacy_file_is_rejected_without_modification() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ops.json");
        let original = b"[\"admin\",\"another\"]";
        std::fs::write(&path, original).unwrap();
        assert!(matches!(
            OperatorStore::load(&path),
            Err(OperatorStoreError::LegacyNames)
        ));
        assert_eq!(std::fs::read(path).unwrap(), original);
    }

    #[test]
    fn invalid_entries_duplicates_and_unknown_fields_fail_closed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ops.json");
        let entry = serde_json::json!({"uuid":first(),"name":"Admin","level":4,"bypassesPlayerLimit":false});
        let mut cases = vec![
            serde_json::json!([entry.clone(), entry.clone()]),
            serde_json::json!([entry.clone(),{"uuid":second(),"name":"ADMIN","level":4,"bypassesPlayerLimit":false}]),
        ];
        for (key, value) in [
            ("uuid", serde_json::json!(Uuid::nil())),
            ("name", serde_json::json!("bad name")),
            ("level", serde_json::json!(0)),
            ("level", serde_json::json!(5)),
            ("surprise", serde_json::json!(true)),
        ] {
            let mut changed = entry.clone();
            changed[key] = value;
            cases.push(serde_json::json!([changed]));
        }
        for case in cases {
            let bytes = serde_json::to_vec(&case).unwrap();
            std::fs::write(&path, &bytes).unwrap();
            assert!(OperatorStore::load(&path).is_err(), "{case}");
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
        }
    }

    #[test]
    fn stale_and_corrupted_external_state_cannot_be_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ops.json");
        let mut store = OperatorStore::load(&path).unwrap();
        store.grant(first(), "Admin", 4).unwrap();
        for bytes in [b"[]".as_slice(), b"malformed", b"[\"legacy\"]"] {
            std::fs::write(&path, bytes).unwrap();
            assert!(store.grant(second(), "Other", 4).is_err());
            assert_eq!(store.level(first()), 4);
            assert_eq!(store.level(second()), 0);
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
        }
    }

    #[test]
    fn absent_and_existing_noops_do_not_rewrite_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ops.json");
        let mut store = OperatorStore::load(&path).unwrap();
        assert!(!store.revoke(first()).unwrap());
        assert!(!path.exists());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
        store.grant(first(), "Admin", 4).unwrap();
        let original = std::fs::read(&path).unwrap();
        let modified = path.metadata().unwrap().modified().unwrap();
        assert!(!store.grant(first(), "Admin", 4).unwrap());
        assert!(!store.revoke(second()).unwrap());
        assert_eq!(path.metadata().unwrap().modified().unwrap(), modified);
        assert_eq!(std::fs::read(path).unwrap(), original);
    }

    #[test]
    fn failed_writes_and_invalid_grants_preserve_in_memory_authorization() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ops.json");
        let mut store = OperatorStore::load(&path).unwrap();
        for (uuid, name, level) in [
            (Uuid::nil(), "Admin", 4),
            (first(), "", 4),
            (first(), "bad-name", 4),
            (first(), "Admin", 0),
            (first(), "Admin", 5),
        ] {
            assert!(store.grant(uuid, name, level).is_err());
            assert!(!store.is_operator(first()));
            assert!(!path.exists());
        }
        std::fs::create_dir(&path).unwrap();
        assert!(store.grant(first(), "Admin", 4).is_err());
        assert!(!store.is_operator(first()));
        assert!(path.is_dir());
    }

    #[test]
    fn multiple_store_instances_cannot_replace_each_others_committed_grants() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ops.json");
        let mut first_store = OperatorStore::load(&path).unwrap();
        let mut stale_store = OperatorStore::load(&path).unwrap();
        first_store.grant(first(), "First", 4).unwrap();
        assert!(matches!(
            stale_store.grant(second(), "Second", 4),
            Err(OperatorStoreError::Stale)
        ));
        assert!(!stale_store.is_operator(second()));
        let final_store = OperatorStore::load(&path).unwrap();
        assert!(final_store.is_operator(first()));
        assert!(!final_store.is_operator(second()));
    }

    #[test]
    fn concurrent_writer_lock_rejects_mutation_without_partial_authorization() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ops.json");
        let mut store = OperatorStore::load(&path).unwrap();
        let lock = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(dir.path().join("ops.json.lock"))
            .unwrap();
        lock.try_lock().unwrap();
        assert!(matches!(
            store.grant(first(), "Admin", 4),
            Err(OperatorStoreError::Busy)
        ));
        assert!(!path.exists());
        assert!(!store.is_operator(first()));
        drop(lock);
        assert!(store.grant(first(), "Admin", 4).unwrap());
    }

    #[test]
    fn oversized_corruption_and_duplicate_json_keys_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ops.json");
        File::create(&path).unwrap().set_len(1_048_577).unwrap();
        assert!(matches!(
            OperatorStore::load(&path),
            Err(OperatorStoreError::TooLarge)
        ));
        for bytes in [b"not json".as_slice(),b"{}",b"[null]",b"\xff",br#"[{"uuid":"00000000-0000-0000-0000-000000000001","uuid":"00000000-0000-0000-0000-000000000002","name":"Admin","level":4}]"#]{fs::write(&path,bytes).unwrap();assert!(OperatorStore::load(&path).is_err());assert_eq!(fs::read(&path).unwrap(),bytes);}
    }

    #[test]
    fn regrant_preserves_existing_bypass_metadata_and_updates_only_selected_uuid() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ops.json");
        fs::write(&path,serde_json::to_vec(&serde_json::json!([{"uuid":first(),"name":"Admin","level":3,"bypassesPlayerLimit":true}])).unwrap()).unwrap();
        let mut store = OperatorStore::load(&path).unwrap();
        store.grant(first(), "Renamed", 4).unwrap();
        let entry = store.entries().next().unwrap();
        assert!(entry.bypasses_player_limit);
        assert_eq!(entry.level, 4);
        let original = fs::read(&path).unwrap();
        assert!(matches!(
            store.grant(second(), "RENAMED", 4),
            Err(OperatorStoreError::Duplicate)
        ));
        assert_eq!(fs::read(path).unwrap(), original);
        assert!(!store.is_operator(second()));
    }

    #[cfg(windows)]
    #[test]
    fn sharing_violation_keeps_the_previous_authorization_snapshot() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ops.json");
        let mut store = OperatorStore::load(&path).unwrap();
        store.grant(first(), "Admin", 4).unwrap();
        let original = fs::read(&path).unwrap();
        let locked = fs::OpenOptions::new()
            .read(true)
            .share_mode(1)
            .open(&path)
            .unwrap();
        assert!(store.grant(second(), "Other", 4).is_err());
        assert_eq!(fs::read(&path).unwrap(), original);
        assert!(store.is_operator(first()));
        assert!(!store.is_operator(second()));
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2);
        drop(locked);
        assert!(store.grant(second(), "Other", 4).unwrap());
    }
}
