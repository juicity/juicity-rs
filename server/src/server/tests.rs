use super::*;
use std::time::Duration;

#[tokio::test]
async fn failing_dns_lookup_does_not_block_next_lookup() {
    let cache = StdMutex::new(IndexMap::new());
    let resolving = StdMutex::new(HashMap::new());
    for _ in 0..2 {
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            resolve_udp_target("\0", 53, &cache, &resolving),
        )
        .await
        .expect("a failed lookup must not leave the next lookup waiting");
        assert!(result.is_err());
    }
    assert!(resolving
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_empty());
}

#[tokio::test]
async fn underlay_packets_use_fresh_salts_without_consuming_invalid_auth() {
    use crate::underlay_socket::UnderlayPacket;

    let target = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let target_addr = target.local_addr().unwrap();
    let server_socket = Arc::new(tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let client = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let peer = client.local_addr().unwrap();
    let salt = [0u8; 32];
    let psk = vec![0x42; 32];
    let in_flight = Arc::new(crate::inflight::InFlightUnderlayKey::new(
        Duration::from_secs(1),
        Duration::from_millis(100),
    ));
    in_flight.store(
        salt,
        protocol::UnderlayAuth {
            iv: salt,
            psk: psk.clone(),
            metadata: protocol::ProxyMetadata {
                network: protocol::Network::Udp,
                hostname: "127.0.0.1".to_owned(),
                port: target_addr.port(),
                uuid: Uuid::nil(),
            },
            uuid: Uuid::nil(),
        },
    );
    let pool = Arc::new(crate::udp::UdpEndpointPool::new(16));
    let sessions = Arc::new(Cache::new(16));
    let valid = juicity_underlay::encrypt_udp(&psk, b"authenticated payload", &salt).unwrap();
    let mut invalid = valid.clone();
    *invalid.last_mut().unwrap() ^= 1;
    for payload in [invalid, valid] {
        handle_non_quic_underlay_packet(
            UnderlayPacket { peer, payload },
            in_flight.clone(),
            pool.clone(),
            sessions.clone(),
            server_socket.clone(),
            false,
        )
        .await
        .unwrap();
    }
    let mut buf = [0u8; 128];
    let (n, relay_addr) = tokio::time::timeout(Duration::from_secs(1), target.recv_from(&mut buf))
        .await
        .expect("valid packet must still be relayed after a forged packet")
        .unwrap();
    assert_eq!(&buf[..n], b"authenticated payload");

    let mut next_salt = salt;
    next_salt[31] ^= 1;
    let payload = juicity_underlay::encrypt_udp(&psk, b"second", &next_salt).unwrap();
    handle_non_quic_underlay_packet(
        UnderlayPacket { peer, payload },
        in_flight.clone(),
        pool.clone(),
        sessions.clone(),
        server_socket.clone(),
        false,
    )
    .await
    .unwrap();
    let (n, second_relay_addr) =
        tokio::time::timeout(Duration::from_secs(1), target.recv_from(&mut buf))
            .await
            .expect("second datagram with a fresh salt must reach the target")
            .unwrap();
    assert_eq!(&buf[..n], b"second");
    assert_eq!(second_relay_addr, relay_addr);

    target.send_to(b"reply", relay_addr).await.unwrap();
    let (n, _) = tokio::time::timeout(Duration::from_secs(1), client.recv_from(&mut buf))
        .await
        .expect("reply must reach the client")
        .unwrap();
    assert_eq!(
        juicity_underlay::decrypt_udp(&psk, &buf[..n]).unwrap(),
        b"reply"
    );

    if let Some(session) = sessions.get(&peer) {
        if let Some(abort) = session.relay_abort {
            abort.abort();
        }
    }
}

#[test]
fn cancelled_and_timed_out_dns_lookups_release_key_and_wake_waiter() {
    use std::task::Poll;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap();
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let blocker = runtime.spawn_blocking(move || {
        started_tx.send(()).unwrap();
        release_rx.recv().unwrap();
    });
    started_rx.recv().unwrap();
    runtime.block_on(async {
        let cache = StdMutex::new(IndexMap::new());
        let resolving = StdMutex::new(HashMap::new());
        let mut resolver = Box::pin(resolve_udp_target("localhost", 53, &cache, &resolving));
        let mut waiter = Box::pin(resolve_udp_target("localhost", 53, &cache, &resolving));
        std::future::poll_fn(|cx| {
            assert!(resolver.as_mut().poll(cx).is_pending());
            assert!(waiter.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        drop(resolver);
        assert!(resolving
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty());
        std::future::poll_fn(|cx| {
            assert!(waiter.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        assert_eq!(resolving.lock().unwrap_or_else(|e| e.into_inner()).len(), 1);
        let result =
            tokio::time::timeout(consts::DNS_QUERY_TIMEOUT + Duration::from_secs(1), waiter)
                .await
                .unwrap();
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("DNS query timeout"));
        assert!(resolving
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty());
        release_tx.send(()).unwrap();
        let addr = tokio::time::timeout(
            Duration::from_secs(1),
            resolve_udp_target("localhost", 53, &cache, &resolving),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(addr.ip().is_loopback());
        assert_eq!(addr.port(), 53);
        blocker.await.unwrap();
    });
}

#[tokio::test]
async fn dns_waiter_is_bounded_by_query_timeout() {
    let cache = StdMutex::new(IndexMap::new());
    let resolving = StdMutex::new(HashMap::from([(
        (Arc::from("localhost"), 53),
        Arc::new(tokio::sync::Notify::new()),
    )]));
    let result = tokio::time::timeout(
        consts::DNS_QUERY_TIMEOUT + Duration::from_secs(1),
        resolve_udp_target("localhost", 53, &cache, &resolving),
    )
    .await
    .expect("waiting for another resolver must also time out");
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("DNS query timeout"));
}
