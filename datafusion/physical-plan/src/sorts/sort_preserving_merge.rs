// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

//! Defines the sort preserving merge plan

use std::any::Any;
use std::sync::Arc;

use crate::common::spawn_buffered;
use crate::expressions::PhysicalSortExpr;
use crate::limit::LimitStream;
use crate::metrics::{BaselineMetrics, ExecutionPlanMetricsSet, MetricsSet};
use crate::sorts::streaming_merge::StreamingMergeBuilder;
use crate::{
    DisplayAs, DisplayFormatType, Distribution, ExecutionPlan, ExecutionPlanProperties,
    Partitioning, PlanProperties, SendableRecordBatchStream, Statistics,
};

use datafusion_common::{internal_err, Result};
use datafusion_execution::memory_pool::MemoryConsumer;
use datafusion_execution::TaskContext;
use datafusion_physical_expr::PhysicalSortRequirement;

use datafusion_physical_expr_common::sort_expr::{LexOrdering, LexRequirement};
use log::{debug, trace};

/// Sort preserving merge execution plan
///
/// This takes an input execution plan and a list of sort expressions, and
/// provided each partition of the input plan is sorted with respect to
/// these sort expressions, this operator will yield a single partition
/// that is also sorted with respect to them
///
/// ```text
/// ┌─────────────────────────┐
/// │ ┌───┬───┬───┬───┐       │
/// │ │ A │ B │ C │ D │ ...   │──┐
/// │ └───┴───┴───┴───┘       │  │
/// └─────────────────────────┘  │  ┌───────────────────┐    ┌───────────────────────────────┐
///   Stream 1                   │  │                   │    │ ┌───┬───╦═══╦───┬───╦═══╗     │
///                              ├─▶│SortPreservingMerge│───▶│ │ A │ B ║ B ║ C │ D ║ E ║ ... │
///                              │  │                   │    │ └───┴─▲─╩═══╩───┴───╩═══╝     │
/// ┌─────────────────────────┐  │  └───────────────────┘    └─┬─────┴───────────────────────┘
/// │ ╔═══╦═══╗               │  │
/// │ ║ B ║ E ║     ...       │──┘                             │
/// │ ╚═══╩═══╝               │              Note Stable Sort: the merged stream
/// └─────────────────────────┘                places equal rows from stream 1
///   Stream 2
///
///
///  Input Streams                                             Output stream
///    (sorted)                                                  (sorted)
/// ```
///
/// # Error Handling
///
/// If any of the input partitions return an error, the error is propagated to
/// the output and inputs are not polled again.
#[derive(Debug)]
pub struct SortPreservingMergeExec {
    /// Input plan
    input: Arc<dyn ExecutionPlan>,
    /// Sort expressions
    expr: LexOrdering,
    /// Execution metrics
    metrics: ExecutionPlanMetricsSet,
    /// Optional number of rows to fetch. Stops producing rows after this fetch
    fetch: Option<usize>,
    /// Cache holding plan properties like equivalences, output partitioning etc.
    cache: PlanProperties,
}

impl SortPreservingMergeExec {
    /// Create a new sort execution plan
    pub fn new(expr: LexOrdering, input: Arc<dyn ExecutionPlan>) -> Self {
        let cache = Self::compute_properties(&input, expr.clone());
        Self {
            input,
            expr,
            metrics: ExecutionPlanMetricsSet::new(),
            fetch: None,
            cache,
        }
    }
    /// Sets the number of rows to fetch
    pub fn with_fetch(mut self, fetch: Option<usize>) -> Self {
        self.fetch = fetch;
        self
    }

    /// Input schema
    pub fn input(&self) -> &Arc<dyn ExecutionPlan> {
        &self.input
    }

    /// Sort expressions
    pub fn expr(&self) -> &[PhysicalSortExpr] {
        &self.expr
    }

    /// Fetch
    pub fn fetch(&self) -> Option<usize> {
        self.fetch
    }

    /// This function creates the cache object that stores the plan properties such as schema, equivalence properties, ordering, partitioning, etc.
    fn compute_properties(
        input: &Arc<dyn ExecutionPlan>,
        ordering: LexOrdering,
    ) -> PlanProperties {
        let mut eq_properties = input.equivalence_properties().clone();
        eq_properties.clear_per_partition_constants();
        eq_properties.add_new_orderings(vec![ordering]);
        PlanProperties::new(
            eq_properties,                        // Equivalence Properties
            Partitioning::UnknownPartitioning(1), // Output Partitioning
            input.execution_mode(),               // Execution Mode
        )
    }
}

impl DisplayAs for SortPreservingMergeExec {
    fn fmt_as(
        &self,
        t: DisplayFormatType,
        f: &mut std::fmt::Formatter,
    ) -> std::fmt::Result {
        match t {
            DisplayFormatType::Default | DisplayFormatType::Verbose => {
                write!(f, "SortPreservingMergeExec: [{}]", self.expr)?;
                if let Some(fetch) = self.fetch {
                    write!(f, ", fetch={fetch}")?;
                };

                Ok(())
            }
        }
    }
}

impl ExecutionPlan for SortPreservingMergeExec {
    fn name(&self) -> &'static str {
        "SortPreservingMergeExec"
    }

    /// Return a reference to Any that can be used for downcasting
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn properties(&self) -> &PlanProperties {
        &self.cache
    }

    fn fetch(&self) -> Option<usize> {
        self.fetch
    }

    /// Sets the number of rows to fetch
    fn with_fetch(&self, limit: Option<usize>) -> Option<Arc<dyn ExecutionPlan>> {
        Some(Arc::new(Self {
            input: Arc::clone(&self.input),
            expr: self.expr.clone(),
            metrics: self.metrics.clone(),
            fetch: limit,
            cache: self.cache.clone(),
        }))
    }

    fn required_input_distribution(&self) -> Vec<Distribution> {
        vec![Distribution::UnspecifiedDistribution]
    }

    fn benefits_from_input_partitioning(&self) -> Vec<bool> {
        vec![false]
    }

    fn required_input_ordering(&self) -> Vec<Option<LexRequirement>> {
        vec![Some(PhysicalSortRequirement::from_sort_exprs(
            self.expr.iter(),
        ))]
    }

    fn maintains_input_order(&self) -> Vec<bool> {
        vec![true]
    }

    fn children(&self) -> Vec<&Arc<dyn ExecutionPlan>> {
        vec![&self.input]
    }

    fn with_new_children(
        self: Arc<Self>,
        children: Vec<Arc<dyn ExecutionPlan>>,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        Ok(Arc::new(
            SortPreservingMergeExec::new(self.expr.clone(), Arc::clone(&children[0]))
                .with_fetch(self.fetch),
        ))
    }

    fn execute(
        &self,
        partition: usize,
        context: Arc<TaskContext>,
    ) -> Result<SendableRecordBatchStream> {
        trace!(
            "Start SortPreservingMergeExec::execute for partition: {}",
            partition
        );
        if 0 != partition {
            return internal_err!(
                "SortPreservingMergeExec invalid partition {partition}"
            );
        }

        let input_partitions = self.input.output_partitioning().partition_count();
        trace!(
            "Number of input partitions of  SortPreservingMergeExec::execute: {}",
            input_partitions
        );
        let schema = self.schema();

        let reservation =
            MemoryConsumer::new(format!("SortPreservingMergeExec[{partition}]"))
                .register(&context.runtime_env().memory_pool);

        match input_partitions {
            0 => internal_err!(
                "SortPreservingMergeExec requires at least one input partition"
            ),
            1 => match self.fetch {
                Some(fetch) => {
                    let stream = self.input.execute(0, context)?;
                    debug!("Done getting stream for SortPreservingMergeExec::execute with 1 input with {fetch}");
                    Ok(Box::pin(LimitStream::new(
                        stream,
                        0,
                        Some(fetch),
                        BaselineMetrics::new(&self.metrics, partition),
                    )))
                }
                None => {
                    let stream = self.input.execute(0, context);
                    debug!("Done getting stream for SortPreservingMergeExec::execute with 1 input without fetch");
                    stream
                }
            },
            _ => {
                let receivers = (0..input_partitions)
                    .map(|partition| {
                        let stream =
                            self.input.execute(partition, Arc::clone(&context))?;
                        Ok(spawn_buffered(stream, 1))
                    })
                    .collect::<Result<_>>()?;

                debug!("Done setting up sender-receiver for SortPreservingMergeExec::execute");

                let result = StreamingMergeBuilder::new()
                    .with_streams(receivers)
                    .with_schema(schema)
                    .with_expressions(self.expr.as_ref().inner)
                    .with_metrics(BaselineMetrics::new(&self.metrics, partition))
                    .with_batch_size(context.session_config().batch_size())
                    .with_fetch(self.fetch)
                    .with_reservation(reservation)
                    .build()?;

                debug!("Got stream result from SortPreservingMergeStream::new_from_receivers");

                Ok(result)
            }
        }
    }

    fn metrics(&self) -> Option<MetricsSet> {
        Some(self.metrics.clone_inner())
    }

    fn statistics(&self) -> Result<Statistics> {
        self.input.statistics()
    }

    fn supports_limit_pushdown(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use std::fmt::Formatter;
    use std::pin::Pin;
    use std::sync::Mutex;
    use std::task::{Context, Poll};
    use std::time::Duration;

    use super::*;
    use crate::coalesce_partitions::CoalescePartitionsExec;
    use crate::expressions::col;
    use crate::memory::MemoryExec;
    use crate::metrics::{MetricValue, Timestamp};
    use crate::sorts::sort::SortExec;
    use crate::stream::RecordBatchReceiverStream;
    use crate::test::exec::{assert_strong_count_converges_to_zero, BlockingExec};
    use crate::test::{self, assert_is_pending, make_partition};
    use crate::{collect, common, ExecutionMode};

    use arrow::array::{ArrayRef, Int32Array, StringArray, TimestampNanosecondArray};
    use arrow::compute::SortOptions;
    use arrow::datatypes::{DataType, Field, Schema};
    use arrow::record_batch::RecordBatch;
    use arrow_schema::SchemaRef;
    use datafusion_common::{assert_batches_eq, assert_contains, DataFusionError};
    use datafusion_common_runtime::SpawnedTask;
    use datafusion_execution::config::SessionConfig;
    use datafusion_execution::RecordBatchStream;
    use datafusion_physical_expr::expressions::Column;
    use datafusion_physical_expr::EquivalenceProperties;
    use datafusion_physical_expr_common::physical_expr::PhysicalExpr;

    use futures::{FutureExt, Stream, StreamExt};
    use tokio::time::timeout;

    #[tokio::test]
    async fn test_merge_interleave() {
        let task_ctx = Arc::new(TaskContext::default());
        let a: ArrayRef = Arc::new(Int32Array::from(vec![1, 2, 7, 9, 3]));
        let b: ArrayRef = Arc::new(StringArray::from_iter(vec![
            Some("a"),
            Some("c"),
            Some("e"),
            Some("g"),
            Some("j"),
        ]));
        let c: ArrayRef = Arc::new(TimestampNanosecondArray::from(vec![8, 7, 6, 5, 8]));
        let b1 = RecordBatch::try_from_iter(vec![("a", a), ("b", b), ("c", c)]).unwrap();

        let a: ArrayRef = Arc::new(Int32Array::from(vec![10, 20, 70, 90, 30]));
        let b: ArrayRef = Arc::new(StringArray::from_iter(vec![
            Some("b"),
            Some("d"),
            Some("f"),
            Some("h"),
            Some("j"),
        ]));
        let c: ArrayRef = Arc::new(TimestampNanosecondArray::from(vec![4, 6, 2, 2, 6]));
        let b2 = RecordBatch::try_from_iter(vec![("a", a), ("b", b), ("c", c)]).unwrap();

        _test_merge(
            &[vec![b1], vec![b2]],
            &[
                "+----+---+-------------------------------+",
                "| a  | b | c                             |",
                "+----+---+-------------------------------+",
                "| 1  | a | 1970-01-01T00:00:00.000000008 |",
                "| 10 | b | 1970-01-01T00:00:00.000000004 |",
                "| 2  | c | 1970-01-01T00:00:00.000000007 |",
                "| 20 | d | 1970-01-01T00:00:00.000000006 |",
                "| 7  | e | 1970-01-01T00:00:00.000000006 |",
                "| 70 | f | 1970-01-01T00:00:00.000000002 |",
                "| 9  | g | 1970-01-01T00:00:00.000000005 |",
                "| 90 | h | 1970-01-01T00:00:00.000000002 |",
                "| 30 | j | 1970-01-01T00:00:00.000000006 |", // input b2 before b1
                "| 3  | j | 1970-01-01T00:00:00.000000008 |",
                "+----+---+-------------------------------+",
            ],
            task_ctx,
        )
        .await;
    }

    #[tokio::test]
    async fn test_merge_no_exprs() {
        let task_ctx = Arc::new(TaskContext::default());
        let a: ArrayRef = Arc::new(Int32Array::from(vec![1, 2, 7, 9, 3]));
        let batch = RecordBatch::try_from_iter(vec![("a", a)]).unwrap();

        let schema = batch.schema();
        let sort = LexOrdering::default(); // no sort expressions
        let exec = MemoryExec::try_new(&[vec![batch.clone()], vec![batch]], schema, None)
            .unwrap();
        let merge = Arc::new(SortPreservingMergeExec::new(sort, Arc::new(exec)));

        let res = collect(merge, task_ctx).await.unwrap_err();
        assert_contains!(
            res.to_string(),
            "Internal error: Sort expressions cannot be empty for streaming merge"
        );
    }

    #[tokio::test]
    async fn test_merge_some_overlap() {
        let task_ctx = Arc::new(TaskContext::default());
        let a: ArrayRef = Arc::new(Int32Array::from(vec![1, 2, 7, 9, 3]));
        let b: ArrayRef = Arc::new(StringArray::from_iter(vec![
            Some("a"),
            Some("b"),
            Some("c"),
            Some("d"),
            Some("e"),
        ]));
        let c: ArrayRef = Arc::new(TimestampNanosecondArray::from(vec![8, 7, 6, 5, 8]));
        let b1 = RecordBatch::try_from_iter(vec![("a", a), ("b", b), ("c", c)]).unwrap();

        let a: ArrayRef = Arc::new(Int32Array::from(vec![70, 90, 30, 100, 110]));
        let b: ArrayRef = Arc::new(StringArray::from_iter(vec![
            Some("c"),
            Some("d"),
            Some("e"),
            Some("f"),
            Some("g"),
        ]));
        let c: ArrayRef = Arc::new(TimestampNanosecondArray::from(vec![4, 6, 2, 2, 6]));
        let b2 = RecordBatch::try_from_iter(vec![("a", a), ("b", b), ("c", c)]).unwrap();

        _test_merge(
            &[vec![b1], vec![b2]],
            &[
                "+-----+---+-------------------------------+",
                "| a   | b | c                             |",
                "+-----+---+-------------------------------+",
                "| 1   | a | 1970-01-01T00:00:00.000000008 |",
                "| 2   | b | 1970-01-01T00:00:00.000000007 |",
                "| 70  | c | 1970-01-01T00:00:00.000000004 |",
                "| 7   | c | 1970-01-01T00:00:00.000000006 |",
                "| 9   | d | 1970-01-01T00:00:00.000000005 |",
                "| 90  | d | 1970-01-01T00:00:00.000000006 |",
                "| 30  | e | 1970-01-01T00:00:00.000000002 |",
                "| 3   | e | 1970-01-01T00:00:00.000000008 |",
                "| 100 | f | 1970-01-01T00:00:00.000000002 |",
                "| 110 | g | 1970-01-01T00:00:00.000000006 |",
                "+-----+---+-------------------------------+",
            ],
            task_ctx,
        )
        .await;
    }

    #[tokio::test]
    async fn test_merge_no_overlap() {
        let task_ctx = Arc::new(TaskContext::default());
        let a: ArrayRef = Arc::new(Int32Array::from(vec![1, 2, 7, 9, 3]));
        let b: ArrayRef = Arc::new(StringArray::from_iter(vec![
            Some("a"),
            Some("b"),
            Some("c"),
            Some("d"),
            Some("e"),
        ]));
        let c: ArrayRef = Arc::new(TimestampNanosecondArray::from(vec![8, 7, 6, 5, 8]));
        let b1 = RecordBatch::try_from_iter(vec![("a", a), ("b", b), ("c", c)]).unwrap();

        let a: ArrayRef = Arc::new(Int32Array::from(vec![10, 20, 70, 90, 30]));
        let b: ArrayRef = Arc::new(StringArray::from_iter(vec![
            Some("f"),
            Some("g"),
            Some("h"),
            Some("i"),
            Some("j"),
        ]));
        let c: ArrayRef = Arc::new(TimestampNanosecondArray::from(vec![4, 6, 2, 2, 6]));
        let b2 = RecordBatch::try_from_iter(vec![("a", a), ("b", b), ("c", c)]).unwrap();

        _test_merge(
            &[vec![b1], vec![b2]],
            &[
                "+----+---+-------------------------------+",
                "| a  | b | c                             |",
                "+----+---+-------------------------------+",
                "| 1  | a | 1970-01-01T00:00:00.000000008 |",
                "| 2  | b | 1970-01-01T00:00:00.000000007 |",
                "| 7  | c | 1970-01-01T00:00:00.000000006 |",
                "| 9  | d | 1970-01-01T00:00:00.000000005 |",
                "| 3  | e | 1970-01-01T00:00:00.000000008 |",
                "| 10 | f | 1970-01-01T00:00:00.000000004 |",
                "| 20 | g | 1970-01-01T00:00:00.000000006 |",
                "| 70 | h | 1970-01-01T00:00:00.000000002 |",
                "| 90 | i | 1970-01-01T00:00:00.000000002 |",
                "| 30 | j | 1970-01-01T00:00:00.000000006 |",
                "+----+---+-------------------------------+",
            ],
            task_ctx,
        )
        .await;
    }

    #[tokio::test]
    async fn test_merge_three_partitions() {
        let task_ctx = Arc::new(TaskContext::default());
        let a: ArrayRef = Arc::new(Int32Array::from(vec![1, 2, 7, 9, 3]));
        let b: ArrayRef = Arc::new(StringArray::from_iter(vec![
            Some("a"),
            Some("b"),
            Some("c"),
            Some("d"),
            Some("f"),
        ]));
        let c: ArrayRef = Arc::new(TimestampNanosecondArray::from(vec![8, 7, 6, 5, 8]));
        let b1 = RecordBatch::try_from_iter(vec![("a", a), ("b", b), ("c", c)]).unwrap();

        let a: ArrayRef = Arc::new(Int32Array::from(vec![10, 20, 70, 90, 30]));
        let b: ArrayRef = Arc::new(StringArray::from_iter(vec![
            Some("e"),
            Some("g"),
            Some("h"),
            Some("i"),
            Some("j"),
        ]));
        let c: ArrayRef =
            Arc::new(TimestampNanosecondArray::from(vec![40, 60, 20, 20, 60]));
        let b2 = RecordBatch::try_from_iter(vec![("a", a), ("b", b), ("c", c)]).unwrap();

        let a: ArrayRef = Arc::new(Int32Array::from(vec![100, 200, 700, 900, 300]));
        let b: ArrayRef = Arc::new(StringArray::from_iter(vec![
            Some("f"),
            Some("g"),
            Some("h"),
            Some("i"),
            Some("j"),
        ]));
        let c: ArrayRef = Arc::new(TimestampNanosecondArray::from(vec![4, 6, 2, 2, 6]));
        let b3 = RecordBatch::try_from_iter(vec![("a", a), ("b", b), ("c", c)]).unwrap();

        _test_merge(
            &[vec![b1], vec![b2], vec![b3]],
            &[
                "+-----+---+-------------------------------+",
                "| a   | b | c                             |",
                "+-----+---+-------------------------------+",
                "| 1   | a | 1970-01-01T00:00:00.000000008 |",
                "| 2   | b | 1970-01-01T00:00:00.000000007 |",
                "| 7   | c | 1970-01-01T00:00:00.000000006 |",
                "| 9   | d | 1970-01-01T00:00:00.000000005 |",
                "| 10  | e | 1970-01-01T00:00:00.000000040 |",
                "| 100 | f | 1970-01-01T00:00:00.000000004 |",
                "| 3   | f | 1970-01-01T00:00:00.000000008 |",
                "| 200 | g | 1970-01-01T00:00:00.000000006 |",
                "| 20  | g | 1970-01-01T00:00:00.000000060 |",
                "| 700 | h | 1970-01-01T00:00:00.000000002 |",
                "| 70  | h | 1970-01-01T00:00:00.000000020 |",
                "| 900 | i | 1970-01-01T00:00:00.000000002 |",
                "| 90  | i | 1970-01-01T00:00:00.000000020 |",
                "| 300 | j | 1970-01-01T00:00:00.000000006 |",
                "| 30  | j | 1970-01-01T00:00:00.000000060 |",
                "+-----+---+-------------------------------+",
            ],
            task_ctx,
        )
        .await;
    }

    async fn _test_merge(
        partitions: &[Vec<RecordBatch>],
        exp: &[&str],
        context: Arc<TaskContext>,
    ) {
        let schema = partitions[0][0].schema();
        let sort = LexOrdering::new(vec![
            PhysicalSortExpr {
                expr: col("b", &schema).unwrap(),
                options: Default::default(),
            },
            PhysicalSortExpr {
                expr: col("c", &schema).unwrap(),
                options: Default::default(),
            },
        ]);
        let exec = MemoryExec::try_new(partitions, schema, None).unwrap();
        let merge = Arc::new(SortPreservingMergeExec::new(sort, Arc::new(exec)));

        let collected = collect(merge, context).await.unwrap();
        assert_batches_eq!(exp, collected.as_slice());
    }

    async fn sorted_merge(
        input: Arc<dyn ExecutionPlan>,
        sort: LexOrdering,
        context: Arc<TaskContext>,
    ) -> RecordBatch {
        let merge = Arc::new(SortPreservingMergeExec::new(sort, input));
        let mut result = collect(merge, context).await.unwrap();
        assert_eq!(result.len(), 1);
        result.remove(0)
    }

    async fn partition_sort(
        input: Arc<dyn ExecutionPlan>,
        sort: LexOrdering,
        context: Arc<TaskContext>,
    ) -> RecordBatch {
        let sort_exec =
            Arc::new(SortExec::new(sort.clone(), input).with_preserve_partitioning(true));
        sorted_merge(sort_exec, sort, context).await
    }

    async fn basic_sort(
        src: Arc<dyn ExecutionPlan>,
        sort: LexOrdering,
        context: Arc<TaskContext>,
    ) -> RecordBatch {
        let merge = Arc::new(CoalescePartitionsExec::new(src));
        let sort_exec = Arc::new(SortExec::new(sort, merge));
        let mut result = collect(sort_exec, context).await.unwrap();
        assert_eq!(result.len(), 1);
        result.remove(0)
    }

    #[tokio::test]
    async fn test_partition_sort() -> Result<()> {
        let task_ctx = Arc::new(TaskContext::default());
        let partitions = 4;
        let csv = test::scan_partitioned(partitions);
        let schema = csv.schema();

        let sort = LexOrdering::new(vec![PhysicalSortExpr {
            expr: col("i", &schema).unwrap(),
            options: SortOptions {
                descending: true,
                nulls_first: true,
            },
        }]);

        let basic =
            basic_sort(Arc::clone(&csv), sort.clone(), Arc::clone(&task_ctx)).await;
        let partition = partition_sort(csv, sort, Arc::clone(&task_ctx)).await;

        let basic = arrow::util::pretty::pretty_format_batches(&[basic])
            .unwrap()
            .to_string();
        let partition = arrow::util::pretty::pretty_format_batches(&[partition])
            .unwrap()
            .to_string();

        assert_eq!(
            basic, partition,
            "basic:\n\n{basic}\n\npartition:\n\n{partition}\n\n"
        );

        Ok(())
    }

    // Split the provided record batch into multiple batch_size record batches
    fn split_batch(sorted: &RecordBatch, batch_size: usize) -> Vec<RecordBatch> {
        let batches = (sorted.num_rows() + batch_size - 1) / batch_size;

        // Split the sorted RecordBatch into multiple
        (0..batches)
            .map(|batch_idx| {
                let columns = (0..sorted.num_columns())
                    .map(|column_idx| {
                        let length =
                            batch_size.min(sorted.num_rows() - batch_idx * batch_size);

                        sorted
                            .column(column_idx)
                            .slice(batch_idx * batch_size, length)
                    })
                    .collect();

                RecordBatch::try_new(sorted.schema(), columns).unwrap()
            })
            .collect()
    }

    async fn sorted_partitioned_input(
        sort: LexOrdering,
        sizes: &[usize],
        context: Arc<TaskContext>,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        let partitions = 4;
        let csv = test::scan_partitioned(partitions);

        let sorted = basic_sort(csv, sort, context).await;
        let split: Vec<_> = sizes.iter().map(|x| split_batch(&sorted, *x)).collect();

        Ok(Arc::new(
            MemoryExec::try_new(&split, sorted.schema(), None).unwrap(),
        ))
    }

    #[tokio::test]
    async fn test_partition_sort_streaming_input() -> Result<()> {
        let task_ctx = Arc::new(TaskContext::default());
        let schema = make_partition(11).schema();
        let sort = LexOrdering::new(vec![PhysicalSortExpr {
            expr: col("i", &schema).unwrap(),
            options: Default::default(),
        }]);

        let input =
            sorted_partitioned_input(sort.clone(), &[10, 3, 11], Arc::clone(&task_ctx))
                .await?;
        let basic =
            basic_sort(Arc::clone(&input), sort.clone(), Arc::clone(&task_ctx)).await;
        let partition = sorted_merge(input, sort, Arc::clone(&task_ctx)).await;

        assert_eq!(basic.num_rows(), 1200);
        assert_eq!(partition.num_rows(), 1200);

        let basic = arrow::util::pretty::pretty_format_batches(&[basic])
            .unwrap()
            .to_string();
        let partition = arrow::util::pretty::pretty_format_batches(&[partition])
            .unwrap()
            .to_string();

        assert_eq!(basic, partition);

        Ok(())
    }

    #[tokio::test]
    async fn test_partition_sort_streaming_input_output() -> Result<()> {
        let schema = make_partition(11).schema();
        let sort = LexOrdering::new(vec![PhysicalSortExpr {
            expr: col("i", &schema).unwrap(),
            options: Default::default(),
        }]);

        // Test streaming with default batch size
        let task_ctx = Arc::new(TaskContext::default());
        let input =
            sorted_partitioned_input(sort.clone(), &[10, 5, 13], Arc::clone(&task_ctx))
                .await?;
        let basic = basic_sort(Arc::clone(&input), sort.clone(), task_ctx).await;

        // batch size of 23
        let task_ctx = TaskContext::default()
            .with_session_config(SessionConfig::new().with_batch_size(23));
        let task_ctx = Arc::new(task_ctx);

        let merge = Arc::new(SortPreservingMergeExec::new(sort, input));
        let merged = collect(merge, task_ctx).await.unwrap();

        assert_eq!(merged.len(), 53);

        assert_eq!(basic.num_rows(), 1200);
        assert_eq!(merged.iter().map(|x| x.num_rows()).sum::<usize>(), 1200);

        let basic = arrow::util::pretty::pretty_format_batches(&[basic])
            .unwrap()
            .to_string();
        let partition = arrow::util::pretty::pretty_format_batches(merged.as_slice())
            .unwrap()
            .to_string();

        assert_eq!(basic, partition);

        Ok(())
    }

    #[tokio::test]
    async fn test_nulls() {
        let task_ctx = Arc::new(TaskContext::default());
        let a: ArrayRef = Arc::new(Int32Array::from(vec![1, 2, 7, 9, 3]));
        let b: ArrayRef = Arc::new(StringArray::from_iter(vec![
            None,
            Some("a"),
            Some("b"),
            Some("d"),
            Some("e"),
        ]));
        let c: ArrayRef = Arc::new(TimestampNanosecondArray::from(vec![
            Some(8),
            None,
            Some(6),
            None,
            Some(4),
        ]));
        let b1 = RecordBatch::try_from_iter(vec![("a", a), ("b", b), ("c", c)]).unwrap();

        let a: ArrayRef = Arc::new(Int32Array::from(vec![1, 2, 3, 4, 5]));
        let b: ArrayRef = Arc::new(StringArray::from_iter(vec![
            None,
            Some("b"),
            Some("g"),
            Some("h"),
            Some("i"),
        ]));
        let c: ArrayRef = Arc::new(TimestampNanosecondArray::from(vec![
            Some(8),
            None,
            Some(5),
            None,
            Some(4),
        ]));
        let b2 = RecordBatch::try_from_iter(vec![("a", a), ("b", b), ("c", c)]).unwrap();
        let schema = b1.schema();

        let sort = LexOrdering::new(vec![
            PhysicalSortExpr {
                expr: col("b", &schema).unwrap(),
                options: SortOptions {
                    descending: false,
                    nulls_first: true,
                },
            },
            PhysicalSortExpr {
                expr: col("c", &schema).unwrap(),
                options: SortOptions {
                    descending: false,
                    nulls_first: false,
                },
            },
        ]);
        let exec = MemoryExec::try_new(&[vec![b1], vec![b2]], schema, None).unwrap();
        let merge = Arc::new(SortPreservingMergeExec::new(sort, Arc::new(exec)));

        let collected = collect(merge, task_ctx).await.unwrap();
        assert_eq!(collected.len(), 1);

        assert_batches_eq!(
            &[
                "+---+---+-------------------------------+",
                "| a | b | c                             |",
                "+---+---+-------------------------------+",
                "| 1 |   | 1970-01-01T00:00:00.000000008 |",
                "| 1 |   | 1970-01-01T00:00:00.000000008 |",
                "| 2 | a |                               |",
                "| 7 | b | 1970-01-01T00:00:00.000000006 |",
                "| 2 | b |                               |",
                "| 9 | d |                               |",
                "| 3 | e | 1970-01-01T00:00:00.000000004 |",
                "| 3 | g | 1970-01-01T00:00:00.000000005 |",
                "| 4 | h |                               |",
                "| 5 | i | 1970-01-01T00:00:00.000000004 |",
                "+---+---+-------------------------------+",
            ],
            collected.as_slice()
        );
    }

    #[tokio::test]
    async fn test_sort_merge_single_partition_with_fetch() {
        let task_ctx = Arc::new(TaskContext::default());
        let a: ArrayRef = Arc::new(Int32Array::from(vec![1, 2, 7, 9, 3]));
        let b: ArrayRef = Arc::new(StringArray::from(vec!["a", "b", "c", "d", "e"]));
        let batch = RecordBatch::try_from_iter(vec![("a", a), ("b", b)]).unwrap();
        let schema = batch.schema();

        let sort = LexOrdering::new(vec![PhysicalSortExpr {
            expr: col("b", &schema).unwrap(),
            options: SortOptions {
                descending: false,
                nulls_first: true,
            },
        }]);
        let exec = MemoryExec::try_new(&[vec![batch]], schema, None).unwrap();
        let merge = Arc::new(
            SortPreservingMergeExec::new(sort, Arc::new(exec)).with_fetch(Some(2)),
        );

        let collected = collect(merge, task_ctx).await.unwrap();
        assert_eq!(collected.len(), 1);

        assert_batches_eq!(
            &[
                "+---+---+",
                "| a | b |",
                "+---+---+",
                "| 1 | a |",
                "| 2 | b |",
                "+---+---+",
            ],
            collected.as_slice()
        );
    }

    #[tokio::test]
    async fn test_sort_merge_single_partition_without_fetch() {
        let task_ctx = Arc::new(TaskContext::default());
        let a: ArrayRef = Arc::new(Int32Array::from(vec![1, 2, 7, 9, 3]));
        let b: ArrayRef = Arc::new(StringArray::from(vec!["a", "b", "c", "d", "e"]));
        let batch = RecordBatch::try_from_iter(vec![("a", a), ("b", b)]).unwrap();
        let schema = batch.schema();

        let sort = LexOrdering::new(vec![PhysicalSortExpr {
            expr: col("b", &schema).unwrap(),
            options: SortOptions {
                descending: false,
                nulls_first: true,
            },
        }]);
        let exec = MemoryExec::try_new(&[vec![batch]], schema, None).unwrap();
        let merge = Arc::new(SortPreservingMergeExec::new(sort, Arc::new(exec)));

        let collected = collect(merge, task_ctx).await.unwrap();
        assert_eq!(collected.len(), 1);

        assert_batches_eq!(
            &[
                "+---+---+",
                "| a | b |",
                "+---+---+",
                "| 1 | a |",
                "| 2 | b |",
                "| 7 | c |",
                "| 9 | d |",
                "| 3 | e |",
                "+---+---+",
            ],
            collected.as_slice()
        );
    }

    #[tokio::test]
    async fn test_async() -> Result<()> {
        let task_ctx = Arc::new(TaskContext::default());
        let schema = make_partition(11).schema();
        let sort = LexOrdering::new(vec![PhysicalSortExpr {
            expr: col("i", &schema).unwrap(),
            options: SortOptions::default(),
        }]);

        let batches =
            sorted_partitioned_input(sort.clone(), &[5, 7, 3], Arc::clone(&task_ctx))
                .await?;

        let partition_count = batches.output_partitioning().partition_count();
        let mut streams = Vec::with_capacity(partition_count);

        for partition in 0..partition_count {
            let mut builder = RecordBatchReceiverStream::builder(Arc::clone(&schema), 1);

            let sender = builder.tx();

            let mut stream = batches.execute(partition, Arc::clone(&task_ctx)).unwrap();
            builder.spawn(async move {
                while let Some(batch) = stream.next().await {
                    sender.send(batch).await.unwrap();
                    // This causes the MergeStream to wait for more input
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }

                Ok(())
            });

            streams.push(builder.build());
        }

        let metrics = ExecutionPlanMetricsSet::new();
        let reservation =
            MemoryConsumer::new("test").register(&task_ctx.runtime_env().memory_pool);

        let fetch = None;
        let merge_stream = StreamingMergeBuilder::new()
            .with_streams(streams)
            .with_schema(batches.schema())
            .with_expressions(sort.as_ref().inner)
            .with_metrics(BaselineMetrics::new(&metrics, 0))
            .with_batch_size(task_ctx.session_config().batch_size())
            .with_fetch(fetch)
            .with_reservation(reservation)
            .build()?;

        let mut merged = common::collect(merge_stream).await.unwrap();

        assert_eq!(merged.len(), 1);
        let merged = merged.remove(0);
        let basic = basic_sort(batches, sort.clone(), Arc::clone(&task_ctx)).await;

        let basic = arrow::util::pretty::pretty_format_batches(&[basic])
            .unwrap()
            .to_string();
        let partition = arrow::util::pretty::pretty_format_batches(&[merged])
            .unwrap()
            .to_string();

        assert_eq!(
            basic, partition,
            "basic:\n\n{basic}\n\npartition:\n\n{partition}\n\n"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_merge_metrics() {
        let task_ctx = Arc::new(TaskContext::default());
        let a: ArrayRef = Arc::new(Int32Array::from(vec![1, 2]));
        let b: ArrayRef = Arc::new(StringArray::from_iter(vec![Some("a"), Some("c")]));
        let b1 = RecordBatch::try_from_iter(vec![("a", a), ("b", b)]).unwrap();

        let a: ArrayRef = Arc::new(Int32Array::from(vec![10, 20]));
        let b: ArrayRef = Arc::new(StringArray::from_iter(vec![Some("b"), Some("d")]));
        let b2 = RecordBatch::try_from_iter(vec![("a", a), ("b", b)]).unwrap();

        let schema = b1.schema();
        let sort = LexOrdering::new(vec![PhysicalSortExpr {
            expr: col("b", &schema).unwrap(),
            options: Default::default(),
        }]);
        let exec = MemoryExec::try_new(&[vec![b1], vec![b2]], schema, None).unwrap();
        let merge = Arc::new(SortPreservingMergeExec::new(sort, Arc::new(exec)));

        let collected = collect(Arc::clone(&merge) as Arc<dyn ExecutionPlan>, task_ctx)
            .await
            .unwrap();
        let expected = [
            "+----+---+",
            "| a  | b |",
            "+----+---+",
            "| 1  | a |",
            "| 10 | b |",
            "| 2  | c |",
            "| 20 | d |",
            "+----+---+",
        ];
        assert_batches_eq!(expected, collected.as_slice());

        // Now, validate metrics
        let metrics = merge.metrics().unwrap();

        assert_eq!(metrics.output_rows().unwrap(), 4);
        assert!(metrics.elapsed_compute().unwrap() > 0);

        let mut saw_start = false;
        let mut saw_end = false;
        metrics.iter().for_each(|m| match m.value() {
            MetricValue::StartTimestamp(ts) => {
                saw_start = true;
                assert!(nanos_from_timestamp(ts) > 0);
            }
            MetricValue::EndTimestamp(ts) => {
                saw_end = true;
                assert!(nanos_from_timestamp(ts) > 0);
            }
            _ => {}
        });

        assert!(saw_start);
        assert!(saw_end);
    }

    fn nanos_from_timestamp(ts: &Timestamp) -> i64 {
        ts.value().unwrap().timestamp_nanos_opt().unwrap()
    }

    #[tokio::test]
    async fn test_drop_cancel() -> Result<()> {
        let task_ctx = Arc::new(TaskContext::default());
        let schema =
            Arc::new(Schema::new(vec![Field::new("a", DataType::Float32, true)]));

        let blocking_exec = Arc::new(BlockingExec::new(Arc::clone(&schema), 2));
        let refs = blocking_exec.refs();
        let sort_preserving_merge_exec = Arc::new(SortPreservingMergeExec::new(
            LexOrdering::new(vec![PhysicalSortExpr {
                expr: col("a", &schema)?,
                options: SortOptions::default(),
            }]),
            blocking_exec,
        ));

        let fut = collect(sort_preserving_merge_exec, task_ctx);
        let mut fut = fut.boxed();

        assert_is_pending(&mut fut);
        drop(fut);
        assert_strong_count_converges_to_zero(refs).await;

        Ok(())
    }

    #[tokio::test]
    async fn test_stable_sort() {
        let task_ctx = Arc::new(TaskContext::default());

        // Create record batches like:
        // batch_number |value
        // -------------+------
        //    1         | A
        //    1         | B
        //
        // Ensure that the output is in the same order the batches were fed
        let partitions: Vec<Vec<RecordBatch>> = (0..10)
            .map(|batch_number| {
                let batch_number: Int32Array =
                    vec![Some(batch_number), Some(batch_number)]
                        .into_iter()
                        .collect();
                let value: StringArray = vec![Some("A"), Some("B")].into_iter().collect();

                let batch = RecordBatch::try_from_iter(vec![
                    ("batch_number", Arc::new(batch_number) as ArrayRef),
                    ("value", Arc::new(value) as ArrayRef),
                ])
                .unwrap();

                vec![batch]
            })
            .collect();

        let schema = partitions[0][0].schema();

        let sort = LexOrdering::new(vec![PhysicalSortExpr {
            expr: col("value", &schema).unwrap(),
            options: SortOptions {
                descending: false,
                nulls_first: true,
            },
        }]);

        let exec = MemoryExec::try_new(&partitions, schema, None).unwrap();
        let merge = Arc::new(SortPreservingMergeExec::new(sort, Arc::new(exec)));

        let collected = collect(merge, task_ctx).await.unwrap();
        assert_eq!(collected.len(), 1);

        // Expect the data to be sorted first by "batch_number" (because
        // that was the order it was fed in, even though only "value"
        // is in the sort key)
        assert_batches_eq!(
            &[
                "+--------------+-------+",
                "| batch_number | value |",
                "+--------------+-------+",
                "| 0            | A     |",
                "| 1            | A     |",
                "| 2            | A     |",
                "| 3            | A     |",
                "| 4            | A     |",
                "| 5            | A     |",
                "| 6            | A     |",
                "| 7            | A     |",
                "| 8            | A     |",
                "| 9            | A     |",
                "| 0            | B     |",
                "| 1            | B     |",
                "| 2            | B     |",
                "| 3            | B     |",
                "| 4            | B     |",
                "| 5            | B     |",
                "| 6            | B     |",
                "| 7            | B     |",
                "| 8            | B     |",
                "| 9            | B     |",
                "+--------------+-------+",
            ],
            collected.as_slice()
        );
    }

    /// It returns pending for the 2nd partition until the 3rd partition is polled. The 1st
    /// partition is exhausted from the start, and if it is polled more than one, it panics.
    #[derive(Debug, Clone)]
    struct CongestedExec {
        schema: Schema,
        cache: PlanProperties,
        congestion_cleared: Arc<Mutex<bool>>,
    }

    impl CongestedExec {
        fn compute_properties(schema: SchemaRef) -> PlanProperties {
            let columns = schema
                .fields
                .iter()
                .enumerate()
                .map(|(i, f)| Arc::new(Column::new(f.name(), i)) as Arc<dyn PhysicalExpr>)
                .collect::<Vec<_>>();
            let mut eq_properties = EquivalenceProperties::new(schema);
            eq_properties.add_new_orderings(vec![columns
                .iter()
                .map(|expr| PhysicalSortExpr::new_default(Arc::clone(expr)))
                .collect::<LexOrdering>()]);
            let mode = ExecutionMode::Unbounded;
            PlanProperties::new(eq_properties, Partitioning::Hash(columns, 3), mode)
        }
    }

    impl ExecutionPlan for CongestedExec {
        fn name(&self) -> &'static str {
            Self::static_name()
        }
        fn as_any(&self) -> &dyn Any {
            self
        }
        fn properties(&self) -> &PlanProperties {
            &self.cache
        }
        fn children(&self) -> Vec<&Arc<dyn ExecutionPlan>> {
            vec![]
        }
        fn with_new_children(
            self: Arc<Self>,
            _: Vec<Arc<dyn ExecutionPlan>>,
        ) -> Result<Arc<dyn ExecutionPlan>> {
            Ok(self)
        }
        fn execute(
            &self,
            partition: usize,
            _context: Arc<TaskContext>,
        ) -> Result<SendableRecordBatchStream> {
            Ok(Box::pin(CongestedStream {
                schema: Arc::new(self.schema.clone()),
                none_polled_once: false,
                congestion_cleared: Arc::clone(&self.congestion_cleared),
                partition,
            }))
        }
    }

    impl DisplayAs for CongestedExec {
        fn fmt_as(&self, t: DisplayFormatType, f: &mut Formatter) -> std::fmt::Result {
            match t {
                DisplayFormatType::Default | DisplayFormatType::Verbose => {
                    write!(f, "CongestedExec",).unwrap()
                }
            }
            Ok(())
        }
    }

    /// It returns pending for the 2nd partition until the 3rd partition is polled. The 1st
    /// partition is exhausted from the start, and if it is polled more than once, it panics.
    #[derive(Debug)]
    pub struct CongestedStream {
        schema: SchemaRef,
        none_polled_once: bool,
        congestion_cleared: Arc<Mutex<bool>>,
        partition: usize,
    }

    impl Stream for CongestedStream {
        type Item = Result<RecordBatch>;
        fn poll_next(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Option<Self::Item>> {
            match self.partition {
                0 => {
                    if self.none_polled_once {
                        panic!("Exhausted stream is polled more than one")
                    } else {
                        self.none_polled_once = true;
                        Poll::Ready(None)
                    }
                }
                1 => {
                    let cleared = self.congestion_cleared.lock().unwrap();
                    if *cleared {
                        Poll::Ready(None)
                    } else {
                        Poll::Pending
                    }
                }
                2 => {
                    let mut cleared = self.congestion_cleared.lock().unwrap();
                    *cleared = true;
                    Poll::Ready(None)
                }
                _ => unreachable!(),
            }
        }
    }

    impl RecordBatchStream for CongestedStream {
        fn schema(&self) -> SchemaRef {
            Arc::clone(&self.schema)
        }
    }

    #[tokio::test]
    async fn test_spm_congestion() -> Result<()> {
        let task_ctx = Arc::new(TaskContext::default());
        let schema = Schema::new(vec![Field::new("c1", DataType::UInt64, false)]);
        let source = CongestedExec {
            schema: schema.clone(),
            cache: CongestedExec::compute_properties(Arc::new(schema.clone())),
            congestion_cleared: Arc::new(Mutex::new(false)),
        };
        let spm = SortPreservingMergeExec::new(
            LexOrdering::new(vec![PhysicalSortExpr::new_default(Arc::new(Column::new(
                "c1", 0,
            )))]),
            Arc::new(source),
        );
        let spm_task = SpawnedTask::spawn(collect(Arc::new(spm), task_ctx));

        let result = timeout(Duration::from_secs(3), spm_task.join()).await;
        match result {
            Ok(Ok(Ok(_batches))) => Ok(()),
            Ok(Ok(Err(e))) => Err(e),
            Ok(Err(_)) => Err(DataFusionError::Execution(
                "SortPreservingMerge task panicked or was cancelled".to_string(),
            )),
            Err(_) => Err(DataFusionError::Execution(
                "SortPreservingMerge caused a deadlock".to_string(),
            )),
        }
    }
}
