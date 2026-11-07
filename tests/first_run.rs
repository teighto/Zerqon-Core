use std::{path::Path, process::Stdio, time::Duration};
use tokio::{io::AsyncWriteExt, process::Command, time::timeout};

async fn run(
    directory: &Path,
    extra: &[&str],
    input: Option<&[u8]>,
    deadline: Duration,
) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_zerqon"))
        .env("RUST_LOG", "info")
        .arg("--directory")
        .arg(directory)
        .arg("--no-color")
        .args(extra)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    if let Some(input) = input {
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(input).await.unwrap();
    }
    timeout(deadline, child.wait_with_output())
        .await
        .expect("first-run process timed out")
        .unwrap()
}

#[tokio::test]
async fn first_launch_creates_config_but_requires_explicit_eula_before_any_download() {
    let directory = tempfile::tempdir().unwrap();
    let output = run(directory.path(), &[], None, Duration::from_secs(5)).await;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("eula=true"));
    assert_eq!(
        std::fs::read_dir(directory.path().join(".cache"))
            .unwrap()
            .count(),
        0
    );
    assert_eq!(
        std::fs::read_dir(directory.path().join("libraries"))
            .unwrap()
            .count(),
        0
    );
    assert!(!directory.path().join("world").exists());
    assert!(!directory.path().join("logs/latest.log").exists());
    assert!(
        std::fs::read_to_string(directory.path().join("eula.txt"))
            .unwrap()
            .contains("eula=false")
    );
}

#[tokio::test]
async fn duplicate_eula_values_fail_closed_and_preserve_original_bytes() {
    let directory = tempfile::tempdir().unwrap();
    assert!(
        run(directory.path(), &["--init"], None, Duration::from_secs(5))
            .await
            .status
            .success()
    );
    let original = b"eula=true\neula=false\n";
    std::fs::write(directory.path().join("eula.txt"), original).unwrap();
    let output = run(directory.path(), &[], None, Duration::from_secs(5)).await;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("duplicate"));
    assert_eq!(
        std::fs::read(directory.path().join("eula.txt")).unwrap(),
        original
    );
    assert_eq!(
        std::fs::read_dir(directory.path().join(".cache"))
            .unwrap()
            .count(),
        0
    );
}

#[tokio::test]
async fn missing_explicit_java_path_is_not_replaced_by_an_automatic_download() {
    let directory = tempfile::tempdir().unwrap();
    assert!(
        run(directory.path(), &["--init"], None, Duration::from_secs(5))
            .await
            .status
            .success()
    );
    std::fs::write(directory.path().join("eula.txt"), "eula=true\n").unwrap();
    std::fs::write(directory.path().join("server.settings"),"java_executable='missing-custom-java-for-regression'\nauto_download_runtime=true\nupdate_check=false\n").unwrap();
    let output = run(directory.path(), &[], None, Duration::from_secs(5)).await;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("configured Java executable"));
    assert!(!directory.path().join(".cache/java").exists());
    assert!(
        !directory
            .path()
            .join(".cache/vanilla/26.3/server.jar")
            .exists()
    );
}

#[tokio::test]
async fn legacy_name_only_operators_prevent_startup_without_rewriting_user_records() {
    let directory = tempfile::tempdir().unwrap();
    assert!(
        run(directory.path(), &["--init"], None, Duration::from_secs(5))
            .await
            .status
            .success()
    );
    std::fs::write(directory.path().join("eula.txt"), "eula=true\n").unwrap();
    std::fs::write(
        directory.path().join("server.properties"),
        "server-ip=127.0.0.1\nserver-port=0\n",
    )
    .unwrap();
    std::fs::write(
        directory.path().join("server.settings"),
        "update_check=false\n",
    )
    .unwrap();
    let original = b"[\"FormerAdmin\"]";
    std::fs::write(directory.path().join("ops.json"), original).unwrap();
    let output = run(
        directory.path(),
        &["--status-only"],
        None,
        Duration::from_secs(5),
    )
    .await;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("name-only ops.json"));
    assert_eq!(
        std::fs::read(directory.path().join("ops.json")).unwrap(),
        original
    );
    assert!(!directory.path().join("ops.json.lock").exists());
}

#[tokio::test]
#[ignore = "Downloads official Minecraft data from a fresh server directory; uses Java25 or managed runtime"]
async fn accepted_fresh_launch_prepares_data_starts_and_stops_without_python() {
    let directory = tempfile::tempdir().unwrap();
    assert!(
        run(directory.path(), &["--init"], None, Duration::from_secs(5))
            .await
            .status
            .success()
    );
    std::fs::write(directory.path().join("eula.txt"), "eula=true\n").unwrap();
    std::fs::write(
        directory.path().join("server.properties"),
        "server-ip=127.0.0.1\nserver-port=0\nview-distance=1\n",
    )
    .unwrap();
    std::fs::write(
        directory.path().join("server.settings"),
        "update_check=false\nspawn_mobs=false\n",
    )
    .unwrap();
    let output = run(
        directory.path(),
        &[],
        Some(b"stop\n"),
        Duration::from_secs(120),
    )
    .await;
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let log = String::from_utf8_lossy(&output.stdout);
    assert!(log.contains("Game data ready"));
    assert!(log.contains("Listening on 127.0.0.1:"));
    assert!(
        directory
            .path()
            .join(".cache/vanilla/26.3/prepared.json")
            .is_file()
    );
    assert!(directory.path().join("world/world.lock").is_file());
}
