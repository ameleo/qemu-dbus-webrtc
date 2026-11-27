// SPDX-License-Identifier: MPL-2.0

//! The broker service between a real gRPC client (a host) and server (a broker), over a local
//! connection: the host's credential in the metadata, the Hello and the inventory on the control
//! stream, a Connect back, and a relayed connection both ways.

#![cfg(feature = "broker")]

use std::pin::Pin;

use qdw_protocol::broker::v1::broker_client::BrokerClient;
use qdw_protocol::broker::v1::broker_server::{Broker, BrokerServer};
use qdw_protocol::broker::v1::{
    BrokerMessage, Connect, Hello, HostMessage, Inventory, RelayMessage, User, Vm, broker_message,
    host_message, relay_message,
};
use tokio::sync::mpsc;
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};
use tokio_stream::{Stream, StreamExt};
use tonic::{Request, Response, Status, Streaming};

type Messages<T> = Pin<Box<dyn Stream<Item = Result<T, Status>> + Send>>;

const CREDENTIAL: &str = "Bearer host-credential";

/// A broker that checks the host's credential, takes its Hello and inventory, asks it for one
/// connection, and echoes the relayed messages back, reversed.
struct TestBroker;

fn check(metadata: &tonic::metadata::MetadataMap) -> Result<(), Status> {
    match metadata.get("authorization").map(|v| v.to_str()) {
        Some(Ok(CREDENTIAL)) => Ok(()),
        _ => Err(Status::unauthenticated("no host credential")),
    }
}

#[tonic::async_trait]
impl Broker for TestBroker {
    type ControlStream = Messages<BrokerMessage>;
    type RelayStream = Messages<RelayMessage>;

    async fn control(
        &self,
        request: Request<Streaming<HostMessage>>,
    ) -> Result<Response<Self::ControlStream>, Status> {
        check(request.metadata())?;
        let mut host = request.into_inner();
        let hello = match host.next().await {
            Some(Ok(HostMessage {
                message: Some(host_message::Message::Hello(hello)),
            })) => hello,
            _ => return Err(Status::invalid_argument("the first message is a Hello")),
        };
        let Some(Ok(HostMessage {
            message: Some(host_message::Message::Inventory(inventory)),
        })) = host.next().await
        else {
            return Err(Status::invalid_argument("an inventory after the Hello"));
        };
        assert_eq!(hello.host_id, "host-1");
        let vm = inventory.vms[0].uuid.clone();
        let (tx, rx) = mpsc::channel(4);
        tx.send(Ok(BrokerMessage {
            message: Some(broker_message::Message::Connect(Connect {
                connection: "c1".into(),
                vm_uuid: vm,
                user: Some(User {
                    id: "https://idp|alice".into(),
                    name: "Alice".into(),
                }),
                shadow: false,
                power: false,
            })),
        }))
        .await
        .unwrap();
        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }

    async fn relay(
        &self,
        request: Request<Streaming<RelayMessage>>,
    ) -> Result<Response<Self::RelayStream>, Status> {
        check(request.metadata())?;
        let mut host = request.into_inner();
        match host.next().await {
            Some(Ok(RelayMessage {
                message: Some(relay_message::Message::Connection(c)),
            })) if c == "c1" => {}
            _ => {
                return Err(Status::invalid_argument(
                    "the first message names the connection",
                ));
            }
        }
        let echoed = host.map(|message| {
            let mut message = message?;
            if let Some(relay_message::Message::Data(data)) = &mut message.message {
                data.reverse();
            }
            Ok(message)
        });
        Ok(Response::new(Box::pin(echoed)))
    }
}

#[tokio::test]
async fn host_and_broker() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(
        tonic::transport::Server::builder()
            .add_service(BrokerServer::new(TestBroker))
            .serve_with_incoming(TcpListenerStream::new(listener)),
    );

    let channel = tonic::transport::Endpoint::from_shared(format!("http://{address}"))
        .unwrap()
        .connect()
        .await
        .unwrap();
    let authorize = |mut request: Request<()>| {
        request
            .metadata_mut()
            .insert("authorization", CREDENTIAL.parse().unwrap());
        Ok(request)
    };
    let mut host = BrokerClient::with_interceptor(channel.clone(), authorize);

    // The control stream: Hello, inventory, and the broker's Connect.
    let (to_broker, outgoing) = mpsc::channel(4);
    for message in [
        host_message::Message::Hello(Hello {
            host_id: "host-1".into(),
            version: "0.2.0".into(),
        }),
        host_message::Message::Inventory(Inventory {
            vms: vec![Vm {
                uuid: "b8ff54ee-77f5-43dd-aa19-de8eaed5eb75".into(),
                names: vec!["win11".into()],
                running: true,
                startable: false,
            }],
        }),
    ] {
        to_broker
            .send(HostMessage {
                message: Some(message),
            })
            .await
            .unwrap();
    }
    let mut control = host
        .control(ReceiverStream::new(outgoing))
        .await
        .unwrap()
        .into_inner();
    let Some(broker_message::Message::Connect(connect)) =
        control.next().await.unwrap().unwrap().message
    else {
        panic!("a Connect");
    };
    assert_eq!(connect.vm_uuid, "b8ff54ee-77f5-43dd-aa19-de8eaed5eb75");
    assert_eq!(connect.user.unwrap().name, "Alice");

    // The relayed connection it asks for: named first, then messages both ways.
    let relayed = tokio_stream::iter([
        relay_message::Message::Connection(connect.connection),
        relay_message::Message::Data(vec![1, 2, 3]),
    ])
    .map(|message| RelayMessage {
        message: Some(message),
    });
    let mut back = host.relay(relayed).await.unwrap().into_inner();
    let Some(relay_message::Message::Data(data)) = back.next().await.unwrap().unwrap().message
    else {
        panic!("data");
    };
    assert_eq!(data, [3, 2, 1]);
    // The host's side ended: so does the broker's.
    assert!(back.next().await.is_none());

    // Without the host's credential, the broker refuses the call.
    let mut stranger = BrokerClient::new(channel);
    let refused = stranger.relay(tokio_stream::empty()).await.unwrap_err();
    assert_eq!(refused.code(), tonic::Code::Unauthenticated);
}
