// Copyright 2022 Datafuse Labs.
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

use std::collections::HashSet;

use common_datavalues::type_coercion::compare_coercion;
use common_datavalues::BooleanType;
use common_datavalues::DataTypeImpl;
use common_datavalues::NullableType;
use common_exception::ErrorCode;
use common_exception::Result;
use common_planner::IndexType;
use common_planner::MetadataRef;

use crate::sql::binder::wrap_cast;
use crate::sql::binder::JoinPredicate;
use crate::sql::binder::Visibility;
use crate::sql::optimizer::heuristic::subquery_rewriter::FlattenInfo;
use crate::sql::optimizer::heuristic::subquery_rewriter::SubqueryRewriter;
use crate::sql::optimizer::heuristic::subquery_rewriter::UnnestResult;
use crate::sql::optimizer::ColumnSet;
use crate::sql::optimizer::RelExpr;
use crate::sql::optimizer::SExpr;
use crate::sql::plans::Aggregate;
use crate::sql::plans::AggregateFunction;
use crate::sql::plans::AggregateMode;
use crate::sql::plans::AndExpr;
use crate::sql::plans::BoundColumnRef;
use crate::sql::plans::CastExpr;
use crate::sql::plans::ComparisonExpr;
use crate::sql::plans::EvalScalar;
use crate::sql::plans::Filter;
use crate::sql::plans::FunctionCall;
use crate::sql::plans::JoinType;
use crate::sql::plans::LogicalGet;
use crate::sql::plans::LogicalInnerJoin;
use crate::sql::plans::OrExpr;
use crate::sql::plans::PatternPlan;
use crate::sql::plans::RelOp;
use crate::sql::plans::RelOperator;
use crate::sql::plans::Scalar;
use crate::sql::plans::ScalarItem;
use crate::sql::plans::SubqueryExpr;
use crate::sql::plans::SubqueryType;
use crate::sql::ColumnBinding;
use crate::sql::ScalarExpr;

/// Decorrelate subqueries inside `s_expr`.
///
/// We only need to process three kinds of join: Scalar Subquery, Any Subquery, and Exists Subquery.
/// Other kinds of subqueries have be converted to one of the above subqueries in `type_check`.
///
/// It will rewrite `s_expr` to all kinds of join.
/// Correlated scalar subquery -> Single join
/// Any subquery -> Marker join
/// Correlated exists subquery -> Marker join
///
/// More information can be found in the paper: Unnesting Arbitrary Queries
pub fn decorrelate_subquery(metadata: MetadataRef, s_expr: SExpr) -> Result<SExpr> {
    let mut rewriter = SubqueryRewriter::new(metadata);
    let hoisted = rewriter.rewrite(&s_expr)?;

    Ok(hoisted)
}

impl SubqueryRewriter {
    // Try to decorrelate a `CrossApply` into `SemiJoin` or `AntiJoin`.
    // We only do simple decorrelation here, the scheme is:
    // 1. If the subquery is correlated, we will try to decorrelate it into `SemiJoin`
    pub fn try_decorrelate_simple_subquery(
        &self,
        input: &SExpr,
        subquery: &SubqueryExpr,
    ) -> Result<Option<SExpr>> {
        if subquery.outer_columns.is_empty() {
            return Ok(None);
        }

        // TODO(leiysky): this is the canonical plan generated by Binder, we should find a proper
        // way to address such a pattern.
        //
        //   EvalScalar
        //    \
        //     Filter
        //      \
        //       Get
        let pattern = SExpr::create_unary(
            PatternPlan {
                plan_type: RelOp::EvalScalar,
            }
            .into(),
            SExpr::create_unary(
                PatternPlan {
                    plan_type: RelOp::Filter,
                }
                .into(),
                SExpr::create_leaf(
                    PatternPlan {
                        plan_type: RelOp::LogicalGet,
                    }
                    .into(),
                ),
            ),
        );

        if !subquery.subquery.match_pattern(&pattern) {
            return Ok(None);
        }

        let filter_tree = subquery
            .subquery // EvalScalar
            .child(0)?; // Filter
        let filter_expr = RelExpr::with_s_expr(filter_tree);
        let filter: Filter = subquery
            .subquery // EvalScalar
            .child(0)? // Filter
            .plan()
            .clone()
            .try_into()?;
        let filter_prop = filter_expr.derive_relational_prop()?;
        let filter_child_prop = filter_expr.derive_relational_prop_child(0)?;

        let input_expr = RelExpr::with_s_expr(input);
        let input_prop = input_expr.derive_relational_prop()?;

        // First, we will check if all the outer columns are in the filter.
        if !filter_child_prop.outer_columns.is_empty() {
            return Ok(None);
        }

        // Second, we will check if the filter only contains equi-predicates.
        // This is not necessary, but it is a good heuristic for most cases.
        let mut left_conditions = vec![];
        let mut right_conditions = vec![];
        let mut other_conditions = vec![];
        let mut left_filters = vec![];
        let mut right_filters = vec![];
        for pred in filter.predicates.iter() {
            let join_condition = JoinPredicate::new(pred, &input_prop, &filter_prop);
            match join_condition {
                JoinPredicate::Left(filter) => {
                    left_filters.push(filter.clone());
                }
                JoinPredicate::Right(filter) => {
                    right_filters.push(filter.clone());
                }

                JoinPredicate::Other(pred) => {
                    other_conditions.push(pred.clone());
                }

                JoinPredicate::Both { left, right } => {
                    if left.data_type().eq(&right.data_type()) {
                        left_conditions.push(left.clone());
                        right_conditions.push(right.clone());
                        continue;
                    }
                    let join_type = compare_coercion(&left.data_type(), &right.data_type())?;
                    let left = wrap_cast(left.clone(), &join_type);
                    let right = wrap_cast(right.clone(), &join_type);
                    left_conditions.push(left);
                    right_conditions.push(right);
                }
            }
        }

        let join = LogicalInnerJoin {
            left_conditions,
            right_conditions,
            other_conditions,
            join_type: match &subquery.typ {
                SubqueryType::Any | SubqueryType::All | SubqueryType::Scalar => {
                    return Ok(None);
                }
                SubqueryType::Exists => JoinType::LeftSemi,
                SubqueryType::NotExists => JoinType::LeftAnti,
            },
            marker_index: None,
            from_correlated_subquery: true,
        };

        // Rewrite plan to semi-join.
        let mut left_child = input.clone();
        if !left_filters.is_empty() {
            left_child = SExpr::create_unary(
                Filter {
                    predicates: left_filters,
                    is_having: false,
                }
                .into(),
                left_child,
            );
        }

        // Remove `Filter` from subquery.
        let mut right_child = SExpr::create_unary(
            subquery.subquery.plan().clone(),
            SExpr::create_unary(
                subquery.subquery.plan().clone(),
                SExpr::create_leaf(filter_tree.child(0)?.plan().clone()),
            ),
        );
        if !right_filters.is_empty() {
            right_child = SExpr::create_unary(
                Filter {
                    predicates: right_filters,
                    is_having: false,
                }
                .into(),
                right_child,
            );
        }

        let result = SExpr::create_binary(join.into(), left_child, right_child);

        Ok(Some(result))
    }

    pub fn try_decorrelate_subquery(
        &mut self,
        left: &SExpr,
        subquery: &SubqueryExpr,
        flatten_info: &mut FlattenInfo,
        is_conjunctive_predicate: bool,
    ) -> Result<(SExpr, UnnestResult)> {
        match subquery.typ {
            SubqueryType::Scalar => {
                let correlated_columns = subquery.outer_columns.clone();
                let flatten_plan =
                    self.flatten(&subquery.subquery, &correlated_columns, flatten_info)?;
                // Construct single join
                let mut left_conditions = Vec::with_capacity(correlated_columns.len());
                let mut right_conditions = Vec::with_capacity(correlated_columns.len());
                self.add_equi_conditions(
                    &correlated_columns,
                    &mut right_conditions,
                    &mut left_conditions,
                )?;
                let join_plan = LogicalInnerJoin {
                    left_conditions,
                    right_conditions,
                    other_conditions: vec![],
                    join_type: JoinType::Single,
                    marker_index: None,
                    from_correlated_subquery: true,
                };
                let s_expr = SExpr::create_binary(join_plan.into(), left.clone(), flatten_plan);
                Ok((s_expr, UnnestResult::SingleJoin))
            }
            SubqueryType::Exists | SubqueryType::NotExists => {
                if is_conjunctive_predicate {
                    if let Some(result) = self.try_decorrelate_simple_subquery(left, subquery)? {
                        return Ok((result, UnnestResult::SimpleJoin));
                    }
                }
                let correlated_columns = subquery.outer_columns.clone();
                let flatten_plan =
                    self.flatten(&subquery.subquery, &correlated_columns, flatten_info)?;
                // Construct mark join
                let mut left_conditions = Vec::with_capacity(correlated_columns.len());
                let mut right_conditions = Vec::with_capacity(correlated_columns.len());
                self.add_equi_conditions(
                    &correlated_columns,
                    &mut left_conditions,
                    &mut right_conditions,
                )?;
                let marker_index = if let Some(idx) = subquery.projection_index {
                    idx
                } else {
                    self.metadata.write().add_column(
                        "marker".to_string(),
                        NullableType::new_impl(BooleanType::new_impl()),
                        None,
                        None,
                    )
                };
                let join_plan = LogicalInnerJoin {
                    left_conditions,
                    right_conditions,
                    other_conditions: vec![],
                    join_type: JoinType::LeftMark,
                    marker_index: Some(marker_index),
                    from_correlated_subquery: true,
                };
                let s_expr = SExpr::create_binary(join_plan.into(), flatten_plan, left.clone());
                Ok((s_expr, UnnestResult::MarkJoin { marker_index }))
            }
            SubqueryType::Any => {
                let correlated_columns = subquery.outer_columns.clone();
                let flatten_plan =
                    self.flatten(&subquery.subquery, &correlated_columns, flatten_info)?;
                let mut left_conditions = Vec::with_capacity(correlated_columns.len());
                let mut right_conditions = Vec::with_capacity(correlated_columns.len());
                self.add_equi_conditions(
                    &correlated_columns,
                    &mut left_conditions,
                    &mut right_conditions,
                )?;
                let index = subquery.output_column;
                let column_name = format!("subquery_{}", index);
                let right_condition = Scalar::BoundColumnRef(BoundColumnRef {
                    column: ColumnBinding {
                        database_name: None,
                        table_name: None,
                        column_name,
                        index,
                        data_type: subquery.data_type.clone(),
                        visibility: Visibility::Visible,
                    },
                });
                let child_expr = *subquery.child_expr.as_ref().unwrap().clone();
                let op = subquery.compare_op.as_ref().unwrap().clone();
                // Make <child_expr op right_condition> as other_conditions even if op is equal operator.
                // Because it's not null-safe.
                let other_conditions = vec![Scalar::ComparisonExpr(ComparisonExpr {
                    op,
                    left: Box::new(child_expr),
                    right: Box::new(right_condition),
                    return_type: Box::new(NullableType::new_impl(BooleanType::new_impl())),
                })];
                let marker_index = if let Some(idx) = subquery.projection_index {
                    idx
                } else {
                    self.metadata.write().add_column(
                        "marker".to_string(),
                        NullableType::new_impl(BooleanType::new_impl()),
                        None,
                        None,
                    )
                };
                let mark_join = LogicalInnerJoin {
                    left_conditions,
                    right_conditions,
                    other_conditions,
                    join_type: JoinType::LeftMark,
                    marker_index: Some(marker_index),
                    from_correlated_subquery: true,
                }
                .into();
                Ok((
                    SExpr::create_binary(mark_join, flatten_plan, left.clone()),
                    UnnestResult::MarkJoin { marker_index },
                ))
            }
            _ => unreachable!(),
        }
    }

    fn flatten(
        &mut self,
        plan: &SExpr,
        correlated_columns: &ColumnSet,
        flatten_info: &mut FlattenInfo,
    ) -> Result<SExpr> {
        let rel_expr = RelExpr::with_s_expr(plan);
        let prop = rel_expr.derive_relational_prop()?;
        if prop.outer_columns.is_empty() {
            // Construct a LogicalGet plan by correlated columns.
            // Finally generate a cross join, so we finish flattening the subquery.
            let mut metadata = self.metadata.write();
            // Currently, we don't support left plan's from clause contains subquery.
            // Such as: select t2.a from (select a + 1 as a from t) as t2 where (select sum(a) from t as t1 where t1.a < t2.a) = 1;
            let table_index = metadata
                .table_index_by_column_indexes(correlated_columns)
                .unwrap();
            for correlated_column in correlated_columns.iter() {
                let column_entry = metadata.column(*correlated_column).clone();
                self.derived_columns.insert(
                    *correlated_column,
                    metadata.add_column(
                        column_entry.name().to_string(),
                        if let DataTypeImpl::Nullable(_) = column_entry.data_type() {
                            column_entry.data_type().clone()
                        } else {
                            DataTypeImpl::Nullable(NullableType::create(
                                column_entry.data_type().clone(),
                            ))
                        },
                        None,
                        None,
                    ),
                );
            }
            let logical_get = SExpr::create_leaf(
                LogicalGet {
                    table_index,
                    columns: self.derived_columns.values().cloned().collect(),
                    push_down_predicates: None,
                    limit: None,
                    order_by: None,
                    statistics: None,
                    prewhere: None,
                }
                .into(),
            );
            // Todo(xudong963): Wrap logical get with distinct to eliminate duplicates rows.
            let cross_join = LogicalInnerJoin {
                left_conditions: vec![],
                right_conditions: vec![],
                other_conditions: vec![],
                join_type: JoinType::Cross,
                marker_index: None,
                from_correlated_subquery: false,
            }
            .into();
            return Ok(SExpr::create_binary(cross_join, logical_get, plan.clone()));
        }

        match plan.plan() {
            RelOperator::EvalScalar(eval_scalar) => {
                let flatten_plan =
                    self.flatten(plan.child(0)?, correlated_columns, flatten_info)?;
                let mut items = Vec::with_capacity(eval_scalar.items.len());
                for item in eval_scalar.items.iter() {
                    let new_item = ScalarItem {
                        scalar: self.flatten_scalar(&item.scalar, correlated_columns)?,
                        index: item.index,
                    };
                    items.push(new_item);
                }
                let metadata = self.metadata.read();
                for derived_column in self.derived_columns.values() {
                    let column_entry = metadata.column(*derived_column);
                    let column_binding = ColumnBinding {
                        database_name: None,
                        table_name: None,
                        column_name: format!("subquery_{}", derived_column),
                        index: *derived_column,
                        data_type: Box::from(column_entry.data_type().clone()),
                        visibility: Visibility::Visible,
                    };
                    items.push(ScalarItem {
                        scalar: Scalar::BoundColumnRef(BoundColumnRef {
                            column: column_binding,
                        }),
                        index: *derived_column,
                    });
                }
                Ok(SExpr::create_unary(
                    EvalScalar { items }.into(),
                    flatten_plan,
                ))
            }
            RelOperator::Filter(filter) => {
                let flatten_plan =
                    self.flatten(plan.child(0)?, correlated_columns, flatten_info)?;
                let mut predicates = Vec::with_capacity(filter.predicates.len());
                for predicate in filter.predicates.iter() {
                    predicates.push(self.flatten_scalar(predicate, correlated_columns)?);
                }
                let filter_plan = Filter {
                    predicates,
                    is_having: filter.is_having,
                }
                .into();
                Ok(SExpr::create_unary(filter_plan, flatten_plan))
            }
            RelOperator::LogicalInnerJoin(join) => {
                // Currently, we don't support join conditions contain subquery
                let left_flatten_plan =
                    self.flatten(plan.child(0)?, correlated_columns, flatten_info)?;
                let right_flatten_plan =
                    self.flatten(plan.child(1)?, correlated_columns, flatten_info)?;
                Ok(SExpr::create_binary(
                    LogicalInnerJoin {
                        left_conditions: join.left_conditions.clone(),
                        right_conditions: join.right_conditions.clone(),
                        other_conditions: join.other_conditions.clone(),
                        join_type: join.join_type.clone(),
                        marker_index: join.marker_index,
                        from_correlated_subquery: false,
                    }
                    .into(),
                    left_flatten_plan,
                    right_flatten_plan,
                ))
            }
            RelOperator::Aggregate(aggregate) => {
                let flatten_plan =
                    self.flatten(plan.child(0)?, correlated_columns, flatten_info)?;
                let mut group_items = Vec::with_capacity(aggregate.group_items.len());
                for item in aggregate.group_items.iter() {
                    let scalar = self.flatten_scalar(&item.scalar, correlated_columns)?;
                    group_items.push(ScalarItem {
                        scalar,
                        index: item.index,
                    })
                }
                for derived_column in self.derived_columns.values() {
                    let column_binding = {
                        let metadata = self.metadata.read();
                        let column_entry = metadata.column(*derived_column);
                        ColumnBinding {
                            database_name: None,
                            table_name: None,
                            column_name: format!("subquery_{}", derived_column),
                            index: *derived_column,
                            data_type: Box::from(column_entry.data_type().clone()),
                            visibility: Visibility::Visible,
                        }
                    };
                    group_items.push(ScalarItem {
                        scalar: Scalar::BoundColumnRef(BoundColumnRef {
                            column: column_binding,
                        }),
                        index: *derived_column,
                    });
                }
                let mut agg_items = Vec::with_capacity(aggregate.aggregate_functions.len());
                for item in aggregate.aggregate_functions.iter() {
                    let scalar = self.flatten_scalar(&item.scalar, correlated_columns)?;
                    if let Scalar::AggregateFunction(AggregateFunction { func_name, .. }) = &scalar
                    {
                        if func_name.eq_ignore_ascii_case("count") || func_name.eq("count_distinct")
                        {
                            flatten_info.from_count_func = true;
                        }
                    }
                    agg_items.push(ScalarItem {
                        scalar,
                        index: item.index,
                    })
                }
                Ok(SExpr::create_unary(
                    Aggregate {
                        mode: AggregateMode::Initial,
                        group_items,
                        aggregate_functions: agg_items,
                        from_distinct: aggregate.from_distinct,
                    }
                    .into(),
                    flatten_plan,
                ))
            }
            RelOperator::Sort(_) | RelOperator::Limit(_) => {
                // Currently, we don't support sort and limit contain subquery.
                let flatten_plan =
                    self.flatten(plan.child(0)?, correlated_columns, flatten_info)?;
                Ok(SExpr::create_unary(plan.plan().clone(), flatten_plan))
            }

            RelOperator::UnionAll(op) => {
                let left_flatten_plan =
                    self.flatten(plan.child(0)?, correlated_columns, flatten_info)?;
                let right_flatten_plan =
                    self.flatten(plan.child(1)?, correlated_columns, flatten_info)?;
                Ok(SExpr::create_binary(
                    op.clone().into(),
                    left_flatten_plan,
                    right_flatten_plan,
                ))
            }

            RelOperator::Exchange(_)
            | RelOperator::Pattern(_)
            | RelOperator::LogicalGet(_)
            | RelOperator::PhysicalScan(_)
            | RelOperator::DummyTableScan(_)
            | RelOperator::PhysicalHashJoin(_) => Err(ErrorCode::LogicalError(
                "Invalid plan type for flattening subquery",
            )),
        }
    }

    fn flatten_scalar(
        &mut self,
        scalar: &Scalar,
        correlated_columns: &ColumnSet,
    ) -> Result<Scalar> {
        match scalar {
            Scalar::BoundColumnRef(bound_column) => {
                let column_binding = bound_column.column.clone();
                if correlated_columns.contains(&column_binding.index) {
                    let index = self.derived_columns.get(&column_binding.index).unwrap();
                    return Ok(Scalar::BoundColumnRef(BoundColumnRef {
                        column: ColumnBinding {
                            database_name: None,
                            table_name: None,
                            column_name: format!("subquery_{}", index),
                            index: *index,
                            data_type: column_binding.data_type.clone(),
                            visibility: column_binding.visibility,
                        },
                    }));
                }
                Ok(scalar.clone())
            }
            Scalar::ConstantExpr(_) => Ok(scalar.clone()),
            Scalar::AndExpr(and_expr) => {
                let left = self.flatten_scalar(&and_expr.left, correlated_columns)?;
                let right = self.flatten_scalar(&and_expr.right, correlated_columns)?;
                Ok(Scalar::AndExpr(AndExpr {
                    left: Box::new(left),
                    right: Box::new(right),
                    return_type: and_expr.return_type.clone(),
                }))
            }
            Scalar::OrExpr(or_expr) => {
                let left = self.flatten_scalar(&or_expr.left, correlated_columns)?;
                let right = self.flatten_scalar(&or_expr.right, correlated_columns)?;
                Ok(Scalar::OrExpr(OrExpr {
                    left: Box::new(left),
                    right: Box::new(right),
                    return_type: or_expr.return_type.clone(),
                }))
            }
            Scalar::ComparisonExpr(comparison_expr) => {
                let left = self.flatten_scalar(&comparison_expr.left, correlated_columns)?;
                let right = self.flatten_scalar(&comparison_expr.right, correlated_columns)?;
                Ok(Scalar::ComparisonExpr(ComparisonExpr {
                    op: comparison_expr.op.clone(),
                    left: Box::new(left),
                    right: Box::new(right),
                    return_type: comparison_expr.return_type.clone(),
                }))
            }
            Scalar::AggregateFunction(agg) => {
                let mut args = Vec::with_capacity(agg.args.len());
                for arg in &agg.args {
                    args.push(self.flatten_scalar(arg, correlated_columns)?);
                }
                Ok(Scalar::AggregateFunction(AggregateFunction {
                    display_name: agg.display_name.clone(),
                    func_name: agg.func_name.clone(),
                    distinct: agg.distinct,
                    params: agg.params.clone(),
                    args,
                    return_type: agg.return_type.clone(),
                }))
            }
            Scalar::FunctionCall(fun_call) => {
                let mut arguments = Vec::with_capacity(fun_call.arguments.len());
                for arg in &fun_call.arguments {
                    arguments.push(self.flatten_scalar(arg, correlated_columns)?);
                }
                Ok(Scalar::FunctionCall(FunctionCall {
                    arguments,
                    func_name: fun_call.func_name.clone(),
                    arg_types: fun_call.arg_types.clone(),
                    return_type: fun_call.return_type.clone(),
                }))
            }
            Scalar::CastExpr(cast_expr) => {
                let scalar = self.flatten_scalar(&cast_expr.argument, correlated_columns)?;
                Ok(Scalar::CastExpr(CastExpr {
                    argument: Box::new(scalar),
                    from_type: cast_expr.from_type.clone(),
                    target_type: cast_expr.target_type.clone(),
                }))
            }
            _ => Err(ErrorCode::LogicalError(
                "Invalid scalar for flattening subquery",
            )),
        }
    }

    fn add_equi_conditions(
        &self,
        correlated_columns: &HashSet<IndexType>,
        left_conditions: &mut Vec<Scalar>,
        right_conditions: &mut Vec<Scalar>,
    ) -> Result<()> {
        for correlated_column in correlated_columns.iter() {
            let data_type = {
                let metadata = self.metadata.read();
                let column_entry = metadata.column(*correlated_column);
                column_entry.data_type().clone()
            };
            let right_column = Scalar::BoundColumnRef(BoundColumnRef {
                column: ColumnBinding {
                    database_name: None,
                    table_name: None,
                    column_name: format!("subquery_{}", correlated_column),
                    index: *correlated_column,
                    data_type: Box::from(data_type.clone()),
                    visibility: Visibility::Visible,
                },
            });
            let derive_column = self.derived_columns.get(correlated_column).unwrap();
            let left_column = Scalar::BoundColumnRef(BoundColumnRef {
                column: ColumnBinding {
                    database_name: None,
                    table_name: None,
                    column_name: format!("subquery_{}", derive_column),
                    index: *derive_column,
                    data_type: Box::from(data_type),
                    visibility: Visibility::Visible,
                },
            });
            left_conditions.push(left_column);
            right_conditions.push(right_column);
        }
        Ok(())
    }
}
