// Copyright 2024 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under The General Public License (GPL), version 3.
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied. Please review the Licences for the specific language governing
// permissions and limitations relating to use of the SAFE Network Software.

#[cfg(feature = "open-metrics")]
use crate::metrics::NetworkMetrics;
#[cfg(feature = "open-metrics")]
use crate::metrics_service::run_metrics_server;
use crate::{
    bootstrap::{ContinuousBootstrap, BOOTSTRAP_INTERVAL},
    circular_vec::CircularVec,
    cmd::{LocalSwarmCmd, NetworkSwarmCmd},
    error::{NetworkError, Result},
    event::{NetworkEvent, NodeEvent},
    multiaddr_pop_p2p,
    network_discovery::NetworkDiscovery,
    record_store::{ClientRecordStore, NodeRecordStore, NodeRecordStoreConfig},
    record_store_api::UnifiedRecordStore,
    relay_manager::RelayManager,
    replication_fetcher::ReplicationFetcher,
    target_arch::{interval, spawn, Instant},
    version::{
        IDENTIFY_CLIENT_VERSION_STR, IDENTIFY_NODE_VERSION_STR, IDENTIFY_PROTOCOL_STR,
        REQ_RESPONSE_VERSION_STR,
    },
    GetRecordError, Network, CLOSE_GROUP_SIZE,
};
use crate::{transport, NodeIssue};
use futures::future::Either;
use futures::StreamExt;
#[cfg(feature = "local-discovery")]
use libp2p::mdns;
use libp2p::{core::muxing::StreamMuxerBox, relay};
use libp2p::{
    identity::Keypair,
    kad::{self, QueryId, Quorum, Record, K_VALUE},
    multiaddr::Protocol,
    request_response::{self, Config as RequestResponseConfig, OutboundRequestId, ProtocolSupport},
    swarm::{
        dial_opts::{DialOpts, PeerCondition},
        ConnectionId, DialError, NetworkBehaviour, StreamProtocol, Swarm,
    },
    Multiaddr, PeerId,
};
use libp2p::{kad::KBucketDistance, Transport as _};
#[cfg(feature = "open-metrics")]
use prometheus_client::registry::Registry;
use sn_protocol::{
    messages::{ChunkProof, Nonce, Request, Response},
    storage::RetryStrategy,
    NetworkAddress, PrettyPrintKBucketKey, PrettyPrintRecordKey,
};
use sn_transfers::PaymentQuote;
use std::{
    collections::{btree_map::Entry, BTreeMap, BTreeSet, HashMap, HashSet, VecDeque},
    fmt::Debug,
    net::SocketAddr,
    num::NonZeroUsize,
    path::PathBuf,
};
use tokio::sync::{
    mpsc::{self, error::TryRecvError},
    oneshot,
};
use tokio::time::Duration;
use tracing::warn;
use xor_name::XorName;

/// Interval over which we check for the farthest record we _should_ be holding
/// based upon our knowledge of the CLOSE_GROUP
pub(crate) const CLOSET_RECORD_CHECK_INTERVAL: Duration = Duration::from_secs(15);

/// Interval over which we query relay manager to check if we can make any more reservations.
pub(crate) const RELAY_MANAGER_RESERVATION_INTERVAL: Duration = Duration::from_secs(30);

// Number of range distances to keep in the circular buffer
pub const X_RANGE_STORAGE_LIMIT: usize = 100;

/// The ways in which the Get Closest queries are used.
pub(crate) enum PendingGetClosestType {
    /// The network discovery method is present at the networking layer
    /// Thus we can just process the queries made by NetworkDiscovery without using any channels
    NetworkDiscovery,
    /// These are queries made by a function at the upper layers and contains a channel to send the result back.
    FunctionCall(oneshot::Sender<Vec<PeerId>>),
}
type PendingGetClosest = HashMap<QueryId, (PendingGetClosestType, Vec<PeerId>)>;

/// Using XorName to differentiate different record content under the same key.
type GetRecordResultMap = HashMap<XorName, (Record, HashSet<PeerId>)>;
pub(crate) type PendingGetRecord = HashMap<
    QueryId,
    (
        oneshot::Sender<std::result::Result<Record, GetRecordError>>,
        GetRecordResultMap,
        GetRecordCfg,
    ),
>;

/// 10 is the max number of issues per node we track to avoid mem leaks
/// The boolean flag to indicate whether the node is considered as bad or not
pub(crate) type BadNodes = BTreeMap<PeerId, (Vec<(NodeIssue, Instant)>, bool)>;

/// What is the largest packet to send over the network.
/// Records larger than this will be rejected.
// TODO: revisit once cashnote_redemption is in
pub const MAX_PACKET_SIZE: usize = 1024 * 1024 * 5; // the chunk size is 1mb, so should be higher than that to prevent failures, 5mb here to allow for CashNote storage

// Timeout for requests sent/received through the request_response behaviour.
const REQUEST_TIMEOUT_DEFAULT_S: Duration = Duration::from_secs(30);
// Sets the keep-alive timeout of idle connections.
const CONNECTION_KEEP_ALIVE_TIMEOUT: Duration = Duration::from_secs(30);

// Inverval of resending identify to connected peers.
const RESEND_IDENTIFY_INVERVAL: Duration = Duration::from_secs(3600);

const NETWORKING_CHANNEL_SIZE: usize = 10_000;

/// Time before a Kad query times out if no response is received
const KAD_QUERY_TIMEOUT_S: Duration = Duration::from_secs(10);

/// The various settings to apply to when fetching a record from network
#[derive(Clone)]
pub struct GetRecordCfg {
    /// The query will result in an error if we get records less than the provided Quorum
    pub get_quorum: Quorum,
    /// If enabled, the provided `RetryStrategy` is used to retry if a GET attempt fails.
    pub retry_strategy: Option<RetryStrategy>,
    /// Only return if we fetch the provided record.
    pub target_record: Option<Record>,
    /// Logs if the record was not fetched from the provided set of peers.
    pub expected_holders: HashSet<PeerId>,
}

impl GetRecordCfg {
    pub fn does_target_match(&self, record: &Record) -> bool {
        self.target_record.as_ref().is_some_and(|t| t == record)
    }
}

impl Debug for GetRecordCfg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut f = f.debug_struct("GetRecordCfg");
        f.field("get_quorum", &self.get_quorum)
            .field("retry_strategy", &self.retry_strategy);

        match &self.target_record {
            Some(record) => {
                let pretty_key = PrettyPrintRecordKey::from(&record.key);
                f.field("target_record", &pretty_key);
            }
            None => {
                f.field("target_record", &"None");
            }
        };

        f.field("expected_holders", &self.expected_holders).finish()
    }
}

/// The various settings related to writing a record to the network.
#[derive(Debug, Clone)]
pub struct PutRecordCfg {
    /// The quorum used by KAD PUT. KAD still sends out the request to all the peers set by the `replication_factor`, it
    /// just makes sure that we get atleast `n` successful responses defined by the Quorum.
    /// Our nodes currently send `Ok()` response for every KAD PUT. Thus this field does not do anything atm.
    pub put_quorum: Quorum,
    /// If enabled, the provided `RetryStrategy` is used to retry if a PUT attempt fails.
    pub retry_strategy: Option<RetryStrategy>,
    /// Use the `kad::put_record_to` to PUT the record only to the specified peers. If this option is set to None, we
    /// will be using `kad::put_record` which would PUT the record to all the closest members of the record.
    pub use_put_record_to: Option<Vec<PeerId>>,
    /// Enables verification after writing. The VerificationKind is used to determine the method to use.
    pub verification: Option<(VerificationKind, GetRecordCfg)>,
}

/// The methods in which verification on a PUT can be carried out.
#[derive(Debug, Clone)]
pub enum VerificationKind {
    /// Uses the default KAD GET to perform verification.
    Network,
    /// Uses the hash based verification for chunks.
    ChunkProof {
        expected_proof: ChunkProof,
        nonce: Nonce,
    },
}

/// NodeBehaviour struct
#[derive(NetworkBehaviour)]
#[behaviour(to_swarm = "NodeEvent")]
pub(super) struct NodeBehaviour {
    #[cfg(feature = "upnp")]
    pub(super) upnp: libp2p::swarm::behaviour::toggle::Toggle<libp2p::upnp::tokio::Behaviour>,
    pub(super) request_response: request_response::cbor::Behaviour<Request, Response>,
    pub(super) kademlia: kad::Behaviour<UnifiedRecordStore>,
    #[cfg(feature = "local-discovery")]
    pub(super) mdns: mdns::tokio::Behaviour,
    pub(super) identify: libp2p::identify::Behaviour,
    pub(super) dcutr: libp2p::dcutr::Behaviour,
    pub(super) relay_client: libp2p::relay::client::Behaviour,
    pub(super) relay_server: libp2p::relay::Behaviour,
}

#[derive(Debug)]
pub struct NetworkBuilder {
    is_behind_home_network: bool,
    keypair: Keypair,
    local: bool,
    root_dir: PathBuf,
    listen_addr: Option<SocketAddr>,
    request_timeout: Option<Duration>,
    concurrency_limit: Option<usize>,
    initial_peers: Vec<Multiaddr>,
    #[cfg(feature = "open-metrics")]
    metrics_registry: Option<Registry>,
    #[cfg(feature = "open-metrics")]
    /// Set to Some to enable the metrics server
    metrics_server_port: Option<u16>,
    #[cfg(feature = "upnp")]
    upnp: bool,
}

impl NetworkBuilder {
    pub fn new(keypair: Keypair, local: bool, root_dir: PathBuf) -> Self {
        Self {
            is_behind_home_network: false,
            keypair,
            local,
            root_dir,
            listen_addr: None,
            request_timeout: None,
            concurrency_limit: None,
            initial_peers: Default::default(),
            #[cfg(feature = "open-metrics")]
            metrics_registry: None,
            #[cfg(feature = "open-metrics")]
            metrics_server_port: None,
            #[cfg(feature = "upnp")]
            upnp: false,
        }
    }

    pub fn is_behind_home_network(&mut self, enable: bool) {
        self.is_behind_home_network = enable;
    }

    pub fn listen_addr(&mut self, listen_addr: SocketAddr) {
        self.listen_addr = Some(listen_addr);
    }

    pub fn request_timeout(&mut self, request_timeout: Duration) {
        self.request_timeout = Some(request_timeout);
    }

    pub fn concurrency_limit(&mut self, concurrency_limit: usize) {
        self.concurrency_limit = Some(concurrency_limit);
    }

    pub fn initial_peers(&mut self, initial_peers: Vec<Multiaddr>) {
        self.initial_peers = initial_peers;
    }

    #[cfg(feature = "open-metrics")]
    pub fn metrics_registry(&mut self, metrics_registry: Option<Registry>) {
        self.metrics_registry = metrics_registry;
    }

    #[cfg(feature = "open-metrics")]
    pub fn metrics_server_port(&mut self, port: Option<u16>) {
        self.metrics_server_port = port;
    }

    #[cfg(feature = "upnp")]
    pub fn upnp(&mut self, upnp: bool) {
        self.upnp = upnp;
    }

    /// Creates a new `SwarmDriver` instance, along with a `Network` handle
    /// for sending commands and an `mpsc::Receiver<NetworkEvent>` for receiving
    /// network events. It initializes the swarm, sets up the transport, and
    /// configures the Kademlia and mDNS behaviour for peer discovery.
    ///
    /// # Returns
    ///
    /// A tuple containing a `Network` handle, an `mpsc::Receiver<NetworkEvent>`,
    /// and a `SwarmDriver` instance.
    ///
    /// # Errors
    ///
    /// Returns an error if there is a problem initializing the mDNS behaviour.
    pub fn build_node(self) -> Result<(Network, mpsc::Receiver<NetworkEvent>, SwarmDriver)> {
        let mut kad_cfg = kad::Config::default();
        let _ = kad_cfg
            .set_kbucket_inserts(libp2p::kad::BucketInserts::Manual)
            // how often a node will replicate records that it has stored, aka copying the key-value pair to other nodes
            // this is a heavier operation than publication, so it is done less frequently
            // Set to `None` to ensure periodic replication disabled.
            .set_replication_interval(None)
            // how often a node will publish a record key, aka telling the others it exists
            // Set to `None` to ensure periodic publish disabled.
            .set_publication_interval(None)
            // 1mb packet size
            .set_max_packet_size(MAX_PACKET_SIZE)
            // How many nodes _should_ store data.
            .set_replication_factor(
                NonZeroUsize::new(CLOSE_GROUP_SIZE)
                    .ok_or_else(|| NetworkError::InvalidCloseGroupSize)?,
            )
            .set_query_timeout(KAD_QUERY_TIMEOUT_S)
            // Require iterative queries to use disjoint paths for increased resiliency in the presence of potentially adversarial nodes.
            .disjoint_query_paths(true)
            // Records never expire
            .set_record_ttl(None)
            // Emit PUT events for validation prior to insertion into the RecordStore.
            // This is no longer needed as the record_storage::put now can carry out validation.
            // .set_record_filtering(KademliaStoreInserts::FilterBoth)
            // Disable provider records publication job
            .set_provider_publication_interval(None);

        let store_cfg = {
            // Configures the disk_store to store records under the provided path and increase the max record size
            let storage_dir_path = self.root_dir.join("record_store");
            if let Err(error) = std::fs::create_dir_all(&storage_dir_path) {
                return Err(NetworkError::FailedToCreateRecordStoreDir {
                    path: storage_dir_path,
                    source: error,
                });
            }
            NodeRecordStoreConfig {
                max_value_bytes: MAX_PACKET_SIZE, // TODO, does this need to be _less_ than MAX_PACKET_SIZE
                storage_dir: storage_dir_path,
                historic_quote_dir: self.root_dir.clone(),
                ..Default::default()
            }
        };

        let listen_addr = self.listen_addr;
        #[cfg(feature = "upnp")]
        let upnp = self.upnp;

        let (network, events_receiver, mut swarm_driver) = self.build(
            kad_cfg,
            Some(store_cfg),
            false,
            ProtocolSupport::Full,
            IDENTIFY_NODE_VERSION_STR.to_string(),
            #[cfg(feature = "upnp")]
            upnp,
        )?;

        // Listen on the provided address
        let listen_socket_addr = listen_addr.ok_or(NetworkError::ListenAddressNotProvided)?;

        // Listen on QUIC
        let addr_quic = Multiaddr::from(listen_socket_addr.ip())
            .with(Protocol::Udp(listen_socket_addr.port()))
            .with(Protocol::QuicV1);
        swarm_driver
            .listen_on(addr_quic)
            .expect("Multiaddr should be supported by our configured transports");

        // Listen on WebSocket
        #[cfg(any(feature = "websockets", target_arch = "wasm32"))]
        {
            let addr_ws = Multiaddr::from(listen_socket_addr.ip())
                .with(Protocol::Tcp(listen_socket_addr.port()))
                .with(Protocol::Ws("/".into()));
            swarm_driver
                .listen_on(addr_ws)
                .expect("Multiaddr should be supported by our configured transports");
        }

        Ok((network, events_receiver, swarm_driver))
    }

    /// Same as `build_node` API but creates the network components in client mode
    pub fn build_client(self) -> Result<(Network, mpsc::Receiver<NetworkEvent>, SwarmDriver)> {
        // Create a Kademlia behaviour for client mode, i.e. set req/resp protocol
        // to outbound-only mode and don't listen on any address
        let mut kad_cfg = kad::Config::default(); // default query timeout is 60 secs

        // 1mb packet size
        let _ = kad_cfg
            .set_kbucket_inserts(libp2p::kad::BucketInserts::Manual)
            .set_max_packet_size(MAX_PACKET_SIZE)
            // Require iterative queries to use disjoint paths for increased resiliency in the presence of potentially adversarial nodes.
            .disjoint_query_paths(true)
            // How many nodes _should_ store data.
            .set_replication_factor(
                NonZeroUsize::new(CLOSE_GROUP_SIZE)
                    .ok_or_else(|| NetworkError::InvalidCloseGroupSize)?,
            );

        let (network, net_event_recv, driver) = self.build(
            kad_cfg,
            None,
            true,
            ProtocolSupport::Outbound,
            IDENTIFY_CLIENT_VERSION_STR.to_string(),
            #[cfg(feature = "upnp")]
            false,
        )?;

        Ok((network, net_event_recv, driver))
    }

    /// Private helper to create the network components with the provided config and req/res behaviour
    fn build(
        self,
        kad_cfg: kad::Config,
        record_store_cfg: Option<NodeRecordStoreConfig>,
        is_client: bool,
        req_res_protocol: ProtocolSupport,
        identify_version: String,
        #[cfg(feature = "upnp")] upnp: bool,
    ) -> Result<(Network, mpsc::Receiver<NetworkEvent>, SwarmDriver)> {
        let peer_id = PeerId::from(self.keypair.public());
        // vdash metric (if modified please notify at https://github.com/happybeing/vdash/issues):
        #[cfg(not(target_arch = "wasm32"))]
        info!(
            "Process (PID: {}) with PeerId: {peer_id}",
            std::process::id()
        );
        info!(
            "Self PeerID {peer_id} is represented as kbucket_key {:?}",
            PrettyPrintKBucketKey(NetworkAddress::from_peer(peer_id).as_kbucket_key())
        );

        #[cfg(feature = "open-metrics")]
        let network_metrics = if let Some(port) = self.metrics_server_port {
            let mut metrics_registry = self.metrics_registry.unwrap_or_default();
            let metrics = NetworkMetrics::new(&mut metrics_registry);
            run_metrics_server(metrics_registry, port);
            Some(metrics)
        } else {
            None
        };

        // RequestResponse Behaviour
        let request_response = {
            let cfg = RequestResponseConfig::default()
                .with_request_timeout(self.request_timeout.unwrap_or(REQUEST_TIMEOUT_DEFAULT_S));

            info!(
                "Building request response with {:?}",
                REQ_RESPONSE_VERSION_STR.as_str()
            );
            request_response::cbor::Behaviour::new(
                [(
                    StreamProtocol::new(&REQ_RESPONSE_VERSION_STR),
                    req_res_protocol,
                )],
                cfg,
            )
        };

        let (network_event_sender, network_event_receiver) = mpsc::channel(NETWORKING_CHANNEL_SIZE);
        let (network_swarm_cmd_sender, network_swarm_cmd_receiver) =
            mpsc::channel(NETWORKING_CHANNEL_SIZE);
        let (local_swarm_cmd_sender, local_swarm_cmd_receiver) =
            mpsc::channel(NETWORKING_CHANNEL_SIZE);

        // Kademlia Behaviour
        let kademlia = {
            match record_store_cfg {
                Some(store_cfg) => {
                    let node_record_store = NodeRecordStore::with_config(
                        peer_id,
                        store_cfg,
                        network_event_sender.clone(),
                        local_swarm_cmd_sender.clone(),
                    );
                    #[cfg(feature = "open-metrics")]
                    let mut node_record_store = node_record_store;
                    #[cfg(feature = "open-metrics")]
                    if let Some(metrics) = &network_metrics {
                        node_record_store = node_record_store
                            .set_record_count_metric(metrics.records_stored.clone());
                    }

                    let store = UnifiedRecordStore::Node(node_record_store);
                    debug!("Using Kademlia with NodeRecordStore!");
                    kad::Behaviour::with_config(peer_id, store, kad_cfg)
                }
                // no cfg provided for client
                None => {
                    let store = UnifiedRecordStore::Client(ClientRecordStore::default());
                    debug!("Using Kademlia with ClientRecordStore!");
                    kad::Behaviour::with_config(peer_id, store, kad_cfg)
                }
            }
        };

        #[cfg(feature = "local-discovery")]
        let mdns_config = mdns::Config {
            // lower query interval to speed up peer discovery
            // this increases traffic, but means we no longer have clients unable to connect
            // after a few minutes
            query_interval: Duration::from_secs(5),
            ..Default::default()
        };

        #[cfg(feature = "local-discovery")]
        let mdns = mdns::tokio::Behaviour::new(mdns_config, peer_id)?;

        // Identify Behaviour
        let identify_protocol_str = IDENTIFY_PROTOCOL_STR.to_string();
        info!("Building Identify with identify_protocol_str: {identify_protocol_str:?} and identify_version: {identify_version:?}");
        let identify = {
            let mut cfg =
                libp2p::identify::Config::new(identify_protocol_str, self.keypair.public())
                    .with_agent_version(identify_version);
            // Enlength the identify interval from default 5 mins to 1 hour.
            cfg.interval = RESEND_IDENTIFY_INVERVAL;
            libp2p::identify::Behaviour::new(cfg)
        };

        let main_transport = transport::build_transport(&self.keypair);

        let transport = if !self.local {
            debug!("Preventing non-global dials");
            // Wrap upper in a transport that prevents dialing local addresses.
            libp2p::core::transport::global_only::Transport::new(main_transport).boxed()
        } else {
            main_transport
        };

        #[cfg(feature = "upnp")]
        let upnp = if !self.local && !is_client && upnp {
            debug!("Enabling UPnP port opening behavior");
            Some(libp2p::upnp::tokio::Behaviour::default())
        } else {
            None
        }
        .into(); // Into `Toggle<T>`

        let (relay_transport, relay_behaviour) =
            libp2p::relay::client::new(self.keypair.public().to_peer_id());
        let relay_transport = relay_transport
            .upgrade(libp2p::core::upgrade::Version::V1Lazy)
            .authenticate(
                libp2p::noise::Config::new(&self.keypair)
                    .expect("Signing libp2p-noise static DH keypair failed."),
            )
            .multiplex(libp2p::yamux::Config::default())
            .or_transport(transport);

        let transport = relay_transport
            .map(|either_output, _| match either_output {
                Either::Left((peer_id, muxer)) => (peer_id, StreamMuxerBox::new(muxer)),
                Either::Right((peer_id, muxer)) => (peer_id, StreamMuxerBox::new(muxer)),
            })
            .boxed();

        let relay_server = {
            let relay_server_cfg = relay::Config::default();
            libp2p::relay::Behaviour::new(peer_id, relay_server_cfg)
        };

        let behaviour = NodeBehaviour {
            relay_client: relay_behaviour,
            relay_server,
            #[cfg(feature = "upnp")]
            upnp,
            request_response,
            kademlia,
            identify,
            #[cfg(feature = "local-discovery")]
            mdns,
            dcutr: libp2p::dcutr::Behaviour::new(peer_id),
        };

        #[cfg(not(target_arch = "wasm32"))]
        let swarm_config = libp2p::swarm::Config::with_tokio_executor()
            .with_idle_connection_timeout(CONNECTION_KEEP_ALIVE_TIMEOUT);
        #[cfg(target_arch = "wasm32")]
        let swarm_config = libp2p::swarm::Config::with_wasm_executor()
            .with_idle_connection_timeout(CONNECTION_KEEP_ALIVE_TIMEOUT);

        let swarm = Swarm::new(transport, behaviour, peer_id, swarm_config);

        let bootstrap = ContinuousBootstrap::new();
        let replication_fetcher = ReplicationFetcher::new(peer_id, network_event_sender.clone());
        let mut relay_manager = RelayManager::new(peer_id);
        if !is_client {
            relay_manager.enable_hole_punching(self.is_behind_home_network);
        }

        let swarm_driver = SwarmDriver {
            swarm,
            self_peer_id: peer_id,
            local: self.local,
            listen_port: self.listen_addr.map(|addr| addr.port()),
            is_client,
            is_behind_home_network: self.is_behind_home_network,
            peers_in_rt: 0,
            bootstrap,
            relay_manager,
            replication_fetcher,
            #[cfg(feature = "open-metrics")]
            network_metrics,
            network_cmd_receiver: network_swarm_cmd_receiver,
            local_cmd_receiver: local_swarm_cmd_receiver,
            event_sender: network_event_sender,
            pending_get_closest_peers: Default::default(),
            pending_requests: Default::default(),
            pending_get_record: Default::default(),
            // We use 255 here which allows covering a network larger than 64k without any rotating.
            // This is based on the libp2p kad::kBuckets peers distribution.
            dialed_peers: CircularVec::new(255),
            network_discovery: NetworkDiscovery::new(&peer_id),
            bootstrap_peers: Default::default(),
            live_connected_peers: Default::default(),
            handling_statistics: Default::default(),
            handled_times: 0,
            hard_disk_write_error: 0,
            bad_nodes: Default::default(),
            bad_nodes_ongoing_verifications: Default::default(),
            quotes_history: Default::default(),
            replication_targets: Default::default(),
            range_distances: VecDeque::with_capacity(X_RANGE_STORAGE_LIMIT),
        };

        let network = Network::new(
            network_swarm_cmd_sender,
            local_swarm_cmd_sender,
            peer_id,
            self.root_dir,
            self.keypair,
        );

        Ok((network, network_event_receiver, swarm_driver))
    }
}

pub struct SwarmDriver {
    pub(crate) swarm: Swarm<NodeBehaviour>,
    pub(crate) self_peer_id: PeerId,
    /// When true, we don't filter our local addresses
    pub(crate) local: bool,
    pub(crate) is_client: bool,
    pub(crate) is_behind_home_network: bool,
    /// The port that was set by the user
    pub(crate) listen_port: Option<u16>,
    pub(crate) peers_in_rt: usize,
    pub(crate) bootstrap: ContinuousBootstrap,
    pub(crate) relay_manager: RelayManager,
    /// The peers that are closer to our PeerId. Includes self.
    pub(crate) replication_fetcher: ReplicationFetcher,
    #[cfg(feature = "open-metrics")]
    pub(crate) network_metrics: Option<NetworkMetrics>,

    local_cmd_receiver: mpsc::Receiver<LocalSwarmCmd>,
    network_cmd_receiver: mpsc::Receiver<NetworkSwarmCmd>,
    event_sender: mpsc::Sender<NetworkEvent>, // Use `self.send_event()` to send a NetworkEvent.

    /// Trackers for underlying behaviour related events
    pub(crate) pending_get_closest_peers: PendingGetClosest,
    pub(crate) pending_requests:
        HashMap<OutboundRequestId, Option<oneshot::Sender<Result<Response>>>>,
    pub(crate) pending_get_record: PendingGetRecord,
    /// A list of the most recent peers we have dialed ourselves. Old dialed peers are evicted once the vec fills up.
    pub(crate) dialed_peers: CircularVec<PeerId>,
    // A list of random `PeerId` candidates that falls into kbuckets,
    // This is to ensure a more accurate network discovery.
    pub(crate) network_discovery: NetworkDiscovery,
    pub(crate) bootstrap_peers: BTreeMap<Option<u32>, HashSet<PeerId>>,
    // Peers that having live connection to. Any peer got contacted during kad network query
    // will have live connection established. And they may not appear in the RT.
    pub(crate) live_connected_peers: BTreeMap<ConnectionId, (PeerId, Instant)>,
    // Record the handling time of the recent 10 for each handling kind.
    handling_statistics: BTreeMap<String, Vec<Duration>>,
    handled_times: usize,
    pub(crate) hard_disk_write_error: usize,
    pub(crate) bad_nodes: BadNodes,
    pub(crate) bad_nodes_ongoing_verifications: BTreeSet<PeerId>,
    pub(crate) quotes_history: BTreeMap<PeerId, PaymentQuote>,
    pub(crate) replication_targets: BTreeMap<PeerId, Instant>,

    // The recent range_distances calculated by the node
    // Each update is generated when there is a routing table change
    // We use the largest of these X_STORAGE_LIMIT values as our X distance.
    pub(crate) range_distances: VecDeque<KBucketDistance>,
}

impl SwarmDriver {
    /// Asynchronously drives the swarm event loop, handling events from both
    /// the swarm and command receiver. This function will run indefinitely,
    /// until the command channel is closed.
    ///
    /// The `tokio::select` macro is used to concurrently process swarm events
    /// and command receiver messages, ensuring efficient handling of multiple
    /// asynchronous tasks.
    pub async fn run(mut self) {
        let mut bootstrap_interval = interval(BOOTSTRAP_INTERVAL);
        let mut set_farthest_record_interval = interval(CLOSET_RECORD_CHECK_INTERVAL);
        let mut relay_manager_reservation_interval = interval(RELAY_MANAGER_RESERVATION_INTERVAL);

        loop {
            // Prioritise any local cmds pending.
            // https://github.com/libp2p/rust-libp2p/blob/master/docs/coding-guidelines.md#prioritize-local-work-over-new-work-from-a-remote
            match self.local_cmd_receiver.try_recv() {
                Ok(cmd) => {
                    let start = Instant::now();
                    let cmd_string = format!("{cmd:?}");
                    if let Err(err) = self.handle_local_cmd(cmd) {
                        warn!("Error while handling local cmd: {err}");
                    }
                    trace!("LocalCmd handled in {:?}: {cmd_string:?}", start.elapsed());

                    continue;
                }
                Err(error) => match error {
                    TryRecvError::Empty => {
                        // no local cmds pending, continue
                    }
                    TryRecvError::Disconnected => {
                        error!("LocalCmd channel disconnected, shutting down SwarmDriver");
                        return;
                    }
                },
            }

            tokio::select! {
                swarm_event = self.swarm.select_next_some() => {
                    // logging for handling events happens inside handle_swarm_events
                    // otherwise we're rewriting match statements etc around this anwyay
                    if let Err(err) = self.handle_swarm_events(swarm_event) {
                        warn!("Error while handling swarm event: {err}");
                    }
                },
                some_cmd = self.network_cmd_receiver.recv() => match some_cmd {
                    Some(cmd) => {
                        let start = Instant::now();
                        let cmd_string = format!("{cmd:?}");
                        if let Err(err) = self.handle_network_cmd(cmd) {
                            warn!("Error while handling cmd: {err}");
                        }
                        trace!("SwarmCmd handled in {:?}: {cmd_string:?}", start.elapsed());
                    },
                    None =>  continue,
                },
                // runs every bootstrap_interval time
                _ = bootstrap_interval.tick() => {
                    if let Some(new_interval) = self.run_bootstrap_continuously(bootstrap_interval.period()).await {
                        bootstrap_interval = new_interval;
                    }
                }
                _ = set_farthest_record_interval.tick() => {
                    if !self.is_client {
                        let closest_k_peers = self.get_closest_k_value_local_peers();

                        if let Some(distance) = self.get_farthest_data_address_estimate(&closest_k_peers) {
                            // set any new distance to farthest record in the store
                            self.swarm.behaviour_mut().kademlia.store_mut().set_distance_range(distance);

                            let replication_distance = self.swarm.behaviour_mut().kademlia.store_mut().get_farthest_replication_distance_bucket().unwrap_or(1);
                            // the distance range within the replication_fetcher shall be in sync as well
                            self.replication_fetcher.set_replication_distance_range(replication_distance);
                        }
                    }
                }
                _ = relay_manager_reservation_interval.tick() => self.relay_manager.try_connecting_to_relay(&mut self.swarm, &self.bad_nodes),
            }
        }
    }

    // --------------------------------------------
    // ---------- Crate helpers -------------------
    // --------------------------------------------

    /// Defines a new X distance range to be used for GETs and data replication
    pub(crate) fn add_distance_range_for_gets(&mut self) {
        // TODO: define how/where this distance comes from

        const TARGET_PEER: usize = 42;

        let our_address = NetworkAddress::from_peer(self.self_peer_id);
        let our_key = our_address.as_kbucket_key();
        let mut sorted_peers_iter = self
            .swarm
            .behaviour_mut()
            .kademlia
            .get_closest_local_peers(&our_key);

        let mut last_peers_distance = KBucketDistance::default();
        let mut prior_peer = sorted_peers_iter.next();

        // get 42nd or farthest
        for (i, peer) in sorted_peers_iter.enumerate() {
            if let Some(prior_peer) = prior_peer {
                let this_last_peers_distance = prior_peer.distance(&peer);

                // only override it if it's larger!
                //
                // how does this play with keeping 100?
                // We only update with peers changes... Perhaps this negates the need for a buffer?
                //
                //
                // if this_last_peers_distance > last_peers_distance {
                last_peers_distance = this_last_peers_distance;
                // }
            }

            // info!("Peeeeeer {i}: {peer:?} - distance: {last_peers_distance:?}");
            prior_peer = Some(peer);

            if i == TARGET_PEER {
                break;
            }
        }

        // last_peers_distance = last_peers_distance * DISTANCE_MULTIPLIER;

        if last_peers_distance == KBucketDistance::default() {
            warn!("No peers found, no range distance can be set/added");
            return;
        }

        if self.range_distances.len() == X_RANGE_STORAGE_LIMIT {
            self.range_distances.pop_front();
        }

        info!("Adding new distance range: {last_peers_distance:?}");

        self.range_distances.push_back(last_peers_distance);
    }

    pub(crate) fn get_peers_within_get_range(&mut self) -> Option<KBucketDistance> {
        // TODO: Is this the correct keytype for comparisons?
        let our_address = NetworkAddress::from_peer(self.self_peer_id);
        let our_key = our_address.as_kbucket_key();

        let sorted_peers_iter = self
            .swarm
            .behaviour_mut()
            .kademlia
            .get_closest_local_peers(&our_key);

        let farthest_get_range_record_distance = self.range_distances.iter().max();

        if let Some(farthest_range) = farthest_get_range_record_distance {
            // lets print how many are within range
            for (i, peer) in sorted_peers_iter.enumerate() {
                let peer_distance_from_us = peer.distance(&our_key);

                if &peer_distance_from_us < farthest_range {
                    info!("Peer {peer:?} is {peer_distance_from_us:?} and would be within the range based search group!");
                    info!("That's {i:?} peers within the range!");
                }
            }
        } else {
            warn!("No range distance has been set, no peers can be found within the range");
            return None;
        }

        farthest_get_range_record_distance.copied()
    }

    /// Returns the farthest bucket, close to but probably farther than our responsibilty range.
    /// This simply uses the closest k peers to estimate the farthest address as
    /// `K_VALUE`th peer's bucket.
    fn get_farthest_data_address_estimate(
        &mut self,
        // Sorted list of closest k peers to our peer id.
        closest_k_peers: &[PeerId],
    ) -> Option<u32> {
        // if we don't have enough peers we don't set the distance range yet.
        let mut farthest_distance = None;

        let our_address = NetworkAddress::from_peer(self.self_peer_id);

        // get K_VALUEth peer's address distance
        // This is a rough estimate of the farthest address we might be responsible for.
        // We want this to be higher than actually necessary, so we retain more data
        // and can be sure to pass bad node checks
        if let Some(peer) = closest_k_peers.last() {
            let address = NetworkAddress::from_peer(*peer);
            farthest_distance = our_address.distance(&address).ilog2();
        }

        farthest_distance
    }

    /// Sends an event after pushing it off thread so as to be non-blocking
    /// this is a wrapper around the `mpsc::Sender::send` call
    pub(crate) fn send_event(&self, event: NetworkEvent) {
        let event_sender = self.event_sender.clone();
        let capacity = event_sender.capacity();

        // push the event off thread so as to be non-blocking
        let _handle = spawn(async move {
            if capacity == 0 {
                warn!(
                    "NetworkEvent channel is full. Await capacity to send: {:?}",
                    event
                );
            }
            if let Err(error) = event_sender.send(event).await {
                error!("SwarmDriver failed to send event: {}", error);
            }
        });
    }

    // get all the peers from our local RoutingTable. Contains self
    pub(crate) fn get_all_local_peers(&mut self) -> Vec<PeerId> {
        let mut all_peers: Vec<PeerId> = vec![];
        for kbucket in self.swarm.behaviour_mut().kademlia.kbuckets() {
            for entry in kbucket.iter() {
                all_peers.push(entry.node.key.clone().into_preimage());
            }
        }
        all_peers.push(self.self_peer_id);
        all_peers
    }

    /// get closest k_value the peers from our local RoutingTable. Contains self.
    /// Is sorted for closeness to self.
    pub(crate) fn get_closest_k_value_local_peers(&mut self) -> Vec<PeerId> {
        let self_peer_id = self.self_peer_id.into();

        // get closest peers from buckets, sorted by increasing distance to us
        let peers = self
            .swarm
            .behaviour_mut()
            .kademlia
            .get_closest_local_peers(&self_peer_id)
            // Map KBucketKey<PeerId> to PeerId.
            .map(|key| key.into_preimage());

        // Start with our own PeerID and chain the closest.
        std::iter::once(self.self_peer_id)
            .chain(peers)
            // Limit ourselves to K_VALUE (20) peers.
            .take(K_VALUE.get())
            .collect()
    }

    /// Dials the given multiaddress. If address contains a peer ID, simultaneous
    /// dials to that peer are prevented.
    pub(crate) fn dial(&mut self, mut addr: Multiaddr) -> Result<(), DialError> {
        trace!(%addr, "Dialing manually");

        let peer_id = multiaddr_pop_p2p(&mut addr);
        let opts = match peer_id {
            Some(peer_id) => DialOpts::peer_id(peer_id)
                // If we have a peer ID, we can prevent simultaneous dials.
                .condition(PeerCondition::NotDialing)
                .addresses(vec![addr])
                .build(),
            None => DialOpts::unknown_peer_id().address(addr).build(),
        };

        self.swarm.dial(opts)
    }

    /// Dials with the `DialOpts` given.
    pub(crate) fn dial_with_opts(&mut self, opts: DialOpts) -> Result<(), DialError> {
        trace!(?opts, "Dialing manually");

        self.swarm.dial(opts)
    }

    /// Record one handling time.
    /// Log for every 100 received.
    pub(crate) fn log_handling(&mut self, handle_string: String, handle_time: Duration) {
        if handle_string.is_empty() {
            return;
        }

        match self.handling_statistics.entry(handle_string) {
            Entry::Occupied(mut entry) => {
                let records = entry.get_mut();
                records.push(handle_time);
            }
            Entry::Vacant(entry) => {
                entry.insert(vec![handle_time]);
            }
        }

        self.handled_times += 1;

        if self.handled_times >= 100 {
            self.handled_times = 0;

            let mut stats: Vec<(String, usize, Duration)> = self
                .handling_statistics
                .iter()
                .map(|(kind, durations)| {
                    let count = durations.len();
                    let avg_time = durations.iter().sum::<Duration>() / count as u32;
                    (kind.clone(), count, avg_time)
                })
                .collect();

            stats.sort_by(|a, b| b.1.cmp(&a.1)); // Sort by count in descending order

            trace!("SwarmDriver Handling Statistics: {:?}", stats);
            // now we've logged, lets clear the stats from the btreemap
            self.handling_statistics.clear();
        }
    }

    /// Listen on the provided address. Also records it within RelayManager
    pub(crate) fn listen_on(&mut self, addr: Multiaddr) -> Result<()> {
        let id = self.swarm.listen_on(addr.clone())?;
        info!("Listening on {id:?} with addr: {addr:?}");
        Ok(())
    }
}
