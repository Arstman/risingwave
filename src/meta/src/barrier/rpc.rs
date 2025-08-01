// Copyright 2025 RisingWave Labs
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::cmp::max;
use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::fmt::{Debug, Formatter};
use std::future::poll_fn;
use std::task::{Context, Poll};
use std::time::Duration;

use anyhow::anyhow;
use fail::fail_point;
use futures::StreamExt;
use futures::future::join_all;
use itertools::Itertools;
use risingwave_common::catalog::{DatabaseId, FragmentTypeFlag, TableId};
use risingwave_common::util::epoch::Epoch;
use risingwave_common::util::tracing::TracingContext;
use risingwave_connector::source::SplitImpl;
use risingwave_connector::source::cdc::{
    CdcTableSnapshotSplitAssignment, build_pb_actor_cdc_table_snapshot_splits,
};
use risingwave_meta_model::WorkerId;
use risingwave_pb::common::{HostAddress, WorkerNode};
use risingwave_pb::hummock::HummockVersionStats;
use risingwave_pb::stream_plan::barrier_mutation::Mutation;
use risingwave_pb::stream_plan::{AddMutation, Barrier, BarrierMutation, SubscriptionUpstreamInfo};
use risingwave_pb::stream_service::inject_barrier_request::build_actor_info::UpstreamActors;
use risingwave_pb::stream_service::inject_barrier_request::{
    BuildActorInfo, FragmentBuildActorInfo,
};
use risingwave_pb::stream_service::streaming_control_stream_request::{
    CreatePartialGraphRequest, PbDatabaseInitialPartialGraph, PbInitRequest, PbInitialPartialGraph,
    RemovePartialGraphRequest, ResetDatabaseRequest,
};
use risingwave_pb::stream_service::{
    BarrierCompleteResponse, InjectBarrierRequest, StreamingControlStreamRequest,
    streaming_control_stream_request, streaming_control_stream_response,
};
use risingwave_rpc_client::StreamingControlHandle;
use thiserror_ext::AsReport;
use tokio::time::sleep;
use tokio_retry::strategy::ExponentialBackoff;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use super::{BarrierKind, Command, InflightSubscriptionInfo, TracedEpoch};
use crate::barrier::checkpoint::{
    BarrierWorkerState, CreatingStreamingJobControl, DatabaseCheckpointControl,
};
use crate::barrier::context::{GlobalBarrierWorkerContext, GlobalBarrierWorkerContextImpl};
use crate::barrier::edge_builder::FragmentEdgeBuildResult;
use crate::barrier::info::{BarrierInfo, InflightDatabaseInfo, InflightStreamingJobInfo};
use crate::barrier::progress::CreateMviewProgressTracker;
use crate::barrier::utils::{NodeToCollect, is_valid_after_worker_err};
use crate::controller::fragment::InflightFragmentInfo;
use crate::manager::MetaSrvEnv;
use crate::model::{ActorId, StreamActor, StreamJobActorsToCreate, StreamJobFragments};
use crate::stream::{StreamFragmentGraph, build_actor_connector_splits};
use crate::{MetaError, MetaResult};

fn to_partial_graph_id(job_id: Option<TableId>) -> u32 {
    job_id
        .map(|table| {
            assert_ne!(table.table_id, u32::MAX);
            table.table_id
        })
        .unwrap_or(u32::MAX)
}

pub(super) fn from_partial_graph_id(partial_graph_id: u32) -> Option<TableId> {
    if partial_graph_id == u32::MAX {
        None
    } else {
        Some(TableId::new(partial_graph_id))
    }
}

struct ControlStreamNode {
    worker_id: WorkerId,
    host: HostAddress,
    handle: StreamingControlHandle,
}

pub(super) struct ControlStreamManager {
    connected_nodes: HashMap<WorkerId, ControlStreamNode>,
    workers: HashMap<WorkerId, WorkerNode>,
    env: MetaSrvEnv,
}

impl ControlStreamManager {
    pub(super) fn new(env: MetaSrvEnv) -> Self {
        Self {
            connected_nodes: Default::default(),
            workers: Default::default(),
            env,
        }
    }

    pub(super) fn is_connected(&self, worker_id: WorkerId) -> bool {
        self.connected_nodes.contains_key(&worker_id)
    }

    pub(super) fn host_addr(&self, worker_id: WorkerId) -> HostAddress {
        self.workers[&worker_id].host.clone().unwrap()
    }

    #[await_tree::instrument("try_reconnect_worker({worker_id})")]
    pub(super) async fn try_reconnect_worker(
        &mut self,
        worker_id: WorkerId,
        inflight_infos: impl Iterator<
            Item = (
                DatabaseId,
                &InflightSubscriptionInfo,
                impl Iterator<Item = TableId>,
            ),
        >,
        term_id: String,
        context: &impl GlobalBarrierWorkerContext,
    ) {
        if self.connected_nodes.contains_key(&worker_id) {
            warn!(worker_id, "node already connected");
            return;
        }
        let node = &self.workers[&worker_id];
        let node_host = node.host.as_ref().unwrap();

        let init_request = Self::collect_init_request(inflight_infos, term_id);
        match context.new_control_stream(node, &init_request).await {
            Ok(handle) => {
                assert!(
                    self.connected_nodes
                        .insert(
                            worker_id,
                            ControlStreamNode {
                                worker_id,
                                host: node.host.clone().unwrap(),
                                handle,
                            }
                        )
                        .is_none()
                );
                info!(?node_host, "add control stream worker");
            }
            Err(e) => {
                error!(err = %e.as_report(), ?node_host, "fail to create worker node");
            }
        }
    }

    pub(super) async fn add_worker(
        &mut self,
        node: WorkerNode,
        inflight_infos: impl Iterator<
            Item = (
                DatabaseId,
                &InflightSubscriptionInfo,
                impl Iterator<Item = TableId>,
            ),
        >,
        term_id: String,
        context: &impl GlobalBarrierWorkerContext,
    ) {
        let node_id = node.id as WorkerId;
        let node = match self.workers.entry(node_id) {
            Entry::Occupied(entry) => {
                let entry = entry.into_mut();
                assert_eq!(entry.host, node.host);
                warn!(id = node.id, host = ?node.host, "node already exists");
                &*entry
            }
            Entry::Vacant(entry) => &*entry.insert(node),
        };
        if self.connected_nodes.contains_key(&node_id) {
            warn!(id = node.id, host = ?node.host, "new node already connected");
            return;
        }
        let node_host = node.host.clone().unwrap();
        let mut backoff = ExponentialBackoff::from_millis(100)
            .max_delay(Duration::from_secs(3))
            .factor(5);
        let init_request = Self::collect_init_request(inflight_infos, term_id);
        const MAX_RETRY: usize = 5;
        for i in 1..=MAX_RETRY {
            match context.new_control_stream(node, &init_request).await {
                Ok(handle) => {
                    assert!(
                        self.connected_nodes
                            .insert(
                                node_id,
                                ControlStreamNode {
                                    worker_id: node.id as _,
                                    host: node.host.clone().unwrap(),
                                    handle,
                                }
                            )
                            .is_none()
                    );
                    info!(?node_host, "add control stream worker");
                    return;
                }
                Err(e) => {
                    // It may happen that the dns information of newly registered worker node
                    // has not been propagated to the meta node and cause error. Wait for a while and retry
                    let delay = backoff.next().unwrap();
                    error!(attempt = i, backoff_delay = ?delay, err = %e.as_report(), ?node_host, "fail to resolve worker node address");
                    sleep(delay).await;
                }
            }
        }
        error!(?node_host, "fail to create worker node after retry");
    }

    pub(super) async fn reset(
        &mut self,
        nodes: &HashMap<WorkerId, WorkerNode>,
        term_id: String,
        context: &impl GlobalBarrierWorkerContext,
    ) -> HashSet<WorkerId> {
        let init_request = PbInitRequest {
            databases: vec![],
            term_id,
        };
        let init_request = &init_request;
        self.workers = nodes.clone();
        let nodes = join_all(nodes.iter().map(|(worker_id, node)| async move {
            let result = context.new_control_stream(node, init_request).await;
            (*worker_id, node.clone(), result)
        }))
        .await;
        self.connected_nodes.clear();
        let mut failed_workers = HashSet::new();
        for (worker_id, node, result) in nodes {
            match result {
                Ok(handle) => {
                    assert!(
                        self.connected_nodes
                            .insert(
                                worker_id,
                                ControlStreamNode {
                                    worker_id: node.id as _,
                                    host: node.host.clone().unwrap(),
                                    handle
                                }
                            )
                            .is_none()
                    );
                }
                Err(e) => {
                    failed_workers.insert(worker_id);
                    warn!(
                        e = %e.as_report(),
                        worker_id,
                        ?node,
                        "failed to connect to node"
                    )
                }
            }
        }

        failed_workers
    }

    /// Clear all nodes and response streams in the manager.
    pub(super) fn clear(&mut self) {
        *self = Self::new(self.env.clone());
    }

    fn poll_next_response(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<(
        WorkerId,
        MetaResult<streaming_control_stream_response::Response>,
    )> {
        if self.connected_nodes.is_empty() {
            return Poll::Pending;
        }
        let mut poll_result: Poll<(WorkerId, MetaResult<_>)> = Poll::Pending;
        {
            for (worker_id, node) in &mut self.connected_nodes {
                match node.handle.response_stream.poll_next_unpin(cx) {
                    Poll::Ready(result) => {
                        poll_result = Poll::Ready((
                            *worker_id,
                            result
                                .ok_or_else(|| anyhow!("end of stream").into())
                                .and_then(|result| {
                                    result.map_err(Into::<MetaError>::into).and_then(|resp| {
                                        match resp
                                            .response
                                            .ok_or_else(||anyhow!("empty response"))?
                                        {
                                            streaming_control_stream_response::Response::Shutdown(_) => Err(anyhow!(
                                                "worker node {worker_id} is shutting down"
                                            )
                                            .into()),
                                            streaming_control_stream_response::Response::Init(_) => {
                                                // This arm should be unreachable.
                                                Err(anyhow!("get unexpected init response").into())
                                            }
                                            resp => Ok(resp),
                                        }
                                    })
                                })
                        ));
                        break;
                    }
                    Poll::Pending => {
                        continue;
                    }
                }
            }
        };

        if let Poll::Ready((worker_id, Err(err))) = &poll_result {
            let node = self
                .connected_nodes
                .remove(worker_id)
                .expect("should exist when get shutdown resp");
            warn!(worker_id = node.worker_id, host = ?node.host, err = %err.as_report(), "get error from response stream");
        }

        poll_result
    }

    #[await_tree::instrument("control_stream_next_response")]
    pub(super) async fn next_response(
        &mut self,
    ) -> (
        WorkerId,
        MetaResult<streaming_control_stream_response::Response>,
    ) {
        poll_fn(|cx| self.poll_next_response(cx)).await
    }

    fn collect_init_request(
        initial_inflight_infos: impl Iterator<
            Item = (
                DatabaseId,
                &InflightSubscriptionInfo,
                impl Iterator<Item = TableId>,
            ),
        >,
        term_id: String,
    ) -> PbInitRequest {
        PbInitRequest {
            databases: initial_inflight_infos
                .map(|(database_id, subscriptions, creating_job_ids)| {
                    let mut graphs = vec![PbInitialPartialGraph {
                        partial_graph_id: to_partial_graph_id(None),
                        subscriptions: subscriptions.into_iter().collect_vec(),
                    }];
                    graphs.extend(creating_job_ids.map(|job_id| PbInitialPartialGraph {
                        partial_graph_id: to_partial_graph_id(Some(job_id)),
                        subscriptions: vec![],
                    }));
                    PbDatabaseInitialPartialGraph {
                        database_id: database_id.database_id,
                        graphs,
                    }
                })
                .collect(),
            term_id,
        }
    }
}

pub(super) struct DatabaseInitialBarrierCollector {
    database_id: DatabaseId,
    node_to_collect: NodeToCollect,
    database_state: BarrierWorkerState,
    create_mview_tracker: CreateMviewProgressTracker,
    creating_streaming_job_controls: HashMap<TableId, CreatingStreamingJobControl>,
    committed_epoch: u64,
}

impl Debug for DatabaseInitialBarrierCollector {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DatabaseInitialBarrierCollector")
            .field("database_id", &self.database_id)
            .field("node_to_collect", &self.node_to_collect)
            .finish()
    }
}

impl DatabaseInitialBarrierCollector {
    pub(super) fn is_collected(&self) -> bool {
        self.node_to_collect.is_empty()
            && self
                .creating_streaming_job_controls
                .values()
                .all(|job| job.is_empty())
    }

    pub(super) fn database_state(
        &self,
    ) -> (
        &BarrierWorkerState,
        &HashMap<TableId, CreatingStreamingJobControl>,
    ) {
        (&self.database_state, &self.creating_streaming_job_controls)
    }

    pub(super) fn collect_resp(&mut self, resp: BarrierCompleteResponse) {
        assert_eq!(self.database_id.database_id, resp.database_id);
        if let Some(creating_job_id) = from_partial_graph_id(resp.partial_graph_id) {
            self.creating_streaming_job_controls
                .get_mut(&creating_job_id)
                .expect("should exist")
                .collect(resp);
        } else {
            assert_eq!(resp.epoch, self.committed_epoch);
            assert!(
                self.node_to_collect
                    .remove(&(resp.worker_id as _))
                    .is_some()
            );
        }
    }

    pub(super) fn finish(self) -> DatabaseCheckpointControl {
        assert!(self.is_collected());
        DatabaseCheckpointControl::recovery(
            self.database_id,
            self.create_mview_tracker,
            self.database_state,
            self.committed_epoch,
            self.creating_streaming_job_controls,
        )
    }

    pub(super) fn is_valid_after_worker_err(&mut self, worker_id: WorkerId) -> bool {
        is_valid_after_worker_err(&mut self.node_to_collect, worker_id)
            && self
                .creating_streaming_job_controls
                .values_mut()
                .all(|job| job.is_valid_after_worker_err(worker_id))
    }
}

impl ControlStreamManager {
    /// Extract information from the loaded runtime barrier worker snapshot info, and inject the initial barrier.
    #[expect(clippy::too_many_arguments)]
    pub(super) fn inject_database_initial_barrier(
        &mut self,
        database_id: DatabaseId,
        jobs: HashMap<TableId, InflightStreamingJobInfo>,
        state_table_committed_epochs: &mut HashMap<TableId, u64>,
        state_table_log_epochs: &mut HashMap<TableId, Vec<(Vec<u64>, u64)>>,
        edges: &mut FragmentEdgeBuildResult,
        stream_actors: &HashMap<ActorId, StreamActor>,
        source_splits: &mut HashMap<ActorId, Vec<SplitImpl>>,
        background_jobs: &mut HashMap<TableId, (String, StreamJobFragments)>,
        mut subscription_info: InflightSubscriptionInfo,
        is_paused: bool,
        hummock_version_stats: &HummockVersionStats,
        cdc_table_snapshot_split_assignment: &mut CdcTableSnapshotSplitAssignment,
    ) -> MetaResult<DatabaseInitialBarrierCollector> {
        self.add_partial_graph(database_id, None);
        let source_split_assignments = jobs
            .values()
            .flat_map(|job| job.fragment_infos())
            .flat_map(|info| info.actors.keys())
            .filter_map(|actor_id| {
                let actor_id = *actor_id as ActorId;
                source_splits
                    .remove(&actor_id)
                    .map(|splits| (actor_id, splits))
            })
            .collect();
        let database_cdc_table_snapshot_split_assignment = jobs
            .values()
            .flat_map(|job| job.fragment_infos())
            .flat_map(|info| info.actors.keys())
            .filter_map(|actor_id| {
                let actor_id = *actor_id as ActorId;
                cdc_table_snapshot_split_assignment
                    .remove(&actor_id)
                    .map(|splits| (actor_id, splits))
            })
            .collect();
        let mutation = Mutation::Add(AddMutation {
            // Actors built during recovery is not treated as newly added actors.
            actor_dispatchers: Default::default(),
            added_actors: Default::default(),
            actor_splits: build_actor_connector_splits(&source_split_assignments),
            actor_cdc_table_snapshot_splits: build_pb_actor_cdc_table_snapshot_splits(
                database_cdc_table_snapshot_split_assignment,
            ),
            pause: is_paused,
            subscriptions_to_add: Default::default(),
            // TODO(kwannoel): recover using backfill order plan
            backfill_nodes_to_pause: Default::default(),
        });

        fn resolve_jobs_committed_epoch(
            state_table_committed_epochs: &mut HashMap<TableId, u64>,
            jobs: impl IntoIterator<Item = &InflightStreamingJobInfo>,
        ) -> u64 {
            let mut epochs = jobs
                .into_iter()
                .flat_map(InflightStreamingJobInfo::existing_table_ids)
                .map(|table_id| {
                    (
                        table_id,
                        state_table_committed_epochs
                            .remove(&table_id)
                            .expect("should exist"),
                    )
                });
            let (first_table_id, prev_epoch) = epochs.next().expect("non-empty");
            for (table_id, epoch) in epochs {
                assert_eq!(
                    prev_epoch, epoch,
                    "{} has different committed epoch to {}",
                    first_table_id, table_id
                );
            }
            prev_epoch
        }

        let mut database_jobs = HashMap::new();
        let mut snapshot_backfill_jobs = HashMap::new();
        let mut background_mviews = HashMap::new();

        for (job_id, job) in jobs {
            if let Some((definition, stream_job_fragments)) = background_jobs.remove(&job_id) {
                if stream_job_fragments.fragments().any(|fragment| {
                    fragment
                        .fragment_type_mask
                        .contains(FragmentTypeFlag::SnapshotBackfillStreamScan)
                }) {
                    debug!(%job_id, definition, "recovered snapshot backfill job");
                    snapshot_backfill_jobs.insert(job_id, (job, definition, stream_job_fragments));
                } else {
                    database_jobs.insert(job_id, job);
                    background_mviews.insert(job_id, (definition, stream_job_fragments));
                }
            } else {
                database_jobs.insert(job_id, job);
            }
        }

        let database_job_log_epochs: HashMap<_, _> = database_jobs
            .keys()
            .filter_map(|job_id| {
                state_table_log_epochs
                    .remove(job_id)
                    .map(|epochs| (*job_id, epochs))
            })
            .collect();

        let prev_epoch =
            resolve_jobs_committed_epoch(state_table_committed_epochs, database_jobs.values());
        let prev_epoch = TracedEpoch::new(Epoch(prev_epoch));
        // Use a different `curr_epoch` for each recovery attempt.
        let curr_epoch = prev_epoch.next();
        let barrier_info = BarrierInfo {
            prev_epoch,
            curr_epoch,
            kind: BarrierKind::Initial,
        };

        let mut ongoing_snapshot_backfill_jobs: HashMap<TableId, _> = HashMap::new();
        for (job_id, (info, definition, stream_job_fragments)) in snapshot_backfill_jobs {
            let committed_epoch =
                resolve_jobs_committed_epoch(state_table_committed_epochs, [&info]);
            if committed_epoch == barrier_info.prev_epoch() {
                info!(
                    "recovered creating snapshot backfill job {} catch up with upstream already",
                    job_id
                );
                background_mviews
                    .try_insert(job_id, (definition, stream_job_fragments))
                    .expect("non-duplicate");
                database_jobs
                    .try_insert(job_id, info)
                    .expect("non-duplicate");
                continue;
            }
            let snapshot_backfill_info = StreamFragmentGraph::collect_snapshot_backfill_info_impl(
                stream_job_fragments
                    .fragments()
                    .map(|fragment| (&fragment.nodes, fragment.fragment_type_mask)),
            )?
            .0
            .ok_or_else(|| {
                anyhow!(
                    "recovered snapshot backfill job {} has no snapshot backfill info",
                    job_id
                )
            })?;
            let mut snapshot_epoch = None;
            let upstream_table_ids: HashSet<_> = snapshot_backfill_info
                .upstream_mv_table_id_to_backfill_epoch
                .keys()
                .cloned()
                .collect();
            for (upstream_table_id, epoch) in
                snapshot_backfill_info.upstream_mv_table_id_to_backfill_epoch
            {
                let epoch = epoch.ok_or_else(|| anyhow!("recovered snapshot backfill job {} to upstream {} has not set snapshot epoch", job_id, upstream_table_id))?;
                let snapshot_epoch = snapshot_epoch.get_or_insert(epoch);
                if *snapshot_epoch != epoch {
                    return Err(anyhow!("snapshot epoch {} to upstream {} different to snapshot epoch {} to previous upstream", epoch, upstream_table_id, snapshot_epoch).into());
                }
            }
            let snapshot_epoch = snapshot_epoch.ok_or_else(|| {
                anyhow!(
                    "snapshot backfill job {} has not set snapshot epoch",
                    job_id
                )
            })?;
            for upstream_table_id in &upstream_table_ids {
                subscription_info
                    .mv_depended_subscriptions
                    .entry(*upstream_table_id)
                    .or_default()
                    .try_insert(job_id.into(), max(snapshot_epoch, committed_epoch))
                    .expect("non-duplicate");
            }
            ongoing_snapshot_backfill_jobs
                .try_insert(
                    job_id,
                    (
                        info,
                        definition,
                        stream_job_fragments,
                        upstream_table_ids,
                        committed_epoch,
                        snapshot_epoch,
                    ),
                )
                .expect("non-duplicated");
        }

        let node_to_collect = {
            let node_actors = edges.collect_actors_to_create(database_jobs.values().flatten().map(
                move |fragment_info| {
                    (
                        fragment_info.fragment_id,
                        &fragment_info.nodes,
                        fragment_info.actors.iter().map(move |(actor_id, actor)| {
                            (
                                stream_actors.get(actor_id).expect("should exist"),
                                actor.worker_id,
                            )
                        }),
                    )
                },
            ));

            let node_to_collect = self.inject_barrier(
                database_id,
                None,
                Some(mutation.clone()),
                &barrier_info,
                database_jobs.values().flatten(),
                database_jobs.values().flatten(),
                Some(node_actors),
                (&subscription_info).into_iter().collect(),
                vec![],
            )?;
            debug!(
                ?node_to_collect,
                database_id = database_id.database_id,
                "inject initial barrier"
            );
            node_to_collect
        };

        let tracker = CreateMviewProgressTracker::recover(
            background_mviews
                .iter()
                .map(|(table_id, (definition, stream_job_fragments))| {
                    (
                        *table_id,
                        (definition.clone(), stream_job_fragments, Default::default()),
                    )
                }),
            hummock_version_stats,
        );

        let mut creating_streaming_job_controls: HashMap<TableId, CreatingStreamingJobControl> =
            HashMap::new();
        for (
            job_id,
            (
                info,
                definition,
                stream_job_fragments,
                upstream_table_ids,
                committed_epoch,
                snapshot_epoch,
            ),
        ) in ongoing_snapshot_backfill_jobs
        {
            let node_actors =
                edges.collect_actors_to_create(info.fragment_infos().map(move |fragment_info| {
                    (
                        fragment_info.fragment_id,
                        &fragment_info.nodes,
                        fragment_info.actors.iter().map(move |(actor_id, actor)| {
                            (
                                stream_actors.get(actor_id).expect("should exist"),
                                actor.worker_id,
                            )
                        }),
                    )
                }));

            creating_streaming_job_controls.insert(
                job_id,
                CreatingStreamingJobControl::recover(
                    database_id,
                    job_id,
                    definition,
                    upstream_table_ids,
                    &database_job_log_epochs,
                    snapshot_epoch,
                    committed_epoch,
                    barrier_info.curr_epoch.value().0,
                    info,
                    stream_job_fragments,
                    hummock_version_stats,
                    node_actors,
                    mutation.clone(),
                    self,
                )?,
            );
        }

        self.env.shared_actor_infos().recover_database(
            database_id,
            database_jobs.values().flatten().chain(
                creating_streaming_job_controls
                    .values()
                    .flat_map(|job| job.graph_info().fragment_infos()),
            ),
        );

        let committed_epoch = barrier_info.prev_epoch();
        let new_epoch = barrier_info.curr_epoch;
        let database_state = BarrierWorkerState::recovery(
            database_id,
            self.env.shared_actor_infos().clone(),
            new_epoch,
            database_jobs.into_values(),
            subscription_info,
            is_paused,
        );
        Ok(DatabaseInitialBarrierCollector {
            database_id,
            node_to_collect,
            database_state,
            create_mview_tracker: tracker,
            creating_streaming_job_controls,
            committed_epoch,
        })
    }

    pub(super) fn inject_command_ctx_barrier(
        &mut self,
        database_id: DatabaseId,
        command: Option<&Command>,
        barrier_info: &BarrierInfo,
        is_paused: bool,
        pre_applied_graph_info: &InflightDatabaseInfo,
        applied_graph_info: &InflightDatabaseInfo,
        edges: &mut Option<FragmentEdgeBuildResult>,
    ) -> MetaResult<NodeToCollect> {
        let mutation = command.and_then(|c| c.to_mutation(is_paused, edges, self));
        let subscriptions_to_add = if let Some(Mutation::Add(add)) = &mutation {
            add.subscriptions_to_add.clone()
        } else {
            vec![]
        };
        let subscriptions_to_remove = if let Some(Mutation::DropSubscriptions(drop)) = &mutation {
            drop.info.clone()
        } else {
            vec![]
        };
        self.inject_barrier(
            database_id,
            None,
            mutation,
            barrier_info,
            pre_applied_graph_info.fragment_infos(),
            applied_graph_info.fragment_infos(),
            command
                .as_ref()
                .map(|command| command.actors_to_create(pre_applied_graph_info, edges, self))
                .unwrap_or_default(),
            subscriptions_to_add,
            subscriptions_to_remove,
        )
    }

    pub(super) fn inject_barrier<'a>(
        &mut self,
        database_id: DatabaseId,
        creating_table_id: Option<TableId>,
        mutation: Option<Mutation>,
        barrier_info: &BarrierInfo,
        pre_applied_graph_info: impl IntoIterator<Item = &InflightFragmentInfo>,
        applied_graph_info: impl IntoIterator<Item = &'a InflightFragmentInfo> + 'a,
        mut new_actors: Option<StreamJobActorsToCreate>,
        subscriptions_to_add: Vec<SubscriptionUpstreamInfo>,
        subscriptions_to_remove: Vec<SubscriptionUpstreamInfo>,
    ) -> MetaResult<NodeToCollect> {
        fail_point!("inject_barrier_err", |_| risingwave_common::bail!(
            "inject_barrier_err"
        ));

        let partial_graph_id = to_partial_graph_id(creating_table_id);

        let node_actors = InflightFragmentInfo::actor_ids_to_collect(pre_applied_graph_info);

        for worker_id in node_actors.keys() {
            if !self.connected_nodes.contains_key(worker_id) {
                return Err(anyhow!("unconnected worker node {}", worker_id).into());
            }
        }

        let table_ids_to_sync: HashSet<_> =
            InflightFragmentInfo::existing_table_ids(applied_graph_info)
                .map(|table_id| table_id.table_id)
                .collect();

        let mut node_need_collect = HashMap::new();

        self.connected_nodes
            .iter()
            .try_for_each(|(node_id, node)| {
                let actor_ids_to_collect = node_actors
                    .get(node_id)
                    .map(|actors| actors.iter().cloned())
                    .into_iter()
                    .flatten()
                    .collect_vec();
                let is_empty = actor_ids_to_collect.is_empty();
                {
                    let mutation = mutation.clone();
                    let barrier = Barrier {
                        epoch: Some(risingwave_pb::data::Epoch {
                            curr: barrier_info.curr_epoch.value().0,
                            prev: barrier_info.prev_epoch(),
                        }),
                        mutation: mutation.clone().map(|_| BarrierMutation { mutation }),
                        tracing_context: TracingContext::from_span(barrier_info.curr_epoch.span())
                            .to_protobuf(),
                        kind: barrier_info.kind.to_protobuf() as i32,
                        passed_actors: vec![],
                    };

                    node.handle
                        .request_sender
                        .send(StreamingControlStreamRequest {
                            request: Some(
                                streaming_control_stream_request::Request::InjectBarrier(
                                    InjectBarrierRequest {
                                        request_id: Uuid::new_v4().to_string(),
                                        barrier: Some(barrier),
                                        database_id: database_id.database_id,
                                        actor_ids_to_collect,
                                        table_ids_to_sync: table_ids_to_sync
                                            .iter()
                                            .cloned()
                                            .collect(),
                                        partial_graph_id,
                                        actors_to_build: new_actors
                                            .as_mut()
                                            .map(|new_actors| new_actors.remove(&(*node_id as _)))
                                            .into_iter()
                                            .flatten()
                                            .flatten()
                                            .map(|(fragment_id, (node, actors))| {
                                                FragmentBuildActorInfo {
                                                    fragment_id,
                                                    node: Some(node),
                                                    actors: actors
                                                        .into_iter()
                                                        .map(|(actor, upstreams, dispatchers)| {
                                                            BuildActorInfo {
                                                                actor_id: actor.actor_id,
                                                                fragment_upstreams: upstreams
                                                                    .into_iter()
                                                                    .map(|(fragment_id, upstreams)| {
                                                                        (
                                                                            fragment_id,
                                                                            UpstreamActors {
                                                                                actors: upstreams
                                                                                    .into_values()
                                                                                    .collect(),
                                                                            },
                                                                        )
                                                                    })
                                                                    .collect(),
                                                                dispatchers,
                                                                vnode_bitmap: actor.vnode_bitmap.map(|bitmap| bitmap.to_protobuf()),
                                                                mview_definition: actor.mview_definition,
                                                                expr_context: actor.expr_context,
                                                            }
                                                        })
                                                        .collect(),
                                                }
                                            })
                                            .collect(),
                                        subscriptions_to_add: subscriptions_to_add.clone(),
                                        subscriptions_to_remove: subscriptions_to_remove.clone(),
                                    },
                                ),
                            ),
                        })
                        .map_err(|_| {
                            MetaError::from(anyhow!(
                                "failed to send request to {} {:?}",
                                node.worker_id,
                                node.host
                            ))
                        })?;

                    node_need_collect.insert(*node_id as WorkerId, is_empty);
                    Result::<_, MetaError>::Ok(())
                }
            })
            .inspect_err(|e| {
                // Record failure in event log.
                use risingwave_pb::meta::event_log;
                let event = event_log::EventInjectBarrierFail {
                    prev_epoch: barrier_info.prev_epoch(),
                    cur_epoch: barrier_info.curr_epoch.value().0,
                    error: e.to_report_string(),
                };
                self.env
                    .event_log_manager_ref()
                    .add_event_logs(vec![event_log::Event::InjectBarrierFail(event)]);
            })?;
        Ok(node_need_collect)
    }

    pub(super) fn add_partial_graph(
        &mut self,
        database_id: DatabaseId,
        creating_job_id: Option<TableId>,
    ) {
        let partial_graph_id = to_partial_graph_id(creating_job_id);
        self.connected_nodes.iter().for_each(|(_, node)| {
            if node
                .handle
                .request_sender
                .send(StreamingControlStreamRequest {
                    request: Some(
                        streaming_control_stream_request::Request::CreatePartialGraph(
                            CreatePartialGraphRequest {
                                database_id: database_id.database_id,
                                partial_graph_id,
                            },
                        ),
                    ),
                }).is_err() {
                warn!(%database_id, ?creating_job_id, worker_id = node.worker_id, "fail to add partial graph to worker")
            }
        });
    }

    pub(super) fn remove_partial_graph(
        &mut self,
        database_id: DatabaseId,
        creating_job_ids: Vec<TableId>,
    ) {
        if creating_job_ids.is_empty() {
            return;
        }
        let partial_graph_ids = creating_job_ids
            .into_iter()
            .map(|job_id| to_partial_graph_id(Some(job_id)))
            .collect_vec();
        self.connected_nodes.iter().for_each(|(_, node)| {
            if node.handle
                .request_sender
                .send(StreamingControlStreamRequest {
                    request: Some(
                        streaming_control_stream_request::Request::RemovePartialGraph(
                            RemovePartialGraphRequest {
                                partial_graph_ids: partial_graph_ids.clone(),
                                database_id: database_id.database_id,
                            },
                        ),
                    ),
                })
                .is_err()
            {
                warn!(worker_id = node.worker_id,node = ?node.host,"failed to send remove partial graph request");
            }
        })
    }

    pub(super) fn reset_database(
        &mut self,
        database_id: DatabaseId,
        reset_request_id: u32,
    ) -> HashSet<WorkerId> {
        self.connected_nodes
            .iter()
            .filter_map(|(worker_id, node)| {
                if node
                    .handle
                    .request_sender
                    .send(StreamingControlStreamRequest {
                        request: Some(streaming_control_stream_request::Request::ResetDatabase(
                            ResetDatabaseRequest {
                                database_id: database_id.database_id,
                                reset_request_id,
                            },
                        )),
                    })
                    .is_err()
                {
                    warn!(worker_id, node = ?node.host,"failed to send reset database request");
                    None
                } else {
                    Some(*worker_id)
                }
            })
            .collect()
    }
}

impl GlobalBarrierWorkerContextImpl {
    pub(super) async fn new_control_stream_impl(
        &self,
        node: &WorkerNode,
        init_request: &PbInitRequest,
    ) -> MetaResult<StreamingControlHandle> {
        let handle = self
            .env
            .stream_client_pool()
            .get(node)
            .await?
            .start_streaming_control(init_request.clone())
            .await?;
        Ok(handle)
    }
}

pub(super) fn merge_node_rpc_errors<E: Error + Send + Sync + 'static>(
    message: &str,
    errors: impl IntoIterator<Item = (WorkerId, E)>,
) -> MetaError {
    use std::fmt::Write;

    use risingwave_common::error::error_request_copy;
    use risingwave_common::error::tonic::extra::Score;

    let errors = errors.into_iter().collect_vec();

    if errors.is_empty() {
        return anyhow!(message.to_owned()).into();
    }

    // Create the error from the single error.
    let single_error = |(worker_id, e)| {
        anyhow::Error::from(e)
            .context(format!("{message}, in worker node {worker_id}"))
            .into()
    };

    if errors.len() == 1 {
        return single_error(errors.into_iter().next().unwrap());
    }

    // Find the error with the highest score.
    let max_score = errors
        .iter()
        .filter_map(|(_, e)| error_request_copy::<Score>(e))
        .max();

    if let Some(max_score) = max_score {
        let mut errors = errors;
        let max_scored = errors
            .extract_if(.., |(_, e)| {
                error_request_copy::<Score>(e) == Some(max_score)
            })
            .next()
            .unwrap();

        return single_error(max_scored);
    }

    // The errors do not have scores, so simply concatenate them.
    let concat: String = errors
        .into_iter()
        .fold(format!("{message}: "), |mut s, (w, e)| {
            write!(&mut s, " in worker node {}, {};", w, e.as_report()).unwrap();
            s
        });
    anyhow!(concat).into()
}
