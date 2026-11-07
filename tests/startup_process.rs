use std::{
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::Duration,
};

struct Server {
    child: Child,
    probe_pid: PathBuf,
    probe_executable: PathBuf,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Ok(text) = std::fs::read_to_string(&self.probe_pid)
            && let Ok(pid) = text.parse::<u32>()
        {
            kill_owned_probe(pid, &self.probe_executable);
        }
    }
}

#[cfg(windows)]
fn kill_owned_probe(pid: u32, executable: &Path) {
    use std::os::windows::process::CommandExt;
    let path = executable.to_string_lossy();
    let path = path
        .strip_prefix(r"\\?\")
        .unwrap_or(&path)
        .replace('\'', "''");
    let script = format!(
        "$zerqonProbe=Get-Process -Id {pid} -ErrorAction SilentlyContinue; if ($zerqonProbe -and $zerqonProbe.Path -eq '{path}') {{ $zerqonProbe.Kill(); $zerqonProbe.WaitForExit(5000) | Out-Null }}"
    );
    let _ = Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .creation_flags(0x0800_0000)
        .output();
}

#[cfg(not(windows))]
fn kill_owned_probe(pid: u32, executable: &Path) {
    if std::fs::read_link(format!("/proc/{pid}/exe"))
        .ok()
        .as_deref()
        == Some(executable)
    {
        let _ = Command::new("kill")
            .args(["-KILL", &pid.to_string()])
            .output();
    }
}

fn probe_alive(pid: u32) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let result = Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
            .creation_flags(0x0800_0000)
            .output()
            .unwrap();
        assert!(result.status.success());
        String::from_utf8_lossy(&result.stdout).contains(&format!("\"{pid}\""))
    }
    #[cfg(not(windows))]
    {
        Path::new("/proc").join(pid.to_string()).exists()
    }
}

#[tokio::test]
async fn shutdown_marker_during_java_probe_reaps_before_exit() {
    let directory = tempfile::tempdir().unwrap();
    let probe = directory.path().join(if cfg!(windows) {
        "startup-probe.exe"
    } else {
        "startup-probe"
    });
    let compiler = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let compiled = Command::new(compiler)
        .args(["--edition=2024", "-C", "debuginfo=0", "-o"])
        .arg(&probe)
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/startup_probe.rs"))
        .output()
        .unwrap();
    assert!(
        compiled.status.success(),
        "{}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    let server_directory = directory.path().join("server");
    let binary = env!("CARGO_BIN_EXE_zerqon");
    let initialized = Command::new(binary)
        .arg("--init")
        .arg("--directory")
        .arg(&server_directory)
        .output()
        .unwrap();
    assert!(initialized.status.success());
    std::fs::write(server_directory.join("eula.txt"), "eula=true\n").unwrap();
    std::fs::write(
        server_directory.join("server.properties"),
        "server-ip=127.0.0.1\nserver-port=0\nonline-mode=false\n",
    )
    .unwrap();
    let settings = toml::Table::from_iter([
        (
            "java_executable".into(),
            toml::Value::String(probe.to_string_lossy().into_owned()),
        ),
        ("auto_download_runtime".into(), toml::Value::Boolean(false)),
        ("java_plugins".into(), toml::Value::Boolean(false)),
        ("update_check".into(), toml::Value::Boolean(false)),
    ]);
    std::fs::write(
        server_directory.join("server.settings"),
        toml::to_string(&settings).unwrap(),
    )
    .unwrap();
    let pid_file = directory.path().join("probe.pid");
    let stop_file = directory.path().join("stop");
    let stdout = directory.path().join("stdout.log");
    let stderr = directory.path().join("stderr.log");
    let mut server = Server {
        child: Command::new(binary)
            .arg("--directory")
            .arg(&server_directory)
            .args(["--no-console", "--no-color"])
            .env("ZERQON_TEST_PROBE_PID", &pid_file)
            .env("ZERQON_SHUTDOWN_FILE", &stop_file)
            .stdin(Stdio::null())
            .stdout(std::fs::File::create(&stdout).unwrap())
            .stderr(std::fs::File::create(&stderr).unwrap())
            .spawn()
            .unwrap(),
        probe_pid: pid_file.clone(),
        probe_executable: probe.canonicalize().unwrap(),
    };
    let pid = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Ok(text) = std::fs::read_to_string(&pid_file)
                && let Ok(pid) = text.parse::<u32>()
            {
                break pid;
            }
            assert!(
                server.child.try_wait().unwrap().is_none(),
                "server exited before Java probe"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("probe did not start");
    assert!(probe_alive(pid));
    std::fs::write(&stop_file, b"stop").unwrap();
    let status = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(status) = server.child.try_wait().unwrap() {
                break status;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("startup shutdown marker was ignored");
    let output = std::fs::read_to_string(stdout).unwrap();
    let errors = std::fs::read_to_string(stderr).unwrap();
    assert!(
        status.success(),
        "startup cancellation failed: {status}\n{output}\n{errors}"
    );
    assert!(!output.contains("Listening on ") && !errors.contains("Listening on "));
    assert!(
        !probe_alive(pid),
        "server exited before Java probe was reaped"
    );
    assert!(!server_directory.join(".cache/java/25").exists());
    assert!(
        !server_directory
            .join(".cache/vanilla/26.3/prepared.json")
            .exists()
    );
}
