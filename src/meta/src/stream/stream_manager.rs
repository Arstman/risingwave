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

use std::collections::{BTreeMap, HashMap, HashSet};
use std::iter;
use std::ops::Deref;
use std::sync::Arc;

use await_tree::{InstrumentAwait, span};
use futures::FutureExt;
use futures::future::join_all;
use itertools::Itertools;
use risingwave_common::bail;
use risingwave_common::catalog::{DatabaseId, TableId};
use risingwave_meta_model::ObjectId;
use risingwave_pb::catalog::{CreateType, PbSink, PbTable, Subscription};
use risingwave_pb::meta::object::PbObjectInfo;
use risingwave_pb::meta::subscribe_response::{Operation, PbInfo};
use risingwave_pb::meta::table_fragments::ActorStatus;
use risingwave_pb::meta::{PbObject, PbObjectGroup};
use risingwave_pb::plan_common::PbColumnCatalog;
use thiserror_ext::AsReport;
use tokio::sync::mpsc::Sender;
use tokio::sync::{Mutex, oneshot};
use tracing::Instrument;

use super::{
    FragmentBackfillOrder, JobParallelismTarget, JobReschedulePolicy, JobReschedulePostUpdates,
    JobRescheduleTarget, JobResourceGroupTarget, Locations, RescheduleOptions, ScaleControllerRef,
};
use crate::barrier::{
    BarrierScheduler, Command, CreateStreamingJobCommandInfo, CreateStreamingJobType,
    ReplaceStreamJobPlan, SnapshotBackfillInfo,
};
use crate::controller::catalog::DropTableConnectorContext;
use crate::controller::fragment::{InflightActorInfo, InflightFragmentInfo};
use crate::error::bail_invalid_parameter;
use crate::manager::{
    MetaSrvEnv, MetadataManager, NotificationVersion, StreamingJob, StreamingJobType,
};
use crate::model::{
    ActorId, Fragment, FragmentDownstreamRelation, FragmentId, FragmentNewNoShuffle,
    FragmentReplaceUpstream, StreamJobFragments, StreamJobFragmentsToCreate, TableParallelism,
};
use crate::stream::cdc::{
    assign_cdc_table_snapshot_splits, assign_cdc_table_snapshot_splits_for_replace_table,
};
use crate::stream::{SourceChange, SourceManagerRef};
use crate::{MetaError, MetaResult};

pub type GlobalStreamManagerRef = Arc<GlobalStreamManager>;

#[derive(Default)]
pub struct CreateStreamingJobOption {
    // leave empty as a placeholder for future option if there is any
}

/// [`CreateStreamingJobContext`] carries one-time infos for creating a streaming job.
///
/// Note: for better readability, keep this struct complete and immutable once created.
pub struct CreateStreamingJobContext {
    /// New fragment relation to add from upstream fragments to downstream fragments.
    pub upstream_fragment_downstreams: FragmentDownstreamRelation,
    pub new_no_shuffle: FragmentNewNoShuffle,
    pub upstream_actors: HashMap<FragmentId, HashSet<ActorId>>,

    /// The locations of the actors to build in the streaming job.
    pub building_locations: Locations,

    /// DDL definition.
    pub definition: String,

    pub mv_table_id: Option<u32>,

    pub create_type: CreateType,

    pub job_type: StreamingJobType,

    /// Context provided for potential replace table, typically used when sinking into a table.
    pub replace_table_job_info: Option<(
        StreamingJob,
        ReplaceStreamJobContext,
        StreamJobFragmentsToCreate,
    )>,

    pub snapshot_backfill_info: Option<SnapshotBackfillInfo>,
    pub cross_db_snapshot_backfill_info: SnapshotBackfillInfo,

    pub option: CreateStreamingJobOption,

    pub streaming_job: StreamingJob,

    pub fragment_backfill_ordering: FragmentBackfillOrder,
}

pub enum CreatingState {
    Failed { reason: MetaError },
    // sender is used to notify the canceling result.
    Canceling { finish_tx: oneshot::Sender<()> },
    Created { version: NotificationVersion },
}

struct StreamingJobExecution {
    id: TableId,
    shutdown_tx: Option<Sender<CreatingState>>,
}

impl StreamingJobExecution {
    fn new(id: TableId, shutdown_tx: Sender<CreatingState>) -> Self {
        Self {
            id,
            shutdown_tx: Some(shutdown_tx),
        }
    }
}

#[derive(Default)]
struct CreatingStreamingJobInfo {
    streaming_jobs: Mutex<HashMap<TableId, StreamingJobExecution>>,
}

impl CreatingStreamingJobInfo {
    async fn add_job(&self, job: StreamingJobExecution) {
        let mut jobs = self.streaming_jobs.lock().await;
        jobs.insert(job.id, job);
    }

    async fn delete_job(&self, job_id: TableId) {
        let mut jobs = self.streaming_jobs.lock().await;
        jobs.remove(&job_id);
    }

    async fn cancel_jobs(
        &self,
        job_ids: Vec<TableId>,
    ) -> (HashMap<TableId, oneshot::Receiver<()>>, Vec<TableId>) {
        let mut jobs = self.streaming_jobs.lock().await;
        let mut receivers = HashMap::new();
        let mut recovered_job_ids = vec![];
        for job_id in job_ids {
            if let Some(job) = jobs.get_mut(&job_id) {
                if let Some(shutdown_tx) = job.shutdown_tx.take() {
                    let (tx, rx) = oneshot::channel();
                    if shutdown_tx
                        .send(CreatingState::Canceling { finish_tx: tx })
                        .await
                        .is_ok()
                    {
                        receivers.insert(job_id, rx);
                    } else {
                        tracing::warn!(id=?job_id, "failed to send canceling state");
                    }
                }
            } else {
                // If these job ids do not exist in streaming_jobs,
                // we can infer they either:
                // 1. are entirely non-existent,
                // 2. OR they are recovered streaming jobs, and managed by BarrierManager.
                recovered_job_ids.push(job_id);
            }
        }
        (receivers, recovered_job_ids)
    }
}

type CreatingStreamingJobInfoRef = Arc<CreatingStreamingJobInfo>;

#[derive(Debug, Clone)]
pub struct AutoRefreshSchemaSinkContext {
    pub tmp_sink_id: ObjectId,
    pub original_sink: PbSink,
    pub original_fragment: Fragment,
    pub new_columns: Vec<PbColumnCatalog>,
    pub new_fragment: Fragment,
    pub new_log_store_table: Option<PbTable>,
    pub actor_status: BTreeMap<ActorId, ActorStatus>,
}

impl AutoRefreshSchemaSinkContext {
    pub fn new_fragment_info(&self) -> InflightFragmentInfo {
        InflightFragmentInfo {
            fragment_id: self.new_fragment.fragment_id,
            distribution_type: self.new_fragment.distribution_type.into(),
            nodes: self.new_fragment.nodes.clone(),
            actors: self
                .new_fragment
                .actors
                .iter()
                .map(|actor| {
                    (
                        actor.actor_id as _,
                        InflightActorInfo {
                            worker_id: self.actor_status[&actor.actor_id]
                                .location
                                .as_ref()
                                .unwrap()
                                .worker_node_id as _,
                            vnode_bitmap: actor.vnode_bitmap.clone(),
                        },
                    )
                })
                .collect(),
            state_table_ids: self
                .new_fragment
                .state_table_ids
                .iter()
                .map(|table| (*table).into())
                .collect(),
        }
    }
}

/// [`ReplaceStreamJobContext`] carries one-time infos for replacing the plan of an existing stream job.
///
/// Note: for better readability, keep this struct complete and immutable once created.
pub struct ReplaceStreamJobContext {
    /// The old job fragments to be replaced.
    pub old_fragments: StreamJobFragments,

    /// The updates to be applied to the downstream chain actors. Used for schema change.
    pub replace_upstream: FragmentReplaceUpstream,
    pub new_no_shuffle: FragmentNewNoShuffle,

    /// New fragment relation to add from existing upstream fragment to downstream fragment.
    pub upstream_fragment_downstreams: FragmentDownstreamRelation,

    /// The locations of the actors to build in the new job to replace.
    pub building_locations: Locations,

    pub streaming_job: StreamingJob,

    pub tmp_id: u32,

    /// Used for dropping an associated source. Dropping source and related internal tables.
    pub drop_table_connector_ctx: Option<DropTableConnectorContext>,

    pub auto_refresh_schema_sinks: Option<Vec<AutoRefreshSchemaSinkContext>>,
}

/// `GlobalStreamManager` manages all the streams in the system.
pub struct GlobalStreamManager {
    pub env: MetaSrvEnv,

    pub metadata_manager: MetadataManager,

    /// Broadcasts and collect barriers
    pub barrier_scheduler: BarrierScheduler,

    /// Maintains streaming sources from external system like kafka
    pub source_manager: SourceManagerRef,

    /// Creating streaming job info.
    creating_job_info: CreatingStreamingJobInfoRef,

    pub scale_controller: ScaleControllerRef,
}

impl GlobalStreamManager {
    pub fn new(
        env: MetaSrvEnv,
        metadata_manager: MetadataManager,
        barrier_scheduler: BarrierScheduler,
        source_manager: SourceManagerRef,
        scale_controller: ScaleControllerRef,
    ) -> MetaResult<Self> {
        Ok(Self {
            env,
            metadata_manager,
            barrier_scheduler,
            source_manager,
            creating_job_info: Arc::new(CreatingStreamingJobInfo::default()),
            scale_controller,
        })
    }

    /// Create streaming job, it works as follows:
    ///
    /// 1. Broadcast the actor info based on the scheduling result in the context, build the hanging
    ///    channels in upstream worker nodes.
    /// 2. (optional) Get the split information of the `StreamSource` via source manager and patch
    ///    actors.
    /// 3. Notify related worker nodes to update and build the actors.
    /// 4. Store related meta data.
    ///
    /// This function is a wrapper over [`Self::run_create_streaming_job_command`].
    #[await_tree::instrument]
    pub async fn create_streaming_job(
        self: &Arc<Self>,
        stream_job_fragments: StreamJobFragmentsToCreate,
        ctx: CreateStreamingJobContext,
        run_command_notifier: Option<oneshot::Sender<MetaResult<()>>>,
    ) -> MetaResult<NotificationVersion> {
        let await_tree_key = format!("Create Streaming Job Worker ({})", ctx.streaming_job.id());
        let await_tree_span = span!(
            "{:?}({})",
            ctx.streaming_job.job_type(),
            ctx.streaming_job.name()
        );

        let table_id = stream_job_fragments.stream_job_id();
        let database_id = ctx.streaming_job.database_id().into();
        let (sender, mut receiver) = tokio::sync::mpsc::channel(10);
        let execution = StreamingJobExecution::new(table_id, sender.clone());
        self.creating_job_info.add_job(execution).await;

        let stream_manager = self.clone();
        let fut = async move {
            let res: MetaResult<_> = try {
                let (source_change, streaming_job) = stream_manager
                    .run_create_streaming_job_command(stream_job_fragments, ctx)
                    .inspect(move |result| {
                        if let Some(tx) = run_command_notifier {
                            let _ = tx.send(match result {
                                Ok(_) => {
                                    Ok(())
                                }
                                Err(err) => {
                                    Err(err.clone())
                                }
                            });
                        }
                    })
                    .await?;
                let version = stream_manager
                    .metadata_manager
                    .wait_streaming_job_finished(
                        streaming_job.database_id().into(),
                        streaming_job.id() as _,
                    )
                    .await?;
                stream_manager.source_manager
                    .apply_source_change(source_change)
                    .await;
                tracing::debug!(?streaming_job, "stream job finish");
                version
            };

            match res {
                Ok(version) => {
                    let _ = sender
                        .send(CreatingState::Created { version })
                        .await
                        .inspect_err(|_| tracing::warn!("failed to notify created: {table_id}"));
                }
                Err(err) => {
                    let _ = sender
                        .send(CreatingState::Failed {
                            reason: err.clone(),
                        })
                        .await
                        .inspect_err(|_| {
                            tracing::warn!(error = %err.as_report(), "failed to notify failed: {table_id}")
                        });
                }
            }
        }
        .in_current_span();

        let fut = (self.env.await_tree_reg())
            .register(await_tree_key, await_tree_span)
            .instrument(Box::pin(fut));
        tokio::spawn(fut);

        while let Some(state) = receiver
            .recv()
            .instrument_await("recv_creating_state")
            .await
        {
            match state {
                CreatingState::Failed { reason } => {
                    tracing::debug!(id=?table_id, "stream job failed");
                    // FIXME(kwannoel): For creating stream jobs
                    // we need to clean up the resources in the stream manager.
                    self.creating_job_info.delete_job(table_id).await;
                    return Err(reason);
                }
                CreatingState::Canceling { finish_tx } => {
                    tracing::debug!(id=?table_id, "cancelling streaming job");
                    if let Ok(table_fragments) = self
                        .metadata_manager
                        .get_job_fragments_by_id(&table_id)
                        .await
                    {
                        // try to cancel buffered creating command.
                        if self
                            .barrier_scheduler
                            .try_cancel_scheduled_create(database_id, table_id)
                        {
                            tracing::debug!("cancelling streaming job {table_id} in buffer queue.");
                        } else if !table_fragments.is_created() {
                            tracing::debug!(
                                "cancelling streaming job {table_id} by issue cancel command."
                            );
                            self.metadata_manager
                                .catalog_controller
                                .try_abort_creating_streaming_job(table_id.table_id as _, true)
                                .await?;

                            self.barrier_scheduler
                                .run_command(database_id, Command::cancel(&table_fragments))
                                .await?;
                        } else {
                            // streaming job is already completed.
                            continue;
                        }
                        let _ = finish_tx.send(()).inspect_err(|_| {
                            tracing::warn!("failed to notify cancelled: {table_id}")
                        });
                        self.creating_job_info.delete_job(table_id).await;
                        return Err(MetaError::cancelled("create"));
                    }
                }
                CreatingState::Created { version } => {
                    self.creating_job_info.delete_job(table_id).await;
                    return Ok(version);
                }
            }
        }
        self.creating_job_info.delete_job(table_id).await;
        bail!("receiver failed to get notification version for finished stream job")
    }

    /// The function will only return after backfilling finishes
    /// ([`crate::manager::MetadataManager::wait_streaming_job_finished`]).
    #[await_tree::instrument]
    async fn run_create_streaming_job_command(
        &self,
        stream_job_fragments: StreamJobFragmentsToCreate,
        CreateStreamingJobContext {
            streaming_job,
            upstream_fragment_downstreams,
            new_no_shuffle,
            upstream_actors,
            definition,
            create_type,
            job_type,
            replace_table_job_info,
            snapshot_backfill_info,
            cross_db_snapshot_backfill_info,
            fragment_backfill_ordering,
            ..
        }: CreateStreamingJobContext,
    ) -> MetaResult<(SourceChange, StreamingJob)> {
        let mut replace_table_command = None;

        tracing::debug!(
            table_id = %stream_job_fragments.stream_job_id(),
            "built actors finished"
        );

        if let Some((streaming_job, context, stream_job_fragments)) = replace_table_job_info {
            self.metadata_manager
                .catalog_controller
                .prepare_stream_job_fragments(&stream_job_fragments, &streaming_job, true)
                .await?;

            let tmp_table_id = stream_job_fragments.stream_job_id();
            let init_split_assignment = self
                .source_manager
                .allocate_splits(&stream_job_fragments)
                .await?;
            let cdc_table_snapshot_split_assignment =
                assign_cdc_table_snapshot_splits_for_replace_table(
                    context.old_fragments.stream_job_id.table_id,
                    &stream_job_fragments.inner,
                    self.env.meta_store_ref(),
                )
                .await?;

            replace_table_command = Some(ReplaceStreamJobPlan {
                old_fragments: context.old_fragments,
                new_fragments: stream_job_fragments,
                replace_upstream: context.replace_upstream,
                upstream_fragment_downstreams: context.upstream_fragment_downstreams,
                init_split_assignment,
                streaming_job,
                tmp_id: tmp_table_id.table_id,
                to_drop_state_table_ids: Vec::new(), /* the create streaming job command will not drop any state table */
                auto_refresh_schema_sinks: None,
                cdc_table_snapshot_split_assignment,
            });
        }

        // Here we need to consider:
        // - Shared source
        // - Table with connector
        // - MV on shared source
        let mut init_split_assignment = self
            .source_manager
            .allocate_splits(&stream_job_fragments)
            .await?;
        init_split_assignment.extend(
            self.source_manager
                .allocate_splits_for_backfill(
                    &stream_job_fragments,
                    &new_no_shuffle,
                    &upstream_actors,
                )
                .await?,
        );

        let cdc_table_snapshot_split_assignment = assign_cdc_table_snapshot_splits(
            iter::once(stream_job_fragments.deref()),
            self.env.meta_store_ref(),
        )
        .await?;

        let source_change = SourceChange::CreateJobFinished {
            finished_backfill_fragments: stream_job_fragments.source_backfill_fragments(),
        };

        let info = CreateStreamingJobCommandInfo {
            stream_job_fragments,
            upstream_fragment_downstreams,
            init_split_assignment,
            definition: definition.clone(),
            streaming_job: streaming_job.clone(),
            job_type,
            create_type,
            fragment_backfill_ordering,
            cdc_table_snapshot_split_assignment,
        };

        let job_type = if let Some(snapshot_backfill_info) = snapshot_backfill_info {
            tracing::debug!(
                ?snapshot_backfill_info,
                "sending Command::CreateSnapshotBackfillStreamingJob"
            );
            CreateStreamingJobType::SnapshotBackfill(snapshot_backfill_info)
        } else {
            tracing::debug!("sending Command::CreateStreamingJob");
            if let Some(replace_table_command) = replace_table_command {
                CreateStreamingJobType::SinkIntoTable(replace_table_command)
            } else {
                CreateStreamingJobType::Normal
            }
        };

        let command = Command::CreateStreamingJob {
            info,
            job_type,
            cross_db_snapshot_backfill_info,
        };

        self.barrier_scheduler
            .run_command(streaming_job.database_id().into(), command)
            .await?;

        tracing::debug!(?streaming_job, "first barrier collected for stream job");

        Ok((source_change, streaming_job))
    }

    /// Send replace job command to barrier scheduler.
    pub async fn replace_stream_job(
        &self,
        new_fragments: StreamJobFragmentsToCreate,
        ReplaceStreamJobContext {
            old_fragments,
            replace_upstream,
            new_no_shuffle,
            upstream_fragment_downstreams,
            tmp_id,
            streaming_job,
            drop_table_connector_ctx,
            auto_refresh_schema_sinks,
            ..
        }: ReplaceStreamJobContext,
    ) -> MetaResult<()> {
        let init_split_assignment = if streaming_job.is_source() {
            self.source_manager
                .allocate_splits_for_replace_source(
                    &new_fragments,
                    &replace_upstream,
                    &new_no_shuffle,
                )
                .await?
        } else {
            self.source_manager.allocate_splits(&new_fragments).await?
        };
        tracing::info!(
            "replace_stream_job - allocate split: {:?}",
            init_split_assignment
        );

        let cdc_table_snapshot_split_assignment =
            assign_cdc_table_snapshot_splits_for_replace_table(
                old_fragments.stream_job_id.table_id,
                &new_fragments.inner,
                self.env.meta_store_ref(),
            )
            .await?;

        self.barrier_scheduler
            .run_command(
                streaming_job.database_id().into(),
                Command::ReplaceStreamJob(ReplaceStreamJobPlan {
                    old_fragments,
                    new_fragments,
                    replace_upstream,
                    upstream_fragment_downstreams,
                    init_split_assignment,
                    streaming_job,
                    tmp_id,
                    to_drop_state_table_ids: {
                        if let Some(drop_table_connector_ctx) = &drop_table_connector_ctx {
                            vec![TableId::new(
                                drop_table_connector_ctx.to_remove_state_table_id as _,
                            )]
                        } else {
                            Vec::new()
                        }
                    },
                    auto_refresh_schema_sinks,
                    cdc_table_snapshot_split_assignment,
                }),
            )
            .await?;

        Ok(())
    }

    /// Drop streaming jobs by barrier manager, and clean up all related resources. The error will
    /// be ignored because the recovery process will take over it in cleaning part. Check
    /// [`Command::DropStreamingJobs`] for details.
    pub async fn drop_streaming_jobs(
        &self,
        database_id: DatabaseId,
        removed_actors: Vec<ActorId>,
        streaming_job_ids: Vec<ObjectId>,
        state_table_ids: Vec<risingwave_meta_model::TableId>,
        fragment_ids: HashSet<FragmentId>,
    ) {
        if !removed_actors.is_empty()
            || !streaming_job_ids.is_empty()
            || !state_table_ids.is_empty()
        {
            let res = self
                .barrier_scheduler
                .run_command(
                    database_id,
                    Command::DropStreamingJobs {
                        table_fragments_ids: streaming_job_ids
                            .iter()
                            .map(|job_id| TableId::new(*job_id as _))
                            .collect(),
                        actors: removed_actors,
                        unregistered_state_table_ids: state_table_ids
                            .iter()
                            .map(|table_id| TableId::new(*table_id as _))
                            .collect(),
                        unregistered_fragment_ids: fragment_ids,
                    },
                )
                .await
                .inspect_err(|err| {
                    tracing::error!(error = ?err.as_report(), "failed to run drop command");
                });
            if res.is_ok() {
                self.post_dropping_streaming_jobs(state_table_ids).await;
            }
        }
    }

    async fn post_dropping_streaming_jobs(
        &self,
        state_table_ids: Vec<risingwave_meta_model::TableId>,
    ) {
        let tables = self
            .metadata_manager
            .catalog_controller
            .complete_dropped_tables(state_table_ids.into_iter())
            .await;
        let objects = tables
            .into_iter()
            .map(|t| PbObject {
                object_info: Some(PbObjectInfo::Table(t)),
            })
            .collect();
        let group = PbInfo::ObjectGroup(PbObjectGroup { objects });
        self.env
            .notification_manager()
            .notify_hummock(Operation::Delete, group.clone())
            .await;
        self.env
            .notification_manager()
            .notify_compactor(Operation::Delete, group)
            .await;
    }

    /// Cancel streaming jobs and return the canceled table ids.
    /// 1. Send cancel message to stream jobs (via `cancel_jobs`).
    /// 2. Send cancel message to recovered stream jobs (via `barrier_scheduler`).
    ///
    /// Cleanup of their state will be cleaned up after the `CancelStreamJob` command succeeds,
    /// by the barrier manager for both of them.
    pub async fn cancel_streaming_jobs(&self, table_ids: Vec<TableId>) -> Vec<TableId> {
        if table_ids.is_empty() {
            return vec![];
        }

        let _reschedule_job_lock = self.reschedule_lock_read_guard().await;
        let (receivers, recovered_job_ids) = self.creating_job_info.cancel_jobs(table_ids).await;

        let futures = receivers.into_iter().map(|(id, receiver)| async move {
            if receiver.await.is_ok() {
                tracing::info!("canceled streaming job {id}");
                Some(id)
            } else {
                tracing::warn!("failed to cancel streaming job {id}");
                None
            }
        });
        let mut cancelled_ids = join_all(futures).await.into_iter().flatten().collect_vec();

        // NOTE(kwannoel): For recovered stream jobs, we can directly cancel them by running the barrier command,
        // since Barrier manager manages the recovered stream jobs.
        let futures = recovered_job_ids.into_iter().map(|id| async move {
            tracing::debug!(?id, "cancelling recovered streaming job");
            let result: MetaResult<()> = try {
                let fragment = self
                    .metadata_manager.get_job_fragments_by_id(&id)
                    .await?;
                if fragment.is_created() {
                    Err(MetaError::invalid_parameter(format!(
                        "streaming job {} is already created",
                        id
                    )))?;
                }

                let (_, database_id) = self.metadata_manager
                    .catalog_controller
                    .try_abort_creating_streaming_job(id.table_id as _, true)
                    .await?;

                if let Some(database_id) = database_id {
                    self.barrier_scheduler
                        .run_command(DatabaseId::new(database_id as _), Command::cancel(&fragment))
                        .await?;
                }
            };
            match result {
                Ok(_) => {
                    tracing::info!(?id, "cancelled recovered streaming job");
                    Some(id)
                }
                Err(err) => {
                    tracing::error!(error=?err.as_report(), "failed to cancel recovered streaming job {id}, does it correspond to any jobs in `SHOW JOBS`?");
                    None
                }
            }
        });
        let cancelled_recovered_ids = join_all(futures).await.into_iter().flatten().collect_vec();

        cancelled_ids.extend(cancelled_recovered_ids);
        cancelled_ids
    }

    pub(crate) async fn reschedule_streaming_job(
        &self,
        job_id: u32,
        target: JobRescheduleTarget,
        deferred: bool,
    ) -> MetaResult<()> {
        let _reschedule_job_lock = self.reschedule_lock_write_guard().await;
        let background_jobs = self
            .metadata_manager
            .list_background_creating_jobs()
            .await?;

        if !background_jobs.is_empty() {
            let related_jobs = self
                .scale_controller
                .resolve_related_no_shuffle_jobs(&background_jobs)
                .await?;

            for job in background_jobs {
                if related_jobs.contains(&job) {
                    bail!(
                        "Cannot alter the job {} because the related job {} is currently being created",
                        job_id,
                        job.table_id
                    );
                }
            }
        }

        let JobRescheduleTarget {
            parallelism: parallelism_change,
            resource_group: resource_group_change,
        } = target;

        let database_id = DatabaseId::new(
            self.metadata_manager
                .catalog_controller
                .get_object_database_id(job_id as ObjectId)
                .await? as _,
        );
        let job_id = TableId::new(job_id);

        let worker_nodes = self
            .metadata_manager
            .list_active_streaming_compute_nodes()
            .await?
            .into_iter()
            .filter(|w| w.is_streaming_schedulable())
            .collect_vec();

        // Check if the provided parallelism is valid.
        let available_parallelism = worker_nodes
            .iter()
            .map(|w| w.compute_node_parallelism())
            .sum::<usize>();
        let max_parallelism = self
            .metadata_manager
            .get_job_max_parallelism(job_id)
            .await?;

        if let JobParallelismTarget::Update(parallelism) = parallelism_change {
            match parallelism {
                TableParallelism::Adaptive => {
                    if available_parallelism > max_parallelism {
                        tracing::warn!(
                            "too many parallelism available, use max parallelism {} will be limited",
                            max_parallelism
                        );
                    }
                }
                TableParallelism::Fixed(parallelism) => {
                    if parallelism > max_parallelism {
                        bail_invalid_parameter!(
                            "specified parallelism {} should not exceed max parallelism {}",
                            parallelism,
                            max_parallelism
                        );
                    }
                }
                TableParallelism::Custom => {
                    bail_invalid_parameter!("should not alter parallelism to custom")
                }
            }
        }

        let table_parallelism_assignment = match &parallelism_change {
            JobParallelismTarget::Update(parallelism) => HashMap::from([(job_id, *parallelism)]),
            JobParallelismTarget::Refresh => HashMap::new(),
        };
        let resource_group_assignment = match &resource_group_change {
            JobResourceGroupTarget::Update(target) => {
                HashMap::from([(job_id.table_id() as ObjectId, target.clone())])
            }
            JobResourceGroupTarget::Keep => HashMap::new(),
        };

        if deferred {
            tracing::debug!(
                "deferred mode enabled for job {}, set the parallelism directly to parallelism {:?}, resource group {:?}",
                job_id,
                parallelism_change,
                resource_group_change,
            );
            self.scale_controller
                .post_apply_reschedule(
                    &HashMap::new(),
                    &JobReschedulePostUpdates {
                        parallelism_updates: table_parallelism_assignment,
                        resource_group_updates: resource_group_assignment,
                    },
                )
                .await?;
        } else {
            let reschedule_plan = self
                .scale_controller
                .generate_job_reschedule_plan(
                    JobReschedulePolicy {
                        targets: HashMap::from([(
                            job_id.table_id,
                            JobRescheduleTarget {
                                parallelism: parallelism_change,
                                resource_group: resource_group_change,
                            },
                        )]),
                    },
                    false,
                )
                .await?;

            if reschedule_plan.reschedules.is_empty() {
                tracing::debug!(
                    "empty reschedule plan generated for job {}, set the parallelism directly to {:?}",
                    job_id,
                    reschedule_plan.post_updates
                );
                self.scale_controller
                    .post_apply_reschedule(&HashMap::new(), &reschedule_plan.post_updates)
                    .await?;
            } else {
                self.reschedule_actors(
                    database_id,
                    reschedule_plan,
                    RescheduleOptions {
                        resolve_no_shuffle_upstream: false,
                        skip_create_new_actors: false,
                    },
                )
                .await?;
            }
        };

        Ok(())
    }

    /// This method is copied from `GlobalStreamManager::reschedule_streaming_job` and modified to handle reschedule CDC table backfill.
    pub(crate) async fn reschedule_cdc_table_backfill(
        &self,
        job_id: u32,
        target: JobRescheduleTarget,
    ) -> MetaResult<()> {
        let _reschedule_job_lock = self.reschedule_lock_write_guard().await;
        let JobRescheduleTarget {
            parallelism: parallelism_change,
            resource_group: resource_group_change,
        } = target;
        let database_id = DatabaseId::new(
            self.metadata_manager
                .catalog_controller
                .get_object_database_id(job_id as ObjectId)
                .await? as _,
        );
        let job_id = TableId::new(job_id);
        if let JobParallelismTarget::Update(parallelism) = &parallelism_change {
            match parallelism {
                TableParallelism::Fixed(_) => {}
                TableParallelism::Custom => {
                    bail_invalid_parameter!("should not alter parallelism to custom")
                }
                TableParallelism::Adaptive => {
                    bail_invalid_parameter!("should not alter parallelism to adaptive")
                }
            }
        } else {
            bail_invalid_parameter!("should not refresh")
        }
        match &resource_group_change {
            JobResourceGroupTarget::Update(_) => {
                bail_invalid_parameter!("should not update resource group")
            }
            JobResourceGroupTarget::Keep => {}
        };
        // Only generate reschedule for fragment of CDC table backfill.
        let reschedule_plan = self
            .scale_controller
            .generate_job_reschedule_plan(
                JobReschedulePolicy {
                    targets: HashMap::from([(
                        job_id.table_id,
                        JobRescheduleTarget {
                            parallelism: parallelism_change,
                            resource_group: resource_group_change,
                        },
                    )]),
                },
                true,
            )
            .await?;
        if reschedule_plan.reschedules.is_empty() {
            tracing::debug!(
                ?job_id,
                post_updates = ?reschedule_plan.post_updates,
                "Empty reschedule plan generated for job.",
            );
            self.scale_controller
                .post_apply_reschedule(&HashMap::new(), &reschedule_plan.post_updates)
                .await?;
        } else {
            self.reschedule_actors(
                database_id,
                reschedule_plan,
                RescheduleOptions {
                    resolve_no_shuffle_upstream: false,
                    skip_create_new_actors: false,
                },
            )
            .await?;
        }

        Ok(())
    }

    // Don't need to add actor, just send a command
    pub async fn create_subscription(
        self: &Arc<Self>,
        subscription: &Subscription,
    ) -> MetaResult<()> {
        let command = Command::CreateSubscription {
            subscription_id: subscription.id,
            upstream_mv_table_id: TableId::new(subscription.dependent_table_id),
            retention_second: subscription.retention_seconds,
        };

        tracing::debug!("sending Command::CreateSubscription");
        self.barrier_scheduler
            .run_command(subscription.database_id.into(), command)
            .await?;
        Ok(())
    }

    // Don't need to add actor, just send a command
    pub async fn drop_subscription(
        self: &Arc<Self>,
        database_id: DatabaseId,
        subscription_id: u32,
        table_id: u32,
    ) {
        let command = Command::DropSubscription {
            subscription_id,
            upstream_mv_table_id: TableId::new(table_id),
        };

        tracing::debug!("sending Command::DropSubscriptions");
        let _ = self
            .barrier_scheduler
            .run_command(database_id, command)
            .await
            .inspect_err(|err| {
                tracing::error!(error = ?err.as_report(), "failed to run drop command");
            });
    }
}
