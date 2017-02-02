use nom_sql::{Column, ConditionBase, ConditionExpression, ConditionTree, FieldExpression, SqlQuery};

use std::collections::HashMap;

pub trait ImpliedTableExpansion {
    fn expand_implied_tables(self, write_schemas: &HashMap<String, Vec<String>>) -> SqlQuery;
}

fn rewrite_conditional<F>(translate_column: &F, ce: ConditionExpression) -> ConditionExpression
    where F: Fn(Column) -> Column
{
    let translate_ct_arm =
        |i: Option<Box<ConditionExpression>>| -> Option<Box<ConditionExpression>> {
            match i {
                Some(bce) => {
                    let new_ce = match *bce {
                        ConditionExpression::Base(ConditionBase::Field(f)) => {
                            ConditionExpression::Base(ConditionBase::Field(translate_column(f)))
                        }
                        ConditionExpression::Base(b) => ConditionExpression::Base(b),
                        x => rewrite_conditional(translate_column, x),
                    };
                    Some(Box::new(new_ce))
                }
                x => x,
            }
        };

    match ce {
        ConditionExpression::ComparisonOp(ct) => {
            let l = translate_ct_arm(ct.left);
            let r = translate_ct_arm(ct.right);
            let rewritten_ct = ConditionTree {
                operator: ct.operator,
                left: l,
                right: r,
            };
            ConditionExpression::ComparisonOp(rewritten_ct)
        }
        ConditionExpression::LogicalOp(ct) => {
            let rewritten_ct = ConditionTree {
                operator: ct.operator,
                left: match ct.left {
                    Some(lct) => Some(Box::new(rewrite_conditional(translate_column, *lct))),
                    x => x,
                },
                right: match ct.right {
                    Some(rct) => Some(Box::new(rewrite_conditional(translate_column, *rct))),
                    x => x,
                },
            };
            ConditionExpression::LogicalOp(rewritten_ct)
        }
        x => x,
    }
}

impl ImpliedTableExpansion for SqlQuery {
    fn expand_implied_tables(self, write_schemas: &HashMap<String, Vec<String>>) -> SqlQuery {
        let translate_column = |mut f: Column| -> Column {
            f.table = match f.table {
                None => {
                    if let Some(_) = f.function {
                        // There is no implied table (other than "self") for anonymous function columns
                        None
                    } else {
                        let mut matches = write_schemas.iter()
                            .filter_map(|(ref t, ref ws)| {
                                let num_matching = ws.iter()
                                    .filter(|c| **c == f.name)
                                    .count();
                                assert!(num_matching <= 1);
                                if num_matching == 1 {
                                    Some((*t).clone())
                                } else {
                                    None
                                }
                            })
                            .collect::<Vec<String>>();
                        if matches.len() > 1 {
                            panic!("Ambiguous column {} specified. Matching tables: {:?}",
                                   f.name,
                                   matches);
                        } else if matches.is_empty() {
                            panic!("Failed to resolve table for column named {}", f.name);
                        } else {
                            // exactly one match
                            Some(matches.pop().unwrap())
                        }
                    }
                }
                Some(x) => Some(x),
            };
            f
        };

        match self {
            // nothing to do for INSERTs, as they cannot have implied tables
            SqlQuery::Insert(i) => SqlQuery::Insert(i),
            SqlQuery::Select(mut sq) => {
                // Expand within field list
                sq.fields = match sq.fields {
                    FieldExpression::All => {
                        panic!("Must apply StarExpansion pass before ImpliedTableExpansion")
                    }
                    FieldExpression::Seq(fs) => {
                        FieldExpression::Seq(fs.into_iter()
                            .map(&translate_column)
                            .collect())
                    }
                };
                // Expand within WHERE clause
                sq.where_clause = match sq.where_clause {
                    None => None,
                    Some(wc) => Some(rewrite_conditional(&translate_column, wc)),
                };

                SqlQuery::Select(sq)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use nom_sql::SelectStatement;
    use nom_sql::{Column, FieldExpression, SqlQuery, Table};
    use std::collections::HashMap;
    use super::ImpliedTableExpansion;

    #[test]
    fn it_expands_implied_tables() {
        use nom_sql::{ConditionBase, ConditionExpression, ConditionTree, Operator};

        // SELECT name, title FROM users, articles WHERE users.id = author;
        // -->
        // SELECT users.name, articles.title FROM users, articles WHERE users.id = articles.author;
        let q = SelectStatement {
            tables: vec![Table::from("users"),
                         Table::from("articles")],
            fields: FieldExpression::Seq(vec![Column::from("name"), Column::from("title")]),
            where_clause: Some(ConditionExpression::ComparisonOp(ConditionTree {
                operator: Operator::Equal,
                left: Some(Box::new(ConditionExpression::Base(
                            ConditionBase::Field(
                                Column::from("users.id"))
                            ))),
                right: Some(Box::new(ConditionExpression::Base(
                            ConditionBase::Field(
                                Column::from("author"))
                            ))),
            })),
            ..Default::default()
        };
        let mut schema = HashMap::new();
        schema.insert("users".into(),
                      vec!["id".into(), "name".into(), "age".into()]);
        schema.insert("articles".into(),
                      vec!["id".into(), "title".into(), "text".into(), "author".into()]);

        let res = SqlQuery::Select(q).expand_implied_tables(&schema);
        match res {
            SqlQuery::Select(tq) => {
                assert_eq!(tq.fields,
                           FieldExpression::Seq(vec![Column::from("users.name"),
                                                     Column::from("articles.title")]));
                assert_eq!(tq.where_clause,
                           Some(ConditionExpression::ComparisonOp(ConditionTree {
                               operator: Operator::Equal,
                               left: Some(Box::new(ConditionExpression::Base(
                                       ConditionBase::Field(
                                           Column::from("users.id"))
                                       ))),
                               right: Some(Box::new(ConditionExpression::Base(
                                        ConditionBase::Field(
                                            Column::from("articles.author"))
                                        ))),
                           })));
            }
            // if we get anything other than a selection query back, something really weird is up
            _ => panic!(),
        }
    }
}
