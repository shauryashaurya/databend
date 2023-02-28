// Copyright 2023 Datafuse Labs.
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

use std::sync::Arc;
use std::vec;

use common_catalog::table_context::TableContext;
use common_exception::ErrorCode;
use common_exception::Result;
use common_expression::DataBlock;
use common_hashtable::HashtableLike;
use common_pipeline_core::processors::port::InputPort;
use common_pipeline_core::processors::port::OutputPort;
use common_pipeline_core::processors::Processor;
use common_pipeline_transforms::processors::transforms::AccumulatingTransform;
use common_pipeline_transforms::processors::transforms::AccumulatingTransformer;
use common_sql::IndexType;

use crate::pipelines::processors::transforms::aggregator::aggregate_meta::AggregateMeta;
use crate::pipelines::processors::transforms::group_by::ArenaHolder;
use crate::pipelines::processors::transforms::group_by::HashMethodBounds;
use crate::pipelines::processors::transforms::group_by::PartitionedHashMethod;
use crate::pipelines::processors::transforms::group_by::PolymorphicKeysHelper;
use crate::pipelines::processors::AggregatorParams;
use crate::sessions::QueryContext;

#[allow(clippy::enum_variant_names)]
enum HashTable<Method: HashMethodBounds> {
    MovedOut,
    HashTable(Method::HashTable<()>),
    PartitionedHashTable(
        <PartitionedHashMethod<Method> as PolymorphicKeysHelper<
                PartitionedHashMethod<Method>,
            >>::HashTable<()>,
    ),
}

impl<Method: HashMethodBounds> Default for HashTable<Method> {
    fn default() -> Self {
        Self::MovedOut
    }
}

struct GroupBySettings {
    convert_threshold: usize,
    spilling_bytes_threshold_per_proc: usize,
}

impl TryFrom<Arc<QueryContext>> for GroupBySettings {
    type Error = ErrorCode;

    fn try_from(ctx: Arc<QueryContext>) -> std::result::Result<Self, Self::Error> {
        let settings = ctx.get_settings();
        let convert_threshold = settings.get_group_by_two_level_threshold()? as usize;
        Ok(GroupBySettings {
            convert_threshold,
            spilling_bytes_threshold_per_proc: usize::MAX,
        })
    }
}

// SELECT column_name FROM table_name GROUP BY column_name
pub struct TransformPartialGroupBy<Method: HashMethodBounds> {
    method: Method,
    hash_table: HashTable<Method>,
    group_columns: Vec<IndexType>,
    settings: GroupBySettings,
}

impl<Method: HashMethodBounds> TransformPartialGroupBy<Method> {
    pub fn try_create(
        ctx: Arc<QueryContext>,
        method: Method,
        input: Arc<InputPort>,
        output: Arc<OutputPort>,
        params: Arc<AggregatorParams>,
    ) -> Result<Box<dyn Processor>> {
        let hash_table = HashTable::HashTable(method.create_hash_table()?);
        Ok(AccumulatingTransformer::create(
            input,
            output,
            TransformPartialGroupBy::<Method> {
                method,
                hash_table,
                group_columns: params.group_columns.clone(),
                settings: GroupBySettings::try_from(ctx)?,
            },
        ))
    }
}

impl<Method: HashMethodBounds> AccumulatingTransform for TransformPartialGroupBy<Method> {
    const NAME: &'static str = "TransformPartialGroupBy";

    fn transform(&mut self, block: DataBlock) -> Result<Vec<DataBlock>> {
        let block = block.convert_to_full();
        let group_columns = self
            .group_columns
            .iter()
            .map(|&index| block.get_by_offset(index))
            .collect::<Vec<_>>();

        let group_columns = group_columns
            .iter()
            .map(|c| (c.value.as_column().unwrap().clone(), c.data_type.clone()))
            .collect::<Vec<_>>();

        unsafe {
            let rows_num = block.num_rows();
            let state = self.method.build_keys_state(&group_columns, rows_num)?;

            match &mut self.hash_table {
                HashTable::MovedOut => unreachable!(),
                HashTable::HashTable(hashtable) => {
                    for key in self.method.build_keys_iter(&state)? {
                        let _ = hashtable.insert_and_entry(key);
                    }
                }
                HashTable::PartitionedHashTable(hashtable) => {
                    for key in self.method.build_keys_iter(&state)? {
                        let _ = hashtable.insert_and_entry(key);
                    }
                }
            };

            #[allow(clippy::collapsible_if)]
            if Method::SUPPORT_PARTITIONED {
                if matches!(&self.hash_table, HashTable::HashTable(hashtable)
                    if hashtable.len() >= self.settings.convert_threshold ||
                        hashtable.bytes_len() >= self.settings.spilling_bytes_threshold_per_proc
                ) {
                    if let HashTable::HashTable(hashtable) = std::mem::take(&mut self.hash_table) {
                        self.hash_table = HashTable::PartitionedHashTable(
                            PartitionedHashMethod::convert_hashtable(&self.method, hashtable)?,
                        );
                    }
                }
            }
        }

        Ok(vec![])
    }

    fn on_finish(&mut self, _output: bool) -> Result<Vec<DataBlock>> {
        Ok(match std::mem::take(&mut self.hash_table) {
            HashTable::MovedOut => unreachable!(),
            HashTable::HashTable(v) => match Method::HashTable::len(&v) == 0 {
                true => vec![],
                false => vec![DataBlock::empty_with_meta(
                    AggregateMeta::<Method, ()>::create_hashtable(-1, v, ArenaHolder::create(None)),
                )],
            },
            HashTable::PartitionedHashTable(v) => {
                let mut blocks = Vec::with_capacity(256);
                for (bucket, hashtable) in v.into_iter_tables().enumerate() {
                    if Method::HashTable::len(&hashtable) != 0 {
                        blocks.push(DataBlock::empty_with_meta(
                            AggregateMeta::<Method, ()>::create_hashtable(
                                bucket as isize,
                                hashtable,
                                ArenaHolder::create(None),
                            ),
                        ));
                    }
                }
                blocks
            }
        })
    }
}
