use std::{
    io::{BufRead, BufReader, Write},
    process::{Child, Command, Stdio},
    time::Duration,
};
use tokio::net::TcpStream;
#[cfg(unix)]
use tokio::time::timeout;
use zerqon_protocol::codec::{
    get_string, get_varint, put_string, put_varint, read_frame, write_frame,
};

struct Process(Child);

#[test]
fn network_settings_cannot_be_overridden_by_launch_flags() {
    let directory = tempfile::tempdir().unwrap();
    for flag in ["--port", "--bind", "--threads", "--world-threads"] {
        let result = Command::new(env!("CARGO_BIN_EXE_zerqon"))
            .args([
                "--init",
                "--directory",
                directory.path().to_str().unwrap(),
                flag,
                "4",
            ])
            .output()
            .unwrap();
        assert_eq!(
            result.status.code(),
            Some(2),
            "unexpectedly accepted {flag}"
        );
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[cfg(unix)]
#[tokio::test]
async fn termination_during_startup_reaps_the_java_probe_without_publishing_readiness() {
    struct Probe {
        pid: u32,
        executable: std::path::PathBuf,
    }
    impl Drop for Probe {
        fn drop(&mut self) {
            let path = format!("/proc/{}/exe", self.pid);
            if std::fs::read_link(path).ok().as_ref() == Some(&self.executable) {
                let _ = Command::new("kill")
                    .args(["-KILL", &self.pid.to_string()])
                    .status();
            }
        }
    }
    let fixture = tempfile::tempdir().unwrap();
    let java = fixture.path().join("java");
    let compiler = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let compiled = Command::new(compiler)
        .args(["--edition=2024", "-C", "debuginfo=0", "-o"])
        .arg(&java)
        .arg(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/startup_probe.rs"),
        )
        .output()
        .unwrap();
    assert!(
        compiled.status.success(),
        "{}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    for signal in ["-TERM", "-INT"] {
        let directory = tempfile::tempdir().unwrap();
        let binary = env!("CARGO_BIN_EXE_zerqon");
        assert!(
            Command::new(binary)
                .args(["--init", "--directory"])
                .arg(directory.path())
                .output()
                .unwrap()
                .status
                .success()
        );
        std::fs::write(directory.path().join("eula.txt"), "eula=true\n").unwrap();
        std::fs::write(
            directory.path().join("server.properties"),
            "server-ip=127.0.0.1\nserver-port=0\nonline-mode=false\n",
        )
        .unwrap();
        let settings = toml::Table::from_iter([
            (
                "java_executable".into(),
                toml::Value::String(java.to_string_lossy().into_owned()),
            ),
            ("auto_download_runtime".into(), toml::Value::Boolean(false)),
            ("java_plugins".into(), toml::Value::Boolean(false)),
            ("update_check".into(), toml::Value::Boolean(false)),
        ]);
        std::fs::write(
            directory.path().join("server.settings"),
            toml::to_string(&settings).unwrap(),
        )
        .unwrap();
        let marker = directory.path().join("probe.pid");
        let mut child = Process(
            Command::new(binary)
                .args(["--directory"])
                .arg(directory.path())
                .args(["--no-console", "--no-color"])
                .env("ZERQON_TEST_PROBE_PID", &marker)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let probe_pid = timeout(Duration::from_secs(10), async {
            loop {
                if let Ok(text) = std::fs::read_to_string(&marker)
                    && let Ok(pid) = text.parse::<u32>()
                {
                    break pid;
                }
                assert!(
                    child.0.try_wait().unwrap().is_none(),
                    "server exited before probe"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("probe did not start");
        let probe = Probe {
            pid: probe_pid,
            executable: java.canonicalize().unwrap(),
        };
        assert_eq!(
            std::fs::read_link(format!("/proc/{probe_pid}/exe")).unwrap(),
            probe.executable
        );
        assert!(
            Command::new("kill")
                .args([signal, &child.0.id().to_string()])
                .status()
                .unwrap()
                .success()
        );
        let status = timeout(Duration::from_secs(5), async {
            loop {
                if let Some(status) = child.0.try_wait().unwrap() {
                    break status;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("startup cancellation did not finish");
        let mut output = String::new();
        use std::io::Read;
        child
            .0
            .stdout
            .take()
            .unwrap()
            .read_to_string(&mut output)
            .unwrap();
        let orphan = std::path::Path::new(&format!("/proc/{probe_pid}")).exists();
        assert!(
            status.success(),
            "{signal} bypassed startup cleanup: {status}; orphan={orphan}"
        );
        assert!(!orphan, "Java probe was not reaped before server exit");
        assert!(!output.contains("Listening on "));
        assert!(
            !directory
                .path()
                .join(".cache/vanilla/26.3/prepared.json")
                .exists()
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn unix_termination_signals_stop_the_process_cleanly_after_readiness() {
    for signal in ["-TERM", "-INT"] {
        let directory = tempfile::tempdir().unwrap();
        let binary = env!("CARGO_BIN_EXE_zerqon");
        assert!(
            Command::new(binary)
                .args(["--init", "--directory"])
                .arg(directory.path())
                .output()
                .unwrap()
                .status
                .success()
        );
        std::fs::write(directory.path().join("eula.txt"), "eula=true\n").unwrap();
        std::fs::write(
            directory.path().join("server.properties"),
            "server-ip=127.0.0.1\nserver-port=0\n",
        )
        .unwrap();
        let mut child = Process(
            Command::new(binary)
                .args(["--status-only", "--no-console", "--no-color", "--directory"])
                .arg(directory.path())
                .env("RUST_LOG", "info")
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .spawn()
                .unwrap(),
        );
        let output = child.0.stdout.take().unwrap();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let observer = std::thread::spawn(move || {
            let mut stopped = false;
            for line in BufReader::new(output).lines().map_while(Result::ok) {
                if line.contains("Listening on ") {
                    let _ = ready_tx.send(());
                }
                stopped |= line.contains("Stopped cleanly");
            }
            stopped
        });
        ready_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("server readiness");
        assert!(
            Command::new("kill")
                .args([signal, &child.0.id().to_string()])
                .status()
                .unwrap()
                .success()
        );
        let status = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some(status) = child.0.try_wait().unwrap() {
                    break status;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("signal did not stop server");
        assert!(
            status.success(),
            "{signal} bypassed graceful shutdown: {status}"
        );
        assert!(
            observer.join().unwrap(),
            "shutdown cleanup was not reported"
        );
    }
}

#[tokio::test]
async fn status_diagnostics_do_not_start_plugin_runtimes() {
    let directory = tempfile::tempdir().unwrap();
    let binary = env!("CARGO_BIN_EXE_zerqon");
    assert!(
        Command::new(binary)
            .args(["--init", "--directory", directory.path().to_str().unwrap()])
            .output()
            .unwrap()
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
        "java_executable='missing-java'\n",
    )
    .unwrap();
    std::fs::write(
        directory.path().join("plugins/java-plugins/broken.jar"),
        b"not a JAR",
    )
    .unwrap();
    let stop = directory.path().join("stop");
    std::fs::write(&stop, "").unwrap();
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::process::Command::new(binary)
            .args([
                "--directory",
                directory.path().to_str().unwrap(),
                "--status-only",
                "--no-console",
                "--no-color",
            ])
            .env("ZERQON_SHUTDOWN_FILE", stop)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}

#[tokio::test]
async fn real_process_status_ping_and_console_shutdown() {
    let directory = tempfile::tempdir().unwrap();
    let binary = env!("CARGO_BIN_EXE_zerqon");
    assert!(
        Command::new(binary)
            .args(["--directory", directory.path().to_str().unwrap(), "--init"])
            .status()
            .unwrap()
            .success()
    );
    std::fs::write(
        directory.path().join("server.properties"),
        "server-ip=127.0.0.1\nserver-port=0\nonline-mode=false\nmotd=Integration server\n",
    )
    .unwrap();
    std::fs::write(directory.path().join("eula.txt"), "eula=true\n").unwrap();
    std::fs::write(
        directory.path().join("server.settings"),
        "update_check=false\n",
    )
    .unwrap();
    let mut child = Process(
        Command::new(binary)
            .env("RUST_LOG", "info")
            .args([
                "--directory",
                directory.path().to_str().unwrap(),
                "--status-only",
                "--no-color",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    let output = child.0.stdout.take().unwrap();
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(output).lines().map_while(Result::ok) {
            if let Some((_, addr)) = line.split_once("Listening on ") {
                let _ = sender.send(addr.trim().to_owned());
            }
        }
    });
    let address = receiver
        .recv_timeout(Duration::from_secs(10))
        .expect("server listening");
    let mut socket = TcpStream::connect(&address).await.unwrap();
    let mut handshake = vec![0];
    put_varint(777, &mut handshake);
    put_string("localhost", &mut handshake);
    handshake.extend_from_slice(&25565u16.to_be_bytes());
    handshake.push(1);
    write_frame(&mut socket, &handshake, None).await.unwrap();
    write_frame(&mut socket, &[0], None).await.unwrap();
    let packet = read_frame(&mut socket, 65536, None).await.unwrap();
    let mut view = packet.as_slice();
    assert_eq!(get_varint(&mut view).unwrap(), 0);
    let status: serde_json::Value =
        serde_json::from_str(get_string(&mut view, 32767).unwrap()).unwrap();
    assert_eq!(status["version"]["protocol"], 777);
    assert_eq!(status["description"]["text"], "Integration server");
    assert_eq!(status["players"]["online"], 0);
    let mut ping = vec![1];
    ping.extend_from_slice(&123456789i64.to_be_bytes());
    write_frame(&mut socket, &ping, None).await.unwrap();
    assert_eq!(read_frame(&mut socket, 64, None).await.unwrap(), ping);
    let mut stdin = child.0.stdin.take().unwrap();
    writeln!(stdin, "stop").unwrap();
    drop(stdin);
    let status = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(status) = child.0.try_wait().unwrap() {
                break status;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert!(status.success());
}
