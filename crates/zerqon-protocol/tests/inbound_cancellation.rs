use std::{sync::Arc, time::Duration};
use tokio::{
    io::AsyncWriteExt,
    sync::{mpsc, oneshot},
};
use zerqon_protocol::inbound::{PacketBudget, read_frame_budgeted};

async fn wait_until(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while !condition() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn cancelling_a_partial_raw_body_returns_the_whole_reservation() {
    let budget = PacketBudget::new(64).unwrap();
    let connection = budget.connection(32).unwrap();
    let account = connection.clone();
    let (mut source, mut destination) = tokio::io::duplex(64);
    source.write_all(&[32, 7]).await.unwrap();
    let task =
        tokio::spawn(
            async move { read_frame_budgeted(&mut destination, 64, None, &account).await },
        );
    wait_until(|| budget.snapshot().used_bytes == 32).await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert_eq!(budget.snapshot().used_bytes, 0);
    assert_eq!(connection.snapshot().used_bytes, 0);
    assert_eq!(budget.snapshot().peak_bytes, 32);
}

#[tokio::test]
async fn cancelling_compressed_input_returns_both_pre_reserved_buffers() {
    let budget = PacketBudget::new(64).unwrap();
    let connection = budget.connection(32).unwrap();
    let account = connection.clone();
    let (mut source, mut destination) = tokio::io::duplex(64);
    source.write_all(&[12, 3, 0x78]).await.unwrap();
    let task =
        tokio::spawn(
            async move { read_frame_budgeted(&mut destination, 64, Some(0), &account).await },
        );
    wait_until(|| budget.snapshot().used_bytes == 14).await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert_eq!(budget.snapshot().used_bytes, 0);
    assert_eq!(connection.snapshot().used_bytes, 0);
    assert_eq!(budget.snapshot().peak_bytes, 14);
}

#[tokio::test]
async fn cancelling_partial_headers_never_reserves_body_memory() {
    for (prefix, compression) in [(vec![128], None), (vec![12, 128], Some(0))] {
        let budget = PacketBudget::new(64).unwrap();
        let connection = budget.connection(64).unwrap();
        let (mut source, mut destination) = tokio::io::duplex(64);
        source.write_all(&prefix).await.unwrap();
        let task = tokio::spawn(async move {
            read_frame_budgeted(&mut destination, 64, compression, &connection).await
        });
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
        assert!(!task.is_finished());
        assert_eq!(budget.snapshot().used_bytes, 0);
        assert_eq!(budget.snapshot().peak_bytes, 0);
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_eq!(budget.snapshot().used_bytes, 0);
    }
}

#[tokio::test]
async fn queue_backpressure_and_receiver_drop_preserve_then_release_packet_leases() {
    let budget = PacketBudget::new(8).unwrap();
    let connection = budget.connection(8).unwrap();
    let first = read_frame_budgeted(&mut &[4, 7, 1, 2, 3][..], 64, None, &connection)
        .await
        .unwrap();
    let second = read_frame_budgeted(&mut &[4, 7, 1, 2, 4][..], 64, None, &connection)
        .await
        .unwrap();
    let (sender, receiver) = mpsc::channel(1);
    sender.send(first).await.unwrap();
    let blocked = tokio::spawn(async move { sender.send(second).await });
    tokio::task::yield_now().await;
    assert!(!blocked.is_finished());
    assert_eq!(budget.snapshot().used_bytes, 8);
    blocked.abort();
    assert!(blocked.await.unwrap_err().is_cancelled());
    assert_eq!(budget.snapshot().used_bytes, 4);
    drop(receiver);
    assert_eq!(budget.snapshot().used_bytes, 0);
    assert_eq!(connection.snapshot().used_bytes, 0);
}

#[tokio::test]
async fn completed_but_unread_task_output_keeps_its_charge() {
    let budget = PacketBudget::new(4).unwrap();
    let connection = budget.connection(4).unwrap();
    let task = tokio::spawn(async move {
        read_frame_budgeted(&mut &[4, 7, 1, 2, 3][..], 64, None, &connection)
            .await
            .unwrap()
    });
    wait_until(|| task.is_finished()).await;
    assert_eq!(budget.snapshot().used_bytes, 4);
    drop(task);
    wait_until(|| budget.snapshot().used_bytes == 0).await;
}

#[tokio::test]
async fn aborting_a_handler_that_owns_the_packet_releases_its_charge() {
    let budget = PacketBudget::new(4).unwrap();
    let connection = budget.connection(4).unwrap();
    let packet = read_frame_budgeted(&mut &[4, 7, 1, 2, 3][..], 64, None, &connection)
        .await
        .unwrap();
    let (started, ready) = oneshot::channel();
    let (_release, finish) = oneshot::channel::<()>();
    let task = tokio::spawn(async move {
        started.send(()).unwrap();
        let _ = finish.await;
        assert_eq!(packet.as_slice(), &[7, 1, 2, 3]);
    });
    ready.await.unwrap();
    assert_eq!(budget.snapshot().used_bytes, 4);
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert_eq!(budget.snapshot().used_bytes, 0);
}

#[test]
fn concurrent_connections_cannot_overcommit_or_wait_for_partial_upgrades() {
    let budget = PacketBudget::new(64).unwrap();
    let start = Arc::new(std::sync::Barrier::new(32));
    let release = Arc::new(std::sync::Barrier::new(33));
    let (sender, receiver) = std::sync::mpsc::channel();
    let handles: Vec<_> = (0..32)
        .map(|_| {
            let connection = budget.connection(16).unwrap();
            let start = Arc::clone(&start);
            let release = Arc::clone(&release);
            let sender = sender.clone();
            std::thread::spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .build()
                    .unwrap();
                start.wait();
                let packet = runtime.block_on(read_frame_budgeted(
                    &mut &[8, 1, 2, 3, 4, 5, 6, 7, 8][..],
                    64,
                    None,
                    &connection,
                ));
                sender
                    .send(packet.as_ref().map_or(0, |packet| packet.len()))
                    .unwrap();
                release.wait();
                drop(packet);
                assert_eq!(connection.snapshot().used_bytes, 0);
            })
        })
        .collect();
    let accepted: usize = (0..32)
        .map(|_| receiver.recv_timeout(Duration::from_secs(5)).unwrap())
        .sum();
    assert_eq!(accepted, 64);
    assert_eq!(budget.snapshot().used_bytes, 64);
    assert_eq!(budget.snapshot().peak_bytes, 64);
    assert_eq!(budget.snapshot().rejections, 24);
    release.wait();
    for handle in handles {
        handle.join().unwrap();
    }
    assert_eq!(budget.snapshot().used_bytes, 0);
}
