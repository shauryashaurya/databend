// Copyright 2021 Datafuse Labs
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

use std::any::Any;
use std::collections::BTreeMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::ops::BitAnd;
use std::sync::Arc;

use databend_common_arrow::arrow::array::Array;
use databend_common_arrow::arrow::bitmap::MutableBitmap;
use databend_common_arrow::native::read::ArrayIter;
use databend_common_arrow::parquet::metadata::ColumnDescriptor;
use databend_common_base::base::Progress;
use databend_common_base::base::ProgressValues;
use databend_common_catalog::plan::gen_mutation_stream_meta;
use databend_common_catalog::plan::DataSourcePlan;
use databend_common_catalog::plan::PartInfoPtr;
use databend_common_catalog::plan::PushDownInfo;
use databend_common_catalog::plan::TopK;
use databend_common_catalog::plan::VirtualColumnInfo;
use databend_common_catalog::table_context::TableContext;
use databend_common_exception::Result;
use databend_common_expression::build_select_expr;
use databend_common_expression::eval_function;
use databend_common_expression::filter_helper::FilterHelpers;
use databend_common_expression::types::BooleanType;
use databend_common_expression::types::DataType;
use databend_common_expression::BlockEntry;
use databend_common_expression::BlockMetaInfoDowncast;
use databend_common_expression::Column;
use databend_common_expression::ColumnId;
use databend_common_expression::DataBlock;
use databend_common_expression::DataField;
use databend_common_expression::DataSchema;
use databend_common_expression::Evaluator;
use databend_common_expression::Expr;
use databend_common_expression::FieldIndex;
use databend_common_expression::FilterExecutor;
use databend_common_expression::FunctionContext;
use databend_common_expression::Scalar;
use databend_common_expression::TopKSorter;
use databend_common_expression::Value;
use databend_common_functions::BUILTIN_FUNCTIONS;
use databend_common_metrics::storage::*;
use databend_common_pipeline_core::processors::Event;
use databend_common_pipeline_core::processors::InputPort;
use databend_common_pipeline_core::processors::OutputPort;
use databend_common_pipeline_core::processors::Processor;
use databend_common_pipeline_core::processors::ProcessorPtr;
use databend_common_sql::IndexType;
use xorf::BinaryFuse8;

use super::fuse_source::fill_internal_column_meta;
use super::native_data_source::NativeDataSource;
use crate::fuse_part::FusePartInfo;
use crate::io::AggIndexReader;
use crate::io::BlockReader;
use crate::io::VirtualColumnReader;
use crate::operations::read::data_source_with_meta::DataSourceWithMeta;
use crate::operations::read::runtime_filter_prunner::update_bitmap_with_bloom_filter;
use crate::DEFAULT_ROW_PER_PAGE;

pub struct NativeDeserializeDataTransform {
    ctx: Arc<dyn TableContext>,
    table_index: IndexType,
    func_ctx: FunctionContext,
    scan_progress: Arc<Progress>,
    block_reader: Arc<BlockReader>,
    column_leaves: Vec<Vec<ColumnDescriptor>>,

    input: Arc<InputPort>,
    output: Arc<OutputPort>,
    output_data: Option<DataBlock>,
    parts: VecDeque<PartInfoPtr>,
    chunks: VecDeque<NativeDataSource>,

    prewhere_columns: Vec<usize>,
    prewhere_schema: DataSchema,
    remain_columns: Vec<usize>,

    src_schema: DataSchema,
    output_schema: DataSchema,
    virtual_columns: Option<Vec<VirtualColumnInfo>>,

    prewhere_filter: Arc<Option<Expr>>,
    prewhere_virtual_columns: Option<Vec<VirtualColumnInfo>>,
    filter_executor: Option<FilterExecutor>,

    skipped_page: usize,
    // The row offset of current part.
    // It's used to compute the row offset in one block (single data file in one segment).
    offset_in_part: usize,

    read_columns: Vec<usize>,
    // Column ids are columns that have been read out,
    // not readded columns have two cases:
    // 1. newly added columns, no data insertion
    // 2. the source columns used to generate virtual columns,
    //    and all the virtual columns have been generated,
    //    then the source columns are not needed.
    // These columns need to fill in the default values.
    read_column_ids: HashSet<ColumnId>,
    top_k: Option<(TopK, TopKSorter, usize)>,
    // Identifies whether the ArrayIter has been initialised.
    inited: bool,
    // The ArrayIter of each columns to read Pages in order.
    array_iters: BTreeMap<usize, ArrayIter<'static>>,
    // The Page numbers of each ArrayIter can skip.
    array_skip_pages: BTreeMap<usize, usize>,

    index_reader: Arc<Option<AggIndexReader>>,
    virtual_reader: Arc<Option<VirtualColumnReader>>,

    base_block_ids: Option<Scalar>,

    cached_bloom_runtime_filter: Option<Vec<(FieldIndex, BinaryFuse8)>>,
}

impl NativeDeserializeDataTransform {
    #[allow(clippy::too_many_arguments)]
    pub fn create(
        ctx: Arc<dyn TableContext>,
        block_reader: Arc<BlockReader>,
        plan: &DataSourcePlan,
        top_k: Option<TopK>,
        input: Arc<InputPort>,
        output: Arc<OutputPort>,
        index_reader: Arc<Option<AggIndexReader>>,
        virtual_reader: Arc<Option<VirtualColumnReader>>,
    ) -> Result<ProcessorPtr> {
        let scan_progress = ctx.get_scan_progress();

        let mut src_schema: DataSchema = (block_reader.schema().as_ref()).into();

        let mut prewhere_columns: Vec<usize> =
            match PushDownInfo::prewhere_of_push_downs(plan.push_downs.as_ref()) {
                None => (0..src_schema.num_fields()).collect(),
                Some(v) => {
                    let projected_schema = v
                        .prewhere_columns
                        .project_schema(plan.source_info.schema().as_ref());

                    projected_schema
                        .fields()
                        .iter()
                        .map(|f| src_schema.index_of(f.name()).unwrap())
                        .collect()
                }
            };

        let top_k = top_k.map(|top_k| {
            let index = src_schema.index_of(top_k.field.name()).unwrap();
            let sorter = TopKSorter::new(top_k.limit, top_k.asc);

            if !prewhere_columns.contains(&index) {
                prewhere_columns.push(index);
                prewhere_columns.sort();
            }
            (top_k, sorter, index)
        });

        // add virtual columns to src_schema
        let (virtual_columns, prewhere_virtual_columns) = match &plan.push_downs {
            Some(push_downs) => {
                if let Some(virtual_columns) = &push_downs.virtual_columns {
                    let mut fields = src_schema.fields().clone();
                    for virtual_column in virtual_columns {
                        let field = DataField::new(
                            &virtual_column.name,
                            DataType::from(&*virtual_column.data_type),
                        );
                        fields.push(field);
                    }
                    src_schema = DataSchema::new(fields);
                }
                if let Some(prewhere) = &push_downs.prewhere {
                    if let Some(virtual_columns) = &prewhere.virtual_columns {
                        for virtual_column in virtual_columns {
                            prewhere_columns
                                .push(src_schema.index_of(&virtual_column.name).unwrap());
                        }
                        prewhere_columns.sort();
                    }
                    (
                        push_downs.virtual_columns.clone(),
                        prewhere.virtual_columns.clone(),
                    )
                } else {
                    (push_downs.virtual_columns.clone(), None)
                }
            }
            None => (None, None),
        };

        let remain_columns: Vec<usize> = (0..src_schema.num_fields())
            .filter(|i| !prewhere_columns.contains(i))
            .collect();

        let func_ctx = ctx.get_function_context()?;
        let prewhere_schema = src_schema.project(&prewhere_columns);
        let prewhere_filter = Self::build_prewhere_filter_expr(plan, &prewhere_schema)?;

        let filter_executor = if let Some(expr) = prewhere_filter.as_ref() {
            let (select_expr, has_or) = build_select_expr(expr);
            Some(FilterExecutor::new(
                select_expr,
                func_ctx.clone(),
                has_or,
                DEFAULT_ROW_PER_PAGE,
                None,
                &BUILTIN_FUNCTIONS,
                false,
            ))
        } else {
            None
        };

        let mut output_schema = plan.schema().as_ref().clone();
        output_schema.remove_internal_fields();
        let output_schema: DataSchema = (&output_schema).into();

        let mut column_leaves = Vec::with_capacity(block_reader.project_column_nodes.len());
        for column_node in &block_reader.project_column_nodes {
            let leaves: Vec<ColumnDescriptor> = column_node
                .leaf_indices
                .iter()
                .map(|i| block_reader.parquet_schema_descriptor.columns()[*i].clone())
                .collect::<Vec<_>>();
            column_leaves.push(leaves);
        }

        Ok(ProcessorPtr::create(Box::new(
            NativeDeserializeDataTransform {
                ctx,
                table_index: plan.table_index,
                func_ctx,
                scan_progress,
                block_reader,
                column_leaves,
                input,
                output,
                output_data: None,
                parts: VecDeque::new(),
                chunks: VecDeque::new(),

                prewhere_columns,
                prewhere_schema,
                remain_columns,
                src_schema,
                output_schema,
                virtual_columns,

                prewhere_filter,
                prewhere_virtual_columns,
                filter_executor,
                skipped_page: 0,
                top_k,
                read_columns: vec![],
                read_column_ids: HashSet::new(),
                inited: false,
                array_iters: BTreeMap::new(),
                array_skip_pages: BTreeMap::new(),
                offset_in_part: 0,

                index_reader,
                virtual_reader,

                base_block_ids: plan.base_block_ids.clone(),
                cached_bloom_runtime_filter: None,
            },
        )))
    }

    fn build_prewhere_filter_expr(
        plan: &DataSourcePlan,
        schema: &DataSchema,
    ) -> Result<Arc<Option<Expr>>> {
        Ok(Arc::new(
            PushDownInfo::prewhere_of_push_downs(plan.push_downs.as_ref()).map(|v| {
                v.filter
                    .as_expr(&BUILTIN_FUNCTIONS)
                    .project_column_ref(|name| schema.column_with_name(name).unwrap().0)
            }),
        ))
    }

    fn add_block(&mut self, data_block: DataBlock) -> Result<()> {
        let rows = data_block.num_rows();
        if rows == 0 {
            return Ok(());
        }
        let progress_values = ProgressValues {
            rows,
            bytes: data_block.memory_size(),
        };
        self.scan_progress.incr(&progress_values);
        self.output_data = Some(data_block);
        Ok(())
    }

    /// If the virtual column has already generated, add it directly,
    /// otherwise extract it from the source column
    fn add_virtual_columns(
        &self,
        chunks: Vec<(usize, Box<dyn Array>)>,
        schema: &DataSchema,
        virtual_columns: &Option<Vec<VirtualColumnInfo>>,
        block: &mut DataBlock,
    ) -> Result<()> {
        if let Some(virtual_columns) = virtual_columns {
            for virtual_column in virtual_columns {
                let src_index = self.src_schema.index_of(&virtual_column.name).unwrap();
                if let Some(array) = chunks
                    .iter()
                    .find(|c| c.0 == src_index)
                    .map(|c| c.1.clone())
                {
                    let data_type: DataType =
                        (*self.src_schema.field(src_index).data_type()).clone();
                    let column = BlockEntry::new(
                        data_type.clone(),
                        Value::Column(Column::from_arrow(array.as_ref(), &data_type)),
                    );
                    // If the source column is the default value, num_rows may be zero
                    if block.num_columns() > 0 && block.num_rows() == 0 {
                        let num_rows = array.len();
                        let mut columns = block.columns().to_vec();
                        columns.push(column);
                        *block = DataBlock::new(columns, num_rows);
                    } else {
                        block.add_column(column);
                    }
                    continue;
                }
                let index = schema.index_of(&virtual_column.source_name).unwrap();
                let source = block.get_by_offset(index);
                let src_arg = (source.value.clone(), source.data_type.clone());
                let path_arg = (
                    Value::Scalar(virtual_column.key_paths.clone()),
                    DataType::String,
                );

                let (value, data_type) = eval_function(
                    None,
                    "get_by_keypath",
                    [src_arg, path_arg],
                    &self.func_ctx,
                    block.num_rows(),
                    &BUILTIN_FUNCTIONS,
                )?;

                let column = BlockEntry::new(data_type, value);
                block.add_column(column);
            }
        }

        Ok(())
    }

    /// If the top-k or all prewhere columns are default values, check if the filter is met,
    /// and if not, ignore all pages, otherwise continue without repeating the check for subsequent processes.
    fn check_default_values(&mut self) -> Result<bool> {
        if self.prewhere_columns.len() > 1 {
            if let Some((_, sorter, index)) = self.top_k.as_mut() {
                if !self.array_iters.contains_key(index) {
                    let default_val = self.block_reader.default_vals[*index].clone();
                    if sorter.never_match_value(&default_val) {
                        return Ok(true);
                    }
                }
            }
        }
        if let Some(filter) = self.prewhere_filter.as_ref() {
            let all_defaults = &self
                .prewhere_columns
                .iter()
                .all(|index| !self.array_iters.contains_key(index));

            let all_virtual_defaults = match &self.prewhere_virtual_columns {
                Some(ref prewhere_virtual_columns) => prewhere_virtual_columns.iter().all(|c| {
                    let src_index = self.src_schema.index_of(&c.source_name).unwrap();
                    !self.array_iters.contains_key(&src_index)
                }),
                None => true,
            };

            if *all_defaults && all_virtual_defaults {
                let columns = &mut self
                    .prewhere_columns
                    .iter()
                    .map(|index| {
                        let data_type = self.src_schema.field(*index).data_type().clone();
                        let default_val = &self.block_reader.default_vals[*index];
                        BlockEntry::new(data_type, Value::Scalar(default_val.to_owned()))
                    })
                    .collect::<Vec<_>>();

                if let Some(ref prewhere_virtual_columns) = &self.prewhere_virtual_columns {
                    for virtual_column in prewhere_virtual_columns {
                        // if the source column is default value, the virtual column is always Null.
                        let column = BlockEntry::new(
                            DataType::from(&*virtual_column.data_type),
                            Value::Scalar(Scalar::Null),
                        );
                        columns.push(column);
                    }
                }

                let prewhere_block = DataBlock::new(columns.to_vec(), 1);
                let evaluator = Evaluator::new(&prewhere_block, &self.func_ctx, &BUILTIN_FUNCTIONS);
                let filter = evaluator
                    .run(filter)
                    .map_err(|e| e.add_message("eval prewhere filter failed:"))?
                    .try_downcast::<BooleanType>()
                    .unwrap();

                if FilterHelpers::is_all_unset(&filter) {
                    return Ok(true);
                }

                // Default value satisfies the filter, update the value of top-k column.
                if let Some((_, sorter, index)) = self.top_k.as_mut() {
                    if !self.array_iters.contains_key(index) {
                        let part = FusePartInfo::from_part(&self.parts[0])?;
                        let num_rows = part.nums_rows;

                        let data_type = self.src_schema.field(*index).data_type().clone();
                        let default_val = self.block_reader.default_vals[*index].clone();
                        let value = Value::Scalar(default_val);
                        let col = value.convert_to_full_column(&data_type, num_rows);
                        let mut bitmap = MutableBitmap::from_len_set(num_rows);
                        sorter.push_column(&col, &mut bitmap);
                    }
                }
            }
        }
        Ok(false)
    }

    /// No more data need to read, finish process.
    fn finish_process(&mut self) -> Result<()> {
        let _ = self.chunks.pop_front();
        let _ = self.parts.pop_front().unwrap();

        self.inited = false;
        self.array_iters.clear();
        self.array_skip_pages.clear();
        self.offset_in_part = 0;
        self.read_column_ids.clear();
        Ok(())
    }

    /// All columns are default values, not need to read.
    fn finish_process_with_default_values(&mut self) -> Result<()> {
        let _ = self.chunks.pop_front();
        let part = self.parts.pop_front().unwrap();
        let fuse_part = FusePartInfo::from_part(&part)?;

        let num_rows = fuse_part.nums_rows;
        let mut data_block = self.block_reader.build_default_values_block(num_rows)?;
        if let Some(ref virtual_columns) = &self.virtual_columns {
            for virtual_column in virtual_columns {
                // if the source column is default value, the virtual column is always Null.
                let column = BlockEntry::new(
                    DataType::from(&*virtual_column.data_type),
                    Value::Scalar(Scalar::Null),
                );
                data_block.add_column(column);
            }
        }

        if self.block_reader.query_internal_columns() {
            data_block = fill_internal_column_meta(
                data_block,
                fuse_part,
                None,
                self.base_block_ids.clone(),
            )?;
        }

        if self.block_reader.update_stream_columns() {
            let inner_meta = data_block.take_meta();
            let meta = gen_mutation_stream_meta(inner_meta, &fuse_part.location)?;
            data_block = data_block.add_meta(Some(Box::new(meta)))?;
        }

        let data_block = data_block.resort(&self.src_schema, &self.output_schema)?;
        self.add_block(data_block)?;

        self.inited = false;
        self.array_iters.clear();
        self.array_skip_pages.clear();
        self.offset_in_part = 0;
        self.read_column_ids.clear();
        Ok(())
    }

    /// Empty projection use empty block.
    fn finish_process_with_empty_block(&mut self) -> Result<()> {
        let _ = self.chunks.pop_front();
        let part = self.parts.pop_front().unwrap();
        let fuse_part = FusePartInfo::from_part(&part)?;

        let num_rows = fuse_part.nums_rows;
        let data_block = DataBlock::new(vec![], num_rows);
        let data_block = if self.block_reader.query_internal_columns() {
            fill_internal_column_meta(data_block, fuse_part, None, self.base_block_ids.clone())?
        } else {
            data_block
        };

        self.add_block(data_block)?;
        Ok(())
    }

    /// Update the number of pages that can be skipped per column.
    fn finish_process_skip_page(&mut self) -> Result<()> {
        self.skipped_page += 1;
        for (i, skip_num) in self.array_skip_pages.iter_mut() {
            if self.read_columns.contains(i) {
                continue;
            }
            *skip_num += 1;
        }
        Ok(())
    }

    // TODO(xudong): add selectivity prediction
    fn bloom_runtime_filter(
        &mut self,
        arrays: &mut Vec<(usize, Box<dyn Array>)>,
        count: Option<usize>,
    ) -> Result<(bool, Option<usize>)> {
        let mut local_arrays = vec![];
        // Check if already cached runtime filters
        if self.cached_bloom_runtime_filter.is_none() {
            let bloom_filters = self.ctx.get_bloom_runtime_filter_with_id(self.table_index);
            let bloom_filters = bloom_filters
                .into_iter()
                .filter_map(|filter| {
                    let name = filter.0.as_str();
                    // Some probe keys are not in the schema, they are derived from expressions.
                    self.src_schema
                        .index_of(name)
                        .ok()
                        .map(|idx| (idx, filter.1.clone()))
                })
                .collect::<Vec<(FieldIndex, BinaryFuse8)>>();
            if bloom_filters.is_empty() {
                return Ok((false, count));
            }
            self.cached_bloom_runtime_filter = Some(bloom_filters);
        }
        let mut bitmaps =
            Vec::with_capacity(self.cached_bloom_runtime_filter.as_ref().unwrap().len());
        for (idx, filter) in self.cached_bloom_runtime_filter.as_ref().unwrap().iter() {
            let mut find_array = false;
            // It's possible that the column has multiple filters, so we need to avoid duplicate reads.
            // Or the column in prewhere columns has been read.
            for (i, array) in arrays.iter() {
                if i == idx {
                    local_arrays.push((*idx, array.clone()));
                    find_array = true;
                    break;
                }
            }
            if !find_array {
                if let Some(array_iter) = self.array_iters.get_mut(idx) {
                    let skip_pages = self.array_skip_pages.get(idx).unwrap();
                    match array_iter.nth(*skip_pages) {
                        Some(array) => {
                            let array = array.as_ref().unwrap();
                            if let Some(pos) = self.remain_columns.iter().position(|i| i == idx) {
                                self.remain_columns.remove(pos);
                            }
                            self.read_columns.push(*idx);
                            arrays.push((*idx, array.clone()));
                            local_arrays.push((*idx, array.clone()));
                            self.array_skip_pages.insert(*idx, 0);
                        }
                        None => {
                            return Ok((false, count));
                        }
                    }
                }
            }
            let probe_block = self.block_reader.build_block(local_arrays.clone(), None)?;
            let mut bitmap = MutableBitmap::from_len_zeroed(probe_block.num_rows());
            local_arrays.clear();
            let probe_column = probe_block.get_last_column().clone();
            update_bitmap_with_bloom_filter(probe_column, filter, &mut bitmap)?;
            let unset_bits = bitmap.unset_bits();
            if unset_bits == bitmap.len() {
                self.offset_in_part += probe_block.num_rows();
                self.finish_process_skip_page()?;
                return Ok((true, None));
            } else if unset_bits != 0 {
                bitmaps.push(bitmap);
            }
        }
        if !bitmaps.is_empty() {
            let rf_bitmap = bitmaps
                .into_iter()
                .reduce(|acc, rf_filter| acc.bitand(&rf_filter.into()))
                .unwrap();
            if self.filter_executor.is_none() {
                // If prewhere filter is None, we need to build a dummy filter executor.
                let dummy_expr = Expr::Constant {
                    span: None,
                    scalar: Scalar::Boolean(true),
                    data_type: DataType::Boolean,
                };
                let (select_expr, has_or) = build_select_expr(&dummy_expr);
                self.filter_executor = Some(FilterExecutor::new(
                    select_expr,
                    self.ctx.get_function_context()?,
                    has_or,
                    DEFAULT_ROW_PER_PAGE,
                    None,
                    &BUILTIN_FUNCTIONS,
                    false,
                ));
            }
            let filter_executor = self.filter_executor.as_mut().unwrap();
            let filter_count = if let Some(count) = count {
                filter_executor.select_bitmap(count, rf_bitmap)
            } else {
                filter_executor.from_bitmap(rf_bitmap)
            };
            Ok((false, Some(filter_count)))
        } else {
            Ok((false, count))
        }
    }
}

impl Processor for NativeDeserializeDataTransform {
    fn name(&self) -> String {
        String::from("NativeDeserializeDataTransform")
    }

    fn as_any(&mut self) -> &mut dyn Any {
        self
    }

    fn event(&mut self) -> Result<Event> {
        if self.output.is_finished() {
            self.input.finish();
            return Ok(Event::Finished);
        }

        if !self.output.can_push() {
            self.input.set_not_need_data();
            return Ok(Event::NeedConsume);
        }

        if let Some(data_block) = self.output_data.take() {
            self.output.push_data(Ok(data_block));
            return Ok(Event::NeedConsume);
        }

        if !self.chunks.is_empty() {
            if !self.input.has_data() {
                self.input.set_need_data();
            }
            return Ok(Event::Sync);
        }

        if self.input.has_data() {
            let mut data_block = self.input.pull_data().unwrap()?;
            if let Some(block_meta) = data_block.take_meta() {
                if let Some(source_meta) = DataSourceWithMeta::downcast_from(block_meta) {
                    self.parts = VecDeque::from(source_meta.meta);
                    self.chunks = VecDeque::from(source_meta.data);
                    return Ok(Event::Sync);
                }
            }

            unreachable!();
        }

        if self.input.is_finished() {
            metrics_inc_pruning_prewhere_nums(self.skipped_page as u64);
            self.output.finish();
            return Ok(Event::Finished);
        }

        self.input.set_need_data();
        Ok(Event::NeedData)
    }

    fn process(&mut self) -> Result<()> {
        if let Some(chunks) = self.chunks.front_mut() {
            let chunks = match chunks {
                NativeDataSource::AggIndex(data) => {
                    let agg_index_reader = self.index_reader.as_ref().as_ref().unwrap();
                    let block = agg_index_reader.deserialize_native_data(data)?;
                    self.output_data = Some(block);
                    return self.finish_process();
                }
                NativeDataSource::Normal(data) => data,
            };

            // this means it's empty projection
            if chunks.is_empty() && !self.inited {
                return self.finish_process_with_empty_block();
            }

            // Init array_iters and array_skip_pages to read pages in subsequent processes.
            if !self.inited {
                let fuse_part = FusePartInfo::from_part(&self.parts[0])?;
                if let Some(range) = fuse_part.range() {
                    self.offset_in_part = fuse_part.page_size() * range.start;
                }

                if let Some(((_top_k, sorter, _index), min_max)) =
                    self.top_k.as_mut().zip(fuse_part.sort_min_max.as_ref())
                {
                    if sorter.never_match(min_max) {
                        return self.finish_process();
                    }
                }

                let mut has_default_value = false;
                self.inited = true;
                for (index, column_node) in
                    self.block_reader.project_column_nodes.iter().enumerate()
                {
                    let readers = chunks.remove(&index).unwrap_or_default();
                    if !readers.is_empty() {
                        let leaves = self.column_leaves.get(index).unwrap().clone();
                        let array_iter =
                            BlockReader::build_array_iter(column_node, leaves, readers)?;
                        self.array_iters.insert(index, array_iter);
                        self.array_skip_pages.insert(index, 0);

                        for column_id in &column_node.leaf_column_ids {
                            self.read_column_ids.insert(*column_id);
                        }
                    } else {
                        has_default_value = true;
                    }
                }
                // Add optional virtual column array_iter
                if let Some(virtual_reader) = self.virtual_reader.as_ref() {
                    for (index, virtual_column_info) in
                        virtual_reader.virtual_column_infos.iter().enumerate()
                    {
                        let virtual_index = index + self.block_reader.project_column_nodes.len();
                        if let Some(readers) = chunks.remove(&virtual_index) {
                            let array_iter = BlockReader::build_virtual_array_iter(
                                virtual_column_info.name.clone(),
                                readers,
                            )?;
                            let index = self.src_schema.index_of(&virtual_column_info.name)?;
                            self.array_iters.insert(index, array_iter);
                            self.array_skip_pages.insert(index, 0);
                        }
                    }
                }

                if has_default_value {
                    // Check if the default value matches the top-k or filter,
                    // if not, return empty block.
                    if self.check_default_values()? {
                        return self.finish_process();
                    }
                }
                // No columns need to read, return default value directly.
                if self.array_iters.is_empty() {
                    return self.finish_process_with_default_values();
                }
            }

            let mut need_to_fill_data = false;
            self.read_columns.clear();
            let mut arrays = Vec::with_capacity(self.array_iters.len());

            // Step 1: Check TOP_K, if prewhere_columns contains not only TOP_K, we can check if TOP_K column can satisfy the heap.
            if self.prewhere_columns.len() > 1 {
                if let Some((top_k, sorter, index)) = self.top_k.as_mut() {
                    if let Some(array_iter) = self.array_iters.get_mut(index) {
                        match array_iter.next() {
                            Some(array) => {
                                let array = array?;
                                self.read_columns.push(*index);
                                let data_type = top_k.field.data_type().into();
                                let col = Column::from_arrow(array.as_ref(), &data_type);

                                arrays.push((*index, array));
                                if sorter.never_match_any(&col) {
                                    self.offset_in_part += col.len();
                                    return self.finish_process_skip_page();
                                }
                            }
                            None => {
                                return self.finish_process();
                            }
                        }
                    }
                }
            }

            // Step 2: Read Prewhere columns and get the filter
            let mut prewhere_default_val_indices = HashSet::new();
            for index in self.prewhere_columns.iter() {
                if self.read_columns.contains(index) {
                    continue;
                }
                if let Some(array_iter) = self.array_iters.get_mut(index) {
                    let skip_pages = self.array_skip_pages.get(index).unwrap();

                    match array_iter.nth(*skip_pages) {
                        Some(array) => {
                            self.read_columns.push(*index);
                            arrays.push((*index, array?));
                            self.array_skip_pages.insert(*index, 0);
                        }
                        None => {
                            return self.finish_process();
                        }
                    }
                } else {
                    prewhere_default_val_indices.insert(*index);
                    need_to_fill_data = true;
                }
            }

            let filtered_count = match self.prewhere_filter.as_ref() {
                Some(_) => {
                    // Arrays are empty means all prewhere columns are default values,
                    // the filter have checked in the first process, don't need check again.
                    if arrays.is_empty() {
                        None
                    } else {
                        let mut prewhere_block = if arrays.len() < self.prewhere_columns.len() {
                            self.block_reader
                                .build_block(arrays.clone(), Some(prewhere_default_val_indices))?
                        } else {
                            self.block_reader.build_block(arrays.clone(), None)?
                        };
                        // Add optional virtual columns for prewhere
                        self.add_virtual_columns(
                            arrays.clone(),
                            &self.prewhere_schema,
                            &self.prewhere_virtual_columns,
                            &mut prewhere_block,
                        )?;

                        let filter_executor = self.filter_executor.as_mut().unwrap();
                        let mut count = filter_executor.select(&prewhere_block)?;

                        // Step 3: Apply the filter, if it's all filtered, we can skip the remain columns.
                        if count == 0 {
                            self.offset_in_part += prewhere_block.num_rows();
                            return self.finish_process_skip_page();
                        }

                        // Step 4: Apply the filter to topk and update the bitmap, this will filter more results
                        if let Some((_, sorter, index)) = &mut self.top_k {
                            let index_prewhere = self
                                .prewhere_columns
                                .iter()
                                .position(|x| x == index)
                                .unwrap();
                            let top_k_column = prewhere_block
                                .get_by_offset(index_prewhere)
                                .value
                                .as_column()
                                .unwrap();
                            count = sorter.push_column_with_selection(
                                top_k_column,
                                filter_executor.mut_true_selection(),
                                count,
                            );
                        };

                        if count == 0 {
                            self.offset_in_part += prewhere_block.num_rows();
                            return self.finish_process_skip_page();
                        }
                        Some(count)
                    }
                }
                None => None,
            };

            let (skipped, filtered_count) =
                self.bloom_runtime_filter(&mut arrays, filtered_count)?;

            if skipped {
                return Ok(());
            }

            // Step 5: read remain columns and filter block if needed.
            for index in self.remain_columns.iter() {
                if let Some(array_iter) = self.array_iters.get_mut(index) {
                    let skip_pages = self.array_skip_pages.get(index).unwrap();

                    match array_iter.nth(*skip_pages) {
                        Some(array) => {
                            self.read_columns.push(*index);
                            arrays.push((*index, array?));
                            self.array_skip_pages.insert(*index, 0);
                        }
                        None => {
                            return self.finish_process();
                        }
                    }
                } else {
                    need_to_fill_data = true;
                }
            }

            let block = self.block_reader.build_block(arrays.clone(), None)?;
            // Step 6: fill missing field default value if need
            let mut block = if need_to_fill_data {
                self.block_reader
                    .fill_missing_native_column_values(block, &self.read_column_ids)?
            } else {
                block
            };

            // Step 7: Add optional virtual columns
            self.add_virtual_columns(arrays, &self.src_schema, &self.virtual_columns, &mut block)?;

            let origin_num_rows = block.num_rows();
            let block = if let Some(count) = &filtered_count {
                let filter_executor = self.filter_executor.as_mut().unwrap();
                filter_executor.take(block, origin_num_rows, *count)?
            } else {
                block
            };

            // Step 8: Fill `InternalColumnMeta` as `DataBlock.meta` if query internal columns,
            // `TransformAddInternalColumns` will generate internal columns using `InternalColumnMeta` in next pipeline.
            let mut block = block.resort(&self.src_schema, &self.output_schema)?;
            if self.block_reader.query_internal_columns() {
                let offsets = if let Some(count) = filtered_count {
                    let filter_executor = self.filter_executor.as_mut().unwrap();
                    filter_executor.mut_true_selection()[0..count]
                        .iter()
                        .map(|idx| *idx as usize + self.offset_in_part)
                        .collect::<Vec<_>>()
                } else {
                    (self.offset_in_part..self.offset_in_part + origin_num_rows).collect()
                };

                let fuse_part = FusePartInfo::from_part(&self.parts[0])?;
                block = fill_internal_column_meta(
                    block,
                    fuse_part,
                    Some(offsets),
                    self.base_block_ids.clone(),
                )?;
            }

            if self.block_reader.update_stream_columns() {
                let inner_meta = block.take_meta();
                let fuse_part = FusePartInfo::from_part(&self.parts[0])?;
                let meta = gen_mutation_stream_meta(inner_meta, &fuse_part.location)?;
                block = block.add_meta(Some(Box::new(meta)))?;
            }

            // Step 9: Add the block to output data
            self.offset_in_part += origin_num_rows;
            self.add_block(block)?;
        }

        Ok(())
    }
}
