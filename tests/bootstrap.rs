#[path = "../src/cache_lock.rs"]
#[allow(dead_code)]
mod cache_lock;
#[path = "../src/lifecycle.rs"]
#[allow(dead_code)]
mod lifecycle;
#[path = "../src/managed_tasks.rs"]
#[allow(dead_code)]
mod managed_tasks;

#[path = "../src/java_launch.rs"]
#[allow(dead_code)]
pub mod java_launch;
include!("../src/bootstrap.rs");

#[cfg(test)]
mod tests {
    use super::*;

    impl Bootstrap {
        async fn prepare(directory: &Path, java: &Path) -> Result<PreparedData, BootstrapError> {
            let lifecycle = Lifecycle::new();
            Self::prepare_with_lifecycle(directory, java, &lifecycle, &lifecycle.token()).await
        }
    }

    async fn acquire_prepare_lock(lock: &File, timeout: Duration) -> Result<(), BootstrapError> {
        acquire_prepare_lock_tracked(|| lock.try_lock(), timeout, &Lifecycle::new().token()).await
    }

    async fn download(
        destination: &Path,
        url: &str,
        size: u64,
        sha1: &str,
    ) -> Result<(), BootstrapError> {
        download_tracked(destination, url, size, sha1, &Lifecycle::new().token()).await
    }

    async fn java_runtime(java: &Path) -> Result<PathBuf, BootstrapError> {
        let lifecycle = Lifecycle::new();
        java_runtime_tracked(java, &lifecycle, &lifecycle.token()).await
    }

    async fn generate_reports(
        java: &Path,
        jar: &Path,
        stage: std::sync::Arc<tempfile::TempDir>,
    ) -> Result<(), BootstrapError> {
        let lifecycle = Lifecycle::new();
        generate_reports_tracked(java, jar, stage, &lifecycle, &lifecycle.token()).await
    }

    #[test]
    fn generator_manifest_requires_one_main_section_entrypoint_with_bounded_size() {
        let directory = tempfile::tempdir().unwrap();
        let jar = directory.path().join("generator.jar");
        for (manifest, expected) in [
            (
                "Manifest-Version: 1.0\r\nMain-Class: teighto.\r\n Generator\r\n\r\n",
                Some("teighto.Generator"),
            ),
            (
                "Manifest-Version: 1.0\nmain-class: Generator\n\n",
                Some("Generator"),
            ),
            (
                "Manifest-Version: 1.0\n\nName: section\nMain-Class: Nested\n",
                None,
            ),
            ("Main-Class: First\nMAIN-CLASS: Second\n\n", None),
            (" continuation\nMain-Class: Generator\n", None),
        ] {
            let mut archive = zip::ZipWriter::new(File::create(&jar).unwrap());
            archive
                .start_file(
                    "META-INF/MANIFEST.MF",
                    zip::write::SimpleFileOptions::default(),
                )
                .unwrap();
            archive.write_all(manifest.as_bytes()).unwrap();
            archive.finish().unwrap();
            assert_eq!(generator_main_class(&jar).ok().as_deref(), expected);
        }
        let mut archive = zip::ZipWriter::new(File::create(&jar).unwrap());
        archive
            .start_file(
                "META-INF/MANIFEST.MF",
                zip::write::SimpleFileOptions::default(),
            )
            .unwrap();
        archive.write_all(&vec![b'x'; 65_537]).unwrap();
        archive.finish().unwrap();
        assert!(
            generator_main_class(&jar)
                .unwrap_err()
                .to_string()
                .contains("64 KiB")
        );
    }
    use serde_json::json;

    fn inventory_cache(directory: &Path) -> Value {
        let reports = directory.join("generated/reports");
        fs::create_dir_all(reports.join("minecraft/components/item")).unwrap();
        fs::create_dir_all(directory.join("data")).unwrap();
        let files = [
            (
                "packets.json",
                json!({"handshake":{"serverbound":{"minecraft:intention":{"protocol_id":0}}},"login":{"clientbound":{"minecraft:login_finished":{"protocol_id":2}}},"play":{"clientbound":{"minecraft:login":{"protocol_id":50}}}}),
            ),
            (
                "blocks.json",
                json!({"minecraft:air":{"states":[{"id":0,"default":true}]}}),
            ),
            (
                "registries.json",
                json!({"minecraft:entity_type":{"entries":{"minecraft:player":{"protocol_id":1}}},"minecraft:item":{"entries":{"minecraft:stone":{"protocol_id":1}}},"minecraft:data_component_type":{"entries":{"minecraft:max_stack_size":{"protocol_id":0}}}}),
            ),
            (
                "minecraft/components/item/stone.json",
                json!({"components":{"minecraft:max_stack_size":64}}),
            ),
        ];
        for (name, value) in files {
            fs::write(reports.join(name), serde_json::to_vec(&value).unwrap()).unwrap();
        }
        fs::write(directory.join("registries-sync.json"), serde_json::to_vec(&json!({"version":VERSION,"protocol":PROTOCOL,"registries":REGISTRIES.iter().map(|name|json!({"name":format!("minecraft:{name}"),"entries":[{"id":"minecraft:test","data":{}}]})).collect::<Vec<_>>()})).unwrap()).unwrap();
        fs::write(directory.join("tags-sync.json"), br#"{"registries":[{"name":"minecraft:item","tags":[{"name":"minecraft:test","ids":[1]}]}]}"#).unwrap();
        fs::write(directory.join("data/unchanged.json"), b"staged payload").unwrap();
        fs::create_dir_all(directory.join("data/minecraft/recipe")).unwrap();
        fs::write(directory.join("data/minecraft/recipe/test.json"), br#"{"type":"minecraft:crafting_shapeless","ingredients":["minecraft:stone"],"result":{"id":"minecraft:stone"}}"#).unwrap();
        let (recipes, recipes_sha256) = recipe_identity(&directory.join("data")).unwrap();
        let prepared = json!({"version":VERSION,"protocol":PROTOCOL,"server_sha1":SERVER_SHA1,"world_version":5023,"java_major":25,"registries":REGISTRIES.len(),"entries":REGISTRIES.len(),"tag_registries":1,"tags":1,"recipes":recipes,"recipes_sha256":recipes_sha256});
        fs::write(
            directory.join("prepared.json"),
            serde_json::to_vec(&prepared).unwrap(),
        )
        .unwrap();
        prepared
    }

    #[test]
    fn legacy_cache_without_recipe_manifest_is_not_ready() {
        let directory = tempfile::tempdir().unwrap();
        let mut identity = inventory_cache(directory.path());
        identity.as_object_mut().unwrap().remove("recipes");
        identity.as_object_mut().unwrap().remove("recipes_sha256");
        fs::write(
            directory.path().join("prepared.json"),
            serde_json::to_vec(&identity).unwrap(),
        )
        .unwrap();
        assert!(base_cache_ready(directory.path()).unwrap());
        assert!(!cache_ready(directory.path()).unwrap());
    }

    #[test]
    fn recipe_loader_returns_namespaced_values_verified_by_the_manifest() {
        let directory = tempfile::tempdir().unwrap();
        inventory_cache(directory.path());
        let recipes = load_prepared_recipes(directory.path()).unwrap();
        assert_eq!(
            recipes.keys().cloned().collect::<Vec<_>>(),
            ["minecraft:test"]
        );
        assert_eq!(
            recipes["minecraft:test"]["ingredients"],
            json!(["minecraft:stone"])
        );
        fs::write(directory.path().join("data/minecraft/recipe/test.json"), br#"{"type":"minecraft:crafting_shapeless","ingredients":["minecraft:stone"],"result":"minecraft:diamond"}"#).unwrap();
        assert!(load_prepared_recipes(directory.path()).is_err());
    }

    #[test]
    fn recipe_loader_refuses_missing_manifest_and_wrong_identity_without_writes() {
        for key in [
            "recipes",
            "recipes_sha256",
            "version",
            "protocol",
            "server_sha1",
        ] {
            let directory = tempfile::tempdir().unwrap();
            let mut identity = inventory_cache(directory.path());
            identity.as_object_mut().unwrap().remove(key);
            let bytes = serde_json::to_vec(&identity).unwrap();
            let path = directory.path().join("prepared.json");
            fs::write(&path, &bytes).unwrap();
            assert!(load_prepared_recipes(directory.path()).is_err());
            assert_eq!(fs::read(path).unwrap(), bytes);
        }
    }

    #[test]
    fn missing_or_modified_recipe_invalidates_prepared_cache() {
        for replacement in [
            None,
            Some(br#"{"type":"minecraft:crafting_shapeless"}"#.as_slice()),
            Some(b"{".as_slice()),
        ] {
            let directory = tempfile::tempdir().unwrap();
            inventory_cache(directory.path());
            assert!(cache_ready(directory.path()).unwrap());
            let path = directory.path().join("data/minecraft/recipe/test.json");
            if let Some(bytes) = replacement {
                fs::write(path, bytes).unwrap();
            } else {
                fs::remove_file(path).unwrap();
            }
            assert!(!cache_ready(directory.path()).unwrap());
        }
    }

    #[test]
    fn corrupt_staged_recipe_cannot_replace_existing_cache() {
        let directory = tempfile::tempdir().unwrap();
        let stage = directory.path().join("stage");
        let destination = directory.path().join("cache");
        let prepared = inventory_cache(&stage);
        inventory_cache(&destination);
        fs::write(destination.join("data/unchanged.json"), b"original payload").unwrap();
        fs::write(stage.join("data/minecraft/recipe/test.json"), b"{}").unwrap();
        let identity = fs::read(destination.join("prepared.json")).unwrap();
        assert!(install_prepared(&stage, &destination, &prepared).is_err());
        assert_eq!(
            fs::read(destination.join("data/unchanged.json")).unwrap(),
            b"original payload"
        );
        assert_eq!(
            fs::read(destination.join("prepared.json")).unwrap(),
            identity
        );
        assert!(cache_ready(&destination).unwrap());
    }

    #[test]
    fn unexpected_cached_recipe_is_preserved_before_installation() {
        let directory = tempfile::tempdir().unwrap();
        let stage = directory.path().join("stage");
        let destination = directory.path().join("cache");
        let prepared = inventory_cache(&stage);
        inventory_cache(&destination);
        fs::write(destination.join("data/unchanged.json"), b"original payload").unwrap();
        let extra = destination.join("data/minecraft/recipe/unexpected.json");
        fs::write(&extra, b"user content").unwrap();
        let identity = fs::read(destination.join("prepared.json")).unwrap();
        assert!(
            install_prepared(&stage, &destination, &prepared)
                .unwrap_err()
                .to_string()
                .contains("unexpected cached recipe")
        );
        assert_eq!(fs::read(extra).unwrap(), b"user content");
        assert_eq!(
            fs::read(destination.join("data/unchanged.json")).unwrap(),
            b"original payload"
        );
        assert_eq!(
            fs::read(destination.join("prepared.json")).unwrap(),
            identity
        );
    }

    #[test]
    fn linked_recipe_tree_is_rejected_before_installation() {
        for link_stage in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let stage = directory.path().join("stage");
            let destination = directory.path().join("cache");
            let prepared = inventory_cache(&stage);
            inventory_cache(&destination);
            let outside = directory.path().join("outside");
            let linked =
                if link_stage { &stage } else { &destination }.join("data/minecraft/recipe");
            fs::rename(&linked, &outside).unwrap();
            let original = fs::read(outside.join("test.json")).unwrap();
            directory_link(&outside, &linked);
            let identity = fs::read(destination.join("prepared.json")).unwrap();
            assert!(install_prepared(&stage, &destination, &prepared).is_err());
            assert_eq!(fs::read(outside.join("test.json")).unwrap(), original);
            assert_eq!(
                fs::read(destination.join("prepared.json")).unwrap(),
                identity
            );
        }
    }

    #[test]
    fn recipe_manifest_rejects_invalid_paths_types_and_oversized_files() {
        for (name, bytes) in [
            (
                "UPPER.json",
                br#"{"type":"minecraft:crafting_shapeless"}"#.to_vec(),
            ),
            ("test.json", br#"{"type":42}"#.to_vec()),
            ("test.json", b"[]".to_vec()),
            ("test.json", vec![b' '; 1024 * 1024 + 1]),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path().join("minecraft/recipe");
            fs::create_dir_all(&root).unwrap();
            fs::write(root.join(name), bytes).unwrap();
            assert!(recipe_identity(directory.path()).is_err(), "{name}");
        }
        assert!(!REGISTRIES.contains(&"recipe"));
    }

    #[tokio::test]
    async fn reusable_reports_do_not_require_java_for_recipe_repair() {
        let directory = tempfile::tempdir().unwrap();
        inventory_cache(directory.path());
        fs::remove_file(directory.path().join("data/minecraft/recipe/test.json")).unwrap();
        let lifecycle = Lifecycle::new();
        assert!(!cache_ready(directory.path()).unwrap());
        assert!(
            !Bootstrap::needs_java_with_lifecycle(directory.path(), &lifecycle, &lifecycle.token())
                .await
                .unwrap()
        );
        let stage = tempfile::tempdir().unwrap();
        assert!(stage_cached_reports(directory.path(), stage.path()).unwrap());
        assert!(
            stage
                .path()
                .join("generated/reports/minecraft/components/item/stone.json")
                .is_file()
        );
        assert!(!stage.path().join("data").exists());
    }

    #[test]
    fn cached_report_staging_rejects_corruption_and_ignores_unlisted_files() {
        let directory = tempfile::tempdir().unwrap();
        inventory_cache(directory.path());
        fs::write(
            directory.path().join("generated/reports/unlisted.json"),
            b"untrusted",
        )
        .unwrap();
        let stage = tempfile::tempdir().unwrap();
        assert!(stage_cached_reports(directory.path(), stage.path()).unwrap());
        assert!(
            !stage
                .path()
                .join("generated/reports/unlisted.json")
                .exists()
        );
        fs::write(
            directory
                .path()
                .join("generated/reports/minecraft/components/item/stone.json"),
            b"{}",
        )
        .unwrap();
        let invalid_stage = tempfile::tempdir().unwrap();
        assert!(!stage_cached_reports(directory.path(), invalid_stage.path()).unwrap());
        assert_eq!(fs::read_dir(invalid_stage.path()).unwrap().count(), 0);
    }

    #[test]
    fn prepared_cache_requires_every_item_default_report() {
        let directory = tempfile::tempdir().unwrap();
        inventory_cache(directory.path());
        assert!(cache_ready(directory.path()).unwrap());
        fs::remove_file(
            directory
                .path()
                .join("generated/reports/minecraft/components/item/stone.json"),
        )
        .unwrap();
        assert!(!cache_ready(directory.path()).unwrap());
    }

    #[test]
    fn install_publishes_item_defaults_before_the_ready_marker() {
        let directory = tempfile::tempdir().unwrap();
        let stage = directory.path().join("stage");
        let destination = directory.path().join("cache");
        let prepared = inventory_cache(&stage);
        install_prepared(&stage, &destination, &prepared).unwrap();
        let catalog = zerqon_inventory::ItemCatalog::load(&destination).unwrap();
        let stone = catalog.item_id("minecraft:stone").unwrap();
        assert_eq!(catalog.item(stone).unwrap().max_stack, 64);
        assert!(cache_ready(&destination).unwrap());
    }

    #[test]
    fn corrupt_staged_item_report_cannot_replace_existing_cache_files() {
        let directory = tempfile::tempdir().unwrap();
        let stage = directory.path().join("stage");
        let destination = directory.path().join("cache");
        let prepared = inventory_cache(&stage);
        inventory_cache(&destination);
        fs::write(destination.join("data/unchanged.json"), b"original payload").unwrap();
        fs::write(
            stage.join("generated/reports/minecraft/components/item/stone.json"),
            br#"{"components":{"minecraft:max_stack_size":0}}"#,
        )
        .unwrap();
        assert!(!cache_ready(&stage).unwrap());
        assert!(install_prepared(&stage, &destination, &prepared).is_err());
        assert_eq!(
            fs::read(destination.join("data/unchanged.json")).unwrap(),
            b"original payload"
        );
        assert!(cache_ready(&destination).unwrap());
    }

    fn directory_link(target: &Path, link: &Path) {
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            let script = format!(
                "New-Item -ItemType Junction -Path '{}' -Target '{}' | Out-Null",
                link.display().to_string().replace('\'', "''"),
                target.display().to_string().replace('\'', "''")
            );
            let result = std::process::Command::new("powershell")
                .args(["-NoProfile", "-NonInteractive", "-Command", &script])
                .creation_flags(0x08000000)
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, link).unwrap();
    }

    #[test]
    fn linked_item_reports_are_rejected_before_cache_writes() {
        for linked_stage in [true, false] {
            let directory = tempfile::tempdir().unwrap();
            let stage = directory.path().join("stage");
            let destination = directory.path().join("cache");
            let outside = directory.path().join("outside");
            let prepared = inventory_cache(&stage);
            inventory_cache(&destination);
            fs::write(destination.join("data/unchanged.json"), b"original payload").unwrap();
            fs::create_dir(&outside).unwrap();
            let linked_root = if linked_stage { &stage } else { &destination };
            let items = linked_root.join("generated/reports/minecraft/components/item");
            fs::rename(items.join("stone.json"), outside.join("stone.json")).unwrap();
            let original = fs::read(outside.join("stone.json")).unwrap();
            fs::remove_dir(&items).unwrap();
            directory_link(&outside, &items);
            assert!(cache_ready(linked_root).is_err());
            assert!(install_prepared(&stage, &destination, &prepared).is_err());
            assert_eq!(
                fs::read(destination.join("data/unchanged.json")).unwrap(),
                b"original payload"
            );
            assert_eq!(fs::read(outside.join("stone.json")).unwrap(), original);
        }
    }

    #[cfg(unix)]
    #[test]
    fn item_file_links_are_rejected_in_a_regular_validated_parent() {
        let directory = tempfile::tempdir().unwrap();
        let cache = directory.path().join("cache");
        inventory_cache(&cache);
        let item = cache.join("generated/reports/minecraft/components/item/stone.json");
        let outside = directory.path().join("outside.json");
        fs::rename(&item, &outside).unwrap();
        let original = fs::read(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, &item).unwrap();
        assert!(cache_ready(&cache).is_err());
        assert_eq!(fs::read(outside).unwrap(), original);
    }

    #[test]
    fn invalid_item_keys_cannot_select_files_outside_the_report_directory() {
        for key in [
            "../:stone",
            "..:stone",
            "minecraft:../stone",
            "minecraft:folder/../../stone",
            "minecraft:stone/",
            "minecraft:con",
        ] {
            let directory = tempfile::tempdir().unwrap();
            inventory_cache(directory.path());
            let path = directory.path().join("generated/reports/registries.json");
            let mut registries = read_json(&path, 1024 * 1024).unwrap();
            registries["minecraft:item"]["entries"] = json!({key:{"protocol_id":1}});
            fs::write(path, serde_json::to_vec(&registries).unwrap()).unwrap();
            assert!(!cache_ready(directory.path()).unwrap(), "{key}");
        }
    }

    #[test]
    fn valid_stage_cannot_publish_through_a_data_directory_link() {
        let directory = tempfile::tempdir().unwrap();
        let stage = directory.path().join("stage");
        let destination = directory.path().join("cache");
        let outside = directory.path().join("outside");
        let prepared = inventory_cache(&stage);
        fs::create_dir(&destination).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("unchanged.json"), b"original payload").unwrap();
        directory_link(&outside, &destination.join("data"));
        assert!(install_prepared(&stage, &destination, &prepared).is_err());
        assert_eq!(
            fs::read(outside.join("unchanged.json")).unwrap(),
            b"original payload"
        );
        assert!(!destination.join("prepared.json").exists());
    }
    #[test]
    fn pinned_download_verification_checks_length_and_content() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("file");
        std::fs::write(&path, b"abc").unwrap();
        assert!(verify_file(&path, 3, "a9993e364706816aba3e25717850c26c9cd0d89d").unwrap());
        assert!(!verify_file(&path, 4, "a9993e364706816aba3e25717850c26c9cd0d89d").unwrap());
        assert!(!verify_file(&path, 3, SERVER_SHA1).unwrap());
    }
    #[test]
    fn tags_resolve_nested_optional_and_duplicate_entries_in_source_order() {
        let definitions = BTreeMap::from([
            (
                "minecraft:first".into(),
                vec![json!("stone"), json!("minecraft:dirt"), json!("stone")],
            ),
            (
                "minecraft:second".into(),
                vec![
                    json!("#first"),
                    json!({"id":"missing","required":false}),
                    json!("minecraft:water"),
                ],
            ),
        ]);
        let ids = BTreeMap::from([
            ("minecraft:stone".into(), 2),
            ("minecraft:dirt".into(), 5),
            ("minecraft:water".into(), 8),
        ]);
        let tags = resolve_tags(&definitions, &ids).unwrap();
        assert_eq!(tags["minecraft:second"], [2, 5, 8]);
    }
    #[test]
    fn tags_reject_cycles_and_missing_required_references() {
        let ids = BTreeMap::new();
        for definitions in [
            BTreeMap::from([("minecraft:a".into(), vec![json!("#a")])]),
            BTreeMap::from([("minecraft:a".into(), vec![json!("missing")])]),
        ] {
            assert!(resolve_tags(&definitions, &ids).is_err());
        }
    }
    #[test]
    fn archive_paths_cannot_escape_or_target_windows_devices() {
        for name in [
            "/data/file.json",
            "../file.json",
            "data/../file.json",
            "data\\file.json",
            "C:/file.json",
            "data/NUL.json",
            "data/a:stream.json",
            "data/file.",
        ] {
            assert!(safe_archive_path(name).is_err(), "{name}");
        }
        assert_eq!(
            safe_archive_path("data/minecraft/worldgen/biome/plains.json").unwrap(),
            ["data", "minecraft", "worldgen", "biome", "plains.json"]
        );
    }

    async fn serve_once(bytes: Vec<u8>) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 4096];
            let _ = socket.read(&mut request).await.unwrap();
            socket.write_all(&bytes).await.unwrap();
            socket.shutdown().await.unwrap();
        });
        format!("http://{address}/fixture")
    }

    #[tokio::test]
    async fn download_verifies_stream_before_publishing_and_preserves_existing_files() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("server.jar");
        let sha = "a9993e364706816aba3e25717850c26c9cd0d89d";
        let url = serve_once(
            b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nConnection: close\r\n\r\nabc".to_vec(),
        )
        .await;
        download(&destination, &url, 3, sha).await.unwrap();
        assert_eq!(fs::read(&destination).unwrap(), b"abc");
        let url = serve_once(
            b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nConnection: close\r\n\r\nxyz".to_vec(),
        )
        .await;
        assert!(download(&destination, &url, 3, sha).await.is_err());
        assert_eq!(fs::read(&destination).unwrap(), b"abc");
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn download_rejects_chunked_oversize_truncation_and_http_errors() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("server.jar");
        let sha = "a9993e364706816aba3e25717850c26c9cd0d89d";
        for response in [b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n4\r\nabcd\r\n0\r\n\r\n".as_slice(),b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nConnection: close\r\n\r\nab",b"HTTP/1.1 404 Not Found\r\nContent-Length: 3\r\nConnection: close\r\n\r\nabc"]{
            let url=serve_once(response.to_vec()).await;assert!(download(&destination,&url,3,sha).await.is_err());assert!(!destination.exists());
        }
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
    }

    #[test]
    fn registry_order_and_tags_match_minecraft_relative_path_then_namespace_order() {
        let data = BTreeMap::from([
            (
                "zeta/worldgen/biome/a.json".into(),
                json!({"temperature":1}),
            ),
            (
                "alpha/worldgen/biome/z.json".into(),
                json!({"temperature":2}),
            ),
            (
                "minecraft/tags/worldgen/biome/test.json".into(),
                json!({"values":["zeta:a","alpha:z"]}),
            ),
        ]);
        let registries = collect_registries(&data).unwrap();
        let biomes = registries
            .iter()
            .find(|registry| registry["name"] == "minecraft:worldgen/biome")
            .unwrap();
        assert_eq!(biomes["entries"][0]["id"], "zeta:a");
        assert_eq!(biomes["entries"][1]["id"], "alpha:z");
        let tags = collect_tags(&data, &json!({}), &registries).unwrap();
        assert_eq!(tags[0]["tags"][0]["ids"], json!([0, 1]));
    }

    #[test]
    fn matching_identity_does_not_make_malformed_protocol_reports_a_valid_cache() {
        let directory = tempfile::tempdir().unwrap();
        let reports = directory.path().join("generated/reports");
        fs::create_dir_all(&reports).unwrap();
        fs::write(directory.path().join("prepared.json"),serde_json::to_vec(&json!({"version":VERSION,"protocol":PROTOCOL,"server_sha1":SERVER_SHA1,"world_version":5023,"java_major":25,"registries":32,"entries":0,"tag_registries":1,"tags":0})).unwrap()).unwrap();
        for name in REPORTS {
            fs::write(reports.join(name), b"{\"garbage\":true}").unwrap();
        }
        fs::write(directory.path().join("registries-sync.json"),serde_json::to_vec(&json!({"version":VERSION,"protocol":PROTOCOL,"registries":REGISTRIES.iter().map(|name|json!({"name":format!("minecraft:{name}"),"entries":[]})).collect::<Vec<_>>()})).unwrap()).unwrap();
        fs::write(
            directory.path().join("tags-sync.json"),
            b"{\"registries\":[{\"name\":\"minecraft:block\",\"tags\":[]}]}",
        )
        .unwrap();
        assert!(!cache_ready(directory.path()).unwrap());
    }

    #[tokio::test]
    #[ignore = "Requires official cached .cache/vanilla/26.3/server.jar, Python-reference outputs and Java25; no Python is invoked by bootstrap"]
    async fn native_prepare_matches_reference_and_reuses_cache_without_java() {
        let reference = Path::new(env!("CARGO_MANIFEST_DIR")).join(".cache/vanilla/26.3");
        let directory = tempfile::tempdir().unwrap();
        fs::copy(
            reference.join("server.jar"),
            directory.path().join("server.jar"),
        )
        .unwrap();
        let prepared = Bootstrap::prepare(directory.path(), Path::new("java"))
            .await
            .unwrap();
        assert!(!prepared.reused);
        assert_eq!(prepared.directory, directory.path().canonicalize().unwrap());
        for name in ["prepared.json", "registries-sync.json", "tags-sync.json"] {
            let mut actual = read_json(&directory.path().join(name), 16 * 1024 * 1024).unwrap();
            let expected = read_json(&reference.join(name), 16 * 1024 * 1024).unwrap();
            if name == "prepared.json" && expected.get("recipes").is_none() {
                // A legacy reference has identical reports but predates recipe extraction.
                assert_eq!(actual["recipes"], 2042);
                actual.as_object_mut().unwrap().remove("recipes");
                actual.as_object_mut().unwrap().remove("recipes_sha256");
            }
            assert_eq!(actual, expected, "{name}");
        }
        for name in REPORTS {
            assert_eq!(
                read_json(
                    &directory.path().join("generated/reports").join(name),
                    16 * 1024 * 1024
                )
                .unwrap(),
                read_json(
                    &reference.join("generated/reports").join(name),
                    16 * 1024 * 1024
                )
                .unwrap(),
                "{name}"
            );
        }
        let catalog = zerqon_inventory::ItemCatalog::load(directory.path()).unwrap();
        assert_eq!(catalog.item_count(), 1658);
        assert_eq!(catalog.component_count(), 122);
        for report in item_report_paths(&reference).unwrap() {
            assert_eq!(
                read_json(&directory.path().join(&report), 1024 * 1024).unwrap(),
                read_json(&reference.join(&report), 1024 * 1024).unwrap(),
                "{}",
                report.display()
            );
        }
        let reused = Bootstrap::prepare(directory.path(), Path::new("nonexistent-java-runtime"))
            .await
            .unwrap();
        assert!(reused.reused);
    }

    #[tokio::test]
    #[ignore = "Downloads the pinned 62 MB official server over HTTPS and requires Java25"]
    async fn native_first_boot_downloads_and_prepares_official_data() {
        let directory = tempfile::tempdir().unwrap();
        let prepared = Bootstrap::prepare(directory.path(), Path::new("java"))
            .await
            .unwrap();
        assert!(!prepared.reused);
        assert!(
            verify_file(
                &directory.path().join("server.jar"),
                SERVER_BYTES,
                SERVER_SHA1
            )
            .unwrap()
        );
        assert!(cache_ready(directory.path()).unwrap());
    }

    #[tokio::test]
    #[ignore = "Requires existing verified .cache/vanilla/26.3; copies only required reports and pinned JAR into isolation"]
    async fn concurrent_servers_can_reuse_the_same_verified_cache() {
        let isolated = legacy_reference_cache();
        let directory = isolated.path();
        Bootstrap::prepare(directory, Path::new("nonexistent-java"))
            .await
            .unwrap();
        let (first, second) = tokio::join!(
            Bootstrap::prepare(directory, Path::new("nonexistent-java")),
            Bootstrap::prepare(directory, Path::new("nonexistent-java"))
        );
        assert!(first.unwrap().reused);
        assert!(second.unwrap().reused);
    }

    fn legacy_reference_cache() -> tempfile::TempDir {
        let reference = Path::new(env!("CARGO_MANIFEST_DIR")).join(".cache/vanilla/26.3");
        let directory = tempfile::tempdir().unwrap();
        assert!(stage_cached_reports(&reference, directory.path()).unwrap());
        for name in [
            "server.jar",
            "registries-sync.json",
            "tags-sync.json",
            "prepared.json",
        ] {
            fs::copy(reference.join(name), directory.path().join(name)).unwrap();
        }
        let mut identity = read_json(&directory.path().join("prepared.json"), 65536).unwrap();
        identity.as_object_mut().unwrap().remove("recipes");
        identity.as_object_mut().unwrap().remove("recipes_sha256");
        fs::write(
            directory.path().join("prepared.json"),
            serde_json::to_vec(&identity).unwrap(),
        )
        .unwrap();
        fs::create_dir(directory.path().join("data")).unwrap();
        directory
    }

    #[tokio::test]
    #[ignore = "Requires existing official cached reports and pinned JAR; never invokes Java or changes shared cache"]
    async fn legacy_recipe_repair_uses_verified_jar_and_reports_without_java() {
        let directory = legacy_reference_cache();
        assert!(base_cache_ready(directory.path()).unwrap());
        assert!(!cache_ready(directory.path()).unwrap());
        let lifecycle = Lifecycle::new();
        assert!(
            !Bootstrap::needs_java_with_lifecycle(directory.path(), &lifecycle, &lifecycle.token())
                .await
                .unwrap()
        );
        let prepared = Bootstrap::prepare(directory.path(), Path::new("nonexistent-java-runtime"))
            .await
            .unwrap();
        assert!(!prepared.reused);
        assert!(cache_ready(directory.path()).unwrap());
        let identity = read_json(&directory.path().join("prepared.json"), 65536).unwrap();
        assert_eq!(identity["recipes"], 2042);
        // Independently produced by scripts/prepare-data.py from the checksum-pinned 26.3 JAR.
        assert_eq!(
            identity["recipes_sha256"],
            "7b160ea898535c76f414e580796b13ab59cc6921160d202ecb3d048bed612222"
        );
        assert_eq!(
            recipe_identity(&directory.path().join("data")).unwrap().0,
            2042
        );
        let recipe = directory
            .path()
            .join("data/minecraft/recipe/oak_planks.json");
        let original = fs::read(&recipe).unwrap();
        fs::write(&recipe, b"{}").unwrap();
        assert!(!cache_ready(directory.path()).unwrap());
        assert!(
            !Bootstrap::prepare(directory.path(), Path::new("nonexistent-java-runtime"))
                .await
                .unwrap()
                .reused
        );
        assert_eq!(fs::read(&recipe).unwrap(), original);
        assert!(
            Bootstrap::prepare(directory.path(), Path::new("nonexistent-java-runtime"))
                .await
                .unwrap()
                .reused
        );
    }

    #[tokio::test]
    #[ignore = "Requires existing official cached reports and pinned JAR; tests isolated archive integrity failure"]
    async fn invalid_archive_preserves_legacy_cache_before_repair() {
        let directory = legacy_reference_cache();
        let before = fs::read(directory.path().join("prepared.json")).unwrap();
        fs::write(directory.path().join("data/unchanged.json"), b"preserve me").unwrap();
        fs::write(directory.path().join("server.jar"), b"invalid archive").unwrap();
        let result =
            Bootstrap::prepare(directory.path(), Path::new("nonexistent-java-runtime")).await;
        assert!(result.err().unwrap().to_string().contains("checksum"));
        assert_eq!(
            fs::read(directory.path().join("prepared.json")).unwrap(),
            before
        );
        assert_eq!(
            fs::read(directory.path().join("data/unchanged.json")).unwrap(),
            b"preserve me"
        );
        assert!(!directory.path().join("data/minecraft").exists());
    }

    #[tokio::test]
    async fn cache_lock_waits_for_owner_release_and_times_out_when_owner_stays() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("prepare.lock");
        let first = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .read(true)
            .open(&path)
            .unwrap();
        first.try_lock().unwrap();
        let second = fs::OpenOptions::new()
            .write(true)
            .read(true)
            .open(&path)
            .unwrap();
        assert!(
            acquire_prepare_lock(&second, Duration::from_millis(50))
                .await
                .is_err()
        );
        let release = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            drop(first);
        });
        acquire_prepare_lock(&second, Duration::from_secs(2))
            .await
            .unwrap();
        release.await.unwrap();
    }

    #[tokio::test]
    async fn cancelled_preparation_keeps_stage_and_exclusive_lock_until_blocking_work_finishes() {
        let directory = tempfile::tempdir().unwrap();
        let lock_path = directory.path().join("prepare.lock");
        let file = fs::OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&lock_path)
            .unwrap();
        let inherited = file.try_clone().unwrap();
        let lifecycle = Lifecycle::new();
        let mut lock =
            TrackedCacheLock::new(file, &lifecycle, "test Minecraft cache lock").unwrap();
        lock.try_lock().unwrap();
        let lock = std::sync::Arc::new(lock);
        let second = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(lock_path)
            .unwrap();
        let stage = std::sync::Arc::new(tempfile::tempdir_in(directory.path()).unwrap());
        let stage_path = stage.path().to_owned();
        let (started, running) = tokio::sync::oneshot::channel();
        let (release, released) = std::sync::mpsc::channel();
        let (completed, completion) = tokio::sync::oneshot::channel();
        let context = lifecycle.clone();
        let task = tokio::spawn(async move {
            preparation_task_tracked(
                stage,
                lock,
                move |stage| {
                    started.send(()).unwrap();
                    released.recv_timeout(Duration::from_secs(5)).unwrap();
                    let result = fs::write(stage.join("late-install.txt"), b"committed");
                    completed.send(result.is_ok()).unwrap();
                    result.map_err(BootstrapError::from)
                },
                &context,
                &context.token(),
            )
            .await
        });
        running.await.unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(stage_path.is_dir());
        assert!(matches!(
            second.try_lock(),
            Err(fs::TryLockError::WouldBlock)
        ));
        let pending = lifecycle
            .drain(tokio::time::Instant::now() + Duration::from_millis(20))
            .await;
        release.send(()).unwrap();
        assert!(completion.await.unwrap());
        lifecycle
            .drain(tokio::time::Instant::now() + Duration::from_secs(3))
            .await
            .unwrap();
        assert!(matches!(
            pending,
            Err(lifecycle::DrainError::Deadline { .. })
        ));
        assert!(!stage_path.exists());
        second.try_lock().unwrap();
        assert!(inherited.metadata().unwrap().is_file());
    }

    async fn generator_fixture(main_class: &str) -> (tempfile::TempDir, PathBuf, PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let java = java_runtime(Path::new("java")).await.unwrap();
        let bin = java.parent().unwrap();
        let classes = directory.path().join("classes");
        fs::create_dir(&classes).unwrap();
        let source = directory.path().join(format!("{main_class}.java"));
        fs::copy(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join(format!("tests/fixtures/java-bootstrap/{main_class}.java")),
            &source,
        )
        .unwrap();
        let compiled =
            std::process::Command::new(bin.join(if cfg!(windows) { "javac.exe" } else { "javac" }))
                .args(["--release", "25", "-encoding", "UTF-8", "-d"])
                .arg(&classes)
                .arg(source)
                .output()
                .unwrap();
        assert!(
            compiled.status.success(),
            "{}",
            String::from_utf8_lossy(&compiled.stderr)
        );
        let jar = directory.path().join("generator.jar");
        let packaged =
            std::process::Command::new(bin.join(if cfg!(windows) { "jar.exe" } else { "jar" }))
                .args(["--create", "--file"])
                .arg(&jar)
                .args(["--main-class", main_class, "-C"])
                .arg(classes)
                .arg(".")
                .output()
                .unwrap();
        assert!(
            packaged.status.success(),
            "{}",
            String::from_utf8_lossy(&packaged.stderr)
        );
        (directory, java, jar)
    }

    #[tokio::test]
    async fn report_generator_runs_with_utf8_stdout_stderr_and_unicode_output_path() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter("bootstrap=debug")
            .with_test_writer()
            .try_init();
        let (directory, java, jar) = generator_fixture("ReportCharsetProbe").await;
        let unicode_jar = directory.path().join("генератор-中文😀.jar");
        fs::copy(&jar, &unicode_jar).unwrap();
        for (index, jar) in [jar, unicode_jar].iter().enumerate() {
            let stage = std::sync::Arc::new(
                tempfile::Builder::new()
                    .prefix(&format!("данные-中文😀-Minecraft-{index}"))
                    .tempdir_in(directory.path())
                    .unwrap(),
            );
            generate_reports(&java, jar, std::sync::Arc::clone(&stage))
                .await
                .unwrap();
            assert_eq!(
                fs::read_to_string(stage.path().join("generated/charset-result.txt")).unwrap(),
                "Привет из генератора"
            );
        }
    }

    #[tokio::test]
    async fn cancelling_report_generation_reaps_jvm_before_removing_manifest() {
        let (directory, java, jar) = generator_fixture("ReportCancellationProbe").await;
        let output = std::sync::Arc::new(
            tempfile::Builder::new()
                .prefix("cancelled-中文😀")
                .tempdir_in(directory.path())
                .unwrap(),
        );
        let stage = output.path().to_owned();
        let lifecycle = Lifecycle::new();
        let context = lifecycle.clone();
        let task = tokio::spawn(async move {
            generate_reports_tracked(&java, &jar, output, &context, &context.token()).await
        });
        let started = tokio::time::Instant::now();
        while !stage.join("generator.ready").is_file() {
            assert!(
                !task.is_finished() && started.elapsed() < Duration::from_secs(15),
                "generator did not start"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let pid: u32 = fs::read_to_string(stage.join("generator.pid"))
            .unwrap()
            .parse()
            .unwrap();
        let manifest =
            PathBuf::from(fs::read_to_string(stage.join("generator.classpath")).unwrap());
        assert!(manifest.is_file());
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        lifecycle
            .drain(tokio::time::Instant::now() + Duration::from_secs(3))
            .await
            .unwrap();
        let alive = if cfg!(windows) {
            let output = std::process::Command::new("tasklist")
                .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
                .output()
                .unwrap();
            String::from_utf8_lossy(&output.stdout).contains(&format!("\"{pid}\""))
        } else {
            std::process::Command::new("kill")
                .args(["-0", &pid.to_string()])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap()
                .success()
        };
        assert!(
            !alive,
            "lifecycle drain acknowledged before generator {pid} was reaped"
        );
        assert!(
            !manifest.exists() && !stage.exists(),
            "lifecycle drain acknowledged before generator resources were removed"
        );
    }
}
