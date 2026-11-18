// SPDX-License-Identifier: MPL-2.0

//! The host's side of a connection broker (`qdw_protocol::broker`, `proto/broker.proto`): the
//! broker is the deployment's entry point, where users sign in and open their VMs, and the host
//! reaches it, never the other way round, so that it needs no inbound port.
//!
//! The host keeps one gRPC connection to the broker, opened again when it drops. On its control
//! stream it says who it is and lists its VMs whenever they change; the broker asks it for
//! connections there. It sends no console page (`Page` is deprecated): the broker serves its
//! own. For each connection, the host opens a relay stream, which carries the user's signalling
//! as a WebSocket would (`web::Socket::Relayed`), and serves it as it serves its own clients, the
//! broker having checked the user and their access to the VM.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use qdw_protocol::broker::v1::broker_client::BrokerClient;
use qdw_protocol::broker::v1::{
    BrokerMessage, Close, Connect, ConnectFailed, Hello, HostMessage, Inventory, RelayMessage, Vm,
    broker_message, connect_failed, host_message, relay_message,
};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::{Channel, Endpoint};
use tracing::{Instrument, debug, info, info_span, warn};

use crate::vms::Vms;
use crate::web;

/// How often the host looks for changes in its VMs, to tell the broker.
const INVENTORY_INTERVAL: Duration = Duration::from_secs(5);
/// How long the host waits before connecting again: doubled after each failure, up to the
/// longest.
const RETRY_FIRST: Duration = Duration::from_secs(1);
const RETRY_LONGEST: Duration = Duration::from_secs(30);
/// The HTTP/2 pings that tell a connection lost without a close (RFC 9113 6.7).
const KEEPALIVE: Duration = Duration::from_secs(10);
/// How long connecting to the broker may take.
const CONNECT_TIME: Duration = Duration::from_secs(10);
/// How long the broker has to end a relayed connection the host closed.
const CLOSE_TIME: Duration = Duration::from_secs(2);
/// Messages waiting for the broker on a stream: the signalling is a few messages a second.
const QUEUE: usize = 64;

/// How the host reaches its broker.
#[derive(Clone)]
pub struct Settings {
    /// "https://broker.example.com:8443".
    pub url: String,
    /// The host's identifier, unique among the broker's hosts.
    pub host_id: String,
    /// The roots that authenticate the broker, instead of the system's.
    pub ca_bundle: Option<PathBuf>,
    /// How the host gets its certificate, with which it authenticates to the broker.
    pub attestation: crate::attestation::Settings,
}

impl std::fmt::Debug for Settings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Settings")
            .field("url", &self.url)
            .field("host_id", &self.host_id)
            .field("ca_bundle", &self.ca_bundle)
            .finish_non_exhaustive()
    }
}

type Client = BrokerClient<Channel>;

/// Stay connected to the broker until the server stops.
pub async fn keep(settings: Settings, vms: Arc<Vms>) {
    let mut stopping = vms.stopping();
    let identity = match crate::attestation::identity::Identity::new(&settings.attestation) {
        Ok(identity) => identity,
        Err(e) => {
            warn!(error = %format!("{e:#}"), "no identity for the broker: not connecting");
            return;
        }
    };
    let mut wait = RETRY_FIRST;
    loop {
        // The certificate renewed when due, before connecting with it.
        if identity.renew_at() <= std::time::SystemTime::now() {
            info!("renewing the host's certificate");
            if let Err(e) = crate::attestation::enroll(&settings.attestation).await {
                warn!(error = %format!("{e:#}"), "renewing the host's certificate");
            }
        }
        let connected = tokio::select! {
            connected = connected(&settings, &identity, &vms) => connected,
            _ = stopping.wait_for(|stopping| *stopping) => return,
        };
        match connected {
            // Connected for a while, then lost: the next try at once.
            Ok(()) => {
                info!(
                    url = settings.url,
                    "connection to the broker lost: connecting again"
                );
                wait = RETRY_FIRST;
            }
            Err(e) => {
                warn!(url = settings.url, error = %format!("{e:#}"), retry_in = ?wait, "broker not reached");
            }
        }
        tokio::select! {
            () = tokio::time::sleep(wait) => {}
            _ = stopping.wait_for(|stopping| *stopping) => return,
        }
        wait = (wait * 2).min(RETRY_LONGEST);
    }
}

/// Connect to the broker and serve it until the connection drops: `Ok` when it dropped after
/// the host was connected, an error when it could not connect.
async fn connected(
    settings: &Settings,
    identity: &crate::attestation::identity::Identity,
    vms: &Arc<Vms>,
) -> Result<()> {
    let client = client(settings, identity).await?;
    // When the certificate is to be renewed: the connection ends then, and the host connects
    // again with the new one.
    let renew = identity
        .renew_at()
        .duration_since(std::time::SystemTime::now())
        .unwrap_or_default();
    let renew = tokio::time::sleep(renew.max(Duration::from_secs(60)));
    tokio::pin!(renew);
    let (to_broker, outgoing) = mpsc::channel(QUEUE);
    let hello = host_message::Message::Hello(Hello {
        host_id: settings.host_id.clone(),
        version: env!("CARGO_PKG_VERSION").into(),
    });
    let mut listed = inventory(vms).await;
    for message in [hello, host_message::Message::Inventory(listed.clone())] {
        to_broker
            .send(HostMessage {
                message: Some(message),
            })
            .await?;
    }
    let mut from_broker = client
        .clone()
        .control(ReceiverStream::new(outgoing))
        .await
        .context("opening the control stream")?
        .into_inner();
    info!(
        url = settings.url,
        host_id = settings.host_id,
        "connected to the broker"
    );
    let mut check = tokio::time::interval(INVENTORY_INTERVAL);
    check.tick().await;
    loop {
        tokio::select! {
            () = &mut renew => {
                info!("the host's certificate is due for renewal: connecting again after it");
                return Ok(());
            }
            message = from_broker.message() => match message {
                Ok(Some(BrokerMessage { message: Some(broker_message::Message::Connect(connect)) })) => {
                    let span = info_span!("relayed", vm = connect.vm_uuid, connection = connect.connection);
                    tokio::spawn(
                        relay(client.clone(), connect, vms.clone(), to_broker.clone()).instrument(span),
                    );
                }
                // A message of a newer broker that this host does not know.
                Ok(Some(_)) => {}
                Ok(None) => return Ok(()),
                Err(status) => {
                    debug!(%status, "control stream ended");
                    return Ok(());
                }
            },
            _ = check.tick() => {
                let now = inventory(vms).await;
                if now != listed {
                    listed = now;
                    let message = host_message::Message::Inventory(listed.clone());
                    if to_broker.send(HostMessage { message: Some(message) }).await.is_err() {
                        return Ok(());
                    }
                }
            }
        }
    }
}

/// A gRPC client of the broker, connected in TLS: the broker authenticated by its certificate,
/// the host by its own (TLS client authentication), its key in the TPM. HTTP/2 (ALPN).
async fn client(
    settings: &Settings,
    identity: &crate::attestation::identity::Identity,
) -> Result<Client> {
    let mut roots = rustls::RootCertStore::empty();
    match &settings.ca_bundle {
        Some(path) => {
            use rustls::pki_types::pem::PemObject;
            let pem = tokio::fs::read(path)
                .await
                .with_context(|| format!("reading {}", path.display()))?;
            for certificate in rustls::pki_types::CertificateDer::pem_slice_iter(&pem) {
                roots.add(certificate?)?;
            }
        }
        None => {
            for certificate in rustls_native_certs::load_native_certs().certs {
                let _ = roots.add(certificate);
            }
        }
    }
    let certified = identity.certified_key()?;
    let mut config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .with_root_certificates(roots)
    .with_client_cert_resolver(Arc::new(crate::attestation::identity::Resolver(certified)));
    config.alpn_protocols = vec![b"h2".to_vec()];
    let tls = tokio_rustls::TlsConnector::from(Arc::new(config));
    let connector = tower::service_fn(move |uri: tonic::codegen::http::Uri| {
        let tls = tls.clone();
        async move {
            let invalid =
                |what: &str| std::io::Error::new(std::io::ErrorKind::InvalidInput, what.to_owned());
            let host = uri
                .host()
                .ok_or_else(|| invalid("the broker's URL has no host"))?
                .to_owned();
            let port = uri.port_u16().unwrap_or(443);
            let tcp = tokio::net::TcpStream::connect((host.trim_matches(['[', ']']), port)).await?;
            tcp.set_nodelay(true)?;
            let name =
                rustls::pki_types::ServerName::try_from(host.trim_matches(['[', ']']).to_owned())
                    .map_err(|_| invalid("the broker's host is no name"))?;
            let stream = tls.connect(name, tcp).await?;
            Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(stream))
        }
    });
    // tonic refuses an https:// address it does not do the TLS of itself: the connector does
    // it, given the address as http://; the requests keep the https:// origin.
    let plain = settings
        .url
        .strip_prefix("https://")
        .map(|rest| format!("http://{rest}"))
        .context("the broker's URL is https://")?;
    let origin: tonic::codegen::http::Uri = settings.url.parse().context("the broker's URL")?;
    let channel = Endpoint::from_shared(plain)
        .context("the broker's URL")?
        .origin(origin)
        .connect_timeout(CONNECT_TIME)
        .http2_keep_alive_interval(KEEPALIVE)
        .keep_alive_timeout(KEEPALIVE)
        .keep_alive_while_idle(true)
        .connect_with_connector(connector)
        .await
        .context("connecting")?;
    Ok(BrokerClient::new(channel))
}

/// The host's VMs, as the broker hears of them.
async fn inventory(vms: &Vms) -> Inventory {
    Inventory {
        vms: vms
            .inventory()
            .await
            .into_iter()
            .map(|vm| Vm {
                uuid: vm.uuid,
                names: vm.names,
                running: vm.running,
                startable: vm.startable,
            })
            .collect(),
    }
}

/// Serve a connection the broker asks for: open its relay stream, then the session, as for a
/// client of the host's own.
async fn relay(
    mut client: Client,
    connect: Connect,
    vms: Arc<Vms>,
    control: mpsc::Sender<HostMessage>,
) {
    let (to_broker, outgoing) = mpsc::channel(QUEUE);
    let named = RelayMessage {
        message: Some(relay_message::Message::Connection(
            connect.connection.clone(),
        )),
    };
    let _ = to_broker.send(named).await;
    let incoming = match client.relay(ReceiverStream::new(outgoing)).await {
        Ok(response) => response.into_inner(),
        Err(status) => {
            warn!(%status, "relay stream not opened");
            let failed = ConnectFailed {
                connection: connect.connection,
                reason: connect_failed::Reason::Unspecified.into(),
                detail: status.message().to_owned(),
            };
            let _ = control
                .send(HostMessage {
                    message: Some(host_message::Message::ConnectFailed(failed)),
                })
                .await;
            return;
        }
    };
    let user = connect
        .user
        .map(|u| if u.name.is_empty() { u.id } else { u.name })
        .unwrap_or_default();
    let socket = web::Socket::Relayed(Relayed {
        to_broker: Some(to_broker),
        incoming,
    });
    let admission = web::Admission::Broker {
        user,
        shadow: connect.shadow,
    };
    if let Err(e) = web::open(socket, &vms, admission, &connect.vm_uuid, vms.stopping()).await {
        warn!(error = %format!("{e:#}"), "relayed session ended");
    }
}

/// A user's signalling, relayed by the broker: one WebSocket message per relay message.
pub struct Relayed {
    /// `None` once the host's side ended (`close`).
    to_broker: Option<mpsc::Sender<RelayMessage>>,
    incoming: tonic::Streaming<RelayMessage>,
}

impl Relayed {
    pub async fn send(&mut self, bytes: Vec<u8>) -> Result<()> {
        let to_broker = self
            .to_broker
            .as_ref()
            .ok_or_else(|| anyhow!("the session ended"))?;
        to_broker
            .send(RelayMessage {
                message: Some(relay_message::Message::Data(bytes)),
            })
            .await
            .map_err(|_| anyhow!("the broker ended the connection"))
    }

    /// The next message; `None` when the user's WebSocket closed.
    pub async fn receive(&mut self) -> Result<Option<Vec<u8>>> {
        loop {
            match self.incoming.message().await? {
                None => return Ok(None),
                Some(RelayMessage {
                    message: Some(relay_message::Message::Data(bytes)),
                }) => return Ok(Some(bytes)),
                Some(_) => continue,
            }
        }
    }

    /// The session ends: the user's WebSocket closes with `code`. The host's side of the stream
    /// ends after the Close, and the broker ends its own: until then, the call stays, as
    /// dropping it would cancel the stream (RST_STREAM, RFC 9113 8.1) and lose the messages not
    /// yet delivered, the last ones, which say why the session ended.
    pub async fn close(&mut self, code: u16) {
        let Some(to_broker) = self.to_broker.take() else {
            return;
        };
        let _ = to_broker
            .send(RelayMessage {
                message: Some(relay_message::Message::Close(Close { code: code.into() })),
            })
            .await;
        drop(to_broker);
        let drained = async { while let Ok(Some(_)) = self.incoming.message().await {} };
        if tokio::time::timeout(CLOSE_TIME, drained).await.is_err() {
            debug!("the broker did not end the relayed connection in time");
        }
    }
}

/// The settings, refused when they cannot work.
pub fn settings(
    url: String,
    host_id: String,
    ca_bundle: Option<PathBuf>,
    attestation: crate::attestation::Settings,
) -> Result<Settings> {
    if !url.starts_with("https://") {
        anyhow::bail!("the broker's URL is https://host:port: the host talks to it in TLS only");
    }
    if host_id.trim().is_empty() {
        anyhow::bail!("no host_id");
    }
    Ok(Settings {
        url,
        host_id,
        ca_bundle,
        attestation,
    })
}

#[cfg(test)]
mod tests {
    use std::pin::Pin;

    use qdw_protocol::broker::v1::broker_server::{Broker, BrokerServer};
    use qdw_protocol::v1::{ClientMessage, Hello as ClientHello, ServerMessage};
    use qdw_protocol::{Message, v1};
    use tokio_stream::{Stream, StreamExt};
    use tonic::transport::{Identity, Server, ServerTlsConfig};
    use tonic::{Request, Response, Status, Streaming};

    use super::*;

    type Messages<T> = Pin<Box<dyn Stream<Item = Result<T, Status>> + Send>>;

    /// What the test broker saw of the host.
    #[derive(Default)]
    struct Seen {
        hello: Option<Hello>,
        /// Whether the second message was the inventory.
        inventory: bool,
        /// A third message soon after: the host sends nothing more until its VMs change.
        more: bool,
    }

    /// A broker that takes the host's Hello and inventory, asks for one connection, and
    /// plays its user: a hello that decodes nothing, then the host's answer.
    struct TestBroker {
        seen: mpsc::Sender<Seen>,
        answer: mpsc::Sender<(Vec<ServerMessage>, Option<u32>)>,
    }

    /// The host authenticated with a certificate (the TLS layer verified it against the test
    /// authority).
    fn checked<T>(request: &Request<T>) -> Result<(), Status> {
        match request.peer_certs() {
            Some(certificates) if !certificates.is_empty() => Ok(()),
            _ => Err(Status::unauthenticated("no host certificate")),
        }
    }

    /// A test authority and a host's certificate from it, written where the host keeps its own
    /// (a host without a TPM: its key in a file). The authority's certificate, PEM.
    fn host_certificate(state: &std::path::Path) -> String {
        use rcgen::{
            BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer,
            KeyPair,
        };
        let ca_key = KeyPair::generate().unwrap();
        let mut ca = CertificateParams::new(Vec::<String>::new()).unwrap();
        ca.distinguished_name
            .push(DnType::CommonName, "Test hosts CA");
        ca.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
        let ca_cert = ca.self_signed(&ca_key).unwrap();
        let issuer = Issuer::new(ca, ca_key);
        let key = KeyPair::generate().unwrap();
        let mut host = CertificateParams::new(Vec::<String>::new()).unwrap();
        host.distinguished_name.push(DnType::CommonName, "host-1");
        host.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        let cert = host.signed_by(&key, &issuer).unwrap();
        std::fs::create_dir_all(state).unwrap();
        std::fs::write(state.join("host.pem"), cert.pem() + &ca_cert.pem()).unwrap();
        std::fs::write(state.join("host-key.pk8"), key.serialize_der()).unwrap();
        ca_cert.pem()
    }

    #[tonic::async_trait]
    impl Broker for TestBroker {
        type ControlStream = Messages<BrokerMessage>;
        type RelayStream = Messages<RelayMessage>;

        async fn control(
            &self,
            request: Request<Streaming<HostMessage>>,
        ) -> Result<Response<Self::ControlStream>, Status> {
            checked(&request)?;
            let mut host = request.into_inner();
            let mut seen = Seen::default();
            if let Some(host_message::Message::Hello(hello)) =
                host.message().await?.and_then(|m| m.message)
            {
                seen.hello = Some(hello);
            }
            seen.inventory = matches!(
                host.message().await?.and_then(|m| m.message),
                Some(host_message::Message::Inventory(_))
            );
            seen.more = tokio::time::timeout(Duration::from_millis(300), host.message())
                .await
                .is_ok();
            self.seen.send(seen).await.unwrap();
            let connect = BrokerMessage {
                message: Some(broker_message::Message::Connect(Connect {
                    connection: "c1".into(),
                    vm_uuid: "b8ff54ee-77f5-43dd-aa19-de8eaed5eb75".into(),
                    user: Some(qdw_protocol::broker::v1::User {
                        id: "https://idp|alice".into(),
                        name: "Alice".into(),
                    }),
                    shadow: false,
                })),
            };
            // The stream stays open after the Connect, as the host's connection does.
            let stream = tokio_stream::iter([Ok(connect)]).chain(tokio_stream::pending());
            Ok(Response::new(Box::pin(stream)))
        }

        async fn relay(
            &self,
            request: Request<Streaming<RelayMessage>>,
        ) -> Result<Response<Self::RelayStream>, Status> {
            checked(&request)?;
            let mut host = request.into_inner();
            let Some(relay_message::Message::Connection(c)) =
                host.message().await?.and_then(|m| m.message)
            else {
                return Err(Status::invalid_argument("the connection first"));
            };
            assert_eq!(c, "c1");
            // The user's hello: no codec it decodes.
            let hello = ClientMessage {
                message: Some(v1::client_message::Message::Hello(ClientHello::default())),
            }
            .encode_to_vec();
            let answer = self.answer.clone();
            // The broker's side: the user's hello, then the end, once the host's side ended.
            let (to_host, from_broker) = mpsc::channel(4);
            to_host
                .send(Ok(RelayMessage {
                    message: Some(relay_message::Message::Data(hello)),
                }))
                .await
                .unwrap();
            tokio::spawn(async move {
                let _to_host = to_host;
                let (mut messages, mut close) = (Vec::new(), None);
                while let Ok(Some(message)) = host.message().await {
                    match message.message {
                        Some(relay_message::Message::Data(bytes)) => {
                            messages.push(ServerMessage::decode(bytes.as_slice()).unwrap());
                        }
                        Some(relay_message::Message::Close(c)) => close = Some(c.code),
                        _ => {}
                    }
                }
                answer.send((messages, close)).await.unwrap();
            });
            Ok(Response::new(Box::pin(ReceiverStream::new(from_broker))))
        }
    }

    /// The host connects to a broker in TLS with its certificate, says hello, lists its VMs and
    /// sends nothing else (no page), and serves the connection it is asked for as its own
    /// clients: here a client that decodes nothing, refused with the reason, and a normal close.
    #[tokio::test]
    async fn host_serves_the_broker() {
        let _ = tracing_subscriber::fmt().with_test_writer().try_init();
        let key = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let dir = std::env::temp_dir().join(format!("qdw-broker-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ca = dir.join("ca.pem");
        std::fs::write(&ca, key.cert.pem()).unwrap();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (seen_tx, mut seen) = mpsc::channel(1);
        let (answer_tx, mut answer) = mpsc::channel(1);
        let identity = Identity::from_pem(key.cert.pem(), key.signing_key.serialize_pem());
        let state = dir.join("state");
        let hosts_ca = host_certificate(&state);
        tokio::spawn(
            Server::builder()
                .tls_config(
                    ServerTlsConfig::new()
                        .identity(identity)
                        .client_ca_root(tonic::transport::Certificate::from_pem(hosts_ca)),
                )
                .unwrap()
                .add_service(BrokerServer::new(TestBroker {
                    seen: seen_tx,
                    answer: answer_tx,
                }))
                .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener)),
        );

        let config = crate::config::parse(Vec::new()).unwrap();
        let vms = Vms::new(crate::providers::Providers::default(), config.vm);
        let attestation = crate::attestation::Settings {
            directory: format!("https://localhost:{port}/acme/directory"),
            host_id: "host-1".into(),
            roots: None,
            state,
            tpm: None,
            enrollment: None,
        };
        let settings = settings(
            format!("https://localhost:{port}"),
            "host-1".into(),
            Some(ca),
            attestation,
        )
        .unwrap();
        tokio::spawn(keep(settings, vms.clone()));

        let seen = seen.recv().await.unwrap();
        assert_eq!(seen.hello.unwrap().host_id, "host-1");
        assert!(seen.inventory);
        // No page: the broker serves its own.
        assert!(!seen.more);
        let (messages, close) = answer.recv().await.unwrap();
        let Some(v1::server_message::Message::Error(error)) =
            messages.first().and_then(|m| m.message.clone())
        else {
            panic!("an error first: {messages:?}");
        };
        assert!(
            error.message.contains("does not decode"),
            "{}",
            error.message
        );
        assert_eq!(close, Some(1000));
        vms.stop();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn settings_refused() {
        let attestation = crate::attestation::Settings {
            directory: "https://broker:8443/acme/directory".into(),
            host_id: "h".into(),
            roots: None,
            state: "/nonexistent".into(),
            tpm: None,
            enrollment: None,
        };
        let ok = |url: &str, id: &str| {
            settings(url.into(), id.into(), None, attestation.clone()).is_ok()
        };
        assert!(ok("https://broker:8443", "h"));
        assert!(!ok("http://broker:8443", "h"));
        assert!(!ok("https://broker:8443", " "));
    }
}
