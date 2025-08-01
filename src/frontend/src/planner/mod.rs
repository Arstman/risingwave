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

use crate::binder::{BoundStatement, ShareId};
use crate::error::Result;
use crate::optimizer::{LogicalPlanRoot, OptimizerContextRef};

mod changelog;
mod delete;
mod insert;
mod query;
mod recursive_union;
mod relation;
mod select;
mod set_expr;
mod set_operation;
mod statement;
mod update;
mod values;
pub use query::LIMIT_ALL_COUNT;

use crate::optimizer::plan_node::LogicalPlanRef as PlanRef;

/// `Planner` converts a bound statement to a [`crate::optimizer::plan_node::LogicalPlanNode`] tree
pub struct Planner {
    ctx: OptimizerContextRef,
    /// Mapping of `ShareId` to its share plan.
    /// The share plan can be a CTE, a source, a view and so on.
    share_cache: HashMap<ShareId, PlanRef>,
    /// Plan for stream or batch.
    plan_for: PlanFor,
}

#[derive(Debug, Copy, Clone)]
pub enum PlanFor {
    Stream,
    /// Is the `Sink` in iceberg table engine.
    /// It connects to the table node directly, while external stream jobs may connect to an iceberg source.
    StreamIcebergEngineInternal,
    /// Other batch queries, e.g., DML.
    Batch,
    /// Batch `SELECT` queries.
    ///
    /// ## Special handling
    ///
    /// Iceberg engine table will be converted to iceberg source based on this mode.
    BatchDql,
}

impl Planner {
    pub fn new_for_batch_dql(ctx: OptimizerContextRef) -> Planner {
        Planner {
            ctx,
            share_cache: Default::default(),
            plan_for: PlanFor::BatchDql,
        }
    }

    pub fn new_for_batch(ctx: OptimizerContextRef) -> Planner {
        Planner {
            ctx,
            share_cache: Default::default(),
            plan_for: PlanFor::Batch,
        }
    }

    pub fn new_for_stream(ctx: OptimizerContextRef) -> Planner {
        Planner {
            ctx,
            share_cache: Default::default(),
            plan_for: PlanFor::Stream,
        }
    }

    pub fn new_for_iceberg_table_engine_sink(ctx: OptimizerContextRef) -> Planner {
        Planner {
            ctx,
            share_cache: Default::default(),
            plan_for: PlanFor::StreamIcebergEngineInternal,
        }
    }

    /// Plan a [`BoundStatement`]. Need to bind a statement before plan.
    pub fn plan(&mut self, stmt: BoundStatement) -> Result<LogicalPlanRoot> {
        self.plan_statement(stmt)
    }

    pub fn ctx(&self) -> OptimizerContextRef {
        self.ctx.clone()
    }

    pub fn plan_for(&self) -> PlanFor {
        self.plan_for
    }
}
