// SPDX-License-Identifier: GPL-3.0-or-later

//! Native encrypted QUIC datagrams, with NAT traversal and HTTPS relay fallback.
//! Network discovery, timers and credentials never enter the emulated machine.

use super::{PacketQueue, Settings, Transport, MAX_PACKET};
use anyhow::{bail, ensure, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use iroh::{endpoint::presets, Endpoint, EndpointAddr, RelayMode, RelayUrl, SecretKey};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::oneshot;

const ALPN: &[u8] = b"copperline/netplay/1";
pub const CODE_LIMIT: usize = 4096;
const PREFIX: &str = "CLNI1.";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Invitation {
    pub endpoint: EndpointAddr,
    pub session: [u8; 16],
    pub delay: u8,
    pub window: u8,
}

impl Invitation {
    pub fn decode(code: &str) -> Result<Self> {
        let code = code.trim();
        ensure!(code.len() <= CODE_LIMIT, "Internet invitation is too long");
        let encoded = code
            .strip_prefix(PREFIX)
            .context("Paste a desktop Internet invitation")?;
        let bytes = URL_SAFE_NO_PAD
            .decode(encoded)
            .context("Invalid Internet invitation")?;
        let invitation: Self =
            serde_json::from_slice(&bytes).context("Invalid Internet invitation")?;
        invitation.settings(1).validate()?;
        ensure!(
            !invitation.endpoint.is_empty() && invitation.endpoint.addrs.len() <= 8,
            "Invitation has no usable route"
        );
        for address in &invitation.endpoint.addrs {
            match address {
                iroh::TransportAddr::Relay(url) => {
                    validate_relay(url.as_str())?;
                }
                iroh::TransportAddr::Ip(addr) => ensure!(
                    addr.port() != 0 && !addr.ip().is_unspecified() && !addr.ip().is_multicast(),
                    "Invalid invitation address"
                ),
                _ => bail!("Unsupported invitation route"),
            }
        }
        Ok(invitation)
    }

    pub fn encode(&self) -> Result<String> {
        let code = format!(
            "{PREFIX}{}",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(self)?)
        );
        ensure!(code.len() <= CODE_LIMIT, "Internet invitation is too long");
        Ok(code)
    }

    pub fn settings(&self, player: usize) -> Settings {
        Settings {
            player,
            session: self.session,
            input_delay: self.delay,
            rollback_frames: self.window,
        }
    }
}

/// A private host key is kept in memory separately from the shareable invitation.
#[derive(Clone, Debug)]
pub struct Options {
    pub invitation: Invitation,
    pub host_key: Option<SecretKey>,
    pub relay_only: bool,
}

pub fn validate_relay(value: &str) -> Result<RelayUrl> {
    let url: RelayUrl = value.parse().context("Relay needs an HTTPS URL")?;
    ensure!(
        url.scheme() == "https"
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "Relay needs an HTTPS URL without credentials, query or fragment"
    );
    Ok(url)
}

impl Options {
    pub fn host(delay: u8, window: u8, relay: &str, relay_only: bool) -> Result<Self> {
        let key = SecretKey::generate();
        let mut endpoint = EndpointAddr::new(key.public());
        if relay.trim().is_empty() {
            for url in iroh::defaults::prod::default_relay_map().urls::<Vec<_>>() {
                endpoint = endpoint.with_relay_url(url);
            }
        } else {
            endpoint = endpoint.with_relay_url(validate_relay(relay.trim())?);
        }
        // The invitation is also a capability: knowing the endpoint ID alone
        // must not let an unrelated client claim the second controller port.
        let random = SecretKey::generate().to_bytes();
        let invitation = Invitation {
            endpoint,
            session: random[..16].try_into()?,
            delay,
            window,
        };
        let options = Self {
            invitation,
            host_key: Some(key),
            relay_only,
        };
        options.validate()?;
        Ok(options)
    }

    pub fn join(code: &str, relay_only: bool) -> Result<Self> {
        Ok(Self {
            invitation: Invitation::decode(code)?,
            host_key: None,
            relay_only,
        })
    }

    pub fn settings(&self) -> Settings {
        self.invitation
            .settings(usize::from(self.host_key.is_none()))
    }

    pub fn validate(&self) -> Result<()> {
        Invitation::decode(&self.invitation.encode()?)?;
        if let Some(key) = &self.host_key {
            ensure!(
                key.public() == self.invitation.endpoint.id,
                "Host key does not match the invitation"
            );
        }
        ensure!(
            !self.relay_only || self.invitation.endpoint.relay_urls().next().is_some(),
            "Relay-only mode needs a relay"
        );
        Ok(())
    }
}

#[derive(Default)]
struct Shared {
    packets: PacketQueue,
    ready: bool,
    route: Option<&'static str>,
    failure: Option<String>,
}

pub struct InternetTransport {
    pub(super) options: Options,
    shared: Arc<Mutex<Shared>>,
    cancel: Option<oneshot::Sender<()>>,
}

impl InternetTransport {
    pub(super) fn new(options: Options) -> Result<Self> {
        options.validate()?;
        let shared = Arc::new(Mutex::new(Shared::default()));
        let (cancel, cancelled) = oneshot::channel();
        let worker_state = shared.clone();
        let worker_options = options.clone();
        std::thread::Builder::new()
            .name("netplay-internet".into())
            .spawn(move || {
                let result = (|| -> Result<()> {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()?;
                    runtime.block_on(worker(worker_options, &worker_state, cancelled))
                })();
                if let Err(error) = result {
                    worker_state.lock().unwrap().failure = Some(format!("{error:#}"));
                }
            })
            .context("Starting Internet netplay")?;
        Ok(Self {
            options,
            shared,
            cancel: Some(cancel),
        })
    }
}

impl Drop for InternetTransport {
    fn drop(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            let _ = cancel.send(());
        }
    }
}

impl Transport for InternetTransport {
    fn route(&self) -> &'static str {
        self.shared.lock().unwrap().route.unwrap_or("connecting")
    }
    fn ready(&mut self) -> Result<bool> {
        let shared = self.shared.lock().unwrap();
        if let Some(error) = &shared.failure {
            bail!("{error}");
        }
        Ok(shared.ready)
    }

    fn receive(&mut self, buffer: &mut [u8]) -> Result<Option<usize>> {
        self.ready()?;
        self.shared.lock().unwrap().packets.receive(buffer)
    }

    fn send(&mut self, packet: &[u8]) -> Result<bool> {
        if !self.ready()? {
            return Ok(false);
        }
        self.shared.lock().unwrap().packets.send(packet)
    }
}

async fn worker(
    options: Options,
    shared: &Mutex<Shared>,
    mut cancelled: oneshot::Receiver<()>,
) -> Result<()> {
    let relay_map = options.invitation.endpoint.relay_urls().cloned().collect();
    let config = iroh::endpoint::QuicTransportConfig::builder()
        .datagram_receive_buffer_size(Some(64 * MAX_PACKET))
        .datagram_send_buffer_size(64 * MAX_PACKET)
        .max_concurrent_bidi_streams(1u32.into())
        .max_concurrent_uni_streams(0u32.into())
        .keep_alive_interval(Duration::from_secs(2))
        .build();
    let mut builder = Endpoint::builder(presets::Minimal)
        .relay_mode(RelayMode::Custom(relay_map))
        .alpns(vec![ALPN.to_vec()])
        .transport_config(config);
    if let Some(key) = &options.host_key {
        builder = builder.secret_key(key.clone());
    }
    if options.relay_only {
        builder = builder.clear_ip_transports();
    }
    let endpoint = tokio::select! {
        _ = &mut cancelled => return Ok(()),
        result = builder.bind() => result.context("Opening Internet netplay endpoint")?,
    };
    let result = tokio::select! {
        _ = &mut cancelled => Ok(()),
        result = async {
            let connection = tokio::time::timeout(Duration::from_secs(15 * 60), establish(&endpoint, &options))
                .await.context("Internet invitation timed out; start a new session")??;
            ensure!(connection.max_datagram_size().is_some_and(|size| size >= MAX_PACKET), "Peer cannot carry netplay packets");
            shared.lock().unwrap().ready = true;
            let mut tick = tokio::time::interval(Duration::from_millis(2));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut route_tick = tokio::time::interval(Duration::from_secs(1));
            loop {
                tokio::select! {
                    _ = route_tick.tick() => {
                        if let Some(path) = connection.paths().iter().find(|path| path.is_selected()) {
                            let route = if path.is_relay() { "relay" } else { "direct" };
                            let mut state = shared.lock().unwrap();
                            if state.route != Some(route) {
                                log::info!("netplay: Internet route is {route}");
                                state.route = Some(route);
                            }
                        }
                    }
                    packet = connection.read_datagram() => {
                        let packet = packet.context("Internet peer disconnected")?;
                        shared.lock().unwrap().packets.push(&packet)?;
                    }
                    _ = tick.tick() => {
                        let mut state = shared.lock().unwrap();
                        while let Some(packet) = state.packets.pop() {
                            connection.send_datagram(packet.into()).context("Sending Internet netplay packet")?;
                        }
                    }
                }
            }
        } => result,
    };
    // A cancelled or failed session releases sockets and relay registration.
    let _ = tokio::time::timeout(Duration::from_secs(2), endpoint.close()).await;
    result
}

async fn establish(endpoint: &Endpoint, options: &Options) -> Result<iroh::endpoint::Connection> {
    if options.host_key.is_none() {
        let connection = endpoint
            .connect(options.invitation.endpoint.clone(), ALPN)
            .await
            .context("Connecting to host; check that the host has pressed Run")?;
        tokio::time::timeout(Duration::from_secs(10), async {
            let (mut send, mut recv) = connection.open_bi().await?;
            send.write_all(&options.invitation.session).await?;
            send.finish()?;
            let reply = recv.read_to_end(1).await?;
            ensure!(reply == [1], "Host rejected the invitation");
            Ok::<_, anyhow::Error>(())
        })
        .await
        .context("Host did not accept the invitation")??;
        return Ok(connection);
    }
    loop {
        let incoming = endpoint
            .accept()
            .await
            .context("Internet endpoint closed")?;
        let accepted = tokio::time::timeout(Duration::from_secs(5), async {
            let connection = incoming.await?;
            let (mut send, mut recv) = connection.accept_bi().await?;
            let capability = recv.read_to_end(16).await?;
            if capability != options.invitation.session {
                connection.close(1u32.into(), b"Invalid invitation");
                bail!("Invalid invitation");
            }
            send.write_all(&[1]).await?;
            send.finish()?;
            Ok::<_, anyhow::Error>(connection)
        })
        .await;
        // Failed or unrelated handshakes cannot claim the only player slot.
        if let Ok(Ok(connection)) = accepted {
            return Ok(connection);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invitations_keep_host_keys_private_and_validate_routes_and_settings() -> Result<()> {
        let host = Options::host(6, 12, "https://relay.example.com", false)?;
        let code = host.invitation.encode()?;
        let guest = Options::join(&code, true)?;
        assert_eq!(guest.settings().player, 1);
        assert_eq!(guest.invitation.session, host.invitation.session);
        assert_eq!(
            guest.invitation.endpoint.id,
            host.host_key.as_ref().unwrap().public()
        );
        assert!(guest.host_key.is_none());
        assert_eq!(guest.settings().input_delay, 6);
        assert_eq!(guest.settings().rollback_frames, 12);
        let decoded = URL_SAFE_NO_PAD.decode(code.strip_prefix(PREFIX).unwrap())?;
        assert!(!String::from_utf8(decoded)?.contains("host_key"));
        for bad in ["", "CLNP1.bad", "CLNI1.bad", &"X".repeat(CODE_LIMIT + 1)] {
            assert!(Invitation::decode(bad).is_err());
        }
        for bad in [
            "http://relay.example.com",
            "https://user:secret@relay.example.com",
            "https://relay.example.com?token=1",
            "https://relay.example.com#fragment",
        ] {
            assert!(Options::host(2, 8, bad, false).is_err());
        }
        let mut invalid = host.clone();
        invalid.invitation.delay = 7;
        assert!(invalid.validate().is_err());
        invalid = host.clone();
        invalid.invitation.window = 0;
        assert!(invalid.validate().is_err());
        invalid = host;
        invalid.host_key = Some(SecretKey::generate());
        assert!(invalid.validate().is_err());
        Ok(())
    }

    #[test]
    fn encrypted_loopback_rejects_wrong_invitation_then_exchanges_datagrams() -> Result<()> {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?
            .block_on(async {
                tokio::time::timeout(Duration::from_secs(20), async {
                    let host_ep = Endpoint::builder(presets::Minimal)
                        .relay_mode(RelayMode::Disabled)
                        .alpns(vec![ALPN.to_vec()])
                        .clear_ip_transports()
                        .bind_addr("127.0.0.1:0")?
                        .bind()
                        .await?;
                    let guest_ep = Endpoint::builder(presets::Minimal)
                        .relay_mode(RelayMode::Disabled)
                        .clear_ip_transports()
                        .bind_addr("127.0.0.1:0")?
                        .bind()
                        .await?;
                    let mut host = Options::host(0, 8, "", false)?;
                    host.host_key = Some(host_ep.secret_key().clone());
                    host.invitation.endpoint = host_ep.addr();
                    host.validate()?;
                    let guest = Options::join(&host.invitation.encode()?, false)?;
                    let (accepted, joined) = tokio::join!(establish(&host_ep, &host), async {
                        let mut wrong = guest.clone();
                        wrong.invitation.session[0] ^= 1;
                        assert!(establish(&guest_ep, &wrong).await.is_err());
                        establish(&guest_ep, &guest).await
                    });
                    let accepted = accepted?;
                    let joined = joined?;
                    let packet = vec![0x5a; MAX_PACKET];
                    joined.send_datagram(packet.clone().into())?;
                    assert_eq!(accepted.read_datagram().await?.as_ref(), packet);
                    accepted.send_datagram(vec![0xa5; MAX_PACKET].into())?;
                    assert_eq!(
                        joined.read_datagram().await?.as_ref(),
                        vec![0xa5; MAX_PACKET]
                    );
                    joined.close(0u32.into(), b"done");
                    assert!(accepted.read_datagram().await.is_err());
                    host_ep.close().await;
                    guest_ep.close().await;
                    Ok::<_, anyhow::Error>(())
                })
                .await
                .context("Loopback connection timed out")?
            })
    }
}
