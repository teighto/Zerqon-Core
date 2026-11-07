use super::*;
use tokio::{net::TcpStream, sync::oneshot};
use zerqon_protocol::codec::{
    get_string, get_varint, put_string, put_varint, read_frame, write_frame,
};

#[tokio::test]
async fn stop_after_readiness_before_worker_registration_is_a_clean_stop() {
    let lifecycle = crate::lifecycle::Lifecycle::new();
    lifecycle.mark_ready().unwrap();
    lifecycle.stop();
    assert!(
        matches!(ready_jobs(&lifecycle), Ok(None)),
        "a latched stop before worker registration became a startup failure"
    );
    lifecycle
        .drain(tokio::time::Instant::now() + Duration::from_secs(1))
        .await
        .unwrap();
}

#[tokio::test]
async fn ready_worker_capacity_and_sealed_errors_are_not_hidden_as_expected_stop() {
    let lifecycle = crate::lifecycle::Lifecycle::new();
    lifecycle.mark_ready().unwrap();
    let jobs: Vec<_> = (0..4096)
        .map(|_| lifecycle.register("occupied").unwrap())
        .collect();
    let Err(error) = ready_jobs(&lifecycle) else {
        panic!("worker capacity error was hidden");
    };
    assert!(matches!(
        error.downcast_ref::<crate::lifecycle::RegistrationError>(),
        Some(crate::lifecycle::RegistrationError::Capacity)
    ));
    for job in jobs {
        job.finish(Ok(()));
    }
    lifecycle.stop();
    lifecycle
        .drain(tokio::time::Instant::now() + Duration::from_secs(1))
        .await
        .unwrap();
    let Err(error) = ready_jobs(&lifecycle) else {
        panic!("sealed lifecycle was hidden by its stopped flag");
    };
    assert!(matches!(
        error.downcast_ref::<crate::lifecycle::RegistrationError>(),
        Some(crate::lifecycle::RegistrationError::Sealed)
    ));
}

#[tokio::test]
async fn full_console_queue_does_not_hide_stop_or_diagnostics() {
    let lifecycle = crate::lifecycle::Lifecycle::new();
    let (commands, mut queued) = mpsc::channel(32);
    let (controls, mut diagnostic) = mpsc::channel(8);
    for number in 0..40 {
        assert!(submit_console_line(
            format!("command{number}"),
            &commands,
            &controls,
            &lifecycle
        ));
    }
    assert_eq!(queued.len(), 32);
    for line in ["version", "/tps", "help"] {
        assert!(submit_console_line(
            line.into(),
            &commands,
            &controls,
            &lifecycle
        ));
    }
    assert!(matches!(diagnostic.try_recv(), Ok(ConsoleControl::Version)));
    assert!(matches!(diagnostic.try_recv(), Ok(ConsoleControl::Tps)));
    assert!(matches!(diagnostic.try_recv(), Ok(ConsoleControl::Help)));
    assert!(!submit_console_line(
        "/stop".into(),
        &commands,
        &controls,
        &lifecycle
    ));
    assert!(lifecycle.token().is_stopped());
    for number in 0..32 {
        assert_eq!(queued.try_recv().unwrap(), format!("command{number}"));
    }
    drop(queued);
    assert!(!submit_console_line(
        "another".into(),
        &commands,
        &controls,
        &lifecycle
    ));
}

async fn status_ping(address: std::net::SocketAddr) {
    let mut socket = TcpStream::connect(address).await.unwrap();
    let mut handshake = vec![0];
    put_varint(777, &mut handshake);
    put_string("localhost", &mut handshake);
    handshake.extend_from_slice(&address.port().to_be_bytes());
    handshake.push(1);
    write_frame(&mut socket, &handshake, None).await.unwrap();
    write_frame(&mut socket, &[0], None).await.unwrap();
    let response = read_frame(&mut socket, 65536, None).await.unwrap();
    let mut body = response.as_slice();
    assert_eq!(get_varint(&mut body).unwrap(), 0);
    let status: serde_json::Value =
        serde_json::from_str(get_string(&mut body, 32767).unwrap()).unwrap();
    assert_eq!(status["version"]["protocol"], 777);
    let ping = [1, 0, 0, 0, 0, 0, 0, 0, 99];
    write_frame(&mut socket, &ping, None).await.unwrap();
    assert_eq!(read_frame(&mut socket, 64, None).await.unwrap(), ping);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_tcp_status_and_stop_remain_responsive_while_tick_or_console_work_is_paused() {
    for kind in [JobKind::Tick, JobKind::Console] {
        let directory = tempfile::tempdir().unwrap();
        crate::config::initialize(directory.path()).unwrap();
        let lifecycle = crate::lifecycle::Lifecycle::new();
        let state = State::new_with_lifecycle(
            directory.path(),
            Properties::parse("server-ip=127.0.0.1\nserver-port=0\nonline-mode=false\n").unwrap(),
            Settings {
                java_plugins: false,
                update_check: false,
                ..Settings::default()
            },
            true,
            None,
            &lifecycle,
            &lifecycle.token(),
        )
        .await
        .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let mut jobs = OrderedJobs::new(&lifecycle).unwrap();
        let (started, ready) = oneshot::channel();
        let (release, blocked) = oneshot::channel();
        jobs.submit(kind, async move {
            started.send(()).unwrap();
            blocked.await?;
            Ok(())
        })
        .unwrap();
        ready.await.unwrap();
        let (commands, receiver) = mpsc::channel(32);
        let (controls, diagnostics) = mpsc::channel(8);
        let loop_state = Arc::clone(&state);
        let loop_lifecycle = lifecycle.clone();
        let server = tokio::spawn(async move {
            accept_loop(
                listener,
                loop_state,
                receiver,
                diagnostics,
                true,
                &loop_lifecycle,
                jobs,
            )
            .await
        });
        submit_console_line("version".into(), &commands, &controls, &lifecycle);
        submit_console_line("tps".into(), &commands, &controls, &lifecycle);
        for _ in 0..3 {
            tokio::time::timeout(Duration::from_secs(2), status_ping(address))
                .await
                .expect("accept/status blocked behind server work");
        }
        assert!(!server.is_finished());
        assert!(!submit_console_line(
            "stop".into(),
            &commands,
            &controls,
            &lifecycle
        ));
        assert!(lifecycle.token().is_stopped());
        let mut server = server;
        assert!(
            tokio::time::timeout(Duration::from_millis(30), &mut server)
                .await
                .is_err(),
            "accepted work was abandoned instead of drained"
        );
        release.send(()).unwrap();
        let failures = tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap();
        assert!(failures.is_empty(), "{failures:?}");
        shutdown_components(&state).await.unwrap();
        lifecycle
            .drain(tokio::time::Instant::now() + Duration::from_secs(2))
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn queued_tick_work_does_not_record_scheduler_wait_as_tick_work() {
    let lifecycle = crate::lifecycle::Lifecycle::new();
    let directory = tempfile::tempdir().unwrap();
    crate::config::initialize(directory.path()).unwrap();
    let state = State::new_with_lifecycle(
        directory.path(),
        Properties::parse("online-mode=false").unwrap(),
        Settings {
            java_plugins: false,
            update_check: false,
            ..Settings::default()
        },
        true,
        None,
        &lifecycle,
        &lifecycle.token(),
    )
    .await
    .unwrap();
    let mut jobs = OrderedJobs::new(&lifecycle).unwrap();
    let (started, ready) = oneshot::channel();
    let (release, blocked) = oneshot::channel();
    jobs.submit(JobKind::Console, async move {
        started.send(()).unwrap();
        blocked.await?;
        Ok(())
    })
    .unwrap();
    ready.await.unwrap();
    jobs.submit(JobKind::Tick, run_tick(Arc::clone(&state), 1))
        .unwrap();
    tokio::time::sleep(Duration::from_millis(60)).await;
    let released = Instant::now();
    release.send(()).unwrap();
    jobs.next().await.unwrap();
    jobs.next().await.unwrap();
    {
        let samples = state.stats.lock().unwrap();
        assert_eq!(samples.samples.len(), 1);
        assert!(samples.samples[0].0 >= released);
    }
    assert!(jobs.drain().await.is_empty());
    lifecycle.stop();
    shutdown_components(&state).await.unwrap();
    lifecycle
        .drain(tokio::time::Instant::now() + Duration::from_secs(2))
        .await
        .unwrap();
}

#[tokio::test]
async fn ready_work_error_stops_the_loop_and_survives_the_returned_failure_report() {
    let lifecycle = crate::lifecycle::Lifecycle::new();
    let directory = tempfile::tempdir().unwrap();
    crate::config::initialize(directory.path()).unwrap();
    let state = State::new_with_lifecycle(
        directory.path(),
        Properties::parse("online-mode=false").unwrap(),
        Settings {
            java_plugins: false,
            update_check: false,
            ..Settings::default()
        },
        true,
        None,
        &lifecycle,
        &lifecycle.token(),
    )
    .await
    .unwrap();
    let mut jobs = OrderedJobs::new(&lifecycle).unwrap();
    jobs.submit(JobKind::Tick, async {
        Err("injected world work failure".into())
    })
    .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (_commands, receiver) = mpsc::channel(32);
    let (_controls, diagnostics) = mpsc::channel(8);
    let failures = tokio::time::timeout(
        Duration::from_secs(2),
        accept_loop(
            listener,
            Arc::clone(&state),
            receiver,
            diagnostics,
            false,
            &lifecycle,
            jobs,
        ),
    )
    .await
    .unwrap();
    assert!(lifecycle.token().is_stopped());
    assert!(
        failures
            .iter()
            .any(|failure| failure.contains("injected world work failure"))
    );
    shutdown_components(&state).await.unwrap();
    assert!(
        lifecycle
            .drain(tokio::time::Instant::now() + Duration::from_secs(1))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn invalid_console_requests_do_not_stop_the_server_or_fail_its_lifecycle() {
    let lifecycle = crate::lifecycle::Lifecycle::new();
    let directory = tempfile::tempdir().unwrap();
    crate::config::initialize(directory.path()).unwrap();
    let state = State::new_with_lifecycle(
        directory.path(),
        Properties::parse("online-mode=false").unwrap(),
        Settings {
            java_plugins: false,
            update_check: false,
            ..Settings::default()
        },
        true,
        None,
        &lifecycle,
        &lifecycle.token(),
    )
    .await
    .unwrap();
    let mut jobs = OrderedJobs::new(&lifecycle).unwrap();
    for line in [
        "setblock nope 64 0 1",
        "give Missing minecraft:stone",
        "op Missing",
    ] {
        jobs.submit(
            JobKind::Console,
            run_console(Arc::clone(&state), line.into()),
        )
        .unwrap();
        assert!(
            jobs.next().await.unwrap().result.is_ok(),
            "ordinary request rejection became a job failure: {line}"
        );
        assert!(!lifecycle.token().is_stopped());
    }
    assert!(jobs.drain().await.is_empty());
    lifecycle.stop();
    shutdown_components(&state).await.unwrap();
    lifecycle
        .drain(tokio::time::Instant::now() + Duration::from_secs(1))
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_plugin_fuel_fault_disables_only_that_plugin_and_status_remains_available() {
    let lifecycle = crate::lifecycle::Lifecycle::new();
    let directory = tempfile::tempdir().unwrap();
    crate::config::initialize(directory.path()).unwrap();
    let state = State::new_with_lifecycle(
        directory.path(),
        Properties::parse("online-mode=false").unwrap(),
        Settings {
            java_plugins: false,
            update_check: false,
            ..Settings::default()
        },
        true,
        None,
        &lifecycle,
        &lifecycle.token(),
    )
    .await
    .unwrap();
    let faulty = wat::parse_str(
        r#"(module
        (import "zerqon" "event_len" (func $len (result i32)))
        (import "zerqon" "event_read" (func $read (param i32 i32) (result i32)))
        (memory (export "memory") 1)
        (func (export "zerqon_abi_version") (result i32) i32.const 1)
        (func (export "zerqon_on_event") (result i32)
            i32.const 0 call $len call $read drop
            i32.const 10 i32.load8_u i32.const 116 i32.eq if (loop $again br $again) end
            i32.const 0))"#,
    )
    .unwrap();
    let healthy = wat::parse_str(
        r#"(module (memory (export "memory") 1)
        (func (export "zerqon_abi_version") (result i32) i32.const 1)
        (func (export "zerqon_on_event") (result i32) i32.const 0))"#,
    )
    .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    state
        .plugins
        .load("faulty", &faulty, &lifecycle.token(), deadline)
        .await
        .unwrap();
    state
        .plugins
        .load("healthy", &healthy, &lifecycle.token(), deadline)
        .await
        .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let jobs = OrderedJobs::new(&lifecycle).unwrap();
    let (_commands, receiver) = mpsc::channel(32);
    let (_controls, diagnostics) = mpsc::channel(8);
    let loop_state = Arc::clone(&state);
    let loop_lifecycle = lifecycle.clone();
    let server = tokio::spawn(async move {
        accept_loop(
            listener,
            loop_state,
            receiver,
            diagnostics,
            false,
            &loop_lifecycle,
            jobs,
        )
        .await
    });
    tokio::time::timeout(Duration::from_secs(3), async {
        while !state
            .plugins
            .list()
            .iter()
            .any(|plugin| plugin.name == "faulty" && !plugin.enabled)
        {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("faulty plugin was not isolated");
    assert!(
        state
            .plugins
            .list()
            .iter()
            .any(|plugin| plugin.name == "healthy" && plugin.enabled)
    );
    assert!(!lifecycle.token().is_stopped());
    tokio::time::timeout(Duration::from_secs(2), status_ping(address))
        .await
        .unwrap();
    lifecycle.stop();
    assert!(
        tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap()
            .is_empty()
    );
    shutdown_components(&state).await.unwrap();
    lifecycle
        .drain(tokio::time::Instant::now() + Duration::from_secs(2))
        .await
        .unwrap();
}
