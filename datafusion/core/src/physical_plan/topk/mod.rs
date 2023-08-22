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

//! TopK: Combination of Sort / LIMIT

use arrow::{
    compute::interleave,
    row::{RowConverter, Rows, SortField},
};
use std::{cmp::Ordering, sync::Arc};

use arrow_array::{Array, ArrayRef, RecordBatch};
use arrow_schema::SchemaRef;
use datafusion_common::Result;
use datafusion_execution::{
    memory_pool::{MemoryConsumer, MemoryReservation},
    runtime_env::RuntimeEnv,
};
use datafusion_physical_expr::PhysicalSortExpr;
use hashbrown::HashMap;

use crate::physical_plan::{stream::RecordBatchStreamAdapter, SendableRecordBatchStream};

use super::metrics::{BaselineMetrics, Count, ExecutionPlanMetricsSet, MetricBuilder};

/// Global TopK
///
/// # Background
///
/// "Top K" is a common query optimization used for queries such as
/// "find the top 3 customers by revenue". The (simplified) SQL for
/// such a query might be:
///
/// ```sql
/// SELECT customer_id, revenue FROM 'sales.csv' ORDER BY revenue DESC limit 3;
/// ```
///
/// The simple plan would be:
///
/// ```
/// > explain SELECT customer_id, revenue FROM sales ORDER BY revenue DESC limit 3;
/// +--------------+----------------------------------------+
/// | plan_type    | plan                                   |
/// +--------------+----------------------------------------+
/// | logical_plan | Limit: 3                               |
/// |              |   Sort: revenue DESC NULLS FIRST       |
/// |              |     Projection: customer_id, revenue   |
/// |              |       TableScan: sales                 |
/// +--------------+----------------------------------------+
/// ```
///
/// While this plan produces the correct answer, it will fully sorts the
/// input before discarding everything other than the top 3 elements.
///
/// The same answer can be produced by simply keeping track of the top
/// N elements, reducing the total amount of required buffer memory.
///
/// # Structure
///
/// This operator tracks the top K items using a `TopKHeap`.
pub struct TopK {
    /// schema of the output (and the input)
    schema: SchemaRef,
    /// Runtime metrics
    metrics: TopKMetrics,
    /// Reservation
    reservation: MemoryReservation,
    /// The target number of rows for output batches
    batch_size: usize,
    /// sort expressions
    expr: Arc<[PhysicalSortExpr]>,
    /// row converter, for sort keys
    row_converter: RowConverter,
    /// scratch space for converting rows
    scratch_rows: Rows,
    /// stores the top k values and their sort key values, in order
    heap: TopKHeap,
}

impl TopK {
    /// Create a new [`TopK`] that stores the top `k` values, as
    /// defined by the sort expressions in `expr`.
    // TOOD: make a builder or some other nicer API to avoid the
    // clippy warning
    #[allow(clippy::too_many_arguments)]
    pub fn try_new(
        partition_id: usize,
        schema: SchemaRef,
        expr: Vec<PhysicalSortExpr>,
        k: usize,
        batch_size: usize,
        runtime: Arc<RuntimeEnv>,
        metrics: &ExecutionPlanMetricsSet,
        partition: usize,
    ) -> Result<Self> {
        let reservation = MemoryConsumer::new(format!("TopK[{partition_id}]"))
            .register(&runtime.memory_pool);

        let expr: Arc<[PhysicalSortExpr]> = expr.into();

        let sort_fields: Vec<_> = expr
            .iter()
            .map(|e| {
                Ok(SortField::new_with_options(
                    e.expr.data_type(&schema)?,
                    e.options,
                ))
            })
            .collect::<Result<_>>()?;

        let row_converter = RowConverter::new(sort_fields)?;
        let scratch_rows = row_converter.empty_rows(
            batch_size,
            20 * batch_size, // guestimate 20 bytes per row
        );

        Ok(Self {
            schema: schema.clone(),
            metrics: TopKMetrics::new(metrics, partition),
            reservation,
            batch_size,
            expr,
            row_converter,
            scratch_rows,
            heap: TopKHeap::new(k, schema),
        })
    }

    /// Insert `batch`, remembering it if any of its values are among
    /// the top k seen so far.
    pub fn insert_batch(&mut self, batch: RecordBatch) -> Result<()> {
        // Updates on drop
        let _timer = self.metrics.baseline.elapsed_compute().timer();

        let sort_keys: Vec<ArrayRef> = self
            .expr
            .iter()
            .map(|expr| {
                let value = expr.expr.evaluate(&batch)?;
                Ok(value.into_array(batch.num_rows()))
            })
            .collect::<Result<Vec<_>>>()?;

        // reuse existing `Rows` to avoid reallocations
        let rows = &mut self.scratch_rows;
        rows.clear();
        self.row_converter.append(rows, &sort_keys)?;

        // TODO make this algorithmically better?:
        // 1. only check topk values in rows
        // 2. only do one update through top_k

        let mut batch_entry = self.heap.register_batch(batch);
        for (index, row) in rows.iter().enumerate() {
            match self.heap.k_largest() {
                // heap has k items, and the current row is not
                // smaller than the curret smallest k value, skip
                Some(largest) if largest.row.as_slice() <= row.as_ref() => {}
                // don't yet have k items or new item is greater than
                // current min top k
                None | Some(_) => {
                    self.heap.add(&mut batch_entry, row, index);
                    self.metrics.row_replacements.add(1);
                }
            }
        }
        self.heap.insert_batch_entry(batch_entry);

        // update memory reservation
        self.reservation.try_resize(self.size())?;
        Ok(())
    }

    /// Returns the top k results broken into `batch_size` [`RecordBatch`]es
    pub fn emit(self) -> Result<SendableRecordBatchStream> {
        let Self {
            schema,
            metrics,
            reservation: _,
            batch_size,
            expr: _,
            row_converter: _,
            scratch_rows: _,
            heap,
        } = self;
        let _timer = metrics.baseline.elapsed_compute().timer(); // time updated on drop

        let mut batch = heap.emit()?;
        metrics.baseline.output_rows().add(batch.num_rows());

        // break into record batches as needed
        let mut batches = vec![];
        loop {
            if batch.num_rows() < batch_size {
                batches.push(Ok(batch));
                break;
            } else {
                batches.push(Ok(batch.slice(0, batch_size)));
                let remaining_length = batch.num_rows() - batch_size;
                batch = batch.slice(batch_size, remaining_length);
            }
        }
        Ok(Box::pin(RecordBatchStreamAdapter::new(
            schema,
            futures::stream::iter(batches),
        )))
    }

    /// return the size of memory used by this operator, in bytes
    fn size(&self) -> usize {
        std::mem::size_of::<Self>()
            + self.row_converter.size()
            + self.scratch_rows.size()
            + self.heap.size()
    }
}

struct TopKMetrics {
    /// metrics
    pub baseline: BaselineMetrics,

    /// count of how many rows were replaced in the heap
    pub row_replacements: Count,
}

impl TopKMetrics {
    fn new(metrics: &ExecutionPlanMetricsSet, partition: usize) -> Self {
        Self {
            baseline: BaselineMetrics::new(metrics, partition),
            row_replacements: MetricBuilder::new(metrics)
                .counter("row_replacements", partition),
        }
    }
}

/// This structure keeps at most the *smallest* k items, using the
/// [arrow::row] format for sort keys. While it is called "topK" for
/// values like `1, 2, 3, 4, 5` the "top 3" really means the
/// *smallest* 3 , `1, 2, 3`, not the *largest* 3 `3, 4, 5`.
///
/// Using the `Row` format handles things such as ascending vs
/// descending and nulls first vs nulls last.
///
/// It doesn't use `BinaryHeap` in the Rust standard library because
/// it is important to check the current minimum value in the heap
/// prior to creating a new value to insert.
struct TopKHeap {
    /// The maximum number of elemenents to store in this heap.
    k: usize,
    /// Storage for up at most `k` items, in ascending
    /// order. `inner[0]` holds the smallest value of the smallest k
    /// so far, `inner[len-1]` holds the largest value smallest k so far.
    inner: Vec<TopKRow>,
    /// Storage the original row values (TopKRow only has the sort key)
    store: RecordBatchStore,
    /// The size of all owned data held by this heap
    owned_bytes: usize,
}

impl TopKHeap {
    fn new(k: usize, schema: SchemaRef) -> Self {
        assert!(k > 0);
        Self {
            k,
            inner: Vec::with_capacity(k),
            store: RecordBatchStore::new(schema),
            owned_bytes: 0,
        }
    }

    /// Register a [`RecordBatch`] with the heap, returning the
    /// appropriate entry
    pub fn register_batch(&mut self, batch: RecordBatch) -> RecordBatchEntry {
        self.store.register(batch)
    }

    /// Insert a [`RecordBatchEntry`] created by a previous call to
    /// [`Self::register_batch`] into storage.
    pub fn insert_batch_entry(&mut self, entry: RecordBatchEntry) {
        self.store.insert(entry)
    }

    /// Returns the largest value stored by the heap if there are k
    /// items, otherwise returns None
    fn k_largest(&self) -> Option<&TopKRow> {
        if self.inner.len() < self.k {
            None
        } else {
            self.inner.last()
        }
    }

    /// Adds `row` to this heap. If inserting this new item would
    /// increase the size past `k`, removes the previously smallest
    /// item.
    fn add(
        &mut self,
        batch_entry: &mut RecordBatchEntry,
        row: impl AsRef<[u8]>,
        index: usize,
    ) {
        let batch_id = batch_entry.id;
        batch_entry.uses += 1;

        assert!(self.inner.len() <= self.k);
        let row = row.as_ref();

        // Reuse storage for evicted item if possible
        let new_top_k = if self.inner.len() == self.k {
            let prev_min = self.inner.pop().unwrap();

            // Update batch use
            if prev_min.batch_id == batch_entry.id {
                batch_entry.uses -= 1;
            } else {
                self.store.unuse(prev_min.batch_id);
            }

            // update memory accounting
            self.owned_bytes -= prev_min.owned_size();
            prev_min.with_new_row(row, batch_id, index)
        } else {
            TopKRow::new(row, batch_id, index)
        };

        self.owned_bytes += new_top_k.owned_size();

        // put the new row into the correct location to maintain that
        // self.inner is sorted in descending order
        let insertion_point = self
            .inner
            .partition_point(|current_row| current_row.row() <= row.as_ref());
        self.inner.insert(insertion_point, new_top_k);
    }

    /// Returns the values stored in this heap, from values low to high, as a single
    /// [`RecordBatch`]
    pub fn emit(&self) -> Result<RecordBatch> {
        let schema = self.store.schema().clone();

        // Indicies for each row within its respective RecordBatch
        let indicies: Vec<_> = self
            .inner
            .iter()
            .enumerate()
            .map(|(i, k)| (i, k.index))
            .collect();

        let num_columns = schema.fields().len();

        // build the output columns one at time, using the
        // `interleave` kernel to pick rows from different arrays
        let output_columns: Vec<_> = (0..num_columns)
            .map(|col| {
                let input_arrays: Vec<_> = self
                    .inner
                    .iter()
                    .map(|k| {
                        let entry =
                            self.store.get(k.batch_id).expect("invalid stored batch id");
                        entry.batch.column(col) as &dyn Array
                    })
                    .collect();

                // at this point `indices` contains indexes within the
                // rows and `input_arrays` contains a reference to the
                // relevant Array for that index. `interleave` pulls
                // them together into a single new array
                Ok(interleave(&input_arrays, &indicies)?)
            })
            .collect::<Result<_>>()?;

        Ok(RecordBatch::try_new(schema, output_columns)?)
    }

    /// Compact this heap, rewriting all stored batches
    fn compact(&mut self) {
        //let new_batch = self.emit(
    }

    /// return the size of memory used by this heap, in bytes
    fn size(&self) -> usize {
        std::mem::size_of::<Self>()
            + (self.inner.capacity() * std::mem::size_of::<TopKRow>())
            + self.store.size()
            + self.owned_bytes
    }
}

/// Represents one of the top K rows held in this heap. Orders
/// according to memcmp of row (e.g. the arrow Row format, but could
/// also be primtive values)
///
/// Reuses allocations to minimize runtime overhead of creating new Vecs
#[derive(Debug, PartialEq)]
struct TopKRow {
    /// the value of the sort key for this row. This contains the
    /// bytes that could be stored in `OwnedRow` but uses `Vec<u8>` to
    /// reuse allocations.
    row: Vec<u8>,
    /// the RecordBatch this row came from: an id into a [`RecordBatchStore`]
    batch_id: u32,
    /// the index in this record batch the row came from
    index: usize,
}

impl TopKRow {
    /// Create a new TopKRow with new allocation
    fn new(row: impl AsRef<[u8]>, batch_id: u32, index: usize) -> Self {
        Self {
            row: row.as_ref().to_vec(),
            batch_id,
            index,
        }
    }

    /// Create a new  TopKRow reusing the existing allocation
    fn with_new_row(
        self,
        new_row: impl AsRef<[u8]>,
        batch_id: u32,
        index: usize,
    ) -> Self {
        let Self {
            mut row,
            batch_id: _,
            index: _,
        } = self;
        row.clear();
        row.extend_from_slice(new_row.as_ref());

        Self {
            row,
            batch_id,
            index,
        }
    }

    /// Returns the number of bytes owned by this row in the heap (not
    /// including itself)
    fn owned_size(&self) -> usize {
        self.row.capacity()
    }

    /// Returns a slice to the owned row value
    fn row(&self) -> &[u8] {
        self.row.as_slice()
    }
}

impl Eq for TopKRow {}

impl PartialOrd for TopKRow {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for TopKRow {
    fn cmp(&self, other: &Self) -> Ordering {
        self.row.cmp(&other.row)
    }
}

#[derive(Debug)]
struct RecordBatchEntry {
    id: u32,
    batch: RecordBatch,
    // for this batch, how many times has it been used
    uses: usize,
}

/// This structure tracks [`RecordBatch`] by an id so that:
///
/// 1. The baches can be tracked via an id that can be copied cheaply
/// 2. The total memory held by all batches is tracked
#[derive(Debug)]
struct RecordBatchStore {
    /// id generator
    next_id: u32,
    /// storage
    batches: HashMap<u32, RecordBatchEntry>,
    /// total size of all record batches tracked by this store
    batches_size: usize,
    /// schema of the batches
    schema: SchemaRef,
}

impl RecordBatchStore {
    fn new(schema: SchemaRef) -> Self {
        Self {
            next_id: 0,
            batches: HashMap::new(),
            batches_size: 0,
            schema,
        }
    }

    /// Register this batch with the store and assign an ID. No
    /// attempt is made to compare this batch to other batches
    pub fn register(&mut self, batch: RecordBatch) -> RecordBatchEntry {
        let id = self.next_id;
        self.next_id += 1;
        RecordBatchEntry { id, batch, uses: 0 }
    }

    /// Insert a record batch entry into this store, tracking its
    /// memory use, if it has any uses
    pub fn insert(&mut self, entry: RecordBatchEntry) {
        // uses of 0 means that none of the rows in the batch were stored in the topk
        if entry.uses > 0 {
            self.batches_size += entry.batch.get_array_memory_size();
            self.batches.insert(entry.id, entry);
        }
    }

    fn get(&self, id: u32) -> Option<&RecordBatchEntry> {
        self.batches.get(&id)
    }

    /// returns the total number of batches stored in this store
    fn len(&self) -> usize {
        self.batches.len()
    }

    /// return the schema of batches stored
    fn schema(&self) -> &SchemaRef {
        &self.schema
    }

    /// remove a use from the specified batch id. If the use count
    /// reaches zero the batch entry is removed from the store
    ///
    /// panics if there were no remaining uses of id
    pub fn unuse(&mut self, id: u32) {
        let remove = if let Some(batch_entry) = self.batches.get_mut(&id) {
            batch_entry.uses = batch_entry.uses.checked_sub(1).expect("underflow");
            batch_entry.uses == 0
        } else {
            panic!("No entry for id {id}");
        };

        if remove {
            let old_entry = self.batches.remove(&id).unwrap();
            self.batches_size = self
                .batches_size
                .checked_sub(old_entry.batch.get_array_memory_size())
                .unwrap();
        }
    }

    /// returns the size of memory used by this store, including all
    /// referenced `RecordBatch`es, in bytes
    pub fn size(&self) -> usize {
        std::mem::size_of::<Self>()
            + self.batches.capacity()
                * (std::mem::size_of::<u32>() + std::mem::size_of::<RecordBatchEntry>())
            + self.batches_size
    }
}
