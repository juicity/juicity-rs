use std::{io, time::Duration};

use tokio::{
    io::{duplex, split, AsyncReadExt, AsyncWriteExt},
    time::timeout,
};

use super::relay;

const IDLE_TIMEOUT: Duration = Duration::from_millis(500);
const CHUNK_INTERVAL: Duration = Duration::from_millis(100);
const TEST_TIMEOUT: Duration = Duration::from_secs(3);

#[tokio::test]
async fn client_half_close_keeps_receiving_remote_response() {
    timeout(TEST_TIMEOUT, async {
        let (client, relay_quic) = duplex(4096);
        let (mut client_read, mut client_write) = split(client);
        let (relay_read, relay_write) = split(relay_quic);
        let (relay_remote, mut remote) = duplex(4096);
        let relay_task = tokio::spawn(relay(relay_read, relay_write, relay_remote, IDLE_TIMEOUT));

        client_write.write_all(b"request").await.unwrap();
        client_write.shutdown().await.unwrap();
        let mut request = Vec::new();
        remote.read_to_end(&mut request).await.unwrap();
        assert_eq!(request, b"request");
        remote.write_all(b"response after eof").await.unwrap();
        remote.shutdown().await.unwrap();
        let mut response = Vec::new();
        client_read.read_to_end(&mut response).await.unwrap();
        assert_eq!(response, b"response after eof");
        assert!(relay_task.await.unwrap().is_ok());
    })
    .await
    .expect("relay test should complete");
}

#[tokio::test]
async fn remote_half_close_keeps_accepting_client_data() {
    timeout(TEST_TIMEOUT, async {
        let (client, relay_quic) = duplex(4096);
        let (mut client_read, mut client_write) = split(client);
        let (relay_read, relay_write) = split(relay_quic);
        let (relay_remote, remote) = duplex(4096);
        let (mut remote_read, mut remote_write) = split(remote);
        let relay_task = tokio::spawn(relay(relay_read, relay_write, relay_remote, IDLE_TIMEOUT));

        remote_write
            .write_all(b"remote finished writing")
            .await
            .unwrap();
        remote_write.shutdown().await.unwrap();
        let mut greeting = Vec::new();
        client_read.read_to_end(&mut greeting).await.unwrap();
        assert_eq!(greeting, b"remote finished writing");
        client_write
            .write_all(b"client data after eof")
            .await
            .unwrap();
        client_write.shutdown().await.unwrap();
        let mut received = Vec::new();
        remote_read.read_to_end(&mut received).await.unwrap();
        assert_eq!(received, b"client data after eof");
        assert!(relay_task.await.unwrap().is_ok());
    })
    .await
    .expect("relay test should complete");
}

#[tokio::test]
async fn continuous_download_does_not_hit_idle_timeout() {
    timeout(TEST_TIMEOUT, async {
        let (client, relay_quic) = duplex(4096);
        let (mut client_read, mut client_write) = split(client);
        let (relay_read, relay_write) = split(relay_quic);
        let (relay_remote, mut remote) = duplex(4096);
        let relay_task = tokio::spawn(relay(relay_read, relay_write, relay_remote, IDLE_TIMEOUT));

        client_write.write_all(b"download request").await.unwrap();
        let mut request = [0; 16];
        remote.read_exact(&mut request).await.unwrap();
        assert_eq!(&request, b"download request");
        let mut expected = Vec::new();
        for _ in 0..12 {
            remote.write_all(b"download chunk\n").await.unwrap();
            expected.extend_from_slice(b"download chunk\n");
            tokio::time::sleep(CHUNK_INTERVAL).await;
        }
        remote.shutdown().await.unwrap();
        let mut downloaded = Vec::new();
        client_read.read_to_end(&mut downloaded).await.unwrap();
        assert_eq!(downloaded, expected);
        client_write.shutdown().await.unwrap();
        assert!(relay_task.await.unwrap().is_ok());
    })
    .await
    .expect("relay test should complete");
}

#[tokio::test]
async fn continuous_upload_does_not_hit_idle_timeout() {
    timeout(TEST_TIMEOUT, async {
        let (client, relay_quic) = duplex(4096);
        let (_client_read, mut client_write) = split(client);
        let (relay_read, relay_write) = split(relay_quic);
        let (relay_remote, remote) = duplex(4096);
        let (mut remote_read, mut remote_write) = split(remote);
        let relay_task = tokio::spawn(relay(relay_read, relay_write, relay_remote, IDLE_TIMEOUT));

        let mut expected = Vec::new();
        for _ in 0..12 {
            client_write.write_all(b"upload chunk\n").await.unwrap();
            expected.extend_from_slice(b"upload chunk\n");
            tokio::time::sleep(CHUNK_INTERVAL).await;
        }
        client_write.shutdown().await.unwrap();
        let mut uploaded = Vec::new();
        remote_read.read_to_end(&mut uploaded).await.unwrap();
        assert_eq!(uploaded, expected);
        remote_write.shutdown().await.unwrap();
        assert!(relay_task.await.unwrap().is_ok());
    })
    .await
    .expect("relay test should complete");
}

#[tokio::test]
async fn slow_backpressured_writes_keep_relay_alive() {
    timeout(TEST_TIMEOUT, async {
        let (client, relay_quic) = duplex(4096);
        let (_client_read, mut client_write) = split(client);
        let (relay_read, relay_write) = split(relay_quic);
        let (relay_remote, remote) = duplex(1);
        let (mut remote_read, mut remote_write) = split(remote);
        let relay_task = tokio::spawn(relay(relay_read, relay_write, relay_remote, IDLE_TIMEOUT));
        let payload = b"backpressure!";
        let writer = tokio::spawn(async move {
            client_write.write_all(payload).await.unwrap();
            client_write.shutdown().await.unwrap();
        });

        let mut received = Vec::new();
        for _ in payload {
            let mut byte = [0; 1];
            remote_read.read_exact(&mut byte).await.unwrap();
            received.extend_from_slice(&byte);
            tokio::time::sleep(CHUNK_INTERVAL).await;
        }
        assert_eq!(received, payload);
        writer.await.unwrap();
        remote_write.shutdown().await.unwrap();
        assert!(relay_task.await.unwrap().is_ok());
    })
    .await
    .expect("backpressured relay should keep making progress");
}

#[tokio::test]
async fn inactivity_after_activity_returns_timed_out() {
    timeout(TEST_TIMEOUT, async {
        let (client, relay_quic) = duplex(4096);
        let (_client_read, mut client_write) = split(client);
        let (relay_read, relay_write) = split(relay_quic);
        let (relay_remote, mut remote) = duplex(4096);
        let relay_task = tokio::spawn(relay(
            relay_read,
            relay_write,
            relay_remote,
            Duration::from_millis(100),
        ));
        client_write.write_all(b"x").await.unwrap();
        let mut received = [0; 1];
        remote.read_exact(&mut received).await.unwrap();
        assert_eq!(received, *b"x");
        let result = relay_task
            .await
            .unwrap()
            .expect_err("idle relay should fail");
        assert_eq!(result.kind(), io::ErrorKind::TimedOut);
    })
    .await
    .expect("relay test should complete");
}

#[tokio::test]
async fn cancelling_relay_closes_both_peers() {
    timeout(TEST_TIMEOUT, async {
        let (client, relay_quic) = duplex(4096);
        let (mut client_read, mut client_write) = split(client);
        let (relay_read, relay_write) = split(relay_quic);
        let (relay_remote, mut remote) = duplex(4096);
        let relay_task = tokio::spawn(relay(relay_read, relay_write, relay_remote, IDLE_TIMEOUT));
        client_write.write_all(b"x").await.unwrap();
        let mut activity = [0; 1];
        remote.read_exact(&mut activity).await.unwrap();
        assert_eq!(activity, *b"x");
        relay_task.abort();
        assert!(relay_task.await.unwrap_err().is_cancelled());
        let mut byte = [0; 1];
        assert_eq!(client_read.read(&mut byte).await.unwrap(), 0);
        assert_eq!(remote.read(&mut byte).await.unwrap(), 0);
    })
    .await
    .expect("relay cancellation should release both peers");
}
