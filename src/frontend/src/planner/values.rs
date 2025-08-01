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

use crate::binder::BoundValues;
use crate::error::Result;
use crate::optimizer::plan_node::{LogicalPlanRef as PlanRef, LogicalValues};
use crate::planner::Planner;

impl Planner {
    pub(super) fn plan_values(&mut self, values: BoundValues) -> Result<PlanRef> {
        Ok(LogicalValues::create(
            values.rows,
            values.schema,
            self.ctx(),
        ))
    }
}
