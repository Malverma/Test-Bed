//! Connections across the internet (spec3), separate from the LAN path: this
//! PC's iroh keys, endpoints, invite codes, and the client side of pairing
//! and probing. Hosts are dialed by endpoint id; iroh punches through NAT and
//! falls back to a relay.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use iroh::address_lookup::PkarrResolver;
use iroh::endpoint::{presets, QuicTransportConfig};
use iroh::{Endpoint, EndpointAddr, EndpointId, RelayMode, RelayUrl, SecretKey};

use crate::client::ProbeResult;
use crate::identity::Role;
use crate::net::Conn;
use crate::transport::{ALPN_PAIR, ALPN_PROBE, ALPN_SESSION};
use crate::trust::{KnownHost, KnownHosts};

/// How long an invite code can be used: long enough to send it by chat.
pub const INVITE_LIFETIME: Duration = Duration::from_secs(600);
/// Hole punching can take a few round trips through the relay.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const INVITE_PREFIX: &str = "tb1";

/// One key per role, like the LAN identity: the share service and the viewer
/// run at the same time, and each publishes where it can be reached.
fn key_path(role: Role) -> PathBuf {
    crate::paths::data_dir().join(match role {
        Role::Host => "iroh-host.key",
        Role::Client => "iroh-client.key",
    })
}

/// This PC's iroh secret key for `role`, created on first use.
pub fn secret_key(role: Role) -> Result<SecretKey> {
    let path = key_path(role);
    let read = |path: &PathBuf| -> Result<SecretKey> {
        let s = std::fs::read_to_string(path)?;
        let bytes: [u8; 32] = hex::decode(s.trim())?.try_into().map_err(|_| anyhow!("{} is not a 32-byte key", path.display()))?;
        Ok(SecretKey::from_bytes(&bytes))
    };
    if path.exists() {
        return read(&path).with_context(|| format!("reading {}", path.display()));
    }
    let key = SecretKey::generate();
    crate::paths::write_private(&path, hex::encode(key.to_bytes()).as_bytes(), 0o600)?;
    Ok(key)
}

/// This PC's endpoint id for `role`.
pub fn endpoint_id(role: Role) -> Result<EndpointId> {
    Ok(secret_key(role)?.public())
}

fn transport_config() -> Result<QuicTransportConfig> {
    Ok(QuicTransportConfig::builder()
        .keep_alive_interval(Duration::from_secs(1))
        .max_idle_timeout(Some(Duration::from_secs(10).try_into()?))
        .datagram_receive_buffer_size(Some(8 * 1024 * 1024))
        .datagram_send_buffer_size(4 * 1024 * 1024)
        .build())
}

fn relay_mode(relay: &str) -> Result<RelayMode> {
    let relay = relay.trim();
    if relay.is_empty() {
        return Ok(RelayMode::Default);
    }
    let url: RelayUrl = relay.parse().with_context(|| format!("invalid relay URL {relay:?}"))?;
    Ok(RelayMode::custom([url]))
}

/// The share service's endpoint: publishes where it can be reached, so
/// paired PCs find it by endpoint id alone.
pub async fn host_endpoint(relay: &str) -> Result<Endpoint> {
    Endpoint::builder(presets::N0)
        .secret_key(secret_key(Role::Host)?)
        .alpns(vec![ALPN_SESSION.to_vec(), ALPN_PAIR.to_vec(), ALPN_PROBE.to_vec()])
        .transport_config(transport_config()?)
        .relay_mode(relay_mode(relay)?)
        .bind()
        .await
        .context("starting the internet endpoint")
}

/// The viewer's endpoint: only dials out, so it looks hosts up but publishes
/// nothing.
pub async fn client_endpoint(relay: &str) -> Result<Endpoint> {
    Endpoint::builder(presets::Minimal)
        .address_lookup(PkarrResolver::n0_dns())
        .secret_key(secret_key(Role::Client)?)
        .transport_config(transport_config()?)
        .relay_mode(relay_mode(relay)?)
        .bind()
        .await
        .context("starting the internet endpoint")
}

/// Where to find a host: its endpoint id, and its relay if known.
pub fn host_addr(host: &KnownHost) -> Result<EndpointAddr> {
    let id: EndpointId = host.fingerprint.parse().map_err(|e| anyhow!("{} has an invalid endpoint id: {e}", host.name))?;
    let mut addr = EndpointAddr::new(id);
    if !host.address.is_empty() {
        if let Ok(url) = host.address.parse::<RelayUrl>() {
            addr = addr.with_relay_url(url);
        }
    }
    Ok(addr)
}

pub async fn connect(endpoint: &Endpoint, host: &KnownHost, alpn: &[u8]) -> Result<Conn> {
    let addr = host_addr(host)?;
    let conn = tokio::time::timeout(CONNECT_TIMEOUT, endpoint.connect(addr, alpn))
        .await
        .map_err(|_| anyhow!("{} didn't answer over the internet; is it on, with internet access allowed?", host.name))?
        .with_context(|| format!("connecting to {}", host.name))?;
    Ok(conn.into())
}

/// What a host shows to pair over the internet: its endpoint id, a PIN, and
/// its relay. Written `tb1:<id>:<pin>` or `tb1:<id>:<pin>:<relay URL>`.
#[derive(Debug, Clone, PartialEq)]
pub struct Invite {
    pub id: EndpointId,
    pub pin: String,
    pub relay: Option<RelayUrl>,
}

impl Invite {
    pub fn encode(&self) -> String {
        let relay = self.relay.as_ref().map(|r| format!(":{r}")).unwrap_or_default();
        format!("{INVITE_PREFIX}:{}:{}{relay}", self.id.to_z32(), self.pin)
    }

    pub fn decode(s: &str) -> Result<Self> {
        let s: String = s.split_whitespace().collect();
        let mut parts = s.splitn(4, ':');
        if parts.next() != Some(INVITE_PREFIX) {
            bail!("that isn't a Test Bed invite code (they start with {INVITE_PREFIX}:)");
        }
        let id = parts.next().ok_or_else(|| anyhow!("the invite code is cut off"))?;
        let id = EndpointId::from_z32(id).map_err(|_| anyhow!("the invite code is damaged; copy it again"))?;
        let pin = parts.next().ok_or_else(|| anyhow!("the invite code is cut off"))?.to_string();
        if pin.len() != 6 || !pin.bytes().all(|b| b.is_ascii_digit()) {
            bail!("the invite code is damaged; copy it again");
        }
        let relay = parts.next().map(|r| r.parse::<RelayUrl>()).transpose().map_err(|_| anyhow!("the invite code's relay address is damaged"))?;
        Ok(Self { id, pin, relay })
    }
}

/// Pair with the host that made `invite`, and save it to
/// `internet_hosts.toml`. `name` is how this PC is shown on the host.
pub async fn pair(invite: &str, name: &str, relay: &str) -> Result<KnownHost> {
    let invite = Invite::decode(invite)?;
    let ep = client_endpoint(relay).await?;
    let host = KnownHost {
        name: String::new(),
        address: invite.relay.as_ref().map(|r| r.to_string()).unwrap_or_default(),
        fingerprint: invite.id.to_string(),
        paired_at: crate::unix_now(),
        last_seen: None,
        internet: true,
    };
    let res = async {
        let conn = connect(&ep, &KnownHost { name: "the other PC".into(), ..host.clone() }, ALPN_PAIR).await?;
        let r = crate::pairing::pair_as_client(&conn, &ep.id().to_string(), name, &invite.pin).await;
        conn.close(0, b"");
        r
    }
    .await;
    ep.close().await;
    let paired = res?;
    let now = crate::unix_now();
    let host = KnownHost { name: paired.peer_name, fingerprint: paired.peer_fingerprint, last_seen: Some(now), ..host };
    let mut kh = KnownHosts::load_internet()?;
    kh.upsert(host.clone());
    kh.save()?;
    Ok(host)
}

/// One endpoint for repeated probes (the PC tab checks every few seconds),
/// kept connected to its relay. Bound to the runtime that first asks.
async fn shared_endpoint(relay: &str) -> Result<Endpoint> {
    static EP: tokio::sync::OnceCell<Endpoint> = tokio::sync::OnceCell::const_new();
    EP.get_or_try_init(|| client_endpoint(relay)).await.cloned()
}

/// Check which internet hosts are online. All are probed at once.
pub async fn probe(hosts: &[KnownHost], relay: &str, timeout: Duration) -> Vec<ProbeResult> {
    let fail = |h: &KnownHost, e: String| ProbeResult { name: h.name.clone(), address: h.address.clone(), online: false, rtt_ms: None, error: Some(e) };
    if hosts.is_empty() {
        return Vec::new();
    }
    let ep = match shared_endpoint(relay).await {
        Ok(e) => e,
        Err(e) => return hosts.iter().map(|h| fail(h, format!("{e:#}"))).collect(),
    };
    let tasks: Vec<_> = hosts
        .iter()
        .cloned()
        .map(|h| {
            let ep = ep.clone();
            tokio::spawn(async move {
                let t = Instant::now();
                let addr = host_addr(&h)?;
                let conn = tokio::time::timeout(timeout, ep.connect(addr, ALPN_PROBE)).await.map_err(|_| anyhow!("timed out"))??;
                let rtt = t.elapsed();
                let path = Conn::from(conn.clone()).path();
                conn.close(0u32.into(), b"probe");
                anyhow::Ok((rtt, path))
            })
        })
        .collect();
    let mut out = Vec::with_capacity(tasks.len());
    for (t, h) in tasks.into_iter().zip(hosts) {
        out.push(match t.await {
            Ok(Ok((rtt, _path))) => ProbeResult {
                name: h.name.clone(),
                address: h.address.clone(),
                online: true,
                rtt_ms: Some((rtt.as_secs_f64() * 10_000.0).round() / 10.0),
                error: None,
            },
            Ok(Err(e)) => fail(h, format!("{e:#}")),
            Err(e) => fail(h, e.to_string()),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invite_round_trip() {
        let id = SecretKey::generate().public();
        let a = Invite { id, pin: "012345".into(), relay: None };
        assert_eq!(Invite::decode(&a.encode()).unwrap(), a);
        let b = Invite { relay: Some("https://relay.example.com".parse().unwrap()), ..a };
        assert_eq!(Invite::decode(&b.encode()).unwrap(), b);
        // Line breaks from chat apps are ignored.
        let wrapped = b.encode().replace(':', ":\n");
        assert_eq!(Invite::decode(&wrapped).unwrap(), b);
    }

    #[test]
    fn invite_rejects_damage() {
        assert!(Invite::decode("hello").is_err());
        assert!(Invite::decode("tb1:abc:123456").is_err());
        let id = SecretKey::generate().public().to_z32();
        assert!(Invite::decode(&format!("tb1:{id}:12345")).is_err());
        assert!(Invite::decode(&format!("tb1:{id}:123456")).is_ok());
    }
}
