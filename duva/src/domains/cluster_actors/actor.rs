use super::ClusterCommand;
use super::ConsensusClientResponse;
use super::ConsensusRequest;
use super::LazyOption;
use super::consensus::election::ElectionState;
use super::hash_ring::HashRing;
pub mod client_sessions;
pub(crate) mod heartbeat_scheduler;
use super::replication::ReplicationId;
use super::replication::ReplicationRole;
use super::replication::ReplicationState;
use super::replication::time_in_secs;
use super::*;
use crate::domains::QueryIO;
use crate::domains::caches::cache_manager::CacheManager;
use crate::domains::cluster_actors::consensus::election::ElectionVoting;
use crate::domains::cluster_actors::hash_ring::BatchId;
use crate::domains::cluster_actors::hash_ring::MigrationBatch;
use crate::domains::cluster_actors::hash_ring::PendingMigrationBatch;
use crate::domains::cluster_actors::topology::Topology;
use crate::domains::operation_logs::WriteRequest;
use crate::domains::operation_logs::interfaces::TWriteAheadLog;
use crate::domains::operation_logs::logger::ReplicatedLogs;
use crate::domains::peers::command::BannedPeer;
use crate::domains::peers::command::ElectionVote;
use crate::domains::peers::command::HeartBeat;
use crate::domains::peers::command::MigrateBatch;
use crate::domains::peers::command::MigrationBatchAck;
use crate::domains::peers::command::RejectionReason;
use crate::domains::peers::command::ReplicationAck;
use crate::domains::peers::command::RequestVote;
use crate::domains::peers::connections::inbound::stream::InboundStream;
use crate::domains::peers::connections::outbound::stream::OutboundStream;
use crate::domains::peers::peer::PeerState;
use crate::err;
use crate::res_err;
use crate::types::Callback;
use crate::types::ConnectionStream;
use client_sessions::ClientSessions;

use heartbeat_scheduler::HeartBeatScheduler;

use std::collections::HashMap;
use std::collections::VecDeque;
use std::fmt::Debug;
use std::fs::File;
use std::io::Seek;
use std::io::Write;
use std::iter;
use std::sync::atomic::Ordering;

use tracing::debug;
use tracing::error;
use tracing::info;
use tracing::instrument;
use tracing::warn;
#[cfg(test)]
mod tests;

#[derive(Debug)]
pub struct ClusterActor<T> {
    pub(crate) members: BTreeMap<PeerIdentifier, Peer>,
    pub(crate) replication: ReplicationState,
    pub(crate) node_timeout: u128,
    pub(crate) consensus_tracker: LogConsensusTracker,
    pub(crate) receiver: tokio::sync::mpsc::Receiver<ClusterCommand>,
    pub(crate) self_handler: ClusterCommandHandler,
    pub(crate) heartbeat_scheduler: HeartBeatScheduler,
    pub(crate) topology_writer: std::fs::File,
    pub(crate) node_change_broadcast: tokio::sync::broadcast::Sender<Topology>,

    // * Pending requests are used to store requests that are received while the actor is in the process of election/cluster rebalancing.
    // * These requests will be processed once the actor is back to a stable state.
    pub(crate) client_sessions: ClientSessions,
    pub(crate) logger: ReplicatedLogs<T>,
    pub(crate) hash_ring: HashRing,
    pub(crate) pending_requests: Option<VecDeque<ConsensusRequest>>,
    pub(crate) pending_migrations: Option<HashMap<BatchId, PendingMigrationBatch>>,
}

#[derive(Debug, Clone)]
pub struct ClusterCommandHandler(pub(super) tokio::sync::mpsc::Sender<ClusterCommand>);
impl ClusterCommandHandler {
    pub(crate) async fn send(
        &self,
        cmd: impl Into<ClusterCommand>,
    ) -> Result<(), tokio::sync::mpsc::error::SendError<ClusterCommand>> {
        self.0.send(cmd.into()).await
    }
}

impl<T: TWriteAheadLog> ClusterActor<T> {
    pub(crate) fn run(
        node_timeout: u128,
        topology_writer: std::fs::File,
        heartbeat_interval: u64,
        init_replication: ReplicationState,
        cache_manager: CacheManager,
        wal: T,
    ) -> ClusterCommandHandler {
        let cluster_actor = ClusterActor::new(
            node_timeout,
            init_replication,
            heartbeat_interval,
            topology_writer,
            wal,
        );
        let actor_handler = cluster_actor.self_handler.clone();
        tokio::spawn(cluster_actor.handle(cache_manager));
        actor_handler
    }

    fn new(
        node_timeout: u128,
        init_repl_state: ReplicationState,
        heartbeat_interval_in_mills: u64,
        topology_writer: File,
        log_writer: T,
    ) -> Self {
        let (self_handler, receiver) = tokio::sync::mpsc::channel(100);
        let heartbeat_scheduler = HeartBeatScheduler::run(
            self_handler.clone(),
            init_repl_state.is_leader(),
            heartbeat_interval_in_mills,
        );

        let (tx, _) = tokio::sync::broadcast::channel::<Topology>(100);
        let hash_ring = HashRing::default().add_partitions(vec![(
            init_repl_state.replid.clone(),
            init_repl_state.self_identifier(),
        )]);

        Self {
            logger: ReplicatedLogs::new(
                log_writer,
                init_repl_state.hwm.load(Ordering::Acquire),
                init_repl_state.term,
            ),
            heartbeat_scheduler,
            replication: init_repl_state,
            node_timeout,
            receiver,
            self_handler: ClusterCommandHandler(self_handler),
            topology_writer,
            node_change_broadcast: tx,
            hash_ring,
            members: BTreeMap::new(),
            consensus_tracker: LogConsensusTracker::default(),
            client_sessions: ClientSessions::default(),

            pending_requests: None,
            pending_migrations: None,
        }
    }

    #[instrument(level = tracing::Level::INFO, skip(self, peer, optional_callback),fields(peer_id = %peer.id()))]
    pub(crate) async fn add_peer(
        &mut self,
        peer: Peer,
        optional_callback: Option<Callback<anyhow::Result<()>>>,
    ) {
        let peer_id = peer.id().clone();
        self.replication.banlist.remove(&peer_id);

        // If the map did have this key present, the value is updated, and the old
        // value is returned. The key is not updated,
        if let Some(existing_peer) = self.members.insert(peer_id.clone(), peer) {
            existing_peer.kill().await;
        }

        self.broadcast_topology_change();
        let _ = self.snapshot_topology().await;

        let peer = self.members.get_mut(&peer_id).unwrap();
        if peer.is_follower(&self.replication.replid) && self.replication.is_leader() {
            info!("Sending heartbeat to newly added follower: {}", peer_id);
            let hb = self
                .replication
                .default_heartbeat(0, self.logger.last_log_index, self.logger.last_log_term)
                .set_hashring(self.hash_ring.clone());
            let _ = peer.send(hb).await;
        }

        if let Some(cb) = optional_callback {
            let _ = cb.send(Ok(()));
        }
    }

    #[instrument(skip(self, optional_callback))]
    pub(crate) async fn connect_to_server(
        &mut self,
        connect_to: PeerIdentifier,
        optional_callback: Option<Callback<anyhow::Result<()>>>,
    ) {
        if self.replication.self_identifier() == connect_to {
            if let Some(cb) = optional_callback {
                let _ = cb.send(res_err!("Cannot connect to myself"));
            }
            return;
        }
        let stream = match OutboundStream::new(connect_to, self.replication.clone()).await {
            | Ok(stream) => stream,
            | Err(e) => {
                if let Some(cb) = optional_callback {
                    let _ = cb.send(res_err!(e));
                }
                return;
            },
        };

        tokio::spawn(stream.add_peer(
            self.replication.self_port,
            self.self_handler.clone(),
            optional_callback,
        ));
    }

    pub(crate) fn accept_inbound_stream(&mut self, peer_stream: ConnectionStream) {
        let inbound_stream = InboundStream::new(peer_stream.0, self.replication.clone());
        tokio::spawn(inbound_stream.add_peer(self.self_handler.clone()));
    }

    #[instrument(level = tracing::Level::INFO, skip(self,replid))]
    pub(crate) fn follower_setup(&mut self, replid: ReplicationId) {
        self.set_repl_id(replid.clone());
    }

    pub(crate) fn set_repl_id(&mut self, replid: ReplicationId) {
        self.replication.replid = replid;
        // Update hash ring with leader's replication ID and identifier
    }

    #[instrument(level = tracing::Level::DEBUG, skip(self))]
    pub(crate) async fn send_cluster_heartbeat(&mut self) {
        self.remove_idle_peers().await;

        let hop_count = Self::hop_count(FANOUT, self.members.len());
        let hb = self
            .replication
            .default_heartbeat(hop_count, self.logger.last_log_index, self.logger.last_log_term)
            .set_cluster_nodes(self.cluster_nodes());
        self.send_heartbeat(hb).await;
    }

    pub(crate) async fn forget_peer(
        &mut self,
        peer_addr: PeerIdentifier,
    ) -> anyhow::Result<Option<()>> {
        let res = self.remove_peer(&peer_addr).await;
        self.replication.banlist.insert(BannedPeer { p_id: peer_addr, ban_time: time_in_secs()? });

        Ok(res)
    }

    #[instrument(level = tracing::Level::DEBUG, skip(self, heartbeat,cache_manager), fields(peer_id = %heartbeat.from))]
    pub(crate) async fn receive_cluster_heartbeat(
        &mut self,
        mut heartbeat: HeartBeat,
        cache_manager: &CacheManager,
    ) {
        if self.replication.in_ban_list(&heartbeat.from) {
            err!("The given peer is in the ban list {}", heartbeat.from);
            return;
        }
        self.apply_banlist(std::mem::take(&mut heartbeat.ban_list)).await;
        self.update_cluster_members(&heartbeat.from, heartbeat.hwm, &heartbeat.cluster_nodes).await;
        self.join_peer_network_if_absent(heartbeat.cluster_nodes).await;
        self.gossip(heartbeat.hop_count).await;
        self.maybe_update_hashring(heartbeat.hashring, cache_manager).await;
    }

    pub(crate) async fn leader_req_consensus(&mut self, req: ConsensusRequest) {
        if let Some(pending_requests) = self.pending_requests.as_mut() {
            pending_requests.push_back(req);
            return;
        }
        if self.client_sessions.is_processed(&req.session_req) {
            // mapping between early returned values to client result
            let key = req.request.all_keys().into_iter().map(String::from).collect();
            let _ = req.callback.send(ConsensusClientResponse::AlreadyProcessed {
                key,
                index: self.logger.last_log_index,
            });
            return;
        };

        // TODO get_node_for_keys need to be revisited as currently it takes only the first key
        match self.hash_ring.get_node_for_keys(&req.request.all_keys()) {
            | Ok(replid) if replid == self.replication.replid => {
                self.req_consensus(req).await;
            },
            | Ok(replid) => {
                err!("Given keys {:?} moved to {}", req.request.all_keys(), replid);
                let _ = req.callback.send(format!("MOVED {replid}").into());
            },
            | Err(err) => {
                err!("{}", err);
                let _ = req.callback.send(err.to_string().into());
            },
        }
    }

    async fn req_consensus(&mut self, req: ConsensusRequest) {
        if !self.replication.is_leader() {
            let _ = req.callback.send("Write given to follower".into());
            return;
        }

        // * Check if the request has already been processed
        if let Err(err) = self.logger.write_single_entry(
            &req.request,
            self.replication.term,
            req.session_req.clone(),
        ) {
            let _ = req.callback.send(ConsensusClientResponse::Err(err.to_string()));
            return;
        };

        let repl_cnt = self.replicas().count();
        if repl_cnt == 0 {
            // * If there are no replicas, we can send the response immediately
            self.replication.hwm.fetch_add(1, Ordering::Relaxed);
            req.callback.send(ConsensusClientResponse::LogIndex(self.logger.last_log_index)).ok();
            return;
        }
        self.consensus_tracker.add(self.logger.last_log_index, req, repl_cnt);
        self.send_rpc_to_replicas().await;
    }

    #[instrument(level = tracing::Level::DEBUG, skip(self))]
    pub(crate) async fn send_rpc(&mut self) {
        if self.replicas().count() == 0 {
            return;
        }
        self.send_rpc_to_replicas().await;
    }

    pub(crate) fn cluster_nodes(&self) -> Vec<PeerState> {
        self.members
            .values()
            .map(|p| p.state().clone())
            .chain(std::iter::once(self.replication.self_info()))
            .collect()
    }
    #[instrument(level = tracing::Level::INFO, skip(self, request_vote))]
    pub(crate) async fn vote_election(&mut self, request_vote: RequestVote) {
        if self.find_replica_mut(&request_vote.candidate_id).is_none() {
            return;
        };

        let grant_vote = self.logger.last_log_index <= request_vote.last_log_index
            && self.replication.become_follower_if_term_higher_and_votable(
                &request_vote.candidate_id,
                request_vote.term,
            );

        info!(
            "Voting for {} with term {} and granted: {grant_vote}",
            request_vote.candidate_id, request_vote.term
        );

        let term = self.replication.term;

        let Some(peer) = self.find_replica_mut(&request_vote.candidate_id) else {
            return;
        };

        let _ = peer.send(ElectionVote { term, vote_granted: grant_vote }).await;
    }

    #[instrument(level = tracing::Level::DEBUG, skip(self, repl_res), fields(peer_id = %repl_res.from))]
    pub(crate) async fn ack_replication(&mut self, repl_res: ReplicationAck) {
        if !repl_res.is_granted() {
            info!("vote cannot be granted {:?}", repl_res.rej_reason);
            self.handle_repl_rejection(repl_res).await;
            return;
        }
        self.update_peer_index(&repl_res.from, repl_res.log_idx);
        self.track_replication_progress(repl_res);
    }

    #[instrument(level = tracing::Level::DEBUG, skip(self, cache_manager,heartbeat), fields(peer_id = %heartbeat.from))]
    pub(crate) async fn append_entries_rpc(
        &mut self,
        cache_manager: &CacheManager,
        heartbeat: HeartBeat,
    ) {
        if self.check_term_outdated(&heartbeat).await {
            err!("Term Outdated received:{} self:{}", heartbeat.term, self.replication.term);
            return;
        };
        self.reset_election_timeout(&heartbeat.from);
        self.maybe_update_term(heartbeat.term);
        self.replicate(heartbeat, cache_manager).await;
    }

    #[instrument(level = tracing::Level::DEBUG, skip(self, election_vote))]
    pub(crate) async fn receive_election_vote(&mut self, election_vote: ElectionVote) {
        if !election_vote.vote_granted {
            return;
        }
        if !self.replication.election_state.can_transition_to_leader() {
            return;
        }

        self.become_leader().await;
        let msg = self.replication.default_heartbeat(
            0,
            self.logger.last_log_index,
            self.logger.last_log_term,
        );

        self.replicas_mut()
            .map(|(peer, _)| peer.send(QueryIO::AppendEntriesRPC(msg.clone())))
            .collect::<FuturesUnordered<_>>()
            .for_each(|_| async {})
            .await;

        // * update hash ring with the new leader
        self.hash_ring.update_repl_leader(
            self.replication.replid.clone(),
            self.replication.self_identifier(),
        );
        let msg = msg.set_hashring(self.hash_ring.clone());
        self.send_heartbeat(msg).await;
    }

    // * Forces the current node to become a replica of the given peer.
    pub(crate) async fn replicaof(
        &mut self,
        peer_addr: PeerIdentifier,
        callback: Callback<anyhow::Result<()>>,
    ) {
        self.logger.reset();
        self.replication.hwm.store(0, Ordering::Release);
        self.set_repl_id(ReplicationId::Undecided);
        self.step_down().await;
        self.connect_to_server(peer_addr, Some(callback)).await;
    }

    pub(crate) async fn cluster_meet(
        &mut self,
        peer_addr: PeerIdentifier,
        lazy_option: LazyOption,
        cl_cb: Callback<anyhow::Result<()>>,
    ) {
        if !self.replication.is_leader() || self.replication.self_identifier() == peer_addr {
            let _ = cl_cb.send(res_err!("wrong address or invalid state for cluster meet command"));
            return;
        }

        // ! intercept the callback to ensure that the connection is established before sending the rebalance request
        let (res_callback, conn_awaiter) = tokio::sync::oneshot::channel();
        self.connect_to_server(peer_addr.clone(), Some(res_callback.into())).await;

        tokio::spawn(Self::register_delayed_schedule(
            self.self_handler.clone(),
            conn_awaiter,
            cl_cb,
            SchedulerMessage::RebalanceRequest { request_to: peer_addr, lazy_option },
        ));
    }

    pub(crate) async fn rebalance_request(
        &mut self,
        request_to: PeerIdentifier,
        lazy_option: LazyOption,
    ) {
        // * If lazy option is set, we just send the request and don't wait for the response
        // Ask the given peer to act as rebalancing coordinator
        if lazy_option == LazyOption::Eager {
            let Some(peer) = self.members.get(&request_to) else {
                return;
            };
            if peer.is_replica(&self.replication.replid) {
                warn!("Cannot rebalance to a replica: {}", request_to);
                return;
            }

            self.block_write_reqs();

            let peer = self.members.get_mut(&request_to).unwrap();
            let _ = peer.send(QueryIO::StartRebalance).await;
        }
    }

    #[instrument(level = tracing::Level::INFO, skip(self,cache_manager))]
    pub(crate) async fn start_rebalance(&mut self, cache_manager: &CacheManager) {
        if !self.replication.is_leader() {
            err!("follower cannot start rebalance");
            return;
        }

        let Some(new_hashring) = self.hash_ring.set_partitions(self.shard_leaders()) else {
            warn!("No need for update on hashring");
            return;
        };

        warn!("Rebalancing started! subsequent writes will be blocked until rebalance is done");
        self.block_write_reqs();

        let hb = self
            .replication
            .default_heartbeat(
                Self::hop_count(FANOUT, self.members.len()),
                self.logger.last_log_index,
                self.logger.last_log_term,
            )
            .set_hashring(new_hashring.clone());

        self.send_heartbeat(hb).await;
        self.maybe_update_hashring(Some(Box::new(new_hashring)), cache_manager).await;
    }

    fn hop_count(fanout: usize, node_count: usize) -> u8 {
        if node_count <= fanout {
            return 0;
        }
        node_count.ilog(fanout) as u8
    }

    fn replicas(&self) -> impl Iterator<Item = (&PeerIdentifier, u64)> {
        self.members.iter().filter_map(|(id, peer)| {
            (peer.is_replica(&self.replication.replid)).then_some((id, peer.match_index()))
        })
    }

    fn shard_leaders(&self) -> Vec<(ReplicationId, PeerIdentifier)> {
        let iter = self
            .members
            .iter()
            .filter(|(_, peer)| peer.role() == ReplicationRole::Leader)
            .map(|(id, peer)| (peer.replid().clone(), id.clone()));

        if self.replication.is_leader() {
            iter.chain(iter::once((
                self.replication.replid.clone(),
                self.replication.self_identifier(),
            )))
            .collect()
        } else {
            iter.collect()
        }
    }

    fn replicas_mut(&mut self) -> impl Iterator<Item = (&mut Peer, u64)> {
        self.members.values_mut().filter_map(|peer| {
            let match_index = peer.match_index();
            (peer.is_replica(&self.replication.replid)).then_some((peer, match_index))
        })
    }

    fn find_replica_mut(&mut self, peer_id: &PeerIdentifier) -> Option<&mut Peer> {
        self.members.get_mut(peer_id).filter(|peer| peer.is_replica(&self.replication.replid))
    }

    fn peerid_by_replid(&self, target_repl_id: &ReplicationId) -> Option<&PeerIdentifier> {
        self.members
            .iter()
            .find(|(_, peer)| peer.replid() == target_repl_id)
            .map(|(peer_id, _)| peer_id)
    }

    async fn send_heartbeat(&mut self, heartbeat: HeartBeat) {
        for peer in self.members.values_mut() {
            let _ = peer.send(heartbeat.clone()).await;
        }
    }

    async fn snapshot_topology(&mut self) -> anyhow::Result<()> {
        let topology = self
            .cluster_nodes()
            .into_iter()
            .map(|cn| cn.format(&self.replication.self_identifier()))
            .collect::<Vec<_>>()
            .join("\r\n");
        self.topology_writer.seek(std::io::SeekFrom::Start(0))?;
        self.topology_writer.set_len(topology.len() as u64)?;
        self.topology_writer.write_all(topology.as_bytes())?;
        Ok(())
    }

    // ! BLOCK subsequent requests until rebalance is done
    fn block_write_reqs(&mut self) {
        if self.pending_requests.is_none() {
            self.pending_requests = Some(VecDeque::new());
            self.pending_migrations = Some(HashMap::new());
        }
    }

    // * Broadcasts the current topology to all connected clients
    // TODO hashring information should be included in the broadcast so clients can update their routing tables
    fn broadcast_topology_change(&self) {
        self.node_change_broadcast.send(self.get_topology()).ok();
    }

    pub(crate) fn get_topology(&self) -> Topology {
        Topology::new(
            self.members
                .keys()
                .cloned()
                .chain(iter::once(self.replication.self_identifier()))
                .collect(),
            self.hash_ring.clone(),
        )
    }

    async fn remove_peer(&mut self, peer_addr: &PeerIdentifier) -> Option<()> {
        if let Some(peer) = self.members.remove(peer_addr) {
            warn!("{} is being removed!", peer_addr);
            // stop the runnin process and take the connection in case topology changes are made
            let _read_connected = peer.kill().await;
            self.broadcast_topology_change();
            return Some(());
        }
        None
    }

    //  remove idle peers based on ttl.
    async fn remove_idle_peers(&mut self) {
        // loop over members, if ttl is expired, remove the member
        let now = Instant::now();

        for peer_id in self
            .members
            .iter()
            .filter(|&(_, peer)| now.duration_since(peer.last_seen).as_millis() > self.node_timeout)
            .map(|(id, _)| id)
            .cloned()
            .collect::<Vec<_>>()
        {
            self.remove_peer(&peer_id).await;
        }
    }

    async fn gossip(&mut self, mut hop_count: u8) {
        // If hop_count is 0, don't send the message to other peers
        if hop_count == 0 {
            return;
        };
        hop_count -= 1;

        let hb = self
            .replication
            .default_heartbeat(hop_count, self.logger.last_log_index, self.logger.last_log_term)
            .set_cluster_nodes(self.cluster_nodes());
        self.send_heartbeat(hb).await;
    }

    async fn apply_banlist(&mut self, ban_list: Vec<BannedPeer>) {
        for banned_peer in ban_list {
            let ban_list = &mut self.replication.banlist;
            if let Some(existing) = ban_list.take(&banned_peer) {
                let newer =
                    if banned_peer.ban_time > existing.ban_time { banned_peer } else { existing };
                ban_list.insert(newer);
            } else {
                ban_list.insert(banned_peer);
            }
        }

        let current_time_in_sec = time_in_secs().unwrap();
        self.replication.banlist.retain(|node| current_time_in_sec - node.ban_time < 60);
        for banned_peer in self.replication.banlist.iter().cloned().collect::<Vec<_>>() {
            self.remove_peer(&banned_peer.p_id).await;
        }
    }

    fn update_peer_index(&mut self, from: &PeerIdentifier, log_index: u64) {
        if let Some(peer) = self.members.get_mut(from) {
            peer.set_match_index(log_index);
        }
    }

    async fn send_rpc_to_replicas(&mut self) {
        self.iter_follower_append_entries()
            .await
            .map(|(peer, hb)| peer.send(QueryIO::AppendEntriesRPC(hb)))
            .collect::<FuturesUnordered<_>>()
            .for_each(|_| async {})
            .await;
    }

    /// Creates individualized append entries messages for each follower.
    ///
    /// This function generates customized heartbeat messages containing only the log entries
    /// that each specific follower needs based on their current high watermark.
    ///
    /// For each follower:
    /// - Filters entries to include only those the follower doesn't have
    /// - Sets correct previous log information based on follower's replication state:
    ///   - If follower needs all entries: Uses backup entry or defaults to (0,0)
    ///   - Otherwise: Uses the last entry the follower already has
    /// - Creates a tailored heartbeat message with exactly the entries needed
    ///
    /// Returns an iterator yielding tuples of mutable peer references and their
    /// customized heartbeat messages.
    async fn iter_follower_append_entries(
        &mut self,
    ) -> Box<dyn Iterator<Item = (&mut Peer, HeartBeat)> + '_> {
        let lowest_watermark = self.take_low_watermark();

        let append_entries = self.logger.list_append_log_entries(lowest_watermark);

        let default_heartbeat: HeartBeat = self.replication.default_heartbeat(
            0,
            self.logger.last_log_index,
            self.logger.last_log_term,
        );

        // Handle empty entries case
        if append_entries.is_empty() {
            return Box::new(
                self.replicas_mut().map(move |(peer, _)| (peer, default_heartbeat.clone())),
            );
        }

        // If we have entries, find the entry before the first one to use as backup
        let backup_entry = self.logger.read_at(append_entries[0].log_index - 1);

        let iterator = self.replicas_mut().map(move |(peer, hwm)| {
            let logs =
                append_entries.iter().filter(|op| op.log_index > hwm).cloned().collect::<Vec<_>>();

            // Create base heartbeat
            let mut heart_beat = default_heartbeat.clone();

            if logs.len() == append_entries.len() {
                // Follower needs all entries, use backup entry
                if let Some(backup_entry) = backup_entry.as_ref() {
                    heart_beat.prev_log_index = backup_entry.log_index;
                    heart_beat.prev_log_term = backup_entry.term;
                } else {
                    heart_beat.prev_log_index = 0;
                    heart_beat.prev_log_term = 0;
                }
            } else {
                // Follower has some entries already, use the last one it has
                let last_log = &append_entries[append_entries.len() - logs.len() - 1];
                heart_beat.prev_log_index = last_log.log_index;
                heart_beat.prev_log_term = last_log.term;
            }
            let heart_beat = heart_beat.set_append_entries(logs);
            (peer, heart_beat)
        });

        Box::new(iterator)
    }

    fn take_low_watermark(&self) -> Option<u64> {
        self.members
            .values()
            .filter_map(|peer| {
                if peer.is_replica(&self.replication.replid) {
                    Some(peer.match_index())
                } else {
                    None
                }
            })
            .min()
    }

    fn track_replication_progress(&mut self, res: ReplicationAck) {
        let Some(mut consensus) = self.consensus_tracker.remove(&res.log_idx) else {
            return;
        };

        if consensus.votable(&res.from) {
            info!("Received acks for log index num: {}", res.log_idx);
            if let Some(peer) = self.members.get_mut(&res.from) {
                peer.set_match_index(res.log_idx);
                peer.last_seen = Instant::now();
            }
            consensus.increase_vote(res.from);
        }
        if consensus.cnt < consensus.get_required_votes() {
            self.consensus_tracker.insert(res.log_idx, consensus);
            return;
        }

        // * Increase the high water mark
        self.replication.hwm.fetch_add(1, Ordering::Relaxed);

        self.client_sessions.set_response(consensus.session_req.take());
        let _ = consensus.callback.send(ConsensusClientResponse::LogIndex(res.log_idx));
    }

    // Follower notified the leader of its acknowledgment, then leader store match index for the given follower
    async fn send_replication_ack(&mut self, send_to: &PeerIdentifier, ack: ReplicationAck) {
        let Some(leader) = self.members.get_mut(send_to) else {
            return;
        };
        let _ = leader.send(ack).await;
    }

    async fn replicate(&mut self, mut heartbeat: HeartBeat, cache_manager: &CacheManager) {
        if self.replication.is_leader() {
            return;
        }

        // * write logs
        if let Err(rej_reason) = self.replicate_log_entries(&mut heartbeat).await {
            self.send_replication_ack(
                &heartbeat.from,
                ReplicationAck::reject(self.logger.last_log_index, rej_reason, &self.replication),
            )
            .await;
            return;
        };

        self.replicate_state(heartbeat, cache_manager).await;
    }

    async fn replicate_log_entries(&mut self, rpc: &mut HeartBeat) -> Result<(), RejectionReason> {
        if rpc.append_entries.is_empty() {
            return Ok(());
        }
        let mut entries = Vec::with_capacity(rpc.append_entries.len());
        let mut session_reqs = Vec::with_capacity(rpc.append_entries.len());

        for mut log in std::mem::take(&mut rpc.append_entries) {
            if log.log_index > self.logger.last_log_index {
                if let Some(session_req) = log.session_req.take() {
                    session_reqs.push(session_req);
                }
                entries.push(log);
            }
        }

        self.ensure_prev_consistency(rpc.prev_log_index, rpc.prev_log_term).await?;
        let match_index = self.logger.follower_write_entries(entries).map_err(|e| {
            err!("{}", e);
            RejectionReason::FailToWrite
        })?;

        self.send_replication_ack(&rpc.from, ReplicationAck::ack(match_index, &self.replication))
            .await;

        // * The following is to allow for failure of leader and follower elected as new leader.
        // Subscription request with the same session req should be treated as idempotent operation
        session_reqs.into_iter().for_each(|req| self.client_sessions.set_response(Some(req)));

        Ok(())
    }

    async fn ensure_prev_consistency(
        &mut self,

        prev_log_index: u64,
        prev_log_term: u64,
    ) -> Result<(), RejectionReason> {
        // Case: Empty log
        if self.logger.is_empty() {
            if prev_log_index == 0 {
                return Ok(()); // First entry, no previous log to check
            }
            error!("Log is empty but leader expects an entry");
            return Err(RejectionReason::LogInconsistency); // Log empty but leader expects an entry
        }

        // * Raft followers should truncate their log starting at prev_log_index + 1 and then append the new entries
        // * Just returning an error is breaking consistency
        if let Some(prev_entry) = self.logger.read_at(prev_log_index) {
            debug!("Previous log entry: {:?}", prev_entry);
            if prev_entry.term != prev_log_term {
                // ! Term mismatch -> triggers log truncation
                error!("Term mismatch: {} != {}", prev_entry.term, prev_log_term);
                self.logger.truncate_after(prev_log_index);

                return Err(RejectionReason::LogInconsistency);
            }
        }

        Ok(())
    }

    #[instrument(level = tracing::Level::INFO, skip(self))]
    pub(crate) async fn run_for_election(&mut self) {
        warn!("Running for election term {}", self.replication.term);

        self.become_candidate();
        let request_vote = RequestVote::new(
            &self.replication,
            self.logger.last_log_index,
            self.logger.last_log_index,
        );

        self.replicas_mut()
            .map(|(peer, _)| peer.send(request_vote.clone()))
            .collect::<FuturesUnordered<_>>()
            .for_each(|_| async {})
            .await;
    }

    fn reset_election_timeout(&mut self, leader_id: &PeerIdentifier) {
        if let Some(peer) = self.members.get_mut(leader_id) {
            peer.last_seen = Instant::now();
        }
        self.heartbeat_scheduler.reset_election_timeout();
        self.replication.election_state = ElectionState::Follower { voted_for: None };
    }

    async fn replicate_state(&mut self, leader_hwm: HeartBeat, cache_manager: &CacheManager) {
        let old_hwm = self.replication.hwm.load(Ordering::Acquire);
        if leader_hwm.hwm > old_hwm {
            debug!("Received commit offset {}", leader_hwm.hwm);

            for log_index in (old_hwm + 1)..=leader_hwm.hwm {
                let Some(log) = self.logger.read_at(log_index) else {
                    warn!("log has never been replicated!");
                    self.send_replication_ack(
                        &leader_hwm.from,
                        ReplicationAck::reject(
                            self.logger.last_log_index,
                            RejectionReason::LogInconsistency,
                            &self.replication,
                        ),
                    )
                    .await;
                    return;
                };

                if let Err(e) = cache_manager.apply_log(log.request, log_index).await {
                    // ! DON'T PANIC - post validation is where we just don't update state
                    error!("failed to apply log: {e}")
                }
            }
            self.replication.hwm.store(leader_hwm.hwm, Ordering::Release);
        }
    }

    fn maybe_update_term(&mut self, new_term: u64) {
        if new_term > self.replication.term {
            self.replication.term = new_term;
            self.replication.election_state = ElectionState::Follower { voted_for: None };
            self.replication.role = ReplicationRole::Follower;
        }
    }

    async fn check_term_outdated(&mut self, heartbeat: &HeartBeat) -> bool {
        if heartbeat.term < self.replication.term {
            self.send_replication_ack(
                &heartbeat.from,
                ReplicationAck::reject(
                    self.logger.last_log_index,
                    RejectionReason::ReceiverHasHigherTerm,
                    &self.replication,
                ),
            )
            .await;
            return true;
        }
        false
    }

    /// Used when:
    /// 1) on follower's consensus rejection when term is not matched
    /// 2) step down operation is given from user
    async fn step_down(&mut self) {
        self.replication.vote_for(None);
        self.heartbeat_scheduler.turn_follower_mode().await;
    }

    async fn become_leader(&mut self) {
        info!("\x1b[32mElection succeeded\x1b[0m");

        self.replication.role = ReplicationRole::Leader;
        self.replication.election_state = ElectionState::Leader;
        self.heartbeat_scheduler.turn_leader_mode().await;
    }
    fn become_candidate(&mut self) {
        let replica_count = self.replicas().count() as u8;
        self.replication.term += 1;
        self.replication.election_state =
            ElectionState::Candidate { voting: Some(ElectionVoting::new(replica_count)) };
    }

    async fn handle_repl_rejection(&mut self, repl_res: ReplicationAck) {
        if repl_res.is_granted() {
            return;
        }

        match repl_res.rej_reason.unwrap() {
            | RejectionReason::ReceiverHasHigherTerm => self.step_down().await,
            | RejectionReason::LogInconsistency => {
                info!("Log inconsistency, reverting match index");
                //TODO we can refactor this to set match index to given log index from the follower
                self.decrease_match_index(&repl_res.from, repl_res.log_idx);
            },
            | RejectionReason::FailToWrite => {
                info!("Follower failed to write log for technical reason, resend..");
            },
        }
    }

    fn decrease_match_index(&mut self, from: &PeerIdentifier, current_log_idx: u64) {
        if let Some(peer) = self.members.get_mut(from) {
            peer.set_match_index(current_log_idx);
        }
    }

    async fn register_delayed_schedule<C>(
        cluster_sender: ClusterCommandHandler,
        awaiter: tokio::sync::oneshot::Receiver<anyhow::Result<C>>,
        callback: Callback<anyhow::Result<C>>,
        schedule_cmd: SchedulerMessage,
    ) where
        C: Send + Sync + 'static,
    {
        let result = awaiter.await.unwrap_or_else(|_| Err(anyhow::anyhow!("Channel closed")));
        let _ = callback.send(result);
        // Send schedule command regardless of callback result
        if let Err(e) = cluster_sender.send(schedule_cmd).await {
            // Consider logging this error instead of silently ignoring
            error!("Failed to send schedule command: {}", e);
        }
    }

    async fn join_peer_network_if_absent(&mut self, cluster_nodes: Vec<PeerState>) {
        let self_id = self.replication.self_identifier();

        // Find the first suitable peer to connect to
        for node in cluster_nodes {
            let node_id = node.id();

            // Skip if it's ourselves or has higher ID (avoid connection collisions)
            if node_id == &self_id || node_id >= &self_id {
                continue;
            }

            // Skip if already connected or banned
            if self.members.contains_key(node_id) || self.replication.in_ban_list(node_id) {
                continue;
            }

            // Found a suitable peer - connect and exit
            self.connect_to_server(node_id.clone(), None).await;
            return;
        }
    }

    // * If the hashring is valid, make a plan to migrate data for each paritition
    async fn maybe_update_hashring(
        &mut self,
        hashring: Option<Box<HashRing>>,
        cache_manager: &CacheManager,
    ) {
        let Some(new_ring) = hashring else {
            return;
        };

        if *new_ring == self.hash_ring {
            return;
        }
        if new_ring.last_modified < self.hash_ring.last_modified {
            warn!("Received outdated hashring, ignoring");
            return;
        }

        // For replicas, just update the hash ring and wait for leader to coordinate migrations
        if !self.replication.is_leader() {
            self.hash_ring = *new_ring;
            info!("Replica updated hash ring");
            return;
        }

        // Leader-only migration coordination logic below
        // Keep the old ring to compare with new ring for migration planning
        let keys = cache_manager.route_keys(None).await;
        let migration_plans = self.hash_ring.create_migration_tasks(&new_ring, keys);

        if migration_plans.is_empty() {
            info!("No migration tasks to schedule");
            self.hash_ring = *new_ring;
            let _ = self.node_change_broadcast.send(self.get_topology());
            return;
        }

        info!("Leader scheduling {} migration plan(s)", migration_plans.len());
        self.block_write_reqs();

        let batch_handles = FuturesUnordered::new();
        for (target_replid, mut migration_tasks) in migration_plans {
            while !migration_tasks.is_empty() {
                let mut num = 0;
                let mut batch_to_migrate = Vec::new();

                // Create a batch of tasks up to a certain size.
                while let Some(task) = migration_tasks.pop() {
                    num += task.key_len();
                    batch_to_migrate.push(task);
                    if num > 100 {
                        break;
                    }
                }

                // Spawn each batch as a separate task for parallel execution
                batch_handles.push(tokio::spawn(Self::schedule_migration_in_batch(
                    MigrationBatch::new(target_replid.clone(), batch_to_migrate),
                    self.self_handler.clone(),
                )));
            }
        }

        // Process all batches
        tokio::spawn(async move {
            batch_handles
                .for_each(
                    |result: Result<Result<(), anyhow::Error>, tokio::task::JoinError>| async {
                        if let Err(e) = result {
                            error!("Migration batch panicked: {}", e);
                        }
                    },
                )
                .await;
        });
    }

    async fn schedule_migration_in_batch(
        batch: MigrationBatch,
        handler: ClusterCommandHandler,
    ) -> anyhow::Result<()> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        handler.send(SchedulerMessage::ScheduleMigrationBatch(batch, tx.into())).await?;
        rx.await?
    }

    pub(crate) async fn migrate_batch(
        &mut self,
        target: MigrationBatch,
        cache_manager: &CacheManager,
        callback: impl Into<Callback<anyhow::Result<()>>>,
    ) {
        let callback = callback.into();
        //  Find target peer based on replication ID
        let Some(peer_id) = self.peerid_by_replid(&target.target_repl).cloned() else {
            let _ = callback.send(res_err!("Target peer not found for replication ID"));
            return;
        };

        // Retrieve key-value data from cache
        let keys = target
            .tasks
            .iter()
            .flat_map(|task| task.keys_to_migrate.iter().cloned())
            .collect::<Vec<_>>();

        let cache_entries = cache_manager
            .route_mget(keys.clone())
            .await
            .into_iter()
            .filter_map(|e| e.filter(|e| !e.value.null()))
            .collect::<Vec<_>>();

        // Get mutable reference to the target peer
        let Some(target_peer) = self.members.get_mut(&peer_id) else {
            let _ = callback.send(res_err!("Target peer {} disappeared during migration", peer_id));
            return;
        };

        self.pending_migrations
            .as_mut()
            .map(|p| p.insert(target.id.clone(), PendingMigrationBatch::new(callback, keys)));

        let _ = target_peer.send(MigrateBatch { batch_id: target.id, cache_entries }).await;
    }

    pub(crate) async fn receive_batch(
        &mut self,
        migrate_batch: MigrateBatch,
        cache_manager: &CacheManager,
        from: PeerIdentifier,
    ) {
        // If cache entries are empty, skip consensus and directly send success ack
        if migrate_batch.cache_entries.is_empty() {
            let Some(peer) = self.members.get_mut(&from) else {
                warn!("No Member Found");
                return;
            };
            let _ = peer.send(MigrationBatchAck::with_success(migrate_batch.batch_id)).await;
            return;
        }

        let (tx, rx) = tokio::sync::oneshot::channel();

        self.req_consensus(ConsensusRequest::new(
            WriteRequest::MSet { entries: migrate_batch.cache_entries.clone() },
            tx,
            None,
        ))
        .await;

        // * If there are replicas, we need to wait for the consensus to be applied which should be done in the background
        tokio::spawn({
            let handler = self.self_handler.clone();
            let cache_manager = cache_manager.clone();
            async move {
                if rx.await.is_ok() {
                    let _ = cache_manager.route_mset(migrate_batch.cache_entries.clone()).await; // reflect state change
                    let _ = handler
                        .send(SchedulerMessage::SendBatchAck {
                            batch_id: migrate_batch.batch_id,
                            to: from,
                        })
                        .await;
                    return;
                }
                error!(
                    "Failed to write some keys during migration for batch {}",
                    migrate_batch.batch_id.0
                );
            }
        });
    }

    pub(crate) async fn handle_migration_ack(
        &mut self,
        ack: MigrationBatchAck,
        cache_manager: &CacheManager,
    ) {
        let Some(pending) = self.pending_migrations.as_mut() else {
            warn!("No Pending migration map available");
            return;
        };

        let Some(pending_migration_batch) = pending.remove(&ack.batch_id) else {
            err!("Batch ID {:?} not found in pending migrations", ack.batch_id);
            return;
        };

        if !ack.success {
            let _ = pending_migration_batch
                .callback
                .send(res_err!("Failed to send migration completion signal for batch"));
            return;
        }

        // make consensus request for delete
        let (tx, rx) = tokio::sync::oneshot::channel();
        let w_req = ConsensusRequest::new(
            WriteRequest::Delete { keys: pending_migration_batch.keys.clone() },
            tx,
            None,
        );
        self.req_consensus(w_req).await;
        // ! background synchronization is required.
        tokio::spawn({
            let handler = self.self_handler.clone();
            let cache_manager = cache_manager.clone();

            async move {
                if rx.await.is_ok() {
                    let _ = cache_manager.route_delete(pending_migration_batch.keys).await; // reflect state change
                    let _ = handler.send(SchedulerMessage::TryUnblockWriteReqs).await;
                    let _ = pending_migration_batch.callback.send(Ok(()));
                }
            }
        });
    }

    // New hash ring stored at this point with the current shard leaders
    pub(crate) fn unblock_write_reqs_if_done(&mut self) {
        let migrations_done = self.pending_migrations.as_ref().is_none_or(|p| p.is_empty());

        if migrations_done {
            if let Some(new_ring) = self.hash_ring.set_partitions(self.shard_leaders()) {
                self.hash_ring = new_ring;
            }
            let _ = self.node_change_broadcast.send(self.get_topology());
            if let Some(mut pending_reqs) = self.pending_requests.take() {
                info!("All migrations complete, processing pending requests.");
                self.pending_migrations = None;

                if pending_reqs.is_empty() {
                    return;
                }

                let handler = self.self_handler.clone();
                tokio::spawn(async move {
                    while let Some(req) = pending_reqs.pop_front() {
                        if let Err(err) = handler
                            .send(ClusterCommand::Client(ClientMessage::LeaderReqConsensus(req)))
                            .await
                        {
                            error!("{}", err)
                        }
                    }
                });
            }
        }
    }

    pub(crate) async fn send_batch_ack(&mut self, batch_id: BatchId, to: PeerIdentifier) {
        let Some(peer) = self.members.get_mut(&to) else {
            return;
        };
        let _ = peer.send(MigrationBatchAck::with_success(batch_id)).await;
    }

    async fn update_cluster_members(
        &mut self,
        from: &PeerIdentifier,
        hwm: u64,
        cluster_nodes: &[PeerState],
    ) {
        self.update_peer_index(from, hwm);
        let now = Instant::now();
        for node in cluster_nodes.iter() {
            if let Some(peer) = self.members.get_mut(node.id()) {
                peer.last_seen = now;
                peer.set_role(node.role.clone())
            }
        }
    }
}
