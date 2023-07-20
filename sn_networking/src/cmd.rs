// Copyright 2023 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under The General Public License (GPL), version 3.
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied. Please review the Licences for the specific language governing
// permissions and limitations relating to use of the SAFE Network Software.

use super::{error::Error, MsgResponder, NetworkEvent, SwarmDriver};
use crate::{
    error::Result, event::NodeBehaviour, multiaddr_pop_p2p, replication_fetcher, PendingGetClosest,
    CLOSE_GROUP_SIZE,
};
use libp2p::{
    kad::{
        store::RecordStore, KBucketDistance as Distance, KBucketKey, QueryId, Quorum, Record,
        RecordKey,
    },
    request_response::RequestId,
    swarm::{
        dial_opts::{DialOpts, PeerCondition},
        DialError,
    },
    Multiaddr, PeerId, Swarm,
};
use sn_protocol::{
    messages::{Request, Response},
    NetworkAddress,
};
use std::collections::{HashMap, HashSet};
use tokio::sync::{mpsc, oneshot};

/// Commands to send to the Swarm
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum SwarmCmd {
    StartListening {
        addr: Multiaddr,
        sender: oneshot::Sender<Result<()>>,
    },
    Dial {
        addr: Multiaddr,
        sender: oneshot::Sender<Result<()>>,
    },
    AddToRoutingTable {
        peer_id: PeerId,
        peer_addr: Multiaddr,
        sender: oneshot::Sender<Result<()>>,
    },
    // Get closest peers from the network
    GetClosestPeers {
        key: NetworkAddress,
        sender: oneshot::Sender<HashSet<PeerId>>,
    },
    // Get closest peers from the local RoutingTable
    GetClosestLocalPeers {
        key: NetworkAddress,
        sender: oneshot::Sender<Vec<PeerId>>,
    },
    // Returns all the peers from all the k-buckets from the local Routing Table.
    // This includes our PeerId as well.
    GetAllLocalPeers {
        sender: oneshot::Sender<Vec<PeerId>>,
    },
    // Send Request to the PeerId.
    SendRequest {
        req: Request,
        peer: PeerId,

        // If a `sender` is provided, the requesting node will await for a `Response` from the
        // Peer. The result is then returned at the call site.
        //
        // If a `sender` is not provided, the requesting node will not wait for the Peer's
        // response. Instead we trigger a `NetworkEvent::ResponseReceived` which calls the common
        // `response_handler`
        sender: Option<oneshot::Sender<Result<Response>>>,
    },
    SendResponse {
        resp: Response,
        channel: MsgResponder,
    },
    GetSwarmLocalState(oneshot::Sender<SwarmLocalState>),
    /// Check if the local RecordStore contains the provided key
    RecordStoreHasKey {
        key: RecordKey,
        sender: oneshot::Sender<bool>,
    },
    /// Get Record from the Kad network
    GetNetworkRecord {
        key: RecordKey,
        sender: oneshot::Sender<Result<Record>>,
    },
    /// Get data from the local RecordStore
    GetLocalRecord {
        key: RecordKey,
        sender: oneshot::Sender<Option<Record>>,
    },
    /// Put record to network
    PutRecord {
        record: Record,
        sender: oneshot::Sender<Result<()>>,
    },
    /// Put record to the local RecordStore
    PutLocalRecord {
        record: Record,
    },
    /// Get the list of keys that within the provided distance to the target Key
    GetRecordKeysClosestToTarget {
        key: NetworkAddress,
        distance: Distance,
        sender: oneshot::Sender<Vec<RecordKey>>,
    },
    AddKeysToReplicationFetcher {
        peer: PeerId,
        keys: Vec<NetworkAddress>,
        sender: oneshot::Sender<Vec<(PeerId, NetworkAddress)>>,
    },
    NotifyFetchResult {
        peer: PeerId,
        key: NetworkAddress,
        result: bool,
        sender: oneshot::Sender<Vec<(PeerId, NetworkAddress)>>,
    },
    // Set the acceptable range of `Record` entry
    SetRecordDistanceRange {
        distance: Distance,
    },
}

/// Snapshot of information kept in the Swarm's local state
#[derive(Debug, Clone)]
pub struct SwarmLocalState {
    /// List of currently connected peers
    pub connected_peers: Vec<PeerId>,
    /// List of addresses the node is currently listening on
    pub listeners: Vec<Multiaddr>,
}

impl SwarmDriver {
    /// Returns the list of keys that are within the provided distance to the target
    pub fn get_existing_keys_closest_to_target(
        existing_keys: HashSet<NetworkAddress>,
        target: KBucketKey<Vec<u8>>,
        distance_bar: Distance,
    ) -> Vec<RecordKey> {
        existing_keys
            .clone()
            .into_iter()
            .filter_map(|key| {
                // extract record key from the existing key
                if let NetworkAddress::RecordKey(key) = key {
                    let record_key = KBucketKey::from(key.clone());
                    if target.distance(&record_key) < distance_bar {
                        let record_key = RecordKey::new(&key);
                        Some(record_key)
                    } else {
                        None
                    }
                } else {
                    return None;
                }
            })
            .collect()
    }

    pub(crate) fn handle_record_key_cmds(
        cmd: SwarmCmd,
        replication_fetcher: &mut replication_fetcher::ReplicationFetcher,
        existing_keys: &HashSet<NetworkAddress>,
    ) -> Result<(), Error> {
        let start_time;
        let the_cmd;
        match cmd {
            SwarmCmd::GetRecordKeysClosestToTarget {
                key,
                distance,
                sender,
            } => {
                the_cmd = "GetRecordKeysClosestToTarget";
                start_time = std::time::Instant::now();
                let peers = Self::get_existing_keys_closest_to_target(
                    existing_keys.clone(),
                    key.as_kbucket_key(),
                    distance,
                );
                let _ = sender.send(peers);
            }

            SwarmCmd::AddKeysToReplicationFetcher { peer, keys, sender } => {
                the_cmd = "AddKeysToReplicationFetcher";
                start_time = std::time::Instant::now();

                // remove any keys that we already have from replication fetcher
                replication_fetcher.remove_held_data(&existing_keys);

                let non_existing_keys: Vec<NetworkAddress> = keys
                    .iter()
                    .filter(|key| !existing_keys.contains(key))
                    .cloned()
                    .collect();

                let keys_to_fetch =
                    replication_fetcher.add_keys_to_replicate_per_peer(peer, non_existing_keys);
                let _ = sender.send(keys_to_fetch);
            }
            SwarmCmd::NotifyFetchResult {
                peer,
                key,
                result,
                sender,
            } => {
                the_cmd = "NotifyFetchResult";
                start_time = std::time::Instant::now();
                let keys_to_fetch = replication_fetcher.notify_fetch_result(peer, key, result);
                let _ = sender.send(keys_to_fetch);
            }
            SwarmCmd::RecordStoreHasKey { key, sender } => {
                the_cmd = "RecordStoreHasKey";
                start_time = std::time::Instant::now();

                let key_as_address = NetworkAddress::RecordKey(key.to_vec());
                let has_key = existing_keys.contains(&key_as_address);
                let _ = sender.send(has_key);
            }
            other => {
                the_cmd = "Other in ReplicationHandler";
                start_time = std::time::Instant::now();

                // TODO: return error
                error!(
                    "Swarm Driver ReplicationHandler: Unsupported SwarmCmd: {:?}",
                    other
                );
            }
        }

        trace!(
            "Swarm Driver Record Keys: {the_cmd:?} took: {:?}",
            start_time.elapsed()
        );

        Ok(())
    }

    pub(crate) fn handle_kad_store_cmd(
        swarm: &mut libp2p::Swarm<NodeBehaviour>,
        cmd: SwarmCmd,
        pending_record_put: &mut HashMap<QueryId, oneshot::Sender<Result<()>>>,
    ) -> Result<Option<HashSet<NetworkAddress>>, Error> {
        let mut updated_records = None;
        let start_time;
        let the_cmd;
        match cmd {
            SwarmCmd::SetRecordDistanceRange { distance } => {
                the_cmd = "SetRecordDistanceRange";
                start_time = std::time::Instant::now();
                swarm
                    .behaviour_mut()
                    .kademlia
                    .store_mut()
                    .set_distance_range(distance);
            }

            SwarmCmd::GetLocalRecord { key, sender } => {
                the_cmd = "GetLocalRecord";
                start_time = std::time::Instant::now();
                let record = swarm
                    .behaviour_mut()
                    .kademlia
                    .store_mut()
                    .get(&key)
                    .map(|rec| rec.into_owned());
                let _ = sender.send(record);
            }
            SwarmCmd::PutRecord { record, sender } => {
                the_cmd = "PutRecord";
                start_time = std::time::Instant::now();
                let request_id = swarm
                    .behaviour_mut()
                    .kademlia
                    .put_record(record, Quorum::All)?;
                trace!("Sending record {request_id:?} to network");
                let _ = pending_record_put.insert(request_id, sender);
            }
            SwarmCmd::PutLocalRecord { record } => {
                the_cmd = "PutLocalRecord";
                start_time = std::time::Instant::now();
                swarm
                    .behaviour_mut()
                    .kademlia
                    .store_mut()
                    .put_verified(record)?;

                updated_records = Some(
                    swarm
                        .behaviour_mut()
                        .kademlia
                        .store_mut()
                        .record_addresses(),
                );
            }
            other => {
                start_time = std::time::Instant::now();
                the_cmd = "OTHER in kad store";

                error!("Swarm kad store handler: Unhandled command: {:?}", other);
            }
        }

        trace!(
            "Swarm Driver Kad Store Cmd: {the_cmd:?} took: {:?}",
            start_time.elapsed()
        );

        Ok(updated_records)
    }

    pub(crate) fn handle_cmd(
        swarm: &mut libp2p::Swarm<NodeBehaviour>,
        cmd: SwarmCmd,
        event_sender: mpsc::Sender<NetworkEvent>,
        pending_get_closest_peers: &mut PendingGetClosest,
        pending_requests: &mut HashMap<RequestId, Option<oneshot::Sender<Result<Response>>>>,
        pending_query: &mut HashMap<QueryId, oneshot::Sender<Result<Record>>>,

        our_peer_id: PeerId,
    ) -> Result<(), Error> {
        let start_time;
        let the_cmd;
        match cmd {
            SwarmCmd::GetNetworkRecord { key, sender } => {
                the_cmd = "GetNetworkRecord";
                start_time = std::time::Instant::now();
                let query_id = swarm.behaviour_mut().kademlia.get_record(key);
                let _ = pending_query.insert(query_id, sender);
            }
            SwarmCmd::StartListening { addr, sender } => {
                the_cmd = "StartListening";
                start_time = std::time::Instant::now();
                let _ = match swarm.listen_on(addr) {
                    Ok(_) => sender.send(Ok(())),
                    Err(e) => sender.send(Err(e.into())),
                };
            }
            SwarmCmd::AddToRoutingTable {
                peer_id,
                peer_addr,
                sender,
            } => {
                the_cmd = "AddToRoutingTable";
                start_time = std::time::Instant::now();
                // TODO: This returns RoutingUpdate, but it doesn't implement `Debug`, so it's a hassle to return.
                let _ = swarm
                    .behaviour_mut()
                    .kademlia
                    .add_address(&peer_id, peer_addr);
                let _ = sender.send(Ok(()));
            }
            SwarmCmd::Dial { addr, sender } => {
                the_cmd = "Dial";
                start_time = std::time::Instant::now();
                let _ = match Self::dial(swarm, addr) {
                    Ok(_) => sender.send(Ok(())),
                    Err(e) => sender.send(Err(e.into())),
                };
            }
            SwarmCmd::GetClosestPeers { key, sender } => {
                the_cmd = "GetClosestPeers";
                start_time = std::time::Instant::now();
                let query_id = swarm
                    .behaviour_mut()
                    .kademlia
                    .get_closest_peers(key.as_bytes());
                let _ = pending_get_closest_peers.insert(query_id, (sender, Default::default()));
            }
            SwarmCmd::GetClosestLocalPeers { key, sender } => {
                the_cmd = "GetClosestLocalPeers";
                start_time = std::time::Instant::now();
                let key = key.as_kbucket_key();
                // calls `kbuckets.closest_keys(key)` internally, which orders the peers by
                // increasing distance
                // Note it will return all peers, heance a chop down is required.
                let closest_peers = swarm
                    .behaviour_mut()
                    .kademlia
                    .get_closest_local_peers(&key)
                    .map(|peer| peer.into_preimage())
                    .take(CLOSE_GROUP_SIZE)
                    .collect();

                let _ = sender.send(closest_peers);
            }
            SwarmCmd::GetAllLocalPeers { sender } => {
                the_cmd = "GetAllLocalPeers";
                start_time = std::time::Instant::now();
                let mut all_peers: Vec<PeerId> = vec![];
                for kbucket in swarm.behaviour_mut().kademlia.kbuckets() {
                    for entry in kbucket.iter() {
                        all_peers.push(entry.node.key.clone().into_preimage());
                    }
                }
                all_peers.push(our_peer_id);
                let _ = sender.send(all_peers);
            }
            SwarmCmd::SendRequest { req, peer, sender } => {
                the_cmd = "SendRequest";
                start_time = std::time::Instant::now();
                // If `self` is the recipient, forward the request directly to our upper layer to
                // be handled.
                // `self` then handles the request and sends a response back again to itself.
                if peer == *swarm.local_peer_id() {
                    trace!("Sending request to self");

                    Self::send_event(
                        event_sender,
                        NetworkEvent::RequestReceived {
                            req,
                            channel: MsgResponder::FromSelf(sender),
                        },
                    );
                } else {
                    let request_id = swarm
                        .behaviour_mut()
                        .request_response
                        .send_request(&peer, req);
                    trace!("Sending request {request_id:?} to peer {peer:?}");
                    let _ = pending_requests.insert(request_id, sender);
                }
            }
            SwarmCmd::SendResponse { resp, channel } => match channel {
                // If the response is for `self`, send it directly through the oneshot channel.
                MsgResponder::FromSelf(channel) => {
                    the_cmd = "SendResponse";
                    start_time = std::time::Instant::now();
                    trace!("Sending response to self");
                    match channel {
                        Some(channel) => {
                            channel
                                .send(Ok(resp))
                                .map_err(|_| Error::InternalMsgChannelDropped)?;
                        }
                        None => {
                            // responses that are not awaited at the call site must be handled
                            // separately
                            Self::send_event(
                                event_sender,
                                NetworkEvent::ResponseReceived { res: resp },
                            );
                        }
                    }
                }
                MsgResponder::FromPeer(channel) => {
                    the_cmd = "SendResponse (frompeer)";
                    start_time = std::time::Instant::now();
                    swarm
                        .behaviour_mut()
                        .request_response
                        .send_response(channel, resp)
                        .map_err(Error::OutgoingResponseDropped)?;
                }
            },
            SwarmCmd::GetSwarmLocalState(sender) => {
                the_cmd = "GetSwarmLocalState";
                start_time = std::time::Instant::now();
                let current_state = SwarmLocalState {
                    connected_peers: swarm.connected_peers().cloned().collect(),
                    listeners: swarm.listeners().cloned().collect(),
                };

                sender
                    .send(current_state)
                    .map_err(|_| Error::InternalMsgChannelDropped)?;
            }
            other => {
                start_time = std::time::Instant::now();
                the_cmd = "OTHER";

                error!("Swarm Cmd handler: Unhandled command: {:?}", other);
            }
        }

        trace!(
            "Swarm Driver Cmd: {the_cmd:?} took: {:?}",
            start_time.elapsed()
        );

        Ok(())
    }

    /// Dials the given multiaddress. If address contains a peer ID, simultaneous
    /// dials to that peer are prevented.
    pub(crate) fn dial(
        swarm: &mut Swarm<NodeBehaviour>,
        mut addr: Multiaddr,
    ) -> Result<(), DialError> {
        debug!(%addr, "Dialing manually");

        let peer_id = multiaddr_pop_p2p(&mut addr);
        let opts = match peer_id {
            Some(peer_id) => DialOpts::peer_id(peer_id)
                // If we have a peer ID, we can prevent simultaneous dials.
                .condition(PeerCondition::NotDialing)
                .addresses(vec![addr])
                .build(),
            None => DialOpts::unknown_peer_id().address(addr).build(),
        };

        swarm.dial(opts)
    }
}
