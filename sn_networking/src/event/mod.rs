// Copyright 2024 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under The General Public License (GPL), version 3.
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied. Please review the Licences for the specific language governing
// permissions and limitations relating to use of the SAFE Network Software.

mod kad;
mod request_response;
mod swarm;

use crate::{driver::SwarmDriver, error::Result};
use core::fmt;
use custom_debug::Debug as CustomDebug;
#[cfg(feature = "local-discovery")]
use libp2p::mdns;
use libp2p::{
    kad::{Record, RecordKey, K_VALUE},
    request_response::ResponseChannel as PeerResponseChannel,
    Multiaddr, PeerId,
};

use sn_protocol::{
    messages::{Query, Request, Response},
    NetworkAddress, PrettyPrintRecordKey,
};
use sn_transfers::PaymentQuote;
use std::{
    collections::BTreeSet,
    fmt::{Debug, Formatter},
};
use tokio::sync::oneshot;

/// NodeEvent enum
#[derive(CustomDebug)]
pub(super) enum NodeEvent {
    #[cfg(feature = "upnp")]
    Upnp(libp2p::upnp::Event),
    MsgReceived(libp2p::request_response::Event<Request, Response>),
    Kademlia(libp2p::kad::Event),
    #[cfg(feature = "local-discovery")]
    Mdns(Box<mdns::Event>),
    Identify(Box<libp2p::identify::Event>),
    RelayClient(Box<libp2p::relay::client::Event>),
    RelayServer(Box<libp2p::relay::Event>),
    Void(void::Void),
}

#[cfg(feature = "upnp")]
impl From<libp2p::upnp::Event> for NodeEvent {
    fn from(event: libp2p::upnp::Event) -> Self {
        NodeEvent::Upnp(event)
    }
}

impl From<libp2p::request_response::Event<Request, Response>> for NodeEvent {
    fn from(event: libp2p::request_response::Event<Request, Response>) -> Self {
        NodeEvent::MsgReceived(event)
    }
}

impl From<libp2p::kad::Event> for NodeEvent {
    fn from(event: libp2p::kad::Event) -> Self {
        NodeEvent::Kademlia(event)
    }
}

#[cfg(feature = "local-discovery")]
impl From<mdns::Event> for NodeEvent {
    fn from(event: mdns::Event) -> Self {
        NodeEvent::Mdns(Box::new(event))
    }
}

impl From<libp2p::identify::Event> for NodeEvent {
    fn from(event: libp2p::identify::Event) -> Self {
        NodeEvent::Identify(Box::new(event))
    }
}
impl From<libp2p::relay::client::Event> for NodeEvent {
    fn from(event: libp2p::relay::client::Event) -> Self {
        NodeEvent::RelayClient(Box::new(event))
    }
}
impl From<libp2p::relay::Event> for NodeEvent {
    fn from(event: libp2p::relay::Event) -> Self {
        NodeEvent::RelayServer(Box::new(event))
    }
}

impl From<void::Void> for NodeEvent {
    fn from(event: void::Void) -> Self {
        NodeEvent::Void(event)
    }
}

#[derive(CustomDebug)]
/// Channel to send the `Response` through.
pub enum MsgResponder {
    /// Respond to a request from `self` through a simple one-shot channel.
    FromSelf(Option<oneshot::Sender<Result<Response>>>),
    /// Respond to a request from a peer in the network.
    FromPeer(PeerResponseChannel<Response>),
}

#[allow(clippy::large_enum_variant)]
/// Events forwarded by the underlying Network; to be used by the upper layers
pub enum NetworkEvent {
    /// Incoming `Query` from a peer
    QueryRequestReceived {
        /// Query
        query: Query,
        /// The channel to send the `Response` through
        channel: MsgResponder,
    },
    /// Handles the responses that are not awaited at the call site
    ResponseReceived {
        /// Response
        res: Response,
    },
    /// Peer has been added to the Routing Table. And the number of connected peers.
    PeerAdded(PeerId, usize),
    /// Peer has been removed from the Routing Table. And the number of connected peers.
    PeerRemoved(PeerId, usize),
    /// The peer does not support our protocol
    PeerWithUnsupportedProtocol {
        our_protocol: String,
        their_protocol: String,
    },
    /// The records bearing these keys are to be fetched from the holder or the network
    KeysToFetchForReplication(Vec<(PeerId, RecordKey)>),
    /// Started listening on a new address
    NewListenAddr(Multiaddr),
    /// Report unverified record
    UnverifiedRecord(Record),
    /// Terminate Node on unrecoverable errors
    TerminateNode { reason: TerminateNodeReason },
    /// List of peer nodes that failed to fetch replication copy from.
    FailedToFetchHolders(BTreeSet<PeerId>),
    /// Quotes to be verified
    QuoteVerification { quotes: Vec<(PeerId, PaymentQuote)> },
    /// Carry out chunk proof check against the specified record and peer
    ChunkProofVerification {
        peer_id: PeerId,
        keys_to_verify: Vec<NetworkAddress>,
    },
}

/// Terminate node for the following reason
#[derive(Debug, Clone)]
pub enum TerminateNodeReason {
    HardDiskWriteError,
    UpnpGatewayNotFound,
}

// Manually implement Debug as `#[debug(with = "unverified_record_fmt")]` not working as expected.
impl Debug for NetworkEvent {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            NetworkEvent::QueryRequestReceived { query, .. } => {
                write!(f, "NetworkEvent::QueryRequestReceived({query:?})")
            }
            NetworkEvent::ResponseReceived { res, .. } => {
                write!(f, "NetworkEvent::ResponseReceived({res:?})")
            }
            NetworkEvent::PeerAdded(peer_id, connected_peers) => {
                write!(f, "NetworkEvent::PeerAdded({peer_id:?}, {connected_peers})")
            }
            NetworkEvent::PeerRemoved(peer_id, connected_peers) => {
                write!(
                    f,
                    "NetworkEvent::PeerRemoved({peer_id:?}, {connected_peers})"
                )
            }
            NetworkEvent::PeerWithUnsupportedProtocol {
                our_protocol,
                their_protocol,
            } => {
                write!(f, "NetworkEvent::PeerWithUnsupportedProtocol({our_protocol:?}, {their_protocol:?})")
            }
            NetworkEvent::KeysToFetchForReplication(list) => {
                let keys_len = list.len();
                write!(f, "NetworkEvent::KeysForReplication({keys_len:?})")
            }
            NetworkEvent::NewListenAddr(addr) => {
                write!(f, "NetworkEvent::NewListenAddr({addr:?})")
            }
            NetworkEvent::UnverifiedRecord(record) => {
                let pretty_key = PrettyPrintRecordKey::from(&record.key);
                write!(f, "NetworkEvent::UnverifiedRecord({pretty_key:?})")
            }
            NetworkEvent::TerminateNode { reason } => {
                write!(f, "NetworkEvent::TerminateNode({reason:?})")
            }
            NetworkEvent::FailedToFetchHolders(bad_nodes) => {
                write!(f, "NetworkEvent::FailedToFetchHolders({bad_nodes:?})")
            }
            NetworkEvent::QuoteVerification { quotes } => {
                write!(
                    f,
                    "NetworkEvent::QuoteVerification({} quotes)",
                    quotes.len()
                )
            }
            NetworkEvent::ChunkProofVerification {
                peer_id,
                keys_to_verify,
            } => {
                write!(
                    f,
                    "NetworkEvent::ChunkProofVerification({peer_id:?} {keys_to_verify:?})"
                )
            }
        }
    }
}

impl SwarmDriver {
    /// Update state on addition of a peer to the routing table.
    pub(crate) fn update_on_peer_addition(&mut self, added_peer: PeerId) {
        self.peers_in_rt = self.peers_in_rt.saturating_add(1);
        info!(
            "New peer added to routing table: {added_peer:?}, now we have #{} connected peers",
            self.peers_in_rt
        );
        self.log_kbuckets(&added_peer);
        self.send_event(NetworkEvent::PeerAdded(added_peer, self.peers_in_rt));

        self.add_distance_range_for_gets();
        self.get_request_range();
        #[cfg(feature = "open-metrics")]
        if let Some(metrics) = &self.network_metrics {
            metrics.peers_in_routing_table.set(self.peers_in_rt as i64);
        }
    }

    /// Update state on removal of a peer from the routing table.
    pub(crate) fn update_on_peer_removal(&mut self, removed_peer: PeerId) {
        self.peers_in_rt = self.peers_in_rt.saturating_sub(1);
        info!(
            "Peer removed from routing table: {removed_peer:?}, now we have #{} connected peers",
            self.peers_in_rt
        );
        self.log_kbuckets(&removed_peer);
        self.send_event(NetworkEvent::PeerRemoved(removed_peer, self.peers_in_rt));

        #[cfg(feature = "open-metrics")]
        if let Some(metrics) = &self.network_metrics {
            metrics.peers_in_routing_table.set(self.peers_in_rt as i64);
        }
    }

    /// Logs the kbuckets also records the bucket info.
    pub(crate) fn log_kbuckets(&mut self, peer: &PeerId) {
        let distance = NetworkAddress::from_peer(self.self_peer_id)
            .distance(&NetworkAddress::from_peer(*peer));
        info!("Peer {peer:?} has a {:?} distance to us", distance.ilog2());

        let mut kbucket_table_stats = vec![];
        let mut index = 0;
        let mut total_peers = 0;

        let mut peers_in_non_full_buckets = 0;
        let mut num_of_full_buckets = 0;

        for kbucket in self.swarm.behaviour_mut().kademlia.kbuckets() {
            let range = kbucket.range();
            let num_entires = kbucket.num_entries();

            if num_entires >= K_VALUE.get() {
                num_of_full_buckets += 1;
            } else {
                peers_in_non_full_buckets += num_entires;
            }

            total_peers += num_entires;
            if let Some(distance) = range.0.ilog2() {
                kbucket_table_stats.push((index, num_entires, distance));
            } else {
                // This shall never happen.
                error!("bucket #{index:?} is ourself ???!!!");
            }
            index += 1;
        }

        let estimated_network_size =
            Self::estimate_network_size(peers_in_non_full_buckets, num_of_full_buckets);
        #[cfg(feature = "open-metrics")]
        if let Some(metrics) = &self.network_metrics {
            let _ = metrics
                .estimated_network_size
                .set(estimated_network_size as i64);
        }

        // Just to warn if our tracking goes out of sync with libp2p. Can happen if someone forgets to call
        // update_on_peer_addition or update_on_peer_removal when adding or removing a peer.
        // Only log every 10th peer to avoid spamming the logs.
        if total_peers % 10 == 0 && total_peers != self.peers_in_rt {
            warn!(
                "Total peers in routing table: {}, but kbucket table has {total_peers} peers",
                self.peers_in_rt
            );
        }

        info!("kBucketTable has {index:?} kbuckets {total_peers:?} peers, {kbucket_table_stats:?}, estimated network size: {estimated_network_size:?}");
    }

    /// Estimate the number of nodes in the network
    fn estimate_network_size(
        peers_in_non_full_buckets: usize,
        num_of_full_buckets: usize,
    ) -> usize {
        (peers_in_non_full_buckets + 1) * (2_usize.pow(num_of_full_buckets as u32))
    }
}
