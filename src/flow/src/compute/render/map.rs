// Copyright 2023 Greptime Team
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

use std::collections::BTreeMap;

use hydroflow::scheduled::graph_ext::GraphExt;
use hydroflow::scheduled::port::{PortCtx, SEND};
use itertools::Itertools;
use snafu::OptionExt;

use crate::adapter::error::{Error, PlanSnafu};
use crate::compute::render::Context;
use crate::compute::state::Scheduler;
use crate::compute::types::{Arranged, Collection, CollectionBundle, ErrCollector, Toff};
use crate::expr::{EvalError, MapFilterProject, MfpPlan, ScalarExpr};
use crate::plan::Plan;
use crate::repr::{self, DiffRow, KeyValDiffRow, Row};
use crate::utils::ArrangeHandler;

impl<'referred, 'df> Context<'referred, 'df> {
    /// render MapFilterProject, will only emit the `rows` once. Assume all incoming row's sys time being `now`` and ignore the row's stated sys time
    /// TODO(discord9): schedule mfp operator to run when temporal filter need
    ///
    /// `MapFilterProject`(`mfp` for short) is scheduled to run when there is enough amount of input updates
    /// ***or*** when a future update in it's output buffer(a `Arrangement`) is supposed to emit now.
    // There is a false positive in using `Vec<ScalarExpr>` as key due to `Value` have `bytes` variant
    #[allow(clippy::mutable_key_type)]
    pub fn render_mfp(
        &mut self,
        input: Box<Plan>,
        mfp: MapFilterProject,
    ) -> Result<CollectionBundle, Error> {
        let input = self.render_plan(*input)?;
        // TODO(discord9): consider if check if contain temporal to determine if
        // need arrange or not, or does this added complexity worth it
        let (out_send_port, out_recv_port) = self.df.make_edge::<_, Toff>("mfp");

        let output_arity = mfp.output_arity();

        // default to have a arrange with only future updates, so it can be empty if no temporal filter is applied
        // as stream only sends current updates and etc.
        let arrange_handler = self.compute_state.new_arrange(None);
        let arrange_handler_inner =
            arrange_handler
                .clone_future_only()
                .with_context(|| PlanSnafu {
                    reason: "No write is expected at this point",
                })?;

        // This closure capture following variables:
        let mfp_plan = MfpPlan::create_from(mfp)?;
        let now = self.compute_state.current_time_ref();

        let err_collector = self.err_collector.clone();

        // TODO(discord9): better way to schedule future run
        let scheduler = self.compute_state.get_scheduler();
        let scheduler_inner = scheduler.clone();

        let subgraph = self.df.add_subgraph_in_out(
            "mfp",
            input.collection.into_inner(),
            out_send_port,
            move |_ctx, recv, send| {
                // mfp only need to passively receive updates from recvs
                let data = recv.take_inner().into_iter().flat_map(|v| v.into_iter());

                mfp_subgraph(
                    &arrange_handler_inner,
                    data,
                    &mfp_plan,
                    *now.borrow(),
                    &err_collector,
                    &scheduler_inner,
                    send,
                );
            },
        );

        // register current subgraph in scheduler for future scheduling
        scheduler.set_cur_subgraph(subgraph);

        let arranged = BTreeMap::from([(
            (0..output_arity).map(ScalarExpr::Column).collect_vec(),
            Arranged::new(arrange_handler),
        )]);

        let bundle = CollectionBundle {
            collection: Collection::from_port(out_recv_port),
            arranged,
        };
        Ok(bundle)
    }
}

fn mfp_subgraph(
    arrange: &ArrangeHandler,
    input: impl IntoIterator<Item = DiffRow>,
    mfp_plan: &MfpPlan,
    now: repr::Timestamp,
    err_collector: &ErrCollector,
    scheduler: &Scheduler,
    send: &PortCtx<SEND, Toff>,
) {
    let run_mfp = || {
        let all_updates = eval_mfp_core(input, mfp_plan, now, err_collector);
        arrange.write().apply_updates(now, all_updates)?;
        Ok(())
    };
    err_collector.run(run_mfp);

    // Deal with output:
    // 1. Read all updates that were emitted between the last time this arrangement had updates and the current time.
    // 2. Output the updates.
    // 3. Truncate all updates within that range.

    let from = arrange.read().last_compaction_time().map(|n| n + 1);
    let from = from.unwrap_or(repr::Timestamp::MIN);
    let output_kv = arrange.read().get_updates_in_range(from..=now);
    // the output is expected to be key -> empty val
    let output = output_kv
        .into_iter()
        .map(|((key, _v), ts, diff)| (key, ts, diff))
        .collect_vec();
    send.give(output);
    let run_compaction = || {
        arrange.write().compaction_to(now)?;
        Ok(())
    };
    err_collector.run(run_compaction);

    // schedule next time this subgraph should run
    scheduler.schedule_for_arrange(&arrange.read(), now);
}

/// The core of evaluating MFP operator, given a MFP and a input, evaluate the MFP operator,
/// return the output updates **And** possibly any number of errors that occurred during the evaluation
fn eval_mfp_core(
    input: impl IntoIterator<Item = DiffRow>,
    mfp_plan: &MfpPlan,
    now: repr::Timestamp,
    err_collector: &ErrCollector,
) -> Vec<KeyValDiffRow> {
    let mut all_updates = Vec::new();
    for (mut row, _sys_time, diff) in input.into_iter() {
        // this updates is expected to be only zero to two rows
        let updates = mfp_plan.evaluate::<EvalError>(&mut row.inner, now, diff);
        // TODO(discord9): refactor error handling
        // Expect error in a single row to not interrupt the whole evaluation
        let updates = updates
            .filter_map(|r| match r {
                Ok((key, ts, diff)) => Some(((key, Row::empty()), ts, diff)),
                Err((err, _ts, _diff)) => {
                    err_collector.push_err(err);
                    None
                }
            })
            .collect_vec();

        all_updates.extend(updates);
    }
    all_updates
}

#[cfg(test)]
mod test {
    use std::cell::RefCell;
    use std::rc::Rc;

    use datatypes::data_type::ConcreteDataType;
    use hydroflow::scheduled::graph::Hydroflow;

    use super::*;
    use crate::compute::render::test::{get_output_handle, harness_test_ctx, run_and_check};
    use crate::compute::state::DataflowState;
    use crate::expr::{self, BinaryFunc, GlobalId};

    /// test if temporal filter works properly
    /// namely: if mfp operator can schedule a delete at the correct time
    #[test]
    fn test_render_mfp_with_temporal() {
        let mut df = Hydroflow::new();
        let mut state = DataflowState::default();
        let mut ctx = harness_test_ctx(&mut df, &mut state);

        let rows = vec![
            (Row::new(vec![1i64.into()]), 0, 1),
            (Row::new(vec![2i64.into()]), 0, 1),
            (Row::new(vec![3i64.into()]), 0, 1),
        ];
        let collection = ctx.render_constant(rows.clone());
        ctx.insert_global(GlobalId::User(1), collection);
        let input_plan = Plan::Get {
            id: expr::Id::Global(GlobalId::User(1)),
        };
        // temporal filter: now <= col(0) < now + 4
        let mfp = MapFilterProject::new(1)
            .filter(vec![
                ScalarExpr::Column(0)
                    .call_unary(expr::UnaryFunc::Cast(ConcreteDataType::datetime_datatype()))
                    .call_binary(
                        ScalarExpr::CallUnmaterializable(expr::UnmaterializableFunc::Now),
                        BinaryFunc::Gte,
                    ),
                ScalarExpr::Column(0)
                    .call_binary(
                        ScalarExpr::literal(4i64.into(), ConcreteDataType::int64_datatype()),
                        BinaryFunc::SubInt64,
                    )
                    .call_unary(expr::UnaryFunc::Cast(ConcreteDataType::datetime_datatype()))
                    .call_binary(
                        ScalarExpr::CallUnmaterializable(expr::UnmaterializableFunc::Now),
                        BinaryFunc::Lt,
                    ),
            ])
            .unwrap();

        let bundle = ctx.render_mfp(Box::new(input_plan), mfp).unwrap();
        let output = get_output_handle(&mut ctx, bundle);
        // drop ctx here to simulate actual process of compile first, run later scenario
        drop(ctx);
        // expected output at given time
        let expected_output = BTreeMap::from([
            (
                0, // time
                vec![
                    (Row::new(vec![1i64.into()]), 0, 1),
                    (Row::new(vec![2i64.into()]), 0, 1),
                    (Row::new(vec![3i64.into()]), 0, 1),
                ],
            ),
            (
                2, // time
                vec![(Row::new(vec![1i64.into()]), 2, -1)],
            ),
            (
                3, // time
                vec![(Row::new(vec![2i64.into()]), 3, -1)],
            ),
            (
                4, // time
                vec![(Row::new(vec![3i64.into()]), 4, -1)],
            ),
        ]);
        run_and_check(&mut state, &mut df, 0..5, expected_output, output);
    }

    /// test if mfp operator without temporal filter works properly
    /// that is it filter the rows correctly
    #[test]
    fn test_render_mfp() {
        let mut df = Hydroflow::new();
        let mut state = DataflowState::default();
        let mut ctx = harness_test_ctx(&mut df, &mut state);

        let rows = vec![
            (Row::new(vec![1.into()]), 1, 1),
            (Row::new(vec![2.into()]), 2, 1),
            (Row::new(vec![3.into()]), 3, 1),
        ];
        let collection = ctx.render_constant(rows.clone());
        ctx.insert_global(GlobalId::User(1), collection);
        let input_plan = Plan::Get {
            id: expr::Id::Global(GlobalId::User(1)),
        };
        // filter: col(0)>1
        let mfp = MapFilterProject::new(1)
            .filter(vec![ScalarExpr::Column(0).call_binary(
                ScalarExpr::literal(1.into(), ConcreteDataType::int32_datatype()),
                BinaryFunc::Gt,
            )])
            .unwrap();
        let bundle = ctx.render_mfp(Box::new(input_plan), mfp).unwrap();

        let output = get_output_handle(&mut ctx, bundle);
        drop(ctx);
        let expected = BTreeMap::from([
            (2, vec![(Row::new(vec![2.into()]), 2, 1)]),
            (3, vec![(Row::new(vec![3.into()]), 3, 1)]),
        ]);
        run_and_check(&mut state, &mut df, 1..5, expected, output);
    }
}