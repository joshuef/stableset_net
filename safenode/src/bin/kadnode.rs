mod log;

use async_trait::async_trait;
use bincode::de;
use futures::StreamExt;
use futures::{prelude::*, AsyncWriteExt};
use futures::{select, FutureExt};
use libp2p::{
    core::muxing::StreamMuxerBox,
    core::upgrade::{read_length_prefixed, read_varint, write_length_prefixed, write_varint},
    identity,
    kad::{
        record::store::MemoryStore, GetClosestPeersError, Kademlia, KademliaConfig, KademliaEvent,
        QueryResult,
    },
    mdns, quic, request_response,
    swarm::{NetworkBehaviour, SwarmBuilder, SwarmEvent},
    PeerId, Swarm, Transport,
};
use log::init_node_logging;
// use safenode::error::Result;
use bytes::Bytes;
use eyre::{Error, Result};
use std::{env, io, path::PathBuf, time::Duration};
use xor_name::XorName;

#[macro_use]
extern crate tracing;

// We create a custom network behaviour that combines Kademlia and mDNS.
// mDNS is for local discovery only
#[derive(NetworkBehaviour)]
#[behaviour(out_event = "SafeNetBehaviour")]
struct MyBehaviour {
    kademlia: Kademlia<MemoryStore>,
    req_resp: request_response::Behaviour<PingCodec>,
    mdns: mdns::async_io::Behaviour,
}

impl MyBehaviour {
    fn get_closest_peers_to_xorname(&mut self, addr: XorName) {
        self.kademlia.get_closest_peers(addr.to_vec());
    }

    fn send_to_peer(&mut self, peer: &PeerId, bytes: Bytes) {
        let ping = Ping(bytes.to_vec());
        self.req_resp.send_request(peer, ping.clone());
    }
}

#[allow(clippy::large_enum_variant)]
enum SafeNetBehaviour {
    Kademlia(KademliaEvent),
    Mdns(mdns::Event),
    ReqResp,
}

impl From<KademliaEvent> for SafeNetBehaviour {
    fn from(event: KademliaEvent) -> Self {
        SafeNetBehaviour::Kademlia(event)
    }
}

impl From<mdns::Event> for SafeNetBehaviour {
    fn from(event: mdns::Event) -> Self {
        SafeNetBehaviour::Mdns(event)
    }
}

impl From<request_response::Event<Ping, Pong>> for SafeNetBehaviour {
    fn from(event: request_response::Event<Ping, Pong>) -> Self {
        SafeNetBehaviour::ReqResp
    }
}

#[derive(Debug)]
enum SwarmCmd {
    Search(XorName),
    Get,
}

/// Channel to send Cmds to the swarm
type CmdChannel = tokio::sync::mpsc::Sender<SwarmCmd>;

fn run_swarm() -> CmdChannel {
    let (sender, mut receiver) = tokio::sync::mpsc::channel::<SwarmCmd>(1);

    let handle = tokio::spawn(async move {
        debug!("Starting swarm");
        // Create a random key for ourselves.
        let keypair = identity::Keypair::generate_ed25519();
        let local_peer_id = PeerId::from(keypair.public());
        info!("My PeerId is {local_peer_id}");

        // QUIC configuration
        let quic_config = quic::Config::new(&keypair);
        let mut transport = quic::async_std::Transport::new(quic_config);

        let transport = transport
            .map(|(peer_id, muxer), _| (peer_id, StreamMuxerBox::new(muxer)))
            .boxed();

        // Create a Kademlia instance and connect to the network address.
        // Create a swarm to manage peers and events.
        let mut swarm = {
            // Create a Kademlia behaviour.
            let mut cfg = KademliaConfig::default();
            cfg.set_query_timeout(Duration::from_secs(5 * 60));
            let store = MemoryStore::new(local_peer_id);
            let kademlia = Kademlia::new(local_peer_id, store);
            let mdns = mdns::async_io::Behaviour::new(mdns::Config::default(), local_peer_id)?;

            let protocols =
                std::iter::once((PingProtocol(), request_response::ProtocolSupport::Full));
            let cfg = request_response::Config::default();
            let req_resp = request_response::Behaviour::new(PingCodec(), protocols, cfg);

            let behaviour = MyBehaviour {
                kademlia,
                req_resp,
                mdns,
            };

            let mut swarm =
                SwarmBuilder::with_async_std_executor(transport, behaviour, local_peer_id).build();

            // // Listen on all interfaces and whatever port the OS assigns.
            let addr = "/ip4/0.0.0.0/udp/0/quic-v1".parse().expect("addr okay");
            swarm.listen_on(addr).expect("listening failed");

            swarm
        };

        let net_info = swarm.network_info();

        debug!("network info: {net_info:?}");
        // Kick it off.
        loop {
            select! {
                cmd = receiver.recv().fuse() => {
                    debug!("Cmd in: {cmd:?}");
                    match cmd {
                        Some(SwarmCmd::Search(xor_name)) => swarm.behaviour_mut().get_closest_peers_to_xorname(xor_name),
                        Some(SwarmCmd::Get) => swarm.behaviour_mut().send_to_peer(&PeerId::random(), Bytes::from("hello")),
                        None => {}
                    }
                }

                event = swarm.select_next_some() => match event {
                    SwarmEvent::NewListenAddr { address, .. } => {
                        info!("Listening in {address:?}");
                    },
                    SwarmEvent::Behaviour(SafeNetBehaviour::Mdns(mdns::Event::Discovered(list))) => {
                        for (peer_id, multiaddr) in list {
                            info!("Node discovered: {multiaddr:?}");
                            swarm.behaviour_mut().kademlia.add_address(&peer_id, multiaddr);
                        }
                    }
                    SwarmEvent::Behaviour(SafeNetBehaviour::Kademlia(KademliaEvent::OutboundQueryProgressed {
                        result: QueryResult::GetClosestPeers(result),
                        ..
                    })) => {

                        info!("Result for closest peers is in! {result:?}");
                    }
                    // SwarmEvent::Behaviour(SafeNetBehaviour::Kademlia(KademliaEvent::RoutingUpdated{addresses, ..})) => {

                    //     trace!("Kad routing updated: {addresses:?}");
                    // }
                    // SwarmEvent::Behaviour(SafeNetBehaviour::Kademlia(KademliaEvent::OutboundQueryProgressed { result, ..})) => {
                    //     match result {
                    //         // QueryResult::GetProviders(Ok(GetProvidersOk::FoundProviders { key, providers, .. })) => {
                    //         //     for peer in providers {
                    //         //         println!(
                    //         //             "Peer {peer:?} provides key {:?}",
                    //         //             std::str::from_utf8(key.as_ref()).unwrap()
                    //         //         );
                    //         //     }
                    //         // }
                    //         // QueryResult::GetProviders(Err(err)) => {
                    //         //     eprintln!("Failed to get providers: {err:?}");
                    //         // }
                    //         // QueryResult::GetRecord(Ok(
                    //         //     GetRecordOk::FoundRecord(PeerRecord {
                    //         //         record: Record { key, value, .. },
                    //         //         ..
                    //         //     })
                    //         // )) => {
                    //         //     println!(
                    //         //         "Got record {:?} {:?}",
                    //         //         std::str::from_utf8(key.as_ref()).unwrap(),
                    //         //         std::str::from_utf8(&value).unwrap(),
                    //         //     );
                    //         // }
                    //         // QueryResult::GetRecord(Ok(_)) => {}
                    //         // QueryResult::GetRecord(Err(err)) => {
                    //         //     eprintln!("Failed to get record: {err:?}");
                    //         // }
                    //         // QueryResult::PutRecord(Ok(PutRecordOk { key })) => {
                    //         //     println!(
                    //         //         "Successfully put record {:?}",
                    //         //         std::str::from_utf8(key.as_ref()).unwrap()
                    //         //     );
                    //         // }
                    //         // QueryResult::PutRecord(Err(err)) => {
                    //         //     eprintln!("Failed to put record: {err:?}");
                    //         // }
                    //         // QueryResult::StartProviding(Ok(AddProviderOk { key })) => {
                    //         //     println!(
                    //         //         "Successfully put provider record {:?}",
                    //         //         std::str::from_utf8(key.as_ref()).unwrap()
                    //         //     );
                    //         // }
                    //         // QueryResult::StartProviding(Err(err)) => {
                    //         //     eprintln!("Failed to put provider record: {err:?}");
                    //         // }
                    //         _ => {
                    //             //
                    //         }
                    //     }
                    // }
                    _ => debug!("Other type of SwarmEvent we are not handling!")
                }

            }
        }

        Ok::<(), Error>(())
    });

    sender
}

#[tokio::main]
async fn main() -> Result<()> {
    let log_dir = grab_log_dir();
    let _log_appender_guard = init_node_logging(&log_dir)?;

    info!("start");
    let channel = run_swarm();

    let x = xor_name::XorName::from_content(b"some random content here for you");

    channel.send(SwarmCmd::Search(x)).await;

    tokio::time::sleep(Duration::from_secs(5)).await;

    channel.send(SwarmCmd::Search(x)).await;
    loop {
        tokio::time::sleep(Duration::from_millis(100)).await
    }

    Ok(())
}

/// Grabs the log dir arg if passed in
fn grab_log_dir() -> Option<PathBuf> {
    let mut args = std::env::args().skip(1); // Skip the first argument (the program name)

    let mut log_dir = None;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--log-dir" => {
                log_dir = args.next();
            }
            _ => {
                println!("Unknown argument: {}", arg);
            }
        }
    }

    if let Some(log_dir) = log_dir {
        Some(PathBuf::from(log_dir))
    } else {
        None
    }
}

// ***************************************

// Simple Ping-Pong Protocol

#[derive(Debug, Clone)]
struct PingProtocol();
#[derive(Clone)]
struct PingCodec();
#[derive(Debug, Clone, PartialEq, Eq)]
struct Ping(Vec<u8>);
#[derive(Debug, Clone, PartialEq, Eq)]
struct Pong(Vec<u8>);

impl request_response::ProtocolName for PingProtocol {
    fn protocol_name(&self) -> &[u8] {
        "/ping/1".as_bytes()
    }
}

#[async_trait]
impl request_response::Codec for PingCodec {
    type Protocol = PingProtocol;
    type Request = Ping;
    type Response = Pong;

    async fn read_request<T>(&mut self, _: &PingProtocol, io: &mut T) -> io::Result<Self::Request>
    where
        T: AsyncRead + Unpin + Send,
    {
        let vec = read_length_prefixed(io, 1024).await?;

        if vec.is_empty() {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }

        Ok(Ping(vec))
    }

    async fn read_response<T>(&mut self, _: &PingProtocol, io: &mut T) -> io::Result<Self::Response>
    where
        T: AsyncRead + Unpin + Send,
    {
        let vec = read_length_prefixed(io, 1024).await?;

        if vec.is_empty() {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }

        Ok(Pong(vec))
    }

    async fn write_request<T>(
        &mut self,
        _: &PingProtocol,
        io: &mut T,
        Ping(data): Ping,
    ) -> io::Result<()>
    where
        T: AsyncWrite + Unpin + Send,
    {
        write_length_prefixed(io, data).await?;
        io.close().await?;

        Ok(())
    }

    async fn write_response<T>(
        &mut self,
        _: &PingProtocol,
        io: &mut T,
        Pong(data): Pong,
    ) -> io::Result<()>
    where
        T: AsyncWrite + Unpin + Send,
    {
        write_length_prefixed(io, data).await?;
        io.close().await?;

        Ok(())
    }
}
