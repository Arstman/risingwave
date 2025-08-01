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

mod barrier_control;
mod status;

use std::cmp::max;
use std::collections::{HashMap, HashSet};
use std::mem::take;
use std::ops::Bound::{Excluded, Unbounded};

use barrier_control::CreatingStreamingJobBarrierControl;
use risingwave_common::catalog::{DatabaseId, TableId};
use risingwave_common::metrics::LabelGuardedIntGauge;
use risingwave_common::util::epoch::Epoch;
use risingwave_connector::source::cdc::build_pb_actor_cdc_table_snapshot_splits;
use risingwave_meta_model::{CreateType, WorkerId};
use risingwave_pb::ddl_service::DdlProgress;
use risingwave_pb::hummock::HummockVersionStats;
use risingwave_pb::stream_plan::AddMutation;
use risingwave_pb::stream_plan::barrier::PbBarrierKind;
use risingwave_pb::stream_plan::barrier_mutation::Mutation;
use risingwave_pb::stream_service::BarrierCompleteResponse;
use status::CreatingStreamingJobStatus;
use tracing::{debug, info};

use crate::MetaResult;
use crate::barrier::backfill_order_control::get_nodes_with_backfill_dependencies;
use crate::barrier::checkpoint::creating_job::status::CreateMviewLogStoreProgressTracker;
use crate::barrier::edge_builder::FragmentEdgeBuildResult;
use crate::barrier::info::{BarrierInfo, InflightStreamingJobInfo};
use crate::barrier::progress::CreateMviewProgressTracker;
use crate::barrier::rpc::ControlStreamManager;
use crate::barrier::{
    BackfillOrderState, BarrierKind, Command, CreateStreamingJobCommandInfo, TracedEpoch,
};
use crate::controller::fragment::InflightFragmentInfo;
use crate::model::{StreamJobActorsToCreate, StreamJobFragments};
use crate::rpc::metrics::GLOBAL_META_METRICS;
use crate::stream::build_actor_connector_splits;

#[derive(Debug)]
pub(crate) struct CreatingStreamingJobControl {
    database_id: DatabaseId,
    pub(super) job_id: TableId,
    definition: String,
    create_type: CreateType,
    pub(super) snapshot_backfill_upstream_tables: HashSet<TableId>,
    backfill_epoch: u64,

    graph_info: InflightStreamingJobInfo,

    barrier_control: CreatingStreamingJobBarrierControl,
    status: CreatingStreamingJobStatus,

    upstream_lag: LabelGuardedIntGauge,
}

impl CreatingStreamingJobControl {
    pub(super) fn new(
        info: &CreateStreamingJobCommandInfo,
        snapshot_backfill_upstream_tables: HashSet<TableId>,
        backfill_epoch: u64,
        version_stat: &HummockVersionStats,
        control_stream_manager: &mut ControlStreamManager,
        edges: &mut FragmentEdgeBuildResult,
    ) -> MetaResult<Self> {
        let job_id = info.stream_job_fragments.stream_job_id();
        let database_id = DatabaseId::new(info.streaming_job.database_id());
        debug!(
            %job_id,
            definition = info.definition,
            "new creating job"
        );
        let snapshot_backfill_actors = info.stream_job_fragments.snapshot_backfill_actor_ids();
        let backfill_nodes_to_pause =
            get_nodes_with_backfill_dependencies(&info.fragment_backfill_ordering)
                .into_iter()
                .collect();
        let backfill_order_state = BackfillOrderState::new(
            info.fragment_backfill_ordering.clone(),
            &info.stream_job_fragments,
        );
        let create_mview_tracker = CreateMviewProgressTracker::recover(
            [(
                job_id,
                (
                    info.definition.clone(),
                    &*info.stream_job_fragments,
                    backfill_order_state,
                ),
            )],
            version_stat,
        );
        let fragment_infos: HashMap<_, _> = info.stream_job_fragments.new_fragment_info().collect();

        let actors_to_create =
            edges.collect_actors_to_create(info.stream_job_fragments.actors_to_create());

        let graph_info = InflightStreamingJobInfo {
            job_id,
            fragment_infos,
        };

        let mut barrier_control =
            CreatingStreamingJobBarrierControl::new(job_id, backfill_epoch, false);

        let mut prev_epoch_fake_physical_time = 0;
        let mut pending_non_checkpoint_barriers = vec![];

        let initial_barrier_info = CreatingStreamingJobStatus::new_fake_barrier(
            &mut prev_epoch_fake_physical_time,
            &mut pending_non_checkpoint_barriers,
            PbBarrierKind::Checkpoint,
        );

        let added_actors = info.stream_job_fragments.actor_ids();
        let actor_splits = info
            .init_split_assignment
            .values()
            .flat_map(build_actor_connector_splits)
            .collect();

        let initial_mutation = Mutation::Add(AddMutation {
            // for mutation of snapshot backfill job, we won't include changes to dispatchers of upstream actors.
            actor_dispatchers: Default::default(),
            added_actors,
            actor_splits,
            // we assume that when handling snapshot backfill, the cluster must not be paused
            pause: false,
            subscriptions_to_add: Default::default(),
            backfill_nodes_to_pause,
            actor_cdc_table_snapshot_splits: build_pb_actor_cdc_table_snapshot_splits(
                info.cdc_table_snapshot_split_assignment.clone(),
            ),
        });

        control_stream_manager.add_partial_graph(database_id, Some(job_id));
        Self::inject_barrier(
            database_id,
            job_id,
            control_stream_manager,
            &mut barrier_control,
            &graph_info,
            Some(&graph_info),
            initial_barrier_info,
            Some(actors_to_create),
            Some(initial_mutation),
        )?;

        assert!(pending_non_checkpoint_barriers.is_empty());

        Ok(Self {
            database_id,
            definition: info.definition.clone(),
            create_type: info.create_type.into(),
            job_id,
            snapshot_backfill_upstream_tables,
            barrier_control,
            backfill_epoch,
            graph_info,
            status: CreatingStreamingJobStatus::ConsumingSnapshot {
                prev_epoch_fake_physical_time,
                pending_upstream_barriers: vec![],
                version_stats: version_stat.clone(),
                create_mview_tracker,
                snapshot_backfill_actors,
                backfill_epoch,
                pending_non_checkpoint_barriers,
            },
            upstream_lag: GLOBAL_META_METRICS
                .snapshot_backfill_lag
                .with_guarded_label_values(&[&format!("{}", job_id)]),
        })
    }

    fn resolve_upstream_log_epochs(
        snapshot_backfill_upstream_tables: &HashSet<TableId>,
        upstream_table_log_epochs: &HashMap<TableId, Vec<(Vec<u64>, u64)>>,
        committed_epoch: u64,
        upstream_curr_epoch: u64,
    ) -> MetaResult<Vec<BarrierInfo>> {
        // TODO: add sanity check
        let table_id = snapshot_backfill_upstream_tables.iter().next().unwrap();
        let mut epochs_iter = upstream_table_log_epochs[table_id].iter();
        loop {
            let (_, checkpoint_epoch) = epochs_iter.next().expect("not reach committed epoch yet");
            if *checkpoint_epoch < committed_epoch {
                continue;
            }
            assert_eq!(*checkpoint_epoch, committed_epoch);
            break;
        }
        let mut ret = vec![];
        let mut prev_epoch = committed_epoch;
        let mut pending_non_checkpoint_barriers = vec![];
        for (non_checkpoint_epochs, checkpoint_epoch) in epochs_iter {
            for (i, epoch) in non_checkpoint_epochs
                .iter()
                .chain([checkpoint_epoch])
                .enumerate()
            {
                assert!(*epoch > prev_epoch);
                pending_non_checkpoint_barriers.push(prev_epoch);
                ret.push(BarrierInfo {
                    prev_epoch: TracedEpoch::new(Epoch(prev_epoch)),
                    curr_epoch: TracedEpoch::new(Epoch(*epoch)),
                    kind: if i == 0 {
                        BarrierKind::Checkpoint(take(&mut pending_non_checkpoint_barriers))
                    } else {
                        BarrierKind::Barrier
                    },
                });
                prev_epoch = *epoch;
            }
        }
        ret.push(BarrierInfo {
            prev_epoch: TracedEpoch::new(Epoch(prev_epoch)),
            curr_epoch: TracedEpoch::new(Epoch(upstream_curr_epoch)),
            kind: BarrierKind::Checkpoint(pending_non_checkpoint_barriers),
        });
        Ok(ret)
    }

    fn recover_consuming_snapshot(
        job_id: TableId,
        definition: &String,
        snapshot_backfill_upstream_tables: &HashSet<TableId>,
        upstream_table_log_epochs: &HashMap<TableId, Vec<(Vec<u64>, u64)>>,
        backfill_epoch: u64,
        committed_epoch: u64,
        upstream_curr_epoch: u64,
        stream_job_fragments: StreamJobFragments,
        version_stat: &HummockVersionStats,
    ) -> MetaResult<(CreatingStreamingJobStatus, BarrierInfo)> {
        let snapshot_backfill_actors = stream_job_fragments.snapshot_backfill_actor_ids();
        let mut prev_epoch_fake_physical_time = Epoch(committed_epoch).physical_time();
        let mut pending_non_checkpoint_barriers = vec![];
        let create_mview_tracker = CreateMviewProgressTracker::recover(
            [(
                job_id,
                (
                    definition.clone(),
                    &stream_job_fragments,
                    Default::default(),
                ),
            )],
            version_stat,
        );
        let barrier_info = CreatingStreamingJobStatus::new_fake_barrier(
            &mut prev_epoch_fake_physical_time,
            &mut pending_non_checkpoint_barriers,
            PbBarrierKind::Initial,
        );
        Ok((
            CreatingStreamingJobStatus::ConsumingSnapshot {
                prev_epoch_fake_physical_time,
                pending_upstream_barriers: Self::resolve_upstream_log_epochs(
                    snapshot_backfill_upstream_tables,
                    upstream_table_log_epochs,
                    backfill_epoch,
                    upstream_curr_epoch,
                )?,
                version_stats: version_stat.clone(),
                create_mview_tracker,
                snapshot_backfill_actors,
                backfill_epoch,
                pending_non_checkpoint_barriers,
            },
            barrier_info,
        ))
    }

    fn recover_consuming_log_store(
        snapshot_backfill_upstream_tables: &HashSet<TableId>,
        upstream_table_log_epochs: &HashMap<TableId, Vec<(Vec<u64>, u64)>>,
        committed_epoch: u64,
        upstream_curr_epoch: u64,
        stream_job_fragments: StreamJobFragments,
    ) -> MetaResult<(CreatingStreamingJobStatus, BarrierInfo)> {
        let snapshot_backfill_actors = stream_job_fragments.snapshot_backfill_actor_ids();
        let mut barriers_to_inject = Self::resolve_upstream_log_epochs(
            snapshot_backfill_upstream_tables,
            upstream_table_log_epochs,
            committed_epoch,
            upstream_curr_epoch,
        )?;
        let mut first_barrier = barriers_to_inject.remove(0);
        assert!(first_barrier.kind.is_checkpoint());
        first_barrier.kind = BarrierKind::Initial;
        Ok((
            CreatingStreamingJobStatus::ConsumingLogStore {
                log_store_progress_tracker: CreateMviewLogStoreProgressTracker::new(
                    snapshot_backfill_actors.into_iter(),
                    barriers_to_inject
                        .last()
                        .map(|info| info.prev_epoch() - committed_epoch)
                        .unwrap_or(0),
                ),
                barriers_to_inject: Some(barriers_to_inject),
            },
            first_barrier,
        ))
    }

    #[expect(clippy::too_many_arguments)]
    pub(crate) fn recover(
        database_id: DatabaseId,
        job_id: TableId,
        definition: String,
        snapshot_backfill_upstream_tables: HashSet<TableId>,
        upstream_table_log_epochs: &HashMap<TableId, Vec<(Vec<u64>, u64)>>,
        backfill_epoch: u64,
        committed_epoch: u64,
        upstream_curr_epoch: u64,
        graph_info: InflightStreamingJobInfo,
        stream_job_fragments: StreamJobFragments,
        version_stat: &HummockVersionStats,
        new_actors: StreamJobActorsToCreate,
        initial_mutation: Mutation,
        control_stream_manager: &mut ControlStreamManager,
    ) -> MetaResult<Self> {
        debug!(
            %job_id,
            definition,
            "recovered creating job"
        );
        let mut barrier_control =
            CreatingStreamingJobBarrierControl::new(job_id, backfill_epoch, true);

        let (status, first_barrier_info) = if committed_epoch < backfill_epoch {
            Self::recover_consuming_snapshot(
                job_id,
                &definition,
                &snapshot_backfill_upstream_tables,
                upstream_table_log_epochs,
                backfill_epoch,
                committed_epoch,
                upstream_curr_epoch,
                stream_job_fragments,
                version_stat,
            )?
        } else {
            Self::recover_consuming_log_store(
                &snapshot_backfill_upstream_tables,
                upstream_table_log_epochs,
                committed_epoch,
                upstream_curr_epoch,
                stream_job_fragments,
            )?
        };
        control_stream_manager.add_partial_graph(database_id, Some(job_id));
        Self::inject_barrier(
            database_id,
            job_id,
            control_stream_manager,
            &mut barrier_control,
            &graph_info,
            Some(&graph_info),
            first_barrier_info,
            Some(new_actors),
            Some(initial_mutation),
        )?;
        Ok(Self {
            database_id,
            job_id,
            definition,
            create_type: CreateType::Background,
            snapshot_backfill_upstream_tables,
            backfill_epoch,
            graph_info,
            barrier_control,
            status,
            upstream_lag: GLOBAL_META_METRICS
                .snapshot_backfill_lag
                .with_guarded_label_values(&[&format!("{}", job_id)]),
        })
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.barrier_control.is_empty()
    }

    pub(crate) fn is_valid_after_worker_err(&mut self, worker_id: WorkerId) -> bool {
        self.barrier_control.is_valid_after_worker_err(worker_id)
            && (!self.status.is_finishing()
                || InflightFragmentInfo::contains_worker(
                    self.graph_info.fragment_infos(),
                    worker_id,
                ))
    }

    pub(crate) fn gen_ddl_progress(&self) -> DdlProgress {
        let progress = match &self.status {
            CreatingStreamingJobStatus::ConsumingSnapshot {
                create_mview_tracker,
                ..
            } => {
                if create_mview_tracker.has_pending_finished_jobs() {
                    "Snapshot finished".to_owned()
                } else {
                    let progress = create_mview_tracker
                        .gen_ddl_progress()
                        .remove(&self.job_id.table_id)
                        .expect("should exist");
                    format!("Snapshot [{}]", progress.progress)
                }
            }
            CreatingStreamingJobStatus::ConsumingLogStore {
                log_store_progress_tracker,
                ..
            } => {
                format!(
                    "LogStore [{}]",
                    log_store_progress_tracker.gen_ddl_progress()
                )
            }
            CreatingStreamingJobStatus::Finishing(_) => {
                format!(
                    "Finishing [epoch count: {}]",
                    self.barrier_control.inflight_barrier_count()
                )
            }
        };
        DdlProgress {
            id: self.job_id.table_id as u64,
            statement: self.definition.clone(),
            create_type: self.create_type.as_str().to_owned(),
            progress,
        }
    }

    pub(super) fn pinned_upstream_log_epoch(&self) -> Option<u64> {
        if self.status.is_finishing() {
            None
        } else {
            // TODO: when supporting recoverable snapshot backfill, we should use the max epoch that has committed
            Some(max(
                self.barrier_control.max_collected_epoch().unwrap_or(0),
                self.backfill_epoch,
            ))
        }
    }

    fn inject_barrier(
        database_id: DatabaseId,
        table_id: TableId,
        control_stream_manager: &mut ControlStreamManager,
        barrier_control: &mut CreatingStreamingJobBarrierControl,
        pre_applied_graph_info: &InflightStreamingJobInfo,
        applied_graph_info: Option<&InflightStreamingJobInfo>,
        barrier_info: BarrierInfo,
        new_actors: Option<StreamJobActorsToCreate>,
        mutation: Option<Mutation>,
    ) -> MetaResult<()> {
        let node_to_collect = control_stream_manager.inject_barrier(
            database_id,
            Some(table_id),
            mutation,
            &barrier_info,
            pre_applied_graph_info.fragment_infos(),
            applied_graph_info
                .map(|graph_info| graph_info.fragment_infos())
                .into_iter()
                .flatten(),
            new_actors,
            vec![],
            vec![],
        )?;
        barrier_control.enqueue_epoch(
            barrier_info.prev_epoch(),
            node_to_collect,
            barrier_info.kind.clone(),
        );
        Ok(())
    }

    pub(super) fn on_new_command(
        &mut self,
        control_stream_manager: &mut ControlStreamManager,
        command: Option<&Command>,
        barrier_info: &BarrierInfo,
    ) -> MetaResult<()> {
        let table_id = self.job_id;
        let start_consume_upstream =
            if let Some(Command::MergeSnapshotBackfillStreamingJobs(jobs_to_merge)) = command {
                jobs_to_merge.contains_key(&table_id)
            } else {
                false
            };
        if start_consume_upstream {
            info!(
                table_id = self.job_id.table_id,
                prev_epoch = barrier_info.prev_epoch(),
                "start consuming upstream"
            );
        }
        let progress_epoch =
            if let Some(max_collected_epoch) = self.barrier_control.max_collected_epoch() {
                max(max_collected_epoch, self.backfill_epoch)
            } else {
                self.backfill_epoch
            };
        self.upstream_lag.set(
            barrier_info
                .prev_epoch
                .value()
                .0
                .saturating_sub(progress_epoch) as _,
        );
        if start_consume_upstream {
            self.status.start_consume_upstream(barrier_info);
            Self::inject_barrier(
                self.database_id,
                self.job_id,
                control_stream_manager,
                &mut self.barrier_control,
                &self.graph_info,
                None,
                barrier_info.clone(),
                None,
                None,
            )?;
        } else {
            for (barrier_to_inject, mutation) in self.status.on_new_upstream_epoch(barrier_info) {
                Self::inject_barrier(
                    self.database_id,
                    self.job_id,
                    control_stream_manager,
                    &mut self.barrier_control,
                    &self.graph_info,
                    Some(&self.graph_info),
                    barrier_to_inject,
                    None,
                    mutation,
                )?;
            }
        }
        Ok(())
    }

    pub(crate) fn collect(&mut self, resp: BarrierCompleteResponse) -> bool {
        self.status.update_progress(&resp.create_mview_progress);
        self.barrier_control.collect(resp);
        self.should_merge_to_upstream().is_some()
    }

    pub(super) fn should_merge_to_upstream(&self) -> Option<&InflightStreamingJobInfo> {
        if let CreatingStreamingJobStatus::ConsumingLogStore {
            log_store_progress_tracker,
            barriers_to_inject,
        } = &self.status
            && barriers_to_inject.is_none()
            && log_store_progress_tracker.is_finished()
        {
            Some(&self.graph_info)
        } else {
            None
        }
    }
}

pub(super) enum CompleteJobType {
    /// The first barrier
    First,
    Normal,
    /// The last barrier to complete
    Finished,
}

impl CreatingStreamingJobControl {
    pub(super) fn start_completing(
        &mut self,
        min_upstream_inflight_epoch: Option<u64>,
    ) -> Option<(u64, Vec<BarrierCompleteResponse>, CompleteJobType)> {
        let (finished_at_epoch, epoch_end_bound) = match &self.status {
            CreatingStreamingJobStatus::Finishing(finish_at_epoch) => {
                let epoch_end_bound = min_upstream_inflight_epoch
                    .map(|upstream_epoch| {
                        if upstream_epoch < *finish_at_epoch {
                            Excluded(upstream_epoch)
                        } else {
                            Unbounded
                        }
                    })
                    .unwrap_or(Unbounded);
                (Some(*finish_at_epoch), epoch_end_bound)
            }
            CreatingStreamingJobStatus::ConsumingSnapshot { .. }
            | CreatingStreamingJobStatus::ConsumingLogStore { .. } => (
                None,
                min_upstream_inflight_epoch
                    .map(Excluded)
                    .unwrap_or(Unbounded),
            ),
        };
        self.barrier_control.start_completing(epoch_end_bound).map(
            |(epoch, resps, is_first_commit)| {
                let status = if let Some(finish_at_epoch) = finished_at_epoch {
                    assert!(!is_first_commit);
                    if epoch == finish_at_epoch {
                        self.barrier_control.ack_completed(epoch);
                        assert!(self.barrier_control.is_empty());
                        CompleteJobType::Finished
                    } else {
                        CompleteJobType::Normal
                    }
                } else if is_first_commit {
                    CompleteJobType::First
                } else {
                    CompleteJobType::Normal
                };
                (epoch, resps, status)
            },
        )
    }

    pub(super) fn ack_completed(&mut self, completed_epoch: u64) {
        self.barrier_control.ack_completed(completed_epoch);
    }

    pub(super) fn is_finished(&self) -> bool {
        self.barrier_control.is_empty() && self.status.is_finishing()
    }

    pub fn is_consuming(&self) -> bool {
        match &self.status {
            CreatingStreamingJobStatus::ConsumingSnapshot { .. }
            | CreatingStreamingJobStatus::ConsumingLogStore { .. } => true,
            CreatingStreamingJobStatus::Finishing(_) => false,
        }
    }

    pub fn state_table_ids(&self) -> impl Iterator<Item = TableId> + '_ {
        self.graph_info.existing_table_ids()
    }

    pub fn graph_info(&self) -> &InflightStreamingJobInfo {
        &self.graph_info
    }
}
