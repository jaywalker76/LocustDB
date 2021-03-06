use std::collections::HashMap;
use std::collections::HashSet;
use std::iter::Iterator;
use std::sync::Arc;

use ::QueryError;
use engine::*;
use ingest::raw_val::RawVal;
use mem_store::column::DataSource;
use syntax::expression::*;
use syntax::limit::*;

/// NormalFormQuery observes the following invariants:
/// - none of the expressions contain aggregation functions
/// - if aggregate.len() > 0 then order_by.len() == 0 and vice versa
#[derive(Debug, Clone)]
pub struct NormalFormQuery {
    pub projection: Vec<Expr>,
    pub filter: Expr,
    pub aggregate: Vec<(Aggregator, Expr)>,
    pub order_by: Vec<(Expr, bool)>,
    pub limit: LimitClause,
}

#[derive(Debug, Clone)]
pub struct Query {
    pub select: Vec<Expr>,
    pub table: String,
    pub filter: Expr,
    pub order_by: Vec<(Expr, bool)>,
    pub limit: LimitClause,
}

impl NormalFormQuery {
    #[inline(never)] // produces more useful profiles
    pub fn run<'a>(&self,
                   columns: &'a HashMap<String, Arc<DataSource>>,
                   explain: bool,
                   show: bool,
                   partition: usize,
                   partition_length: usize) -> Result<(BatchResult<'a>, Option<String>), QueryError> {
        let limit = (self.limit.limit + self.limit.offset) as usize;
        let mut planner = QueryPlanner::default();

        let (filter_plan, _) = QueryPlan::compile_expr(&self.filter, Filter::None, columns, &mut planner)?;
        let mut filter = match filter_plan.tag {
            EncodingType::U8 => Filter::U8(filter_plan.u8()?),
            EncodingType::NullableU8 => Filter::NullableU8(filter_plan.nullable_u8()?),
            _ => Filter::None,
        };

        // Sorting
        let mut sort_indices = None;
        for (plan, desc) in self.order_by.iter().rev() {
            let (ranking, _) = query_plan::order_preserving(
                QueryPlan::compile_expr(&plan, filter, columns, &mut planner)?, &mut planner);

            // TODO(clemens): better criterion for using top_n
            // TODO(clemens): top_n for multiple columns?
            sort_indices = Some(if limit < partition_length / 2 && self.order_by.len() == 1 {
                planner.top_n(ranking, limit, *desc)
            } else {
                // TODO(clemens): Optimization: sort directly if only single column selected
                match sort_indices {
                    None => {
                        let indices = planner.indices(ranking);
                        planner.sort_by(ranking, indices,
                                        *desc, false /* unstable sort */)
                    }
                    Some(indices) => planner.sort_by(ranking, indices, *desc, true /* stable sort */)
                }
            });
        }
        if let Some(sort_indices) = sort_indices {
            filter = match filter {
                Filter::U8(where_true) => {
                    let buffer = planner.null_vec(partition_length, EncodingType::Null);
                    let indices = planner.indices(buffer).into();
                    let filter = planner.filter(indices, where_true);
                    Filter::Indices(planner.select(filter, sort_indices).usize()?)
                }
                Filter::NullableU8(where_true) => {
                    let buffer = planner.null_vec(partition_length, EncodingType::Null);
                    let indices = planner.indices(buffer).into();
                    let filter = planner.nullable_filter(indices, where_true);
                    Filter::Indices(planner.select(filter, sort_indices).usize()?)
                }
                Filter::None => Filter::Indices(sort_indices),
                Filter::Indices(_) => unreachable!(),
            };
        }

        let mut select = Vec::new();
        for expr in &self.projection {
            let (mut plan, plan_type) = QueryPlan::compile_expr(expr, filter, columns, &mut planner)?;
            if let Some(codec) = plan_type.codec {
                plan = codec.decode(plan, &mut planner);
            }
            select.push(plan.any());
        }
        let mut order_by = Vec::new();
        for (expr, desc) in &self.order_by {
            let (mut plan, plan_type) = QueryPlan::compile_expr(expr, filter, columns, &mut planner)?;
            if let Some(codec) = plan_type.codec {
                plan = codec.decode(plan, &mut planner);
            }
            order_by.push((plan.any(), *desc));
        };

        for c in columns {
            debug!("{}: {:?}", partition, c);
        }
        let mut executor = planner.prepare(vec![])?;
        let mut results = executor.prepare(NormalFormQuery::column_data(columns));
        debug!("{:#}", &executor);
        executor.run(columns.iter().next().unwrap().1.len(), &mut results, show);
        let (columns, projection, _, order_by) = results.collect_aliased(&select, &[], &order_by);

        Ok(
            (BatchResult {
                columns,
                projection,
                aggregations: vec![],
                order_by,
                level: 0,
                batch_count: 1,
                show,
                unsafe_referenced_buffers: results.collect_pinned(),
            },
             if explain { Some(format!("{}", executor)) } else { None }))
    }

    #[inline(never)] // produces more useful profiles
    pub fn run_aggregate<'a>(&self,
                             columns: &'a HashMap<String, Arc<DataSource>>,
                             explain: bool,
                             show: bool,
                             partition: usize,
                             partition_length: usize)
                             -> Result<(BatchResult<'a>, Option<String>), QueryError> {
        trace_start!("run_aggregate");

        let mut planner = QueryPlanner::default();

        // Filter
        let (filter_plan, filter_type) = QueryPlan::compile_expr(&self.filter, Filter::None, columns, &mut planner)?;
        let filter = match filter_type.encoding_type() {
            EncodingType::U8 => Filter::U8(filter_plan.u8()?),
            EncodingType::NullableU8 => Filter::NullableU8(filter_plan.nullable_u8()?),
            _ => Filter::None,
        };

        // Combine all group by columns into a single decodable grouping key
        let ((raw_grouping_key, raw_grouping_key_type),
            max_grouping_key,
            decode_plans,
            encoded_group_by_placeholder) =
            query_plan::compile_grouping_key(&self.projection, filter, columns, partition_length, &mut planner)?;

        // Reduce cardinality of grouping key if necessary and perform grouping
        // TODO(clemens): also determine and use is_dense. always true for hashmap, depends on group by columns for raw.
        let (encoded_group_by_column,
            grouping_key,
            grouping_key_type,
            aggregation_cardinality) =
        // TODO(clemens): refine criterion
            if max_grouping_key < 1 << 16 && raw_grouping_key_type.is_positive_integer() {
                let max_grouping_key_buf = planner.scalar_i64(max_grouping_key, true);
                (None,
                 raw_grouping_key,
                 raw_grouping_key_type.clone(),
                 max_grouping_key_buf)
            } else {
                query_plan::prepare_hashmap_grouping(
                    raw_grouping_key,
                    max_grouping_key as usize,
                    &mut planner)?
            };

        // Aggregators
        let mut aggregation_results = Vec::new();
        let mut selector = None;
        let mut selector_index = None;
        for (i, &(aggregator, ref expr)) in self.aggregate.iter().enumerate() {
            let (plan, plan_type) = QueryPlan::compile_expr(expr, filter, columns, &mut planner)?;
            let (aggregate, t) = query_plan::prepare_aggregation(
                plan,
                plan_type,
                grouping_key,
                aggregation_cardinality,
                aggregator,
                &mut planner)?;
            // TODO(clemens): if summation column is strictly positive, can use sum as well
            if aggregator == Aggregator::Count {
                selector = Some((aggregate, t.encoding_type()));
                selector_index = Some(i)
            }
            aggregation_results.push((aggregator, aggregate, t))
        }

        // Determine selector
        let selector = match selector {
            None => planner.exists(grouping_key, aggregation_cardinality).into(),
            Some(x) => x.0,
        };

        // Construct (encoded) group by column
        let encoded_group_by_column = match encoded_group_by_column {
            None => planner.nonzero_indices(selector, grouping_key_type.encoding_type()),
            Some(x) => x,
        };
        planner.connect(encoded_group_by_column, encoded_group_by_placeholder);

        // Compact and decode aggregation results
        let mut aggregation_cols = Vec::new();
        {
            let mut decode_compact = |aggregator: Aggregator,
                                      aggregate: TypedBufferRef,
                                      t: Type| {
                let compacted = match aggregator {
                    // TODO(clemens): if summation column is strictly positive, can use NonzeroCompact
                    Aggregator::Sum => planner.compact(aggregate, selector),
                    Aggregator::Count => planner.nonzero_compact(aggregate),
                };
                if t.is_encoded() {
                    Ok(t.codec.clone().unwrap().decode(compacted, &mut planner))
                } else {
                    Ok(compacted)
                }
            };

            for (i, &(aggregator, aggregate, ref t)) in aggregation_results.iter().enumerate() {
                if selector_index != Some(i) {
                    let decode_compacted = decode_compact(aggregator, aggregate, t.clone())?;
                    aggregation_cols.push((decode_compacted, aggregator))
                }
            }

            // TODO(clemens): is there a simpler way to do this?
            if let Some(i) = selector_index {
                let (aggregator, aggregate, ref t) = aggregation_results[i];
                let selector = decode_compact(aggregator, aggregate, t.clone())?;
                aggregation_cols.insert(i, (selector, aggregator));
            }
        }

        //  Reconstruct all group by columns from grouping
        let mut grouping_columns = Vec::with_capacity(decode_plans.len());
        for (decode_plan, _t) in decode_plans {
            let decoded = decode_plan.clone();
            grouping_columns.push(decoded);
        }

        // If the grouping is not order preserving, we need to sort all output columns by using the ordering constructed from the decoded group by columns
        // This is necessary to make it possible to efficiently merge with other batch results
        if !grouping_key_type.is_order_preserving() {
            let sort_indices = if raw_grouping_key_type.is_order_preserving() {
                let indices = planner.indices(encoded_group_by_column);
                planner.sort_by(encoded_group_by_column,
                                indices,
                                false /* desc */,
                                false /* stable */)
            } else {
                if grouping_columns.len() != 1 {
                    // TODO(clemens): debug print `planner`
                    bail!(QueryError::NotImplemented,
                        "Grouping key is not order preserving and more than 1 grouping column\nGrouping key type: {:?}\nTODO: PLANNER",
                        &grouping_key_type )
                }
                let indices = planner.indices(grouping_columns[0]);
                planner.sort_by(grouping_columns[0],
                                indices,
                                false /* desc */,
                                false /* stable */)
            };

            let mut aggregations2 = Vec::new();
            for &(a, aggregator) in &aggregation_cols {
                aggregations2.push(
                    (planner.select(a, sort_indices),
                     aggregator));
            }
            aggregation_cols = aggregations2;

            let mut grouping_columns2 = Vec::new();
            for s in &grouping_columns {
                grouping_columns2.push(planner.select(*s, sort_indices));
            }
            grouping_columns = grouping_columns2;
        }

        for c in columns {
            debug!("{}: {:?}", partition, c);
        }
        let mut executor = planner.prepare(vec![])?;
        let mut results = executor.prepare(NormalFormQuery::column_data(columns));
        debug!("{:#}", &executor);
        executor.run(columns.iter().next().map(|c| c.1.len()).unwrap_or(1), &mut results, show);
        let (columns, projection, aggregations, _) = results.collect_aliased(
            &grouping_columns.iter().map(|s| s.any()).collect::<Vec<_>>(),
            &aggregation_cols.iter().map(|&(s, aggregator)| (s.any(), aggregator)).collect::<Vec<_>>(),
            &[]);

        let batch = BatchResult {
            columns,
            projection,
            aggregations,
            order_by: vec![],
            level: 0,
            batch_count: 1,
            show,
            unsafe_referenced_buffers: results.collect_pinned(),
        };
        if let Err(err) = batch.validate() {
            warn!("Query result failed validation (partition {}): {}\n{:#}\nGroup By: {:?}\nSelect: {:?}",
                  partition, err, &executor, grouping_columns, aggregation_cols);
            Err(err)
        } else {
            Ok((
                batch,
                if explain { Some(format!("{}", executor)) } else { None }
            ))
        }
    }

    fn column_data(columns: &HashMap<String, Arc<DataSource>>) -> HashMap<String, Vec<&Data>> {
        columns.iter()
            .map(|(name, column)| (name.to_string(), column.data_sections()))
            .collect()
    }

    pub fn result_column_names(&self) -> Vec<String> {
        let mut anon_columns = -1;
        let select_cols = self.projection
            .iter()
            .map(|expr| match *expr {
                Expr::ColName(ref name) => name.clone(),
                _ => {
                    anon_columns += 1;
                    // TODO(clemens): collision with existing columns
                    format!("col_{}", anon_columns)
                }
            });
        let mut anon_aggregates = -1;
        let aggregate_cols = self.aggregate
            .iter()
            .map(|&(agg, _)| {
                anon_aggregates += 1;
                match agg {
                    Aggregator::Count => format!("count_{}", anon_aggregates),
                    Aggregator::Sum => format!("sum_{}", anon_aggregates),
                }
            });

        select_cols.chain(aggregate_cols).collect()
    }
}

impl Query {
    pub fn normalize(&self) -> (NormalFormQuery, Option<NormalFormQuery>) {
        let mut final_projection = Vec::new();
        let mut select = Vec::new();
        let mut aggregate = Vec::new();
        let mut aggregate_colnames = Vec::new();
        let mut select_colnames = Vec::new();
        for expr in &self.select {
            let (full_expr, aggregates) = Query::extract_aggregators(expr, &mut aggregate_colnames);
            if aggregates.is_empty() {
                let column_name = format!("_cs{}", select_colnames.len());
                select_colnames.push(column_name.clone());
                select.push(full_expr);
                final_projection.push(Expr::ColName(column_name));
            } else {
                aggregate.extend(aggregates);
                final_projection.push(full_expr);
            }
        }

        let require_final_pass = (!aggregate.is_empty() && !self.order_by.is_empty())
            || final_projection.iter()
            .any(|expr| match expr {
                Expr::ColName(_) => false,
                _ => true,
            });

        if require_final_pass {
            let mut final_order_by = Vec::new();
            for (expr, desc) in &self.order_by {
                let (full_expr, aggregates) = Query::extract_aggregators(expr, &mut aggregate_colnames);
                if aggregates.is_empty() {
                    let column_name = format!("_cs{}", select_colnames.len());
                    select_colnames.push(column_name.clone());
                    select.push(full_expr);
                    final_order_by.push((Expr::ColName(column_name), *desc));
                } else {
                    aggregate.extend(aggregates);
                    final_order_by.push((full_expr, *desc));
                }
            }
            (
                NormalFormQuery {
                    projection: select,
                    filter: self.filter.clone(),
                    aggregate,
                    order_by: vec![],
                    limit: self.limit.clone(),
                },
                Some(NormalFormQuery {
                    projection: final_projection,
                    filter: Expr::Const(RawVal::Int(1)),
                    aggregate: vec![],
                    order_by: final_order_by,
                    limit: self.limit.clone(),
                }),
            )
        } else {
            (
                NormalFormQuery {
                    projection: select,
                    filter: self.filter.clone(),
                    aggregate,
                    order_by: self.order_by.clone(),
                    limit: self.limit.clone(),
                },
                None,
            )
        }
    }

    pub fn extract_aggregators(expr: &Expr, column_names: &mut Vec<String>) -> (Expr, Vec<(Aggregator, Expr)>) {
        match expr {
            Expr::Aggregate(aggregator, expr) => {
                let column_name = format!("_ca{}", column_names.len());
                column_names.push(column_name.clone());
                // TODO(clemens): ensure no nested aggregates
                (Expr::ColName(column_name), vec![(*aggregator, *expr.clone())])
            }
            Expr::Func1(t, expr) => {
                let (expr, aggregates) = Query::extract_aggregators(expr, column_names);
                (Expr::Func1(*t, Box::new(expr)), aggregates)
            }
            Expr::Func2(t, expr1, expr2) => {
                let (expr1, mut aggregates1) = Query::extract_aggregators(expr1, column_names);
                let (expr2, aggregates2) = Query::extract_aggregators(expr2, column_names);
                aggregates1.extend(aggregates2);
                (Expr::Func2(*t, Box::new(expr1), Box::new(expr2)), aggregates1)
            }
            Expr::Const(_) | Expr::ColName(_) => (expr.clone(), vec![]),
        }
    }

    pub fn is_select_star(&self) -> bool {
        if self.select.len() == 1 {
            match self.select[0] {
                Expr::ColName(ref colname) if colname == "*" => true,
                _ => false,
            }
        } else {
            false
        }
    }

    pub fn find_referenced_cols(&self) -> HashSet<String> {
        let mut colnames = HashSet::new();
        for expr in &self.select {
            expr.add_colnames(&mut colnames);
        }
        for expr in &self.order_by {
            expr.0.add_colnames(&mut colnames);
        }
        self.filter.add_colnames(&mut colnames);
        colnames
    }
}


