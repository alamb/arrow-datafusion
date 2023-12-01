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

//! [`LiteralGuarantee`] to analyze predicates and determine if a column is a
//constant.

use crate::utils::split_disjunction;
use crate::{split_conjunction, PhysicalExpr};
use datafusion_common::{Column, ScalarValue};
use datafusion_expr::Operator;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// Represents a predicate where it is known that a column is either:
///
/// 1. One of particular set of values. For example, `(a = 1)`, `(a = 1 OR a =
/// 2) or `a IN (1, 2, 3)`
///
/// 2. Not one of a particular set of values. For example, `(a != 1)`, `(a != 1
/// AND a != 2)` or `a NOT IN (1, 2, 3)`
#[derive(Debug, Clone, PartialEq)]
pub struct LiteralGuarantee {
    pub column: Column,
    pub guarantee: Guarantee,
    pub literals: HashSet<ScalarValue>,
}

/// What can be guaranteed about the values?
#[derive(Debug, Clone, PartialEq)]
pub enum Guarantee {
    /// `column` is one of a set of constant values
    In,
    /// `column` is NOT one of a set of constant values
    NotIn,
}

impl LiteralGuarantee {
    /// Create a new instance of the guarantee if the provided operator is supported
    fn try_new<'a>(
        column_name: impl Into<String>,
        op: &Operator,
        literals: impl IntoIterator<Item = &'a ScalarValue>,
    ) -> Option<Self> {
        let guarantee = match op {
            Operator::Eq => Guarantee::In,
            Operator::NotEq => Guarantee::NotIn,
            _ => return None,
        };

        let literals: HashSet<_> = literals.into_iter().cloned().collect();

        Some(Self {
            column: Column::from_name(column_name),
            guarantee,
            literals,
        })
    }

    /// return a list of `LiteralGuarantees` that can be deduced for this
    /// expression.
    ///
    /// `expr` should be a boolean expression, for example a filter expression
    ///
    /// Notes: this API assumes the expression has already been simplified and
    /// returns duplicate guarantees for expressions like `a = 1 AND a = 1`.
    pub fn analyze(expr: &Arc<dyn PhysicalExpr>) -> Vec<LiteralGuarantee> {
        split_conjunction(expr)
            .into_iter()
            .fold(GuaranteeBuilder::new(), |builder, expr| {
                if let Some(cel) = ColOpLit::try_new(expr) {
                    return builder.aggregate_conjunct(cel);
                } else {
                    // look for pattern like
                    // (col <op> literal) OR (col <op> literal) ...
                    let disjunctions = split_disjunction(expr);

                    let terms = disjunctions
                        .iter()
                        .filter_map(|expr| ColOpLit::try_new(expr))
                        .collect::<Vec<_>>();

                    if terms.is_empty() {
                        return builder;
                    }

                    if terms.len() != disjunctions.len() {
                        // not all terms are of the form (col <op> literal)
                        return builder;
                    }

                    // if all terms are 'col <op> literal' then we can say something about the column
                    let first_term = &terms[0];
                    if terms.iter().all(|term| {
                        term.col.name() == first_term.col.name()
                            && term.op == first_term.op
                    }) {
                        builder.aggregate_multi_conjunct(
                            first_term.col,
                            first_term.op,
                            terms.iter().map(|term| term.lit.value()),
                        )
                    } else {
                        // ignore it
                        builder
                    }
                }
            })
            .build()
    }
}

/// Combines conjuncts together into guarantees, preserving insert order
struct GuaranteeBuilder<'a> {
    /// List of guarantees that have been created so far
    /// if we have determined a subsequent conjunct invalidates a guarantee
    /// e.g. `a = foo AND a = bar` then the relevant guarantee will be None
    guarantees: Vec<Option<LiteralGuarantee>>,

    // Key is the column name, type and value is the index into `guarantees`
    map: HashMap<(&'a crate::expressions::Column, &'a Operator), usize>,
}

impl<'a> GuaranteeBuilder<'a> {
    fn new() -> Self {
        Self {
            guarantees: vec![],
            map: HashMap::new(),
        }
    }

    /// Aggregate a new single guarantee to this builder  combining with existing guarantees
    /// if possible
    fn aggregate_conjunct(self, col_op_lit: ColOpLit<'a>) -> Self {
        self.aggregate_multi_conjunct(
            col_op_lit.col,
            col_op_lit.op,
            [col_op_lit.lit.value()],
        )
    }

    /// Aggreates a new single new guarantee with multiple literals `a IN (1,2,3)` or `a NOT IN (1,2,3)`. So the new values are combined with OR
    fn aggregate_multi_conjunct(
        mut self,
        col: &'a crate::expressions::Column,
        op: &'a Operator,
        new_values: impl IntoIterator<Item = &'a ScalarValue>,
    ) -> Self {
        let key = (col, op);
        if let Some(index) = self.map.get(&key) {
            // already have a guarantee for this column
            let entry = &mut self.guarantees[*index];

            let Some(existing) = entry else {
                // guarantee has been previously invalidated, nothing to do
                return self;
            };

            // can only combine conjuncts if we have `a != foo AND a != bar`.
            // `a = foo AND a = bar` is not correct. Also, can't extend with more than one value.
            match existing.guarantee {
                Guarantee::NotIn => {
                    // can extend if only single literal, otherwise invalidate
                    let new_values: HashSet<_> = new_values.into_iter().collect();
                    if new_values.len() == 1 {
                        existing.literals.extend(new_values.into_iter().cloned())
                    } else {
                        // this is like (a != foo AND (a != bar OR a != baz)).
                        // We can't combine the (a!=bar OR a!=baz) part, but it
                        // also doesn't invalidate a != foo guarantee.
                    }
                }
                Guarantee::In => {
                    // for an IN guarantee, it is ok if the value is the same
                    // e.g. `a = foo AND a = foo` but not if the value is different
                    // e.g. `a = foo AND a = bar`
                    if new_values
                        .into_iter()
                        .all(|new_value| existing.literals.contains(new_value))
                    {
                        // all values are already in the set
                    } else {
                        // at least one was not, so invalidate the guarantee
                        *entry = None;
                    }
                }
            }
        } else {
            // This is a new guarantee
            let new_values: HashSet<_> = new_values.into_iter().collect();

            // new_values are combined with OR, so we can only create a
            // multi-column guarantee for `=` (or a single value).
            // (e.g. ignore `a != foo OR a != bar`)
            if op == &Operator::Eq || new_values.len() == 1 {
                if let Some(guarantee) =
                    LiteralGuarantee::try_new(col.name(), op, new_values)
                {
                    // add it to the list of guarantees
                    self.guarantees.push(Some(guarantee));
                    self.map.insert(key, self.guarantees.len() - 1);
                }
            }
        }

        self
    }

    /// Return all guarantees that have been created so far
    fn build(self) -> Vec<LiteralGuarantee> {
        // filter out any guarantees that have been invalidated
        self.guarantees.into_iter().flatten().collect()
    }
}

/// Represents a single `col <op> literal` expression
struct ColOpLit<'a> {
    col: &'a crate::expressions::Column,
    op: &'a Operator,
    lit: &'a crate::expressions::Literal,
}

impl<'a> ColOpLit<'a> {
    /// Returns Some(ColEqLit) if the expression is either:
    /// 1. `col <op> literal`
    /// 2. `literal <op> col`
    ///
    /// Returns None otherwise
    fn try_new(expr: &'a Arc<dyn PhysicalExpr>) -> Option<Self> {
        let binary_expr = expr
            .as_any()
            .downcast_ref::<crate::expressions::BinaryExpr>()?;

        let (left, op, right) = (
            binary_expr.left().as_any(),
            binary_expr.op(),
            binary_expr.right().as_any(),
        );

        if let (Some(col), Some(lit)) = (
            left.downcast_ref::<crate::expressions::Column>(),
            right.downcast_ref::<crate::expressions::Literal>(),
        ) {
            Some(Self { col, op, lit })
        }
        // literal <op> col
        else if let (Some(lit), Some(col)) = (
            left.downcast_ref::<crate::expressions::Literal>(),
            right.downcast_ref::<crate::expressions::Column>(),
        ) {
            Some(Self { col, op, lit })
        } else {
            None
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::create_physical_expr;
    use crate::execution_props::ExecutionProps;
    use arrow_schema::{DataType, Field, Schema, SchemaRef};
    use datafusion_common::ToDFSchema;
    use datafusion_expr::expr_fn::*;
    use datafusion_expr::{lit, Expr};
    use std::sync::OnceLock;

    #[test]
    fn test_literal() {
        // a single literal offers no guarantee
        test_analyze(lit(true), vec![])
    }

    #[test]
    fn test_single() {
        // a = "foo"
        test_analyze(col("a").eq(lit("foo")), vec![in_guarantee("a", ["foo"])]);
        // "foo" = a
        test_analyze(lit("foo").eq(col("a")), vec![in_guarantee("a", ["foo"])]);
        // a != "foo"
        test_analyze(
            col("a").not_eq(lit("foo")),
            vec![not_in_guarantee("a", ["foo"])],
        );
        // a != "foo"
        test_analyze(
            lit("foo").not_eq(col("a")),
            vec![not_in_guarantee("a", ["foo"])],
        );
    }

    #[test]
    fn test_conjunction() {
        // a = "foo" AND b = 1
        test_analyze(
            col("a").eq(lit("foo")).and(col("b").eq(lit(1))),
            vec![
                // should find both column guarantees
                in_guarantee("a", ["foo"]),
                in_guarantee("b", [1]),
            ],
        );
        // a != "foo" AND b != 1
        test_analyze(
            col("a").not_eq(lit("foo")).and(col("b").not_eq(lit(1))),
            // should find both column guarantees
            vec![not_in_guarantee("a", ["foo"]), not_in_guarantee("b", [1])],
        );
        // a = "foo" AND a = "bar"
        test_analyze(
            col("a").eq(lit("foo")).and(col("a").eq(lit("bar"))),
            // this predicate is impossible ( can't be both foo and bar),
            vec![],
        );
        // a = "foo" AND b != "bar"
        test_analyze(
            col("a").eq(lit("foo")).and(col("a").not_eq(lit("bar"))),
            vec![in_guarantee("a", ["foo"]), not_in_guarantee("a", ["bar"])],
        );
        // a != "foo" AND a != "bar"
        test_analyze(
            col("a").not_eq(lit("foo")).and(col("a").not_eq(lit("bar"))),
            // know it isn't "foo" or "bar"
            vec![not_in_guarantee("a", ["foo", "bar"])],
        );
        // a != "foo" AND a != "bar" and a != "baz"
        test_analyze(
            col("a")
                .not_eq(lit("foo"))
                .and(col("a").not_eq(lit("bar")))
                .and(col("a").not_eq(lit("baz"))),
            // know it isn't "foo" or "bar" or "baz"
            vec![not_in_guarantee("a", ["foo", "bar", "baz"])],
        );
        // a = "foo" AND a = "foo"
        let expr = col("a").eq(lit("foo"));
        test_analyze(expr.clone().and(expr), vec![in_guarantee("a", ["foo"])]);
        // b > 5 AND b = 10 (should get an b = 10 guarantee)
        test_analyze(
            col("b").gt(lit(5)).and(col("b").eq(lit(10))),
            vec![in_guarantee("b", [10])],
        );

        // a != "foo" and (a != "bar" OR a != "baz")
        test_analyze(
            col("a")
                .not_eq(lit("foo"))
                .and(col("a").not_eq(lit("bar")).or(col("a").not_eq(lit("baz")))),
            // a is not foo (we can't represent other knowledge about a)
            vec![not_in_guarantee("a", ["foo"])],
        );
    }

    #[test]
    fn test_disjunction() {
        // a = "foo" OR b = 1
        test_analyze(
            col("a").eq(lit("foo")).or(col("b").eq(lit(1))),
            // no can't have a single column guarantee (if a = "foo" then b != 1) etc
            vec![],
        );
        // a != "foo" OR b != 1
        test_analyze(
            col("a").not_eq(lit("foo")).or(col("b").not_eq(lit(1))),
            // No single column guarantee
            vec![],
        );
        // a = "foo" OR a = "bar"
        test_analyze(
            col("a").eq(lit("foo")).or(col("a").eq(lit("bar"))),
            vec![in_guarantee("a", ["foo", "bar"])],
        );
        // a = "foo" OR a = "foo"
        test_analyze(
            col("a").eq(lit("foo")).or(col("a").eq(lit("foo"))),
            vec![in_guarantee("a", ["foo"])],
        );
        // a != "foo" OR a != "bar"
        test_analyze(
            col("a").not_eq(lit("foo")).or(col("a").not_eq(lit("bar"))),
            // can't represent knowledge about a in this case
            vec![],
        );
        // a = "foo" OR a = "bar" OR a = "baz"
        test_analyze(
            col("a")
                .eq(lit("foo"))
                .or(col("a").eq(lit("bar")))
                .or(col("a").eq(lit("baz"))),
            vec![in_guarantee("a", ["foo", "bar", "baz"])],
        );
        // (a = "foo" OR a = "bar") AND (a = "baz)"
        test_analyze(
            (col("a").eq(lit("foo")).or(col("a").eq(lit("bar"))))
                .and(col("a").eq(lit("baz"))),
            // this could potentially be represented as 2 constraints with a more
            // sophisticated analysis
            vec![],
        );
        // (a = "foo" OR a = "bar") AND (b = 1)
        test_analyze(
            (col("a").eq(lit("foo")).or(col("a").eq(lit("bar"))))
                .and(col("b").eq(lit(1))),
            vec![in_guarantee("a", ["foo", "bar"]), in_guarantee("b", [1])],
        );
        // (a = "foo" OR a = "bar") OR (b = 1)
        test_analyze(
            col("a")
                .eq(lit("foo"))
                .or(col("a").eq(lit("bar")))
                .or(col("b").eq(lit(1))),
            // can't represent knowledge about a or b in this case
            vec![],
        );
    }

    // TODO file ticket to add tests for :
    // a IN (...)
    // b NOT IN (...)

    /// Tests that analyzing expr results in the expected guarantees
    fn test_analyze(expr: Expr, expected: Vec<LiteralGuarantee>) {
        println!("Begin analyze of {expr}");
        let schema = schema();
        let physical_expr = logical2physical(&expr, &schema);

        let actual = LiteralGuarantee::analyze(&physical_expr);
        assert_eq!(
            expected, actual,
            "expr: {expr}\
               \n\nexpected: {expected:#?}\
               \n\nactual: {actual:#?}\
               \n\nexpr: {expr:#?}\
               \n\nphysical_expr: {physical_expr:#?}"
        );
    }

    /// Guarantee that column is a specified value
    fn in_guarantee<'a, I, S>(column: &str, literals: I) -> LiteralGuarantee
    where
        I: IntoIterator<Item = S>,
        S: Into<ScalarValue> + 'a,
    {
        let literals: Vec<_> = literals.into_iter().map(|s| s.into()).collect();
        LiteralGuarantee::try_new(column, &Operator::Eq, literals.iter()).unwrap()
    }

    /// Guarantee that column is NOT a specified value
    fn not_in_guarantee<'a, I, S>(column: &str, literals: I) -> LiteralGuarantee
    where
        I: IntoIterator<Item = S>,
        S: Into<ScalarValue> + 'a,
    {
        let literals: Vec<_> = literals.into_iter().map(|s| s.into()).collect();
        LiteralGuarantee::try_new(column, &Operator::NotEq, literals.iter()).unwrap()
    }

    /// Convert a logical expression to a physical expression (without any simplification, etc)
    fn logical2physical(expr: &Expr, schema: &Schema) -> Arc<dyn PhysicalExpr> {
        let df_schema = schema.clone().to_dfschema().unwrap();
        let execution_props = ExecutionProps::new();
        create_physical_expr(expr, &df_schema, schema, &execution_props).unwrap()
    }

    // Schema for testing
    fn schema() -> SchemaRef {
        SCHEMA
            .get_or_init(|| {
                Arc::new(Schema::new(vec![
                    Field::new("a", DataType::Utf8, false),
                    Field::new("b", DataType::Int32, false),
                ]))
            })
            .clone()
    }

    static SCHEMA: OnceLock<SchemaRef> = OnceLock::new();
}
