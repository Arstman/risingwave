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

use std::collections::HashMap;
use std::time::Duration;

use anyhow::anyhow;
use either::Either;
use itertools::Itertools;
use prometheus::core::{AtomicU64, GenericCounter};
use risingwave_common::array::ArrayRef;
use risingwave_common::catalog::{ColumnId, TableId};
use risingwave_common::metrics::{GLOBAL_ERROR_METRICS, LabelGuardedMetric};
use risingwave_common::system_param::local_manager::SystemParamsReaderRef;
use risingwave_common::system_param::reader::SystemParamsRead;
use risingwave_common::util::epoch::{Epoch, EpochPair};
use risingwave_connector::parser::schema_change::SchemaChangeEnvelope;
use risingwave_connector::source::reader::desc::{SourceDesc, SourceDescBuilder};
use risingwave_connector::source::reader::reader::SourceReader;
use risingwave_connector::source::{
    ConnectorState, SourceContext, SourceCtrlOpts, SplitId, SplitImpl, SplitMetaData,
    StreamChunkWithState, WaitCheckpointTask,
};
use risingwave_hummock_sdk::HummockReadEpoch;
use risingwave_storage::store::TryWaitEpochOptions;
use thiserror_ext::AsReport;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;

use super::executor_core::StreamSourceCore;
use super::{barrier_to_message_stream, get_split_offset_col_idx, prune_additional_cols};
use crate::common::rate_limit::limited_chunk_size;
use crate::executor::UpdateMutation;
use crate::executor::prelude::*;
use crate::executor::source::reader_stream::StreamReaderBuilder;
use crate::executor::stream_reader::StreamReaderWithPause;

/// A constant to multiply when calculating the maximum time to wait for a barrier. This is due to
/// some latencies in network and cost in meta.
pub const WAIT_BARRIER_MULTIPLE_TIMES: u128 = 5;

pub struct SourceExecutor<S: StateStore> {
    actor_ctx: ActorContextRef,

    /// Streaming source for external
    stream_source_core: StreamSourceCore<S>,

    /// Metrics for monitor.
    metrics: Arc<StreamingMetrics>,

    /// Receiver of barrier channel.
    barrier_receiver: Option<UnboundedReceiver<Barrier>>,

    /// System parameter reader to read barrier interval
    system_params: SystemParamsReaderRef,

    /// Rate limit in rows/s.
    rate_limit_rps: Option<u32>,

    is_shared_non_cdc: bool,
}

impl<S: StateStore> SourceExecutor<S> {
    pub fn new(
        actor_ctx: ActorContextRef,
        stream_source_core: StreamSourceCore<S>,
        metrics: Arc<StreamingMetrics>,
        barrier_receiver: UnboundedReceiver<Barrier>,
        system_params: SystemParamsReaderRef,
        rate_limit_rps: Option<u32>,
        is_shared_non_cdc: bool,
    ) -> Self {
        Self {
            actor_ctx,
            stream_source_core,
            metrics,
            barrier_receiver: Some(barrier_receiver),
            system_params,
            rate_limit_rps,
            is_shared_non_cdc,
        }
    }

    fn stream_reader_builder(&self, source_desc: SourceDesc) -> StreamReaderBuilder {
        StreamReaderBuilder {
            source_desc,
            rate_limit: self.rate_limit_rps,
            source_id: self.stream_source_core.source_id,
            source_name: self.stream_source_core.source_name.clone(),
            is_auto_schema_change_enable: self.is_auto_schema_change_enable(),
            actor_ctx: self.actor_ctx.clone(),
            reader_stream: None,
        }
    }

    async fn spawn_wait_checkpoint_worker(
        core: &StreamSourceCore<S>,
        source_reader: SourceReader,
    ) -> StreamExecutorResult<Option<WaitCheckpointTaskBuilder>> {
        let Some(initial_task) = source_reader.create_wait_checkpoint_task().await? else {
            return Ok(None);
        };
        let (wait_checkpoint_tx, wait_checkpoint_rx) = mpsc::unbounded_channel();
        let wait_checkpoint_worker = WaitCheckpointWorker {
            wait_checkpoint_rx,
            state_store: core.split_state_store.state_table().state_store().clone(),
            table_id: core.split_state_store.state_table().table_id().into(),
        };
        tokio::spawn(wait_checkpoint_worker.run());
        Ok(Some(WaitCheckpointTaskBuilder {
            wait_checkpoint_tx,
            source_reader,
            building_task: initial_task,
        }))
    }

    /// build the source column ids and the source context which will be used to build the source stream
    pub fn prepare_source_stream_build(
        &self,
        source_desc: &SourceDesc,
    ) -> (Vec<ColumnId>, SourceContext) {
        let column_ids = source_desc
            .columns
            .iter()
            .map(|column_desc| column_desc.column_id)
            .collect_vec();

        let (schema_change_tx, mut schema_change_rx) =
            mpsc::channel::<(SchemaChangeEnvelope, oneshot::Sender<()>)>(16);
        let schema_change_tx = if self.is_auto_schema_change_enable() {
            let meta_client = self.actor_ctx.meta_client.clone();
            // spawn a task to handle schema change event from source parser
            let _join_handle = tokio::task::spawn(async move {
                while let Some((schema_change, finish_tx)) = schema_change_rx.recv().await {
                    let table_ids = schema_change.table_ids();
                    tracing::info!(
                        target: "auto_schema_change",
                        "recv a schema change event for tables: {:?}", table_ids);
                    // TODO: retry on rpc error
                    if let Some(ref meta_client) = meta_client {
                        match meta_client
                            .auto_schema_change(schema_change.to_protobuf())
                            .await
                        {
                            Ok(_) => {
                                tracing::info!(
                                    target: "auto_schema_change",
                                    "schema change success for tables: {:?}", table_ids);
                                finish_tx.send(()).unwrap();
                            }
                            Err(e) => {
                                tracing::error!(
                                    target: "auto_schema_change",
                                    error = ?e.as_report(), "schema change error");
                                finish_tx.send(()).unwrap();
                            }
                        }
                    }
                }
            });
            Some(schema_change_tx)
        } else {
            info!("auto schema change is disabled in config");
            None
        };

        let source_ctx = SourceContext::new(
            self.actor_ctx.id,
            self.stream_source_core.source_id,
            self.actor_ctx.fragment_id,
            self.stream_source_core.source_name.clone(),
            source_desc.metrics.clone(),
            SourceCtrlOpts {
                chunk_size: limited_chunk_size(self.rate_limit_rps),
                split_txn: self.rate_limit_rps.is_some(), // when rate limiting, we may split txn
            },
            source_desc.source.config.clone(),
            schema_change_tx,
        );

        (column_ids, source_ctx)
    }

    fn is_auto_schema_change_enable(&self) -> bool {
        self.actor_ctx
            .streaming_config
            .developer
            .enable_auto_schema_change
    }

    /// `source_id | source_name | actor_id | fragment_id`
    #[inline]
    fn get_metric_labels(&self) -> [String; 4] {
        [
            self.stream_source_core.source_id.to_string(),
            self.stream_source_core.source_name.clone(),
            self.actor_ctx.id.to_string(),
            self.actor_ctx.fragment_id.to_string(),
        ]
    }

    /// - `should_trim_state`: whether to trim state for dropped splits.
    ///
    ///   For scaling, the connector splits can be migrated to other actors, but
    ///   won't be added or removed. Actors should not trim states for splits that
    ///   are moved to other actors.
    ///
    ///   For source split change, split will not be migrated and we can trim states
    ///   for deleted splits.
    async fn apply_split_change_after_yield_barrier<const BIASED: bool>(
        &mut self,
        barrier_epoch: EpochPair,
        source_desc: &SourceDesc,
        stream: &mut StreamReaderWithPause<BIASED, StreamChunkWithState>,
        apply_mutation: ApplyMutationAfterBarrier<'_>,
    ) -> StreamExecutorResult<()> {
        {
            let mut should_rebuild_stream = false;
            match apply_mutation {
                ApplyMutationAfterBarrier::SplitChange {
                    target_splits,
                    should_trim_state,
                    split_change_count,
                } => {
                    split_change_count.inc();
                    if self
                        .update_state_if_changed(barrier_epoch, target_splits, should_trim_state)
                        .await?
                    {
                        should_rebuild_stream = true;
                    }
                }
                ApplyMutationAfterBarrier::ConnectorPropsChange => {
                    should_rebuild_stream = true;
                }
            }

            if should_rebuild_stream {
                self.rebuild_stream_reader(source_desc, stream)?;
            }
        }

        Ok(())
    }

    /// Returns `true` if split changed. Otherwise `false`.
    async fn update_state_if_changed(
        &mut self,
        barrier_epoch: EpochPair,
        target_splits: Vec<SplitImpl>,
        should_trim_state: bool,
    ) -> StreamExecutorResult<bool> {
        let core = &mut self.stream_source_core;

        let target_splits: HashMap<_, _> = target_splits
            .into_iter()
            .map(|split| (split.id(), split))
            .collect();

        let mut target_state: HashMap<SplitId, SplitImpl> =
            HashMap::with_capacity(target_splits.len());

        let mut split_changed = false;

        let committed_reader = core
            .split_state_store
            .new_committed_reader(barrier_epoch)
            .await?;

        // Checks added splits
        for (split_id, split) in target_splits {
            if let Some(s) = core.latest_split_info.get(&split_id) {
                // For existing splits, we should use the latest offset from the cache.
                // `target_splits` is from meta and contains the initial offset.
                target_state.insert(split_id, s.clone());
            } else {
                split_changed = true;
                // write new assigned split to state cache. snapshot is base on cache.

                let initial_state = if let Some(recover_state) = committed_reader
                    .try_recover_from_state_store(&split)
                    .await?
                {
                    recover_state
                } else {
                    split
                };

                core.updated_splits_in_epoch
                    .entry(split_id.clone())
                    .or_insert_with(|| initial_state.clone());

                target_state.insert(split_id, initial_state);
            }
        }

        // Checks dropped splits
        for existing_split_id in core.latest_split_info.keys() {
            if !target_state.contains_key(existing_split_id) {
                tracing::info!("split dropping detected: {}", existing_split_id);
                split_changed = true;
            }
        }

        if split_changed {
            tracing::info!(
                actor_id = self.actor_ctx.id,
                state = ?target_state,
                "apply split change"
            );

            core.updated_splits_in_epoch
                .retain(|split_id, _| target_state.contains_key(split_id));

            let dropped_splits = core
                .latest_split_info
                .extract_if(|split_id, _| !target_state.contains_key(split_id))
                .map(|(_, split)| split)
                .collect_vec();

            if should_trim_state && !dropped_splits.is_empty() {
                // trim dropped splits' state
                core.split_state_store.trim_state(&dropped_splits).await?;
            }

            core.latest_split_info = target_state;
        }

        Ok(split_changed)
    }

    /// Rebuild stream if there is a err in stream
    fn rebuild_stream_reader_from_error<const BIASED: bool>(
        &mut self,
        source_desc: &SourceDesc,
        stream: &mut StreamReaderWithPause<BIASED, StreamChunkWithState>,
        e: StreamExecutorError,
    ) -> StreamExecutorResult<()> {
        let core = &mut self.stream_source_core;
        tracing::warn!(
            error = ?e.as_report(),
            actor_id = self.actor_ctx.id,
            source_id = %core.source_id,
            "stream source reader error",
        );
        GLOBAL_ERROR_METRICS.user_source_error.report([
            e.variant_name().to_owned(),
            core.source_id.to_string(),
            core.source_name.to_owned(),
            self.actor_ctx.fragment_id.to_string(),
        ]);

        self.rebuild_stream_reader(source_desc, stream)
    }

    fn rebuild_stream_reader<const BIASED: bool>(
        &mut self,
        source_desc: &SourceDesc,
        stream: &mut StreamReaderWithPause<BIASED, StreamChunkWithState>,
    ) -> StreamExecutorResult<()> {
        let core = &mut self.stream_source_core;
        let target_state: Vec<SplitImpl> = core.latest_split_info.values().cloned().collect();

        tracing::info!(
            "actor {:?} apply source split change to {:?}",
            self.actor_ctx.id,
            target_state
        );

        // Replace the source reader with a new one of the new state.
        let reader_stream_builder = self.stream_reader_builder(source_desc.clone());
        let reader_stream =
            reader_stream_builder.into_retry_stream(Some(target_state.clone()), false);

        stream.replace_data_stream(reader_stream);

        Ok(())
    }

    async fn persist_state_and_clear_cache(
        &mut self,
        epoch: EpochPair,
    ) -> StreamExecutorResult<HashMap<SplitId, SplitImpl>> {
        let core = &mut self.stream_source_core;

        let cache = core
            .updated_splits_in_epoch
            .values()
            .map(|split_impl| split_impl.to_owned())
            .collect_vec();

        if !cache.is_empty() {
            tracing::debug!(state = ?cache, "take snapshot");
            core.split_state_store.set_states(cache).await?;
        }

        // commit anyway, even if no message saved
        core.split_state_store.commit(epoch).await?;

        let updated_splits = core.updated_splits_in_epoch.clone();

        core.updated_splits_in_epoch.clear();

        Ok(updated_splits)
    }

    /// try mem table spill
    async fn try_flush_data(&mut self) -> StreamExecutorResult<()> {
        let core = &mut self.stream_source_core;
        core.split_state_store.try_flush().await?;

        Ok(())
    }

    /// A source executor with a stream source receives:
    /// 1. Barrier messages
    /// 2. Data from external source
    /// and acts accordingly.
    #[try_stream(ok = Message, error = StreamExecutorError)]
    async fn execute_inner(mut self) {
        let mut barrier_receiver = self.barrier_receiver.take().unwrap();
        let first_barrier = barrier_receiver
            .recv()
            .instrument_await("source_recv_first_barrier")
            .await
            .ok_or_else(|| {
                anyhow!(
                    "failed to receive the first barrier, actor_id: {:?}, source_id: {:?}",
                    self.actor_ctx.id,
                    self.stream_source_core.source_id
                )
            })?;
        let first_epoch = first_barrier.epoch;
        let mut boot_state =
            if let Some(splits) = first_barrier.initial_split_assignment(self.actor_ctx.id) {
                tracing::debug!(?splits, "boot with splits");
                splits.to_vec()
            } else {
                Vec::default()
            };
        let is_pause_on_startup = first_barrier.is_pause_on_startup();
        let mut is_uninitialized = first_barrier.is_newly_added(self.actor_ctx.id);

        yield Message::Barrier(first_barrier);

        let mut core = self.stream_source_core;
        let source_id = core.source_id;

        // Build source description from the builder.
        let source_desc_builder: SourceDescBuilder = core.source_desc_builder.take().unwrap();
        let mut source_desc = source_desc_builder
            .build()
            .map_err(StreamExecutorError::connector_error)?;

        let mut wait_checkpoint_task_builder =
            Self::spawn_wait_checkpoint_worker(&core, source_desc.source.clone()).await?;

        let (Some(split_idx), Some(offset_idx)) = get_split_offset_col_idx(&source_desc.columns)
        else {
            unreachable!("Partition and offset columns must be set.");
        };

        core.split_state_store.init_epoch(first_epoch).await?;
        {
            let committed_reader = core
                .split_state_store
                .new_committed_reader(first_epoch)
                .await?;
            for ele in &mut boot_state {
                if let Some(recover_state) =
                    committed_reader.try_recover_from_state_store(ele).await?
                {
                    *ele = recover_state;
                    // if state store is non-empty, we consider it's initialized.
                    is_uninitialized = false;
                } else {
                    // This is a new split, not in state table.
                    // make sure it is written to state table later.
                    // Then even it receives no messages, we can observe it in state table.
                    core.updated_splits_in_epoch.insert(ele.id(), ele.clone());
                }
            }
        }

        // init in-memory split states with persisted state if any
        core.init_split_state(boot_state.clone());

        // Return the ownership of `stream_source_core` to the source executor.
        self.stream_source_core = core;

        let recover_state: ConnectorState = (!boot_state.is_empty()).then_some(boot_state);
        tracing::debug!(state = ?recover_state, "start with state");

        let barrier_stream = barrier_to_message_stream(barrier_receiver).boxed();
        let mut reader_stream_builder = self.stream_reader_builder(source_desc.clone());
        let mut latest_splits = None;
        // Build the source stream reader.
        if is_uninitialized {
            let create_split_reader_result = reader_stream_builder
                .fetch_latest_splits(recover_state.clone(), self.is_shared_non_cdc)
                .await?;
            latest_splits = create_split_reader_result.latest_splits;
        }

        if let Some(latest_splits) = latest_splits {
            // make sure it is written to state table later.
            // Then even it receives no messages, we can observe it in state table.
            self.stream_source_core
                .updated_splits_in_epoch
                .extend(latest_splits.into_iter().map(|s| (s.id(), s)));
        }
        // Merge the chunks from source and the barriers into a single stream. We prioritize
        // barriers over source data chunks here.
        let mut stream = StreamReaderWithPause::<true, StreamChunkWithState>::new(
            barrier_stream,
            reader_stream_builder
                .into_retry_stream(recover_state, is_uninitialized && self.is_shared_non_cdc),
        );
        let mut command_paused = false;

        // - If the first barrier requires us to pause on startup, pause the stream.
        if is_pause_on_startup {
            tracing::info!("source paused on startup");
            stream.pause_stream();
            command_paused = true;
        }

        // We allow data to flow for `WAIT_BARRIER_MULTIPLE_TIMES` * `expected_barrier_latency_ms`
        // milliseconds, considering some other latencies like network and cost in Meta.
        let mut max_wait_barrier_time_ms =
            self.system_params.load().barrier_interval_ms() as u128 * WAIT_BARRIER_MULTIPLE_TIMES;
        let mut last_barrier_time = Instant::now();
        let mut self_paused = false;

        let source_output_row_count = self
            .metrics
            .source_output_row_count
            .with_guarded_label_values(&self.get_metric_labels());

        let source_split_change_count = self
            .metrics
            .source_split_change_count
            .with_guarded_label_values(&self.get_metric_labels());

        while let Some(msg) = stream.next().await {
            let Ok(msg) = msg else {
                tokio::time::sleep(Duration::from_millis(1000)).await;
                self.rebuild_stream_reader_from_error(&source_desc, &mut stream, msg.unwrap_err())?;
                continue;
            };

            match msg {
                // This branch will be preferred.
                Either::Left(Message::Barrier(barrier)) => {
                    last_barrier_time = Instant::now();

                    if self_paused {
                        self_paused = false;
                        // command_paused has a higher priority.
                        if !command_paused {
                            stream.resume_stream();
                        }
                    }

                    let epoch = barrier.epoch;

                    let mut split_change = None;

                    if let Some(mutation) = barrier.mutation.as_deref() {
                        match mutation {
                            Mutation::Pause => {
                                command_paused = true;
                                stream.pause_stream()
                            }
                            Mutation::Resume => {
                                command_paused = false;
                                stream.resume_stream()
                            }
                            Mutation::SourceChangeSplit(actor_splits) => {
                                tracing::info!(
                                    actor_id = self.actor_ctx.id,
                                    actor_splits = ?actor_splits,
                                    "source change split received"
                                );

                                split_change = actor_splits.get(&self.actor_ctx.id).cloned().map(
                                    |target_splits| {
                                        (
                                            &source_desc,
                                            &mut stream,
                                            ApplyMutationAfterBarrier::SplitChange {
                                                target_splits,
                                                should_trim_state: true,
                                                split_change_count: &source_split_change_count,
                                            },
                                        )
                                    },
                                );
                            }

                            Mutation::ConnectorPropsChange(maybe_mutation) => {
                                if let Some(new_props) = maybe_mutation.get(&source_id.table_id()) {
                                    // rebuild the stream reader with new props
                                    tracing::info!(
                                        "updating source properties from {:?} to {:?}",
                                        source_desc.source.config,
                                        new_props
                                    );
                                    source_desc.update_reader(new_props.clone())?;
                                    // suppose the connector props change will not involve state change
                                    split_change = Some((
                                        &source_desc,
                                        &mut stream,
                                        ApplyMutationAfterBarrier::ConnectorPropsChange,
                                    ));
                                }
                            }

                            Mutation::Update(UpdateMutation { actor_splits, .. }) => {
                                split_change = actor_splits.get(&self.actor_ctx.id).cloned().map(
                                    |target_splits| {
                                        (
                                            &source_desc,
                                            &mut stream,
                                            ApplyMutationAfterBarrier::SplitChange {
                                                target_splits,
                                                should_trim_state: false,
                                                split_change_count: &source_split_change_count,
                                            },
                                        )
                                    },
                                );
                            }
                            Mutation::Throttle(actor_to_apply) => {
                                if let Some(new_rate_limit) = actor_to_apply.get(&self.actor_ctx.id)
                                    && *new_rate_limit != self.rate_limit_rps
                                {
                                    tracing::info!(
                                        "updating rate limit from {:?} to {:?}",
                                        self.rate_limit_rps,
                                        *new_rate_limit
                                    );
                                    self.rate_limit_rps = *new_rate_limit;
                                    // recreate from latest_split_info
                                    self.rebuild_stream_reader(&source_desc, &mut stream)?;
                                }
                            }
                            _ => {}
                        }
                    }

                    let updated_splits = self.persist_state_and_clear_cache(epoch).await?;

                    // when handle a checkpoint barrier, spawn a task to wait for epoch commit notification
                    if barrier.kind.is_checkpoint()
                        && let Some(task_builder) = &mut wait_checkpoint_task_builder
                    {
                        task_builder.update_task_on_checkpoint(updated_splits);

                        tracing::debug!("epoch to wait {:?}", epoch);
                        task_builder.send(Epoch(epoch.prev)).await?
                    }

                    let barrier_epoch = barrier.epoch;
                    yield Message::Barrier(barrier);

                    if let Some((source_desc, stream, to_apply_mutation)) = split_change {
                        self.apply_split_change_after_yield_barrier(
                            barrier_epoch,
                            source_desc,
                            stream,
                            to_apply_mutation,
                        )
                        .await?;
                    }
                }
                Either::Left(_) => {
                    // For the source executor, the message we receive from this arm
                    // should always be barrier message.
                    unreachable!();
                }

                Either::Right((chunk, latest_state)) => {
                    if let Some(task_builder) = &mut wait_checkpoint_task_builder {
                        let offset_col = chunk.column_at(offset_idx);
                        task_builder.update_task_on_chunk(offset_col.clone());
                    }
                    if last_barrier_time.elapsed().as_millis() > max_wait_barrier_time_ms {
                        // Exceeds the max wait barrier time, the source will be paused.
                        // Currently we can guarantee the
                        // source is not paused since it received stream
                        // chunks.
                        self_paused = true;
                        tracing::warn!(
                            "source paused, wait barrier for {:?}",
                            last_barrier_time.elapsed()
                        );
                        stream.pause_stream();

                        // Only update `max_wait_barrier_time_ms` to capture
                        // `barrier_interval_ms`
                        // changes here to avoid frequently accessing the shared
                        // `system_params`.
                        max_wait_barrier_time_ms = self.system_params.load().barrier_interval_ms()
                            as u128
                            * WAIT_BARRIER_MULTIPLE_TIMES;
                    }

                    latest_state.iter().for_each(|(split_id, new_split_impl)| {
                        if let Some(split_impl) =
                            self.stream_source_core.latest_split_info.get_mut(split_id)
                        {
                            *split_impl = new_split_impl.clone();
                        }
                    });

                    self.stream_source_core
                        .updated_splits_in_epoch
                        .extend(latest_state);

                    source_output_row_count.inc_by(chunk.cardinality() as u64);
                    let chunk =
                        prune_additional_cols(&chunk, split_idx, offset_idx, &source_desc.columns);
                    yield Message::Chunk(chunk);
                    self.try_flush_data().await?;
                }
            }
        }

        // The source executor should only be stopped by the actor when finding a `Stop` mutation.
        tracing::error!(
            actor_id = self.actor_ctx.id,
            "source executor exited unexpectedly"
        )
    }
}

#[derive(Debug, Clone)]
enum ApplyMutationAfterBarrier<'a> {
    SplitChange {
        target_splits: Vec<SplitImpl>,
        should_trim_state: bool,
        split_change_count: &'a LabelGuardedMetric<GenericCounter<AtomicU64>>,
    },
    ConnectorPropsChange,
}

impl<S: StateStore> Execute for SourceExecutor<S> {
    fn execute(self: Box<Self>) -> BoxedMessageStream {
        self.execute_inner().boxed()
    }
}

impl<S: StateStore> Debug for SourceExecutor<S> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SourceExecutor")
            .field("source_id", &self.stream_source_core.source_id)
            .field("column_ids", &self.stream_source_core.column_ids)
            .finish()
    }
}

struct WaitCheckpointTaskBuilder {
    wait_checkpoint_tx: UnboundedSender<(Epoch, WaitCheckpointTask)>,
    source_reader: SourceReader,
    building_task: WaitCheckpointTask,
}

impl WaitCheckpointTaskBuilder {
    fn update_task_on_chunk(&mut self, offset_col: ArrayRef) {
        match &mut self.building_task {
            WaitCheckpointTask::AckPubsubMessage(_, arrays) => {
                arrays.push(offset_col);
            }
            WaitCheckpointTask::AckNatsJetStream(_, arrays, _) => {
                arrays.push(offset_col);
            }
            WaitCheckpointTask::CommitCdcOffset(_) => {}
        }
    }

    fn update_task_on_checkpoint(&mut self, updated_splits: HashMap<SplitId, SplitImpl>) {
        #[expect(clippy::single_match)]
        match &mut self.building_task {
            WaitCheckpointTask::CommitCdcOffset(offsets) => {
                if !updated_splits.is_empty() {
                    // cdc source only has one split
                    assert_eq!(1, updated_splits.len());
                    for (split_id, split_impl) in updated_splits {
                        if split_impl.is_cdc_split() {
                            *offsets = Some((split_id, split_impl.get_cdc_split_offset()));
                        } else {
                            unreachable!()
                        }
                    }
                }
            }
            _ => {}
        }
    }

    /// Send and reset the building task to a new one.
    async fn send(&mut self, epoch: Epoch) -> Result<(), anyhow::Error> {
        let new_task = self
            .source_reader
            .create_wait_checkpoint_task()
            .await?
            .expect("wait checkpoint task should be created");
        self.wait_checkpoint_tx
            .send((epoch, std::mem::replace(&mut self.building_task, new_task)))
            .expect("wait_checkpoint_tx send should succeed");
        Ok(())
    }
}

/// A worker used to do some work after each checkpoint epoch is committed.
///
/// # Usage Cases
///
/// Typically there are 2 issues related with ack on checkpoint:
///
/// 1. Correctness (at-least-once), or don't let upstream clean uncommitted data.
///    For message queueing semantics (delete after ack), we should ack to avoid redelivery,
///    and only ack after checkpoint to avoid data loss.
///
/// 2. Allow upstream to clean data after commit.
///
/// See also <https://github.com/risingwavelabs/risingwave/issues/16736#issuecomment-2109379790>
///
/// ## CDC
///
/// Commit last consumed offset to upstream DB, so that old data can be discarded.
///
/// ## Google Pub/Sub
///
/// Due to queueing semantics.
/// Although Pub/Sub supports `retain_acked_messages` and `seek` functionality,
/// it's quite limited unlike Kafka.
///
/// See also <https://cloud.google.com/pubsub/docs/subscribe-best-practices#process-messages>
struct WaitCheckpointWorker<S: StateStore> {
    wait_checkpoint_rx: UnboundedReceiver<(Epoch, WaitCheckpointTask)>,
    state_store: S,
    table_id: TableId,
}

impl<S: StateStore> WaitCheckpointWorker<S> {
    pub async fn run(mut self) {
        tracing::debug!("wait epoch worker start success");
        loop {
            // poll the rx and wait for the epoch commit
            match self.wait_checkpoint_rx.recv().await {
                Some((epoch, task)) => {
                    tracing::debug!("start to wait epoch {}", epoch.0);
                    let ret = self
                        .state_store
                        .try_wait_epoch(
                            HummockReadEpoch::Committed(epoch.0),
                            TryWaitEpochOptions {
                                table_id: self.table_id,
                            },
                        )
                        .await;

                    match ret {
                        Ok(()) => {
                            tracing::debug!(epoch = epoch.0, "wait epoch success");
                            task.run().await;
                        }
                        Err(e) => {
                            tracing::error!(
                            error = %e.as_report(),
                            "wait epoch {} failed", epoch.0
                            );
                        }
                    }
                }
                None => {
                    tracing::error!("wait epoch rx closed");
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use maplit::{btreemap, convert_args, hashmap};
    use risingwave_common::catalog::{ColumnId, Field, TableId};
    use risingwave_common::system_param::local_manager::LocalSystemParamsManager;
    use risingwave_common::test_prelude::StreamChunkTestExt;
    use risingwave_common::util::epoch::{EpochExt, test_epoch};
    use risingwave_connector::source::datagen::DatagenSplit;
    use risingwave_connector::source::reader::desc::test_utils::create_source_desc_builder;
    use risingwave_pb::catalog::StreamSourceInfo;
    use risingwave_pb::plan_common::PbRowFormatType;
    use risingwave_storage::memory::MemoryStateStore;
    use tokio::sync::mpsc::unbounded_channel;
    use tracing_test::traced_test;

    use super::*;
    use crate::executor::AddMutation;
    use crate::executor::source::{SourceStateTableHandler, default_source_internal_table};

    const MOCK_SOURCE_NAME: &str = "mock_source";

    #[tokio::test]
    async fn test_source_executor() {
        let table_id = TableId::default();
        let schema = Schema {
            fields: vec![Field::with_name(DataType::Int32, "sequence_int")],
        };
        let row_id_index = None;
        let source_info = StreamSourceInfo {
            row_format: PbRowFormatType::Native as i32,
            ..Default::default()
        };
        let (barrier_tx, barrier_rx) = unbounded_channel::<Barrier>();
        let column_ids = vec![0].into_iter().map(ColumnId::from).collect();

        // This datagen will generate 3 rows at one time.
        let properties = convert_args!(btreemap!(
            "connector" => "datagen",
            "datagen.rows.per.second" => "3",
            "fields.sequence_int.kind" => "sequence",
            "fields.sequence_int.start" => "11",
            "fields.sequence_int.end" => "11111",
        ));
        let source_desc_builder =
            create_source_desc_builder(&schema, row_id_index, source_info, properties, vec![]);
        let split_state_store = SourceStateTableHandler::from_table_catalog(
            &default_source_internal_table(0x2333),
            MemoryStateStore::new(),
        )
        .await;
        let core = StreamSourceCore::<MemoryStateStore> {
            source_id: table_id,
            column_ids,
            source_desc_builder: Some(source_desc_builder),
            latest_split_info: HashMap::new(),
            split_state_store,
            updated_splits_in_epoch: HashMap::new(),
            source_name: MOCK_SOURCE_NAME.to_owned(),
        };

        let system_params_manager = LocalSystemParamsManager::for_test();

        let executor = SourceExecutor::new(
            ActorContext::for_test(0),
            core,
            Arc::new(StreamingMetrics::unused()),
            barrier_rx,
            system_params_manager.get_params(),
            None,
            false,
        );
        let mut executor = executor.boxed().execute();

        let init_barrier =
            Barrier::new_test_barrier(test_epoch(1)).with_mutation(Mutation::Add(AddMutation {
                adds: HashMap::new(),
                added_actors: HashSet::new(),
                splits: hashmap! {
                    ActorId::default() => vec![
                        SplitImpl::Datagen(DatagenSplit {
                            split_index: 0,
                            split_num: 1,
                            start_offset: None,
                        }),
                    ],
                },
                pause: false,
                subscriptions_to_add: vec![],
                backfill_nodes_to_pause: Default::default(),
                actor_cdc_table_snapshot_splits: Default::default(),
            }));
        barrier_tx.send(init_barrier).unwrap();

        // Consume barrier.
        executor.next().await.unwrap().unwrap();

        // Consume data chunk.
        let msg = executor.next().await.unwrap().unwrap();

        // Row id will not be filled here.
        assert_eq!(
            msg.into_chunk().unwrap(),
            StreamChunk::from_pretty(
                " i
                + 11
                + 12
                + 13"
            )
        );
    }

    #[traced_test]
    #[tokio::test]
    async fn test_split_change_mutation() {
        let table_id = TableId::default();
        let schema = Schema {
            fields: vec![Field::with_name(DataType::Int32, "v1")],
        };
        let row_id_index = None;
        let source_info = StreamSourceInfo {
            row_format: PbRowFormatType::Native as i32,
            ..Default::default()
        };
        let properties = convert_args!(btreemap!(
            "connector" => "datagen",
            "fields.v1.kind" => "sequence",
            "fields.v1.start" => "11",
            "fields.v1.end" => "11111",
        ));

        let source_desc_builder =
            create_source_desc_builder(&schema, row_id_index, source_info, properties, vec![]);
        let mem_state_store = MemoryStateStore::new();

        let column_ids = vec![ColumnId::from(0)];
        let (barrier_tx, barrier_rx) = unbounded_channel::<Barrier>();
        let split_state_store = SourceStateTableHandler::from_table_catalog(
            &default_source_internal_table(0x2333),
            mem_state_store.clone(),
        )
        .await;

        let core = StreamSourceCore::<MemoryStateStore> {
            source_id: table_id,
            column_ids: column_ids.clone(),
            source_desc_builder: Some(source_desc_builder),
            latest_split_info: HashMap::new(),
            split_state_store,
            updated_splits_in_epoch: HashMap::new(),
            source_name: MOCK_SOURCE_NAME.to_owned(),
        };

        let system_params_manager = LocalSystemParamsManager::for_test();

        let executor = SourceExecutor::new(
            ActorContext::for_test(0),
            core,
            Arc::new(StreamingMetrics::unused()),
            barrier_rx,
            system_params_manager.get_params(),
            None,
            false,
        );
        let mut handler = executor.boxed().execute();

        let mut epoch = test_epoch(1);
        let init_barrier =
            Barrier::new_test_barrier(epoch).with_mutation(Mutation::Add(AddMutation {
                adds: HashMap::new(),
                added_actors: HashSet::new(),
                splits: hashmap! {
                    ActorId::default() => vec![
                        SplitImpl::Datagen(DatagenSplit {
                            split_index: 0,
                            split_num: 3,
                            start_offset: None,
                        }),
                    ],
                },
                pause: false,
                subscriptions_to_add: vec![],
                backfill_nodes_to_pause: Default::default(),
                actor_cdc_table_snapshot_splits: Default::default(),
            }));
        barrier_tx.send(init_barrier).unwrap();

        // Consume barrier.
        handler
            .next()
            .await
            .unwrap()
            .unwrap()
            .into_barrier()
            .unwrap();

        let mut ready_chunks = handler.ready_chunks(10);

        let _ = ready_chunks.next().await.unwrap();

        let new_assignment = vec![
            SplitImpl::Datagen(DatagenSplit {
                split_index: 0,
                split_num: 3,
                start_offset: None,
            }),
            SplitImpl::Datagen(DatagenSplit {
                split_index: 1,
                split_num: 3,
                start_offset: None,
            }),
            SplitImpl::Datagen(DatagenSplit {
                split_index: 2,
                split_num: 3,
                start_offset: None,
            }),
        ];

        epoch.inc_epoch();
        let change_split_mutation =
            Barrier::new_test_barrier(epoch).with_mutation(Mutation::SourceChangeSplit(hashmap! {
                ActorId::default() => new_assignment.clone()
            }));

        barrier_tx.send(change_split_mutation).unwrap();

        let _ = ready_chunks.next().await.unwrap(); // barrier

        epoch.inc_epoch();
        let barrier = Barrier::new_test_barrier(epoch);
        barrier_tx.send(barrier).unwrap();

        ready_chunks.next().await.unwrap(); // barrier

        let mut source_state_handler = SourceStateTableHandler::from_table_catalog(
            &default_source_internal_table(0x2333),
            mem_state_store.clone(),
        )
        .await;

        // there must exist state for new add partition
        source_state_handler
            .init_epoch(EpochPair::new_test_epoch(epoch))
            .await
            .unwrap();
        source_state_handler
            .get(&new_assignment[1].id())
            .await
            .unwrap()
            .unwrap();

        tokio::time::sleep(Duration::from_millis(100)).await;

        let _ = ready_chunks.next().await.unwrap();

        epoch.inc_epoch();
        let barrier = Barrier::new_test_barrier(epoch).with_mutation(Mutation::Pause);
        barrier_tx.send(barrier).unwrap();

        epoch.inc_epoch();
        let barrier = Barrier::new_test_barrier(epoch).with_mutation(Mutation::Resume);
        barrier_tx.send(barrier).unwrap();

        // receive all
        ready_chunks.next().await.unwrap();

        let prev_assignment = new_assignment;
        let new_assignment = vec![prev_assignment[2].clone()];

        epoch.inc_epoch();
        let drop_split_mutation =
            Barrier::new_test_barrier(epoch).with_mutation(Mutation::SourceChangeSplit(hashmap! {
                ActorId::default() => new_assignment.clone()
            }));

        barrier_tx.send(drop_split_mutation).unwrap();

        ready_chunks.next().await.unwrap(); // barrier

        epoch.inc_epoch();
        let barrier = Barrier::new_test_barrier(epoch);
        barrier_tx.send(barrier).unwrap();

        ready_chunks.next().await.unwrap(); // barrier

        let mut source_state_handler = SourceStateTableHandler::from_table_catalog(
            &default_source_internal_table(0x2333),
            mem_state_store.clone(),
        )
        .await;

        let new_epoch = EpochPair::new_test_epoch(epoch);
        source_state_handler.init_epoch(new_epoch).await.unwrap();

        let committed_reader = source_state_handler
            .new_committed_reader(new_epoch)
            .await
            .unwrap();
        assert!(
            committed_reader
                .try_recover_from_state_store(&prev_assignment[0])
                .await
                .unwrap()
                .is_none()
        );

        assert!(
            committed_reader
                .try_recover_from_state_store(&prev_assignment[1])
                .await
                .unwrap()
                .is_none()
        );

        assert!(
            committed_reader
                .try_recover_from_state_store(&prev_assignment[2])
                .await
                .unwrap()
                .is_some()
        );
    }
}
