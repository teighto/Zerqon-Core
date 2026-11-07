#[path = "../src/java_launch.rs"]
#[allow(dead_code)]
pub mod java_launch;
#[allow(dead_code)]
pub mod bootstrap_under_review {
    include!("../src/bootstrap.rs");
    pub fn install_fixture(
        stage: &std::path::Path,
        destination: &std::path::Path,
    ) -> Result<(), BootstrapError> {
        install_prepared(stage, destination, &serde_json::json!({}))
    }
}

#[allow(dead_code)]
pub mod runtime_under_review {
    include!("../src/java_runtime.rs");
    #[cfg(windows)]
    pub fn fallback_fixture(
        cache: &std::path::Path,
        local_data: &std::path::Path,
    ) -> Result<std::path::PathBuf, JavaRuntimeError> {
        windows_cache_location(cache, Some(local_data))
    }
    pub async fn probe_fixture(
        runtime: &std::path::Path,
    ) -> Result<std::path::PathBuf, JavaRuntimeError> {
        let asset = current_asset()?;
        std::fs::write(
            runtime.join("zerqon-runtime.json"),
            serde_json::to_vec(&serde_json::json!({
                "release": RELEASE, "sha256": asset.sha256, "size": asset.size, "platform": asset.platform.cache_name()
            }))?,
        )?;
        let lifecycle = Lifecycle::new();
        Ok(
            validated_cached_runtime_tracked(runtime, asset, (), &lifecycle, &lifecycle.token())
                .await?
                .0,
        )
    }
}

#[cfg(windows)]
#[test]
fn runtime_fallback_rejects_links_in_requested_and_selected_caches() {
    let directory = tempfile::tempdir().unwrap();
    let outside = directory.path().join("outside");
    let local_data = directory.path().join("profile");
    std::fs::create_dir(&outside).unwrap();
    std::fs::create_dir(&local_data).unwrap();
    std::fs::write(outside.join("original"), b"preserved").unwrap();
    let cache = directory.path().join("中文");
    directory_link(&outside, &cache);
    assert!(runtime_under_review::fallback_fixture(&cache, &local_data).is_err());
    directory_link(&outside, &local_data.join("Zerqon"));
    assert!(
        runtime_under_review::fallback_fixture(&directory.path().join("另一个"), &local_data)
            .is_err()
    );
    assert_eq!(
        std::fs::read(outside.join("original")).unwrap(),
        b"preserved"
    );
    assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 1);
}

fn directory_link(target: &std::path::Path, link: &std::path::Path) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let script = format!(
            "New-Item -ItemType Junction -Path '{}' -Target '{}' | Out-Null",
            link.display().to_string().replace('\'', "''"),
            target.display().to_string().replace('\'', "''")
        );
        let output = std::process::Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .creation_flags(0x08000000)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, link).unwrap();
}

#[test]
fn bootstrap_cache_link_must_not_overwrite_files_outside_cache() {
    let directory = tempfile::tempdir().unwrap();
    let stage = directory.path().join("stage");
    let cache = directory.path().join("cache");
    let outside = directory.path().join("outside");
    for path in [stage.join("data"), cache.clone(), outside.clone()] {
        std::fs::create_dir_all(path).unwrap();
    }
    std::fs::write(stage.join("data/important.json"), b"new generated data").unwrap();
    std::fs::write(outside.join("important.json"), b"user-owned original").unwrap();
    directory_link(&outside, &cache.join("data"));
    let _ = bootstrap_under_review::install_fixture(&stage, &cache);
    assert_eq!(
        std::fs::read(outside.join("important.json")).unwrap(),
        b"user-owned original"
    );
}

#[tokio::test]
async fn managed_java_must_reject_bin_link_before_executing_outside_binary() {
    let directory = tempfile::tempdir().unwrap();
    let runtime = directory.path().join("runtime");
    let outside = directory.path().join("outside");
    std::fs::create_dir_all(&runtime).unwrap();
    std::fs::create_dir_all(outside.join("bin")).unwrap();
    let source = directory.path().join("fake_java.rs");
    std::fs::write(
        &source,
        r#"fn main() {
        let binary = std::env::current_exe().unwrap().canonicalize().unwrap();
        let bin = binary.parent().unwrap();
        std::fs::write(bin.join("executed"), b"executed before cache boundary validation").unwrap();
        eprintln!("java.home = {}", bin.parent().unwrap().display());
        eprintln!("java.specification.version = 25");
    }"#,
    )
    .unwrap();
    let binary = outside
        .join("bin")
        .join(if cfg!(windows) { "java.exe" } else { "java" });
    let compiled = std::process::Command::new("rustc")
        .arg("--edition=2024")
        .arg("--crate-name=audit_java")
        .arg(&source)
        .arg("-o")
        .arg(&binary)
        .output()
        .unwrap();
    assert!(
        compiled.status.success(),
        "{}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    directory_link(&outside.join("bin"), &runtime.join("bin"));
    assert!(runtime_under_review::probe_fixture(&runtime).await.is_err());
    assert!(
        !outside.join("bin/executed").exists(),
        "outside binary ran before the boundary check"
    );
}

#[tokio::test]
async fn managed_java_rejects_library_junction_before_executing_the_launcher() {
    let directory = tempfile::tempdir().unwrap();
    let runtime = directory.path().join("runtime");
    let outside = directory.path().join("outside");
    std::fs::create_dir_all(runtime.join("bin")).unwrap();
    std::fs::create_dir(&outside).unwrap();
    let source = directory.path().join("fake_java.rs");
    std::fs::write(&source,r#"fn main(){let executable=std::env::current_exe().unwrap();let bin=executable.parent().unwrap();std::fs::write(bin.join("executed"),b"ran").unwrap();eprintln!("java.home = {}",bin.parent().unwrap().display());eprintln!("java.specification.version = 25");}"#).unwrap();
    let binary = runtime
        .join("bin")
        .join(if cfg!(windows) { "java.exe" } else { "java" });
    let compiled = std::process::Command::new("rustc")
        .arg("--edition=2024")
        .arg("--crate-name=audit_java_library")
        .arg(source)
        .arg("-o")
        .arg(binary)
        .output()
        .unwrap();
    assert!(compiled.status.success());
    directory_link(&outside, &runtime.join("lib"));
    assert!(runtime_under_review::probe_fixture(&runtime).await.is_err());
    assert!(!runtime.join("bin/executed").exists());
}
#[path = "../src/cache_lock.rs"]
#[allow(dead_code)]
mod cache_lock;
#[path = "../src/lifecycle.rs"]
#[allow(dead_code)]
mod lifecycle;
#[path = "../src/managed_tasks.rs"]
#[allow(dead_code)]
mod managed_tasks;
