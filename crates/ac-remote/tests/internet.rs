//! Pairing and messaging over iroh connections, on loopback with no relay
//! (the transport the PC tab uses across the internet, spec3).

use ac_remote::net::Conn;
use ac_remote::pairing::{pair_as_client, pair_as_host, HostPairOutcome};
use ac_remote::proto::{read_msg, write_msg, ClientMsg};
use ac_remote::transport::{ALPN_PAIR, ALPN_SESSION};
use iroh::endpoint::presets;
use iroh::{Endpoint, EndpointAddr, RelayMode};

async fn endpoint(alpns: Vec<Vec<u8>>) -> Endpoint {
    Endpoint::builder(presets::Minimal)
        .relay_mode(RelayMode::Disabled)
        .alpns(alpns)
        .bind_addr("127.0.0.1:0")
        .unwrap()
        .bind()
        .await
        .unwrap()
}

/// A host and a client endpoint, connected with `alpn`.
async fn connect(alpn: &[u8]) -> (Endpoint, Endpoint, Conn, Conn) {
    let host = endpoint(vec![ALPN_SESSION.to_vec(), ALPN_PAIR.to_vec()]).await;
    let client = endpoint(vec![]).await;
    let addr = EndpointAddr::new(host.id()).with_ip_addr(host.bound_sockets()[0]);
    let accept = {
        let host = host.clone();
        tokio::spawn(async move { host.accept().await.unwrap().await.unwrap() })
    };
    let c = client.connect(addr, alpn).await.unwrap();
    let h = accept.await.unwrap();
    (host, client, Conn::from(h), Conn::from(c))
}

#[tokio::test]
async fn pairs_by_endpoint_id() {
    let (host, client, h, c) = connect(ALPN_PAIR).await;
    assert_eq!(h.alpn().as_deref(), Some(ALPN_PAIR));
    let (host_id, client_id) = (host.id().to_string(), client.id().to_string());
    let host_task = {
        let host_id = host_id.clone();
        tokio::spawn(async move { pair_as_host(&h, &host_id, "desk", Some("123456")).await.unwrap() })
    };
    let paired = pair_as_client(&c, &client_id, "laptop", "123456").await.unwrap();
    assert_eq!(paired.peer_name, "desk");
    assert_eq!(paired.peer_fingerprint, host_id);
    match host_task.await.unwrap() {
        HostPairOutcome::Paired(p) => assert_eq!(p.peer_fingerprint, client_id),
        _ => panic!("host did not pair"),
    }
}

#[tokio::test]
async fn wrong_pin_fails() {
    let (host, client, h, c) = connect(ALPN_PAIR).await;
    let host_id = host.id().to_string();
    let host_task = tokio::spawn(async move { pair_as_host(&h, &host_id, "desk", Some("123456")).await.unwrap() });
    let res = pair_as_client(&c, &client.id().to_string(), "laptop", "654321").await;
    assert!(res.unwrap_err().to_string().contains("wrong PIN"));
    assert!(matches!(host_task.await.unwrap(), HostPairOutcome::WrongPin));
}

#[tokio::test]
async fn messages_and_datagrams() {
    let (_host, client, h, c) = connect(ALPN_SESSION).await;
    assert_eq!(h.peer_key().unwrap(), client.id().to_string());
    let (mut send, _recv) = c.open_bi().await.unwrap();
    write_msg(&mut send, &ClientMsg::Ping { t_client_us: 42 }).await.unwrap();
    send.finish();
    let (_s, mut recv) = h.accept_bi().await.unwrap();
    assert!(matches!(read_msg::<ClientMsg>(&mut recv).await.unwrap(), Some(ClientMsg::Ping { t_client_us: 42 })));
    // A finished stream reads as a clean end.
    assert!(read_msg::<ClientMsg>(&mut recv).await.unwrap().is_none());

    assert!(c.max_datagram_size().unwrap() >= 1000);
    c.send_datagram(bytes::Bytes::from_static(b"frame")).unwrap();
    assert_eq!(&h.read_datagram().await.unwrap()[..], b"frame");
    assert_eq!(c.path(), ac_remote::net::Path::Direct);

    c.close(4, b"bye");
    h.closed().await;
    let close = h.app_close().unwrap();
    assert_eq!((close.code, close.reason.as_str()), (4, "bye"));
}

/// Through n0's public relays and address lookup, the way two PCs on
/// different networks meet: the client knows only the host's endpoint id
/// and invite. Needs internet access: `cargo test -p ac-remote --test
/// internet -- --ignored`.
#[tokio::test]
#[ignore]
async fn meets_through_public_relay() {
    let dir = std::env::temp_dir().join(format!("tb-internet-{}", std::process::id()));
    std::env::set_var("XDG_DATA_HOME", dir.join("data"));
    std::env::set_var("XDG_CONFIG_HOME", dir.join("config"));
    let host = ac_remote::internet::host_endpoint("").await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(20), host.online()).await.expect("host reached no relay");
    let relay = host.addr().relay_urls().next().cloned();
    assert!(relay.is_some());
    let invite = ac_remote::internet::Invite { id: host.id(), pin: "123456".into(), relay }.encode();
    let accept = {
        let host = host.clone();
        tokio::spawn(async move {
            let h = Conn::from(host.accept().await.unwrap().await.unwrap());
            let own = host.id().to_string();
            pair_as_host(&h, &own, "desk", Some("123456")).await.unwrap()
        })
    };
    let paired = ac_remote::internet::pair(&invite, "laptop", "").await.unwrap();
    assert_eq!(paired.name, "desk");
    assert!(paired.internet);
    assert!(matches!(accept.await.unwrap(), HostPairOutcome::Paired(_)));
    let saved = ac_remote::trust::KnownHosts::load_internet().unwrap();
    assert_eq!(saved.hosts[0].fingerprint, host.id().to_string());
    let _ = std::fs::remove_dir_all(dir);
}
