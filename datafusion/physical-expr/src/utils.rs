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

use crate::expressions::{BinaryExpr, Column, UnKnownColumn};
use crate::{PhysicalExpr, PhysicalSortExpr};

use arrow::array::{make_array, Array, ArrayRef, BooleanArray, MutableArrayData};
use arrow::compute::{and_kleene, is_not_null, SlicesIterator};
use arrow::datatypes::SchemaRef;
use datafusion_common::tree_node::{
    Transformed, TreeNode, TreeNodeRewriter, VisitRecursion,
};
use datafusion_common::Result;
use datafusion_expr::Operator;

use crate::equivalence::ProjectionMapping;
use itertools::Itertools;
use petgraph::graph::NodeIndex;
use petgraph::stable_graph::StableGraph;
use std::borrow::Borrow;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;

/// Compare the two expr lists are equal no matter the order.
/// For example two InListExpr can be considered to be equals no matter the order:
///
/// In('a','b','c') == In('c','b','a')
pub fn expr_list_eq_any_order(
    list1: &[Arc<dyn PhysicalExpr>],
    list2: &[Arc<dyn PhysicalExpr>],
) -> bool {
    if list1.len() == list2.len() {
        let mut expr_vec1 = list1.to_vec();
        let mut expr_vec2 = list2.to_vec();
        while let Some(expr1) = expr_vec1.pop() {
            if let Some(idx) = expr_vec2.iter().position(|expr2| expr1.eq(expr2)) {
                expr_vec2.swap_remove(idx);
            } else {
                break;
            }
        }
        expr_vec1.is_empty() && expr_vec2.is_empty()
    } else {
        false
    }
}

/// Strictly compare the two expr lists are equal in the given order.
pub fn expr_list_eq_strict_order(
    list1: &[Arc<dyn PhysicalExpr>],
    list2: &[Arc<dyn PhysicalExpr>],
) -> bool {
    list1.len() == list2.len() && list1.iter().zip(list2.iter()).all(|(e1, e2)| e1.eq(e2))
}

/// Assume the predicate is in the form of CNF, split the predicate to a Vec of PhysicalExprs.
///
/// For example, split "a1 = a2 AND b1 <= b2 AND c1 != c2" into ["a1 = a2", "b1 <= b2", "c1 != c2"]
pub fn split_conjunction(
    predicate: &Arc<dyn PhysicalExpr>,
) -> Vec<&Arc<dyn PhysicalExpr>> {
    split_conjunction_impl(predicate, vec![])
}

fn split_conjunction_impl<'a>(
    predicate: &'a Arc<dyn PhysicalExpr>,
    mut exprs: Vec<&'a Arc<dyn PhysicalExpr>>,
) -> Vec<&'a Arc<dyn PhysicalExpr>> {
    match predicate.as_any().downcast_ref::<BinaryExpr>() {
        Some(binary) => match binary.op() {
            Operator::And => {
                let exprs = split_conjunction_impl(binary.left(), exprs);
                split_conjunction_impl(binary.right(), exprs)
            }
            _ => {
                exprs.push(predicate);
                exprs
            }
        },
        None => {
            exprs.push(predicate);
            exprs
        }
    }
}

/// Normalize the output expressions based on projection_map.
///
/// If there is a mapping in projection_map, replace the expression
/// in the output expressions with the expression after mapping.
/// Otherwise, replace the expression with a place holder of [UnKnownColumn]
///
pub fn project_out_expr(
    expr: Arc<dyn PhysicalExpr>,
    projection_map: &ProjectionMapping,
) -> Arc<dyn PhysicalExpr> {
    expr.clone()
        .transform(&|expr| {
            // If expression is not valid after projection. Treat it is as UnknownColumn.
            let mut normalized_form =
                Some(Arc::new(UnKnownColumn::new(&expr.to_string())) as _);
            for (source, target) in projection_map {
                if source.eq(&expr) {
                    normalized_form = Some(target.clone())
                }
            }
            Ok(if let Some(normalized_form) = normalized_form {
                Transformed::Yes(normalized_form)
            } else {
                Transformed::No(expr)
            })
        })
        .unwrap_or(expr)
}

/// This function maps back requirement after ProjectionExec
/// to the Executor for its input.
// Specifically, `ProjectionExec` changes index of `Column`s in the schema of its input executor.
// This function changes requirement given according to ProjectionExec schema to the requirement
// according to schema of input executor to the ProjectionExec.
// For instance, Column{"a", 0} would turn to Column{"a", 1}. Please note that this function assumes that
// name of the Column is unique. If we have a requirement such that Column{"a", 0}, Column{"a", 1}.
// This function will produce incorrect result (It will only emit single Column as a result).
pub fn map_columns_before_projection(
    parent_required: &[Arc<dyn PhysicalExpr>],
    proj_exprs: &[(Arc<dyn PhysicalExpr>, String)],
) -> Vec<Arc<dyn PhysicalExpr>> {
    let column_mapping = proj_exprs
        .iter()
        .filter_map(|(expr, name)| {
            expr.as_any()
                .downcast_ref::<Column>()
                .map(|column| (name.clone(), column.clone()))
        })
        .collect::<HashMap<_, _>>();
    parent_required
        .iter()
        .filter_map(|r| {
            r.as_any()
                .downcast_ref::<Column>()
                .and_then(|c| column_mapping.get(c.name()))
        })
        .map(|e| Arc::new(e.clone()) as _)
        .collect()
}

/// This function returns all `Arc<dyn PhysicalExpr>`s inside the given
/// `PhysicalSortExpr` sequence.
pub fn convert_to_expr<T: Borrow<PhysicalSortExpr>>(
    sequence: impl IntoIterator<Item = T>,
) -> Vec<Arc<dyn PhysicalExpr>> {
    sequence
        .into_iter()
        .map(|elem| elem.borrow().expr.clone())
        .collect()
}

/// This function finds the indices of `targets` within `items` using strict
/// equality.
pub fn get_indices_of_exprs_strict<T: Borrow<Arc<dyn PhysicalExpr>>>(
    targets: impl IntoIterator<Item = T>,
    items: &[Arc<dyn PhysicalExpr>],
) -> Vec<usize> {
    targets
        .into_iter()
        .filter_map(|target| items.iter().position(|e| e.eq(target.borrow())))
        .collect()
}

#[derive(Clone, Debug)]
pub struct ExprTreeNode<T> {
    expr: Arc<dyn PhysicalExpr>,
    data: Option<T>,
    child_nodes: Vec<ExprTreeNode<T>>,
}

impl<T> ExprTreeNode<T> {
    pub fn new(expr: Arc<dyn PhysicalExpr>) -> Self {
        ExprTreeNode {
            expr,
            data: None,
            child_nodes: vec![],
        }
    }

    pub fn expression(&self) -> &Arc<dyn PhysicalExpr> {
        &self.expr
    }

    pub fn children(&self) -> Vec<ExprTreeNode<T>> {
        self.expr
            .children()
            .into_iter()
            .map(ExprTreeNode::new)
            .collect()
    }
}

impl<T: Clone> TreeNode for ExprTreeNode<T> {
    fn apply_children<F>(&self, op: &mut F) -> Result<VisitRecursion>
    where
        F: FnMut(&Self) -> Result<VisitRecursion>,
    {
        for child in self.children() {
            match op(&child)? {
                VisitRecursion::Continue => {}
                VisitRecursion::Skip => return Ok(VisitRecursion::Continue),
                VisitRecursion::Stop => return Ok(VisitRecursion::Stop),
            }
        }

        Ok(VisitRecursion::Continue)
    }

    fn map_children<F>(mut self, transform: F) -> Result<Self>
    where
        F: FnMut(Self) -> Result<Self>,
    {
        self.child_nodes = self
            .children()
            .into_iter()
            .map(transform)
            .collect::<Result<Vec<_>>>()?;
        Ok(self)
    }
}

/// This struct facilitates the [TreeNodeRewriter] mechanism to convert a
/// [PhysicalExpr] tree into a DAEG (i.e. an expression DAG) by collecting
/// identical expressions in one node. Caller specifies the node type in the
/// DAEG via the `constructor` argument, which constructs nodes in the DAEG
/// from the [ExprTreeNode] ancillary object.
struct PhysicalExprDAEGBuilder<'a, T, F: Fn(&ExprTreeNode<NodeIndex>) -> T> {
    // The resulting DAEG (expression DAG).
    graph: StableGraph<T, usize>,
    // A vector of visited expression nodes and their corresponding node indices.
    visited_plans: Vec<(Arc<dyn PhysicalExpr>, NodeIndex)>,
    // A function to convert an input expression node to T.
    constructor: &'a F,
}

impl<'a, T, F: Fn(&ExprTreeNode<NodeIndex>) -> T> TreeNodeRewriter
    for PhysicalExprDAEGBuilder<'a, T, F>
{
    type N = ExprTreeNode<NodeIndex>;
    // This method mutates an expression node by transforming it to a physical expression
    // and adding it to the graph. The method returns the mutated expression node.
    fn mutate(
        &mut self,
        mut node: ExprTreeNode<NodeIndex>,
    ) -> Result<ExprTreeNode<NodeIndex>> {
        // Get the expression associated with the input expression node.
        let expr = &node.expr;

        // Check if the expression has already been visited.
        let node_idx = match self.visited_plans.iter().find(|(e, _)| expr.eq(e)) {
            // If the expression has been visited, return the corresponding node index.
            Some((_, idx)) => *idx,
            // If the expression has not been visited, add a new node to the graph and
            // add edges to its child nodes. Add the visited expression to the vector
            // of visited expressions and return the newly created node index.
            None => {
                let node_idx = self.graph.add_node((self.constructor)(&node));
                for expr_node in node.child_nodes.iter() {
                    self.graph.add_edge(node_idx, expr_node.data.unwrap(), 0);
                }
                self.visited_plans.push((expr.clone(), node_idx));
                node_idx
            }
        };
        // Set the data field of the input expression node to the corresponding node index.
        node.data = Some(node_idx);
        // Return the mutated expression node.
        Ok(node)
    }
}

// A function that builds a directed acyclic graph of physical expression trees.
pub fn build_dag<T, F>(
    expr: Arc<dyn PhysicalExpr>,
    constructor: &F,
) -> Result<(NodeIndex, StableGraph<T, usize>)>
where
    F: Fn(&ExprTreeNode<NodeIndex>) -> T,
{
    // Create a new expression tree node from the input expression.
    let init = ExprTreeNode::new(expr);
    // Create a new `PhysicalExprDAEGBuilder` instance.
    let mut builder = PhysicalExprDAEGBuilder {
        graph: StableGraph::<T, usize>::new(),
        visited_plans: Vec::<(Arc<dyn PhysicalExpr>, NodeIndex)>::new(),
        constructor,
    };
    // Use the builder to transform the expression tree node into a DAG.
    let root = init.rewrite(&mut builder)?;
    // Return a tuple containing the root node index and the DAG.
    Ok((root.data.unwrap(), builder.graph))
}

/// Recursively extract referenced [`Column`]s within a [`PhysicalExpr`].
pub fn collect_columns(expr: &Arc<dyn PhysicalExpr>) -> HashSet<Column> {
    let mut columns = HashSet::<Column>::new();
    expr.apply(&mut |expr| {
        if let Some(column) = expr.as_any().downcast_ref::<Column>() {
            if !columns.iter().any(|c| c.eq(column)) {
                columns.insert(column.clone());
            }
        }
        Ok(VisitRecursion::Continue)
    })
    // pre_visit always returns OK, so this will always too
    .expect("no way to return error during recursion");
    columns
}

/// Re-assign column indices referenced in predicate according to given schema.
/// This may be helpful when dealing with projections.
pub fn reassign_predicate_columns(
    pred: Arc<dyn PhysicalExpr>,
    schema: &SchemaRef,
    ignore_not_found: bool,
) -> Result<Arc<dyn PhysicalExpr>> {
    pred.transform_down(&|expr| {
        let expr_any = expr.as_any();

        if let Some(column) = expr_any.downcast_ref::<Column>() {
            let index = match schema.index_of(column.name()) {
                Ok(idx) => idx,
                Err(_) if ignore_not_found => usize::MAX,
                Err(e) => return Err(e.into()),
            };
            return Ok(Transformed::Yes(Arc::new(Column::new(
                column.name(),
                index,
            ))));
        }
        Ok(Transformed::No(expr))
    })
}

/// Reverses the ORDER BY expression, which is useful during equivalent window
/// expression construction. For instance, 'ORDER BY a ASC, NULLS LAST' turns into
/// 'ORDER BY a DESC, NULLS FIRST'.
pub fn reverse_order_bys(order_bys: &[PhysicalSortExpr]) -> Vec<PhysicalSortExpr> {
    order_bys
        .iter()
        .map(|e| PhysicalSortExpr {
            expr: e.expr.clone(),
            options: !e.options,
        })
        .collect()
}

/// Scatter `truthy` array by boolean mask. When the mask evaluates `true`, next values of `truthy`
/// are taken, when the mask evaluates `false` values null values are filled.
///
/// # Arguments
/// * `mask` - Boolean values used to determine where to put the `truthy` values
/// * `truthy` - All values of this array are to scatter according to `mask` into final result.
pub fn scatter(mask: &BooleanArray, truthy: &dyn Array) -> Result<ArrayRef> {
    let truthy = truthy.to_data();

    // update the mask so that any null values become false
    // (SlicesIterator doesn't respect nulls)
    let mask = and_kleene(mask, &is_not_null(mask)?)?;

    let mut mutable = MutableArrayData::new(vec![&truthy], true, mask.len());

    // the SlicesIterator slices only the true values. So the gaps left by this iterator we need to
    // fill with falsy values

    // keep track of how much is filled
    let mut filled = 0;
    // keep track of current position we have in truthy array
    let mut true_pos = 0;

    SlicesIterator::new(&mask).for_each(|(start, end)| {
        // the gap needs to be filled with nulls
        if start > filled {
            mutable.extend_nulls(start - filled);
        }
        // fill with truthy values
        let len = end - start;
        mutable.extend(0, true_pos, true_pos + len);
        true_pos += len;
        filled = end;
    });
    // the remaining part is falsy
    if filled < mask.len() {
        mutable.extend_nulls(mask.len() - filled);
    }

    let data = mutable.freeze();
    Ok(make_array(data))
}

/// Merge left and right sort expressions, checking for duplicates.
pub fn merge_vectors(
    left: &[PhysicalSortExpr],
    right: &[PhysicalSortExpr],
) -> Vec<PhysicalSortExpr> {
    left.iter()
        .cloned()
        .chain(right.iter().cloned())
        .unique()
        .collect()
}

#[cfg(test)]
mod tests {
    use std::fmt::{Display, Formatter};
    use std::ops::Not;
    use std::sync::Arc;

    use super::*;
    use crate::equivalence::SchemaProperties;
    use crate::expressions::{binary, cast, col, in_list, lit, Column, Literal};
    use crate::{PhysicalSortExpr, PhysicalSortRequirement};

    use arrow::compute::SortOptions;
    use arrow_array::Int32Array;
    use arrow_schema::{DataType, Field, Schema};
    use datafusion_common::cast::{as_boolean_array, as_int32_array};
    use datafusion_common::{Result, ScalarValue};

    use petgraph::visit::Bfs;

    #[derive(Clone)]
    struct DummyProperty {
        expr_type: String,
    }

    /// This is a dummy node in the DAEG; it stores a reference to the actual
    /// [PhysicalExpr] as well as a dummy property.
    #[derive(Clone)]
    struct PhysicalExprDummyNode {
        pub expr: Arc<dyn PhysicalExpr>,
        pub property: DummyProperty,
    }

    impl Display for PhysicalExprDummyNode {
        fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
            write!(f, "{}", self.expr)
        }
    }

    fn make_dummy_node(node: &ExprTreeNode<NodeIndex>) -> PhysicalExprDummyNode {
        let expr = node.expression().clone();
        let dummy_property = if expr.as_any().is::<BinaryExpr>() {
            "Binary"
        } else if expr.as_any().is::<Column>() {
            "Column"
        } else if expr.as_any().is::<Literal>() {
            "Literal"
        } else {
            "Other"
        }
        .to_owned();
        PhysicalExprDummyNode {
            expr,
            property: DummyProperty {
                expr_type: dummy_property,
            },
        }
    }

    // Generate a schema which consists of 5 columns (a, b, c, d, e)
    fn create_test_schema() -> Result<SchemaRef> {
        let a = Field::new("a", DataType::Int32, true);
        let b = Field::new("b", DataType::Int32, true);
        let c = Field::new("c", DataType::Int32, true);
        let d = Field::new("d", DataType::Int32, true);
        let e = Field::new("e", DataType::Int32, true);
        let f = Field::new("f", DataType::Int32, true);
        let schema = Arc::new(Schema::new(vec![a, b, c, d, e, f]));

        Ok(schema)
    }

    fn create_test_params() -> Result<(SchemaRef, SchemaProperties)> {
        // Assume schema satisfies ordering a ASC NULLS LAST
        // and d ASC NULLS LAST, b ASC NULLS LAST and e DESC NULLS FIRST, f ASC NULLS LAST, g ASC NULLS LAST
        // Assume that column a and c are aliases.
        let col_a = &Column::new("a", 0);
        let col_b = &Column::new("b", 1);
        let col_c = &Column::new("c", 2);
        let col_d = &Column::new("d", 3);
        let col_e = &Column::new("e", 4);
        let col_f = &Column::new("f", 5);
        let col_g = &Column::new("g", 6);
        let option1 = SortOptions {
            descending: false,
            nulls_first: false,
        };
        let option2 = SortOptions {
            descending: true,
            nulls_first: true,
        };
        let test_schema = create_test_schema()?;
        let col_a_expr = Arc::new(col_a.clone()) as _;
        let col_c_expr = Arc::new(col_c.clone()) as _;
        let mut schema_properties = SchemaProperties::new(test_schema.clone());
        schema_properties.add_equal_conditions((&col_a_expr, &col_c_expr));
        schema_properties.add_new_orderings(&[
            vec![PhysicalSortExpr {
                expr: Arc::new(col_a.clone()),
                options: option1,
            }],
            vec![
                PhysicalSortExpr {
                    expr: Arc::new(col_d.clone()),
                    options: option1,
                },
                PhysicalSortExpr {
                    expr: Arc::new(col_b.clone()),
                    options: option1,
                },
            ],
        ]);
        schema_properties.add_new_orderings(&[
            vec![PhysicalSortExpr {
                expr: Arc::new(col_a.clone()),
                options: option1,
            }],
            vec![
                PhysicalSortExpr {
                    expr: Arc::new(col_e.clone()),
                    options: option2,
                },
                PhysicalSortExpr {
                    expr: Arc::new(col_f.clone()),
                    options: option1,
                },
                PhysicalSortExpr {
                    expr: Arc::new(col_g.clone()),
                    options: option1,
                },
            ],
        ]);
        Ok((test_schema, schema_properties))
    }

    #[test]
    fn test_build_dag() -> Result<()> {
        let schema = Schema::new(vec![
            Field::new("0", DataType::Int32, true),
            Field::new("1", DataType::Int32, true),
            Field::new("2", DataType::Int32, true),
        ]);
        let expr = binary(
            cast(
                binary(
                    col("0", &schema)?,
                    Operator::Plus,
                    col("1", &schema)?,
                    &schema,
                )?,
                &schema,
                DataType::Int64,
            )?,
            Operator::Gt,
            binary(
                cast(col("2", &schema)?, &schema, DataType::Int64)?,
                Operator::Plus,
                lit(ScalarValue::Int64(Some(10))),
                &schema,
            )?,
            &schema,
        )?;
        let mut vector_dummy_props = vec![];
        let (root, graph) = build_dag(expr, &make_dummy_node)?;
        let mut bfs = Bfs::new(&graph, root);
        while let Some(node_index) = bfs.next(&graph) {
            let node = &graph[node_index];
            vector_dummy_props.push(node.property.clone());
        }

        assert_eq!(
            vector_dummy_props
                .iter()
                .filter(|property| property.expr_type == "Binary")
                .count(),
            3
        );
        assert_eq!(
            vector_dummy_props
                .iter()
                .filter(|property| property.expr_type == "Column")
                .count(),
            3
        );
        assert_eq!(
            vector_dummy_props
                .iter()
                .filter(|property| property.expr_type == "Literal")
                .count(),
            1
        );
        assert_eq!(
            vector_dummy_props
                .iter()
                .filter(|property| property.expr_type == "Other")
                .count(),
            2
        );
        Ok(())
    }

    #[test]
    fn test_convert_to_expr() -> Result<()> {
        let schema = Schema::new(vec![Field::new("a", DataType::UInt64, false)]);
        let sort_expr = vec![PhysicalSortExpr {
            expr: col("a", &schema)?,
            options: Default::default(),
        }];
        assert!(convert_to_expr(&sort_expr)[0].eq(&sort_expr[0].expr));
        Ok(())
    }

    #[test]
    fn test_get_indices_of_exprs_strict() {
        let list1: Vec<Arc<dyn PhysicalExpr>> = vec![
            Arc::new(Column::new("a", 0)),
            Arc::new(Column::new("b", 1)),
            Arc::new(Column::new("c", 2)),
            Arc::new(Column::new("d", 3)),
        ];
        let list2: Vec<Arc<dyn PhysicalExpr>> = vec![
            Arc::new(Column::new("b", 1)),
            Arc::new(Column::new("c", 2)),
            Arc::new(Column::new("a", 0)),
        ];
        assert_eq!(get_indices_of_exprs_strict(&list1, &list2), vec![2, 0, 1]);
        assert_eq!(get_indices_of_exprs_strict(&list2, &list1), vec![1, 2, 0]);
    }

    #[test]
    fn expr_list_eq_test() -> Result<()> {
        let list1: Vec<Arc<dyn PhysicalExpr>> = vec![
            Arc::new(Column::new("a", 0)),
            Arc::new(Column::new("a", 0)),
            Arc::new(Column::new("b", 1)),
        ];
        let list2: Vec<Arc<dyn PhysicalExpr>> = vec![
            Arc::new(Column::new("b", 1)),
            Arc::new(Column::new("b", 1)),
            Arc::new(Column::new("a", 0)),
        ];
        assert!(!expr_list_eq_any_order(list1.as_slice(), list2.as_slice()));
        assert!(!expr_list_eq_any_order(list2.as_slice(), list1.as_slice()));

        assert!(!expr_list_eq_strict_order(
            list1.as_slice(),
            list2.as_slice()
        ));
        assert!(!expr_list_eq_strict_order(
            list2.as_slice(),
            list1.as_slice()
        ));

        let list3: Vec<Arc<dyn PhysicalExpr>> = vec![
            Arc::new(Column::new("a", 0)),
            Arc::new(Column::new("b", 1)),
            Arc::new(Column::new("c", 2)),
            Arc::new(Column::new("a", 0)),
            Arc::new(Column::new("b", 1)),
        ];
        let list4: Vec<Arc<dyn PhysicalExpr>> = vec![
            Arc::new(Column::new("b", 1)),
            Arc::new(Column::new("b", 1)),
            Arc::new(Column::new("a", 0)),
            Arc::new(Column::new("c", 2)),
            Arc::new(Column::new("a", 0)),
        ];
        assert!(expr_list_eq_any_order(list3.as_slice(), list4.as_slice()));
        assert!(expr_list_eq_any_order(list4.as_slice(), list3.as_slice()));
        assert!(expr_list_eq_any_order(list3.as_slice(), list3.as_slice()));
        assert!(expr_list_eq_any_order(list4.as_slice(), list4.as_slice()));

        assert!(!expr_list_eq_strict_order(
            list3.as_slice(),
            list4.as_slice()
        ));
        assert!(!expr_list_eq_strict_order(
            list4.as_slice(),
            list3.as_slice()
        ));
        assert!(expr_list_eq_any_order(list3.as_slice(), list3.as_slice()));
        assert!(expr_list_eq_any_order(list4.as_slice(), list4.as_slice()));

        Ok(())
    }

    fn convert_to_requirement(
        in_data: &[(&Column, Option<SortOptions>)],
    ) -> Vec<PhysicalSortRequirement> {
        in_data
            .iter()
            .map(|(col, options)| {
                PhysicalSortRequirement::new(Arc::new((*col).clone()) as _, *options)
            })
            .collect::<Vec<_>>()
    }

    #[test]
    fn test_normalize_sort_reqs() -> Result<()> {
        let col_a = &Column::new("a", 0);
        let col_b = &Column::new("b", 1);
        let col_c = &Column::new("c", 2);
        let col_d = &Column::new("d", 3);
        let col_e = &Column::new("e", 4);
        let col_f = &Column::new("f", 5);
        let option1 = SortOptions {
            descending: false,
            nulls_first: false,
        };
        let option2 = SortOptions {
            descending: true,
            nulls_first: true,
        };
        // First element in the tuple stores vector of requirement, second element is the expected return value for ordering_satisfy function
        let requirements = vec![
            (vec![(col_a, Some(option1))], vec![(col_a, Some(option1))]),
            (vec![(col_a, Some(option2))], vec![(col_a, Some(option2))]),
            (vec![(col_a, None)], vec![(col_a, Some(option1))]),
            // Test whether equivalence works as expected
            (vec![(col_c, Some(option1))], vec![(col_a, Some(option1))]),
            (vec![(col_c, None)], vec![(col_a, Some(option1))]),
            // Test whether ordering equivalence works as expected
            (
                vec![(col_d, Some(option1)), (col_b, Some(option1))],
                vec![(col_a, Some(option1))],
            ),
            (
                vec![(col_d, None), (col_b, None)],
                vec![(col_a, Some(option1))],
            ),
            (
                vec![(col_e, Some(option2)), (col_f, Some(option1))],
                vec![(col_a, Some(option1))],
            ),
            // We should be able to normalize in compatible requirements also (not exactly equal)
            (
                vec![(col_e, Some(option2)), (col_f, None)],
                vec![(col_a, Some(option1))],
            ),
            (
                vec![(col_e, None), (col_f, None)],
                vec![(col_a, Some(option1))],
            ),
        ];

        let (_test_schema, schema_properties) = create_test_params()?;
        for (reqs, expected_normalized) in requirements.into_iter() {
            let req = convert_to_requirement(&reqs);
            let expected_normalized = convert_to_requirement(&expected_normalized);

            assert_eq!(
                schema_properties.normalize_sort_requirements(&req),
                expected_normalized
            );
        }
        Ok(())
    }

    #[test]
    fn test_reassign_predicate_columns_in_list() {
        let int_field = Field::new("should_not_matter", DataType::Int64, true);
        let dict_field = Field::new(
            "id",
            DataType::Dictionary(Box::new(DataType::Int32), Box::new(DataType::Utf8)),
            true,
        );
        let schema_small = Arc::new(Schema::new(vec![dict_field.clone()]));
        let schema_big = Arc::new(Schema::new(vec![int_field, dict_field]));
        let pred = in_list(
            Arc::new(Column::new_with_schema("id", &schema_big).unwrap()),
            vec![lit(ScalarValue::Dictionary(
                Box::new(DataType::Int32),
                Box::new(ScalarValue::from("2")),
            ))],
            &false,
            &schema_big,
        )
        .unwrap();

        let actual = reassign_predicate_columns(pred, &schema_small, false).unwrap();

        let expected = in_list(
            Arc::new(Column::new_with_schema("id", &schema_small).unwrap()),
            vec![lit(ScalarValue::Dictionary(
                Box::new(DataType::Int32),
                Box::new(ScalarValue::from("2")),
            ))],
            &false,
            &schema_small,
        )
        .unwrap();

        assert_eq!(actual.as_ref(), expected.as_any());
    }

    #[test]
    fn test_normalize_expr_with_equivalence() -> Result<()> {
        let col_a = &Column::new("a", 0);
        let col_b = &Column::new("b", 1);
        let col_c = &Column::new("c", 2);
        let _col_d = &Column::new("d", 3);
        let _col_e = &Column::new("e", 4);
        // Assume that column a and c are aliases.
        let (_test_schema, schema_properties) = create_test_params()?;

        let col_a_expr = Arc::new(col_a.clone()) as Arc<dyn PhysicalExpr>;
        let col_b_expr = Arc::new(col_b.clone()) as Arc<dyn PhysicalExpr>;
        let col_c_expr = Arc::new(col_c.clone()) as Arc<dyn PhysicalExpr>;
        // Test cases for equivalence normalization,
        // First entry in the tuple is argument, second entry is expected result after normalization.
        let expressions = vec![
            // Normalized version of the column a and c should go to a (since a is head)
            (&col_a_expr, &col_a_expr),
            (&col_c_expr, &col_a_expr),
            // Cannot normalize column b
            (&col_b_expr, &col_b_expr),
        ];
        let eq_groups = schema_properties.eq_groups();
        for (expr, expected_eq) in expressions {
            assert!(
                expected_eq.eq(&eq_groups.normalize_expr(expr.clone())),
                "error in test: expr: {expr:?}"
            );
        }

        Ok(())
    }

    #[test]
    fn test_normalize_sort_requirement_with_equivalence() -> Result<()> {
        let option1 = SortOptions {
            descending: false,
            nulls_first: false,
        };
        // Assume that column a and c are aliases.
        let (test_schema, schema_properties) = create_test_params()?;
        let col_a_expr = col("a", &test_schema)?;
        let _col_b_expr = col("b", &test_schema)?;
        let col_c_expr = col("c", &test_schema)?;
        let col_d_expr = col("d", &test_schema)?;
        let _col_e_expr = col("e", &test_schema)?;

        // Test cases for equivalence normalization
        // First entry in the tuple is PhysicalExpr, second entry is its ordering, third entry is result after normalization.
        let expressions = vec![
            (
                vec![PhysicalSortRequirement {
                    expr: col_a_expr.clone(),
                    options: Some(option1),
                }],
                vec![PhysicalSortRequirement {
                    expr: col_a_expr.clone(),
                    options: Some(option1),
                }],
            ),
            (
                vec![PhysicalSortRequirement {
                    expr: col_c_expr.clone(),
                    options: Some(option1),
                }],
                vec![PhysicalSortRequirement {
                    expr: col_a_expr.clone(),
                    options: Some(option1),
                }],
            ),
            (
                vec![PhysicalSortRequirement {
                    expr: col_c_expr.clone(),
                    options: None,
                }],
                vec![PhysicalSortRequirement {
                    expr: col_a_expr.clone(),
                    options: Some(option1),
                }],
            ),
            // d, b occurs in the ordering equivalence
            // requirement d is also satisfied with existing ordering
            // hence, normalized version should be a ASC
            (
                vec![PhysicalSortRequirement {
                    expr: col_d_expr.clone(),
                    options: Some(option1),
                }],
                vec![PhysicalSortRequirement {
                    expr: col_a_expr.clone(),
                    options: Some(option1),
                }],
            ),
        ];
        for (arg, expected) in expressions.into_iter() {
            let normalized = schema_properties.normalize_sort_requirements(&arg);
            assert!(
                expected.eq(&normalized),
                "error in test: arg: {arg:?}, expected: {expected:?}, normalized: {normalized:?}"
            );
        }

        Ok(())
    }

    #[test]
    fn test_collect_columns() -> Result<()> {
        let expr1 = Arc::new(Column::new("col1", 2)) as _;
        let mut expected = HashSet::new();
        expected.insert(Column::new("col1", 2));
        assert_eq!(collect_columns(&expr1), expected);

        let expr2 = Arc::new(Column::new("col2", 5)) as _;
        let mut expected = HashSet::new();
        expected.insert(Column::new("col2", 5));
        assert_eq!(collect_columns(&expr2), expected);

        let expr3 = Arc::new(BinaryExpr::new(expr1, Operator::Plus, expr2)) as _;
        let mut expected = HashSet::new();
        expected.insert(Column::new("col1", 2));
        expected.insert(Column::new("col2", 5));
        assert_eq!(collect_columns(&expr3), expected);
        Ok(())
    }

    #[test]
    fn scatter_int() -> Result<()> {
        let truthy = Arc::new(Int32Array::from(vec![1, 10, 11, 100]));
        let mask = BooleanArray::from(vec![true, true, false, false, true]);

        // the output array is expected to be the same length as the mask array
        let expected =
            Int32Array::from_iter(vec![Some(1), Some(10), None, None, Some(11)]);
        let result = scatter(&mask, truthy.as_ref())?;
        let result = as_int32_array(&result)?;

        assert_eq!(&expected, result);
        Ok(())
    }

    #[test]
    fn scatter_int_end_with_false() -> Result<()> {
        let truthy = Arc::new(Int32Array::from(vec![1, 10, 11, 100]));
        let mask = BooleanArray::from(vec![true, false, true, false, false, false]);

        // output should be same length as mask
        let expected =
            Int32Array::from_iter(vec![Some(1), None, Some(10), None, None, None]);
        let result = scatter(&mask, truthy.as_ref())?;
        let result = as_int32_array(&result)?;

        assert_eq!(&expected, result);
        Ok(())
    }

    #[test]
    fn scatter_with_null_mask() -> Result<()> {
        let truthy = Arc::new(Int32Array::from(vec![1, 10, 11]));
        let mask: BooleanArray = vec![Some(false), None, Some(true), Some(true), None]
            .into_iter()
            .collect();

        // output should treat nulls as though they are false
        let expected = Int32Array::from_iter(vec![None, None, Some(1), Some(10), None]);
        let result = scatter(&mask, truthy.as_ref())?;
        let result = as_int32_array(&result)?;

        assert_eq!(&expected, result);
        Ok(())
    }

    #[test]
    fn scatter_boolean() -> Result<()> {
        let truthy = Arc::new(BooleanArray::from(vec![false, false, false, true]));
        let mask = BooleanArray::from(vec![true, true, false, false, true]);

        // the output array is expected to be the same length as the mask array
        let expected = BooleanArray::from_iter(vec![
            Some(false),
            Some(false),
            None,
            None,
            Some(false),
        ]);
        let result = scatter(&mask, truthy.as_ref())?;
        let result = as_boolean_array(&result)?;

        assert_eq!(&expected, result);
        Ok(())
    }

    #[test]
    fn test_get_indices_of_matching_sort_exprs_with_order_eq() -> Result<()> {
        let sort_options = SortOptions::default();
        let sort_options_not = SortOptions::default().not();

        let required_columns = [
            Arc::new(Column::new("b", 1)) as _,
            Arc::new(Column::new("a", 0)) as _,
        ];
        let schema = Schema::new(vec![
            Field::new("a", DataType::Int32, true),
            Field::new("b", DataType::Int32, true),
        ]);
        let mut ordering_equal_properties = SchemaProperties::new(Arc::new(schema));
        ordering_equal_properties.add_new_orderings(&[vec![
            PhysicalSortExpr {
                expr: Arc::new(Column::new("b", 1)),
                options: sort_options_not,
            },
            PhysicalSortExpr {
                expr: Arc::new(Column::new("a", 0)),
                options: sort_options,
            },
        ]]);
        assert_eq!(
            ordering_equal_properties.set_exactly_satisfy(&required_columns),
            Some(vec![0, 1])
        );

        assert_eq!(
            ordering_equal_properties.get_lex_ordering(&required_columns),
            Some(vec![sort_options_not, sort_options])
        );

        let required_columns = [
            Arc::new(Column::new("b", 1)) as _,
            Arc::new(Column::new("a", 0)) as _,
        ];
        let schema = Schema::new(vec![
            Field::new("a", DataType::Int32, true),
            Field::new("b", DataType::Int32, true),
            Field::new("c", DataType::Int32, true),
        ]);
        let mut ordering_equal_properties = SchemaProperties::new(Arc::new(schema));
        ordering_equal_properties.add_new_orderings(&[
            vec![PhysicalSortExpr {
                expr: Arc::new(Column::new("c", 2)),
                options: sort_options,
            }],
            vec![
                PhysicalSortExpr {
                    expr: Arc::new(Column::new("b", 1)),
                    options: sort_options_not,
                },
                PhysicalSortExpr {
                    expr: Arc::new(Column::new("a", 0)),
                    options: sort_options,
                },
            ],
        ]);
        assert_eq!(
            ordering_equal_properties.set_exactly_satisfy(&required_columns),
            Some(vec![0, 1])
        );

        assert_eq!(
            ordering_equal_properties.get_lex_ordering(&required_columns),
            Some(vec![sort_options_not, sort_options])
        );

        let required_columns = [
            Arc::new(Column::new("b", 1)) as _,
            Arc::new(Column::new("a", 0)) as _,
        ];
        let schema = Schema::new(vec![
            Field::new("a", DataType::Int32, true),
            Field::new("b", DataType::Int32, true),
            Field::new("c", DataType::Int32, true),
        ]);
        let mut ordering_equal_properties = SchemaProperties::new(Arc::new(schema));

        // not satisfied orders
        ordering_equal_properties.add_new_orderings(&[vec![
            PhysicalSortExpr {
                expr: Arc::new(Column::new("b", 1)),
                options: sort_options_not,
            },
            PhysicalSortExpr {
                expr: Arc::new(Column::new("c", 2)),
                options: sort_options,
            },
            PhysicalSortExpr {
                expr: Arc::new(Column::new("a", 0)),
                options: sort_options,
            },
        ]]);
        assert_eq!(
            ordering_equal_properties.set_exactly_satisfy(&required_columns),
            None
        );

        Ok(())
    }

    #[test]
    fn test_normalize_ordering_equivalence_classes() -> Result<()> {
        let sort_options = SortOptions::default();

        let schema = Schema::new(vec![
            Field::new("a", DataType::Int32, true),
            Field::new("b", DataType::Int32, true),
            Field::new("c", DataType::Int32, true),
        ]);
        let col_a_expr = col("a", &schema)?;
        let col_b_expr = col("b", &schema)?;
        let col_c_expr = col("c", &schema)?;
        let mut schema_properties = SchemaProperties::new(Arc::new(schema.clone()));

        schema_properties.add_equal_conditions((&col_a_expr, &col_c_expr));
        let others = vec![
            vec![PhysicalSortExpr {
                expr: col_b_expr.clone(),
                options: sort_options,
            }],
            vec![PhysicalSortExpr {
                expr: col_c_expr.clone(),
                options: sort_options,
            }],
        ];
        schema_properties.add_new_orderings(&others);

        let mut expected_oeq = SchemaProperties::new(Arc::new(schema));
        expected_oeq.add_new_orderings(&[
            vec![PhysicalSortExpr {
                expr: col_b_expr.clone(),
                options: sort_options,
            }],
            vec![PhysicalSortExpr {
                expr: col_c_expr.clone(),
                options: sort_options,
            }],
        ]);

        let oeq_class = schema_properties.oeq_group().clone();
        let expected = expected_oeq.oeq_group();
        assert!(oeq_class.eq(expected));

        Ok(())
    }

    #[test]
    fn project_empty_output_ordering() -> Result<()> {
        let schema = Schema::new(vec![
            Field::new("a", DataType::Int32, true),
            Field::new("b", DataType::Int32, true),
            Field::new("c", DataType::Int32, true),
        ]);
        let mut schema_properties = SchemaProperties::new(Arc::new(schema.clone()));
        let ordering = vec![PhysicalSortExpr {
            expr: Arc::new(Column::new("b", 1)),
            options: SortOptions::default(),
        }];
        schema_properties.add_new_orderings(&[ordering]);
        let source_to_target_mapping = vec![
            (
                Arc::new(Column::new("b", 1)) as _,
                Arc::new(Column::new("b_new", 0)) as _,
            ),
            (
                Arc::new(Column::new("a", 0)) as _,
                Arc::new(Column::new("a_new", 1)) as _,
            ),
        ];
        let projection_schema = Arc::new(Schema::new(vec![
            Field::new("b_new", DataType::Int32, true),
            Field::new("a_new", DataType::Int32, true),
        ]));
        let projected_oeq =
            schema_properties.project(&source_to_target_mapping, projection_schema);
        let orderings = projected_oeq
            .oeq_group()
            .output_ordering()
            .unwrap_or_default();

        assert_eq!(
            vec![PhysicalSortExpr {
                expr: Arc::new(Column::new("b_new", 0)),
                options: SortOptions::default(),
            }],
            orderings
        );

        let schema = Schema::new(vec![
            Field::new("a", DataType::Int32, true),
            Field::new("b", DataType::Int32, true),
            Field::new("c", DataType::Int32, true),
        ]);
        let schema_properties = SchemaProperties::new(Arc::new(schema.clone()));
        let source_to_target_mapping = vec![
            (
                Arc::new(Column::new("c", 2)) as _,
                Arc::new(Column::new("c_new", 0)) as _,
            ),
            (
                Arc::new(Column::new("b", 1)) as _,
                Arc::new(Column::new("b_new", 1)) as _,
            ),
        ];
        let projection_schema = Arc::new(Schema::new(vec![
            Field::new("c_new", DataType::Int32, true),
            Field::new("b_new", DataType::Int32, true),
        ]));
        let projected_oeq =
            schema_properties.project(&source_to_target_mapping, projection_schema);
        // After projection there is no ordering.
        assert!(projected_oeq.oeq_group().output_ordering().is_none());

        Ok(())
    }
}
