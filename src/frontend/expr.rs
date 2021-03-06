// This module contains our definition of expressions, including code to convert parse streams
// to expressions.

use crate::backend::{Token, Tokens};
use crate::frontend::{PlanningContext, Result, Rule};
use crate::Slot;
use pest::iterators::Pair;
use std::collections::HashSet;
use std::str::FromStr;

#[derive(Debug, PartialEq, Clone)]
pub enum Op {
    Eq,
    NotEq,
    Gt,
    Div,
    Mul,
    Add,
    Sub,
}

impl FromStr for Op {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "=" => Ok(Op::Eq),
            "<>" => Ok(Op::NotEq),
            ">" => Ok(Op::Gt),
            "/" => Ok(Op::Div),
            "*" => Ok(Op::Mul),
            "+" => Ok(Op::Add),
            "-" => Ok(Op::Sub),
            _ => bail!("Unknown operator: {}", s),
        }
    }
}

#[derive(Debug, PartialEq, Clone)]
pub enum Expr {
    And(Vec<Self>),
    Or(Vec<Self>),

    // An operator that takes two expressions as its arguments, for instance
    // "a = b" or "a > b".
    BinaryOp {
        left: Box<Self>,
        right: Box<Self>,
        op: Op,
    },

    // Literals
    Bool(bool),
    Int(i64),
    Float(f64),
    // TODO note that this could be like a gigabyte of user data, it would be better if we had
    //      a way to plumb that directly into the backend, rather than malloc it onto the heap
    String(String),
    Map(Vec<MapEntryExpr>),
    List(Vec<Expr>),

    // Lookup a property by id
    Prop(Box<Self>, Vec<Token>),
    Slot(Slot),
    FuncCall {
        name: Token,
        args: Vec<Expr>,
    },

    // True if the Node in the specified Slot has the specified Label
    HasLabel(Slot, Token),
}

impl Expr {
    // Does this expression - when considered recursively - aggregate rows?
    pub fn is_aggregating(&self, aggregating_funcs: &HashSet<Token>) -> bool {
        match self {
            Expr::Prop(c, _) => c.is_aggregating(aggregating_funcs),
            Expr::Slot(_) => false,
            Expr::Float(_) => false,
            Expr::Int(_) => false,
            Expr::String(_) => false,
            Expr::Map(children) => children
                .iter()
                .any(|c| c.val.is_aggregating(aggregating_funcs)),
            Expr::List(children) => children.iter().any(|v| v.is_aggregating(aggregating_funcs)),
            Expr::FuncCall { name, args } => {
                aggregating_funcs.contains(name)
                    || args.iter().any(|c| c.is_aggregating(aggregating_funcs))
            }
            Expr::And(terms) => terms.iter().any(|c| c.is_aggregating(aggregating_funcs)),
            Expr::Or(terms) => terms.iter().any(|c| c.is_aggregating(aggregating_funcs)),
            Expr::Bool(_) => false,
            Expr::BinaryOp { left, right, op: _ } => {
                left.is_aggregating(aggregating_funcs) | right.is_aggregating(aggregating_funcs)
            }
            Expr::HasLabel(_, _) => false,
        }
    }

    pub fn fmt_pretty(&self, _indent: &str, _t: &Tokens) -> String {
        match self {
            Expr::Slot(s) => format!("Slot({})", s),
            _ => format!("Expr_NoPretty({:?})", self),
        }
    }
}

#[derive(Debug, PartialEq, Clone)]
pub struct MapEntryExpr {
    pub key: Token,
    pub val: Expr,
}

pub(super) fn plan_expr(pc: &mut PlanningContext, expression: Pair<Rule>) -> Result<Expr> {
    let mut or_expressions = Vec::new();
    for inner in expression.into_inner() {
        match inner.as_rule() {
            Rule::and_expr => {
                let mut and_expressions: Vec<Expr> = Vec::new();
                for term in inner.into_inner() {
                    and_expressions.push(plan_add_sub(pc, term)?)
                }
                let and_expr = if and_expressions.len() == 1 {
                    and_expressions.remove(0)
                } else {
                    Expr::And(and_expressions)
                };
                or_expressions.push(and_expr);
            }
            _ => bail!("({:?}): {}", inner.as_rule(), inner.as_str()),
        }
    }
    if or_expressions.len() == 1 {
        Ok(or_expressions.remove(0))
    } else {
        Ok(Expr::Or(or_expressions))
    }
}

fn plan_add_sub(pc: &mut PlanningContext, item: Pair<Rule>) -> Result<Expr> {
    match item.as_rule() {
        Rule::add_sub_expr => {
            // let mut out = None;
            let mut inners = item.into_inner();
            let mut out = plan_mul_div(pc, inners.next().unwrap())?;

            // The parser guarantees there is at least one term (eg. *every* expression
            // is wrapped in mult_div for precedence reasons), but there may not actually
            // be any multiplication or division. If so we'll just return the out expression
            // above on its own. If there are multiplication / division parts, we'll get them
            // as (op, expr) groups here in this loop, so we use that to build up the
            // multiplication expressions we want..
            while let Some(op) = inners.next() {
                // Ok so this expression actually does involve some multiplication or division,
                // then the current "out" expression is the left term, we've got the op, and the
                // parser should guarantee us another expression to go on the right:
                let right = plan_mul_div(
                    pc,
                    inners
                        .next()
                        .ok_or(anyhow!("parser error: add / sub without right term?"))?,
                )?;
                out = Expr::BinaryOp {
                    left: Box::new(out),
                    right: Box::new(right),
                    op: Op::from_str(op.as_str())?,
                }
            }
            Ok(out)
        }
        _ => panic!("({:?}): {}", item.as_rule(), item.as_str()),
    }
}

// See plan_add_sub
fn plan_mul_div(pc: &mut PlanningContext, item: Pair<Rule>) -> Result<Expr> {
    match item.as_rule() {
        Rule::mult_div_expr => {
            let mut inners = item.into_inner();
            let mut out = plan_term(pc, inners.next().unwrap())?;

            while let Some(op) = inners.next() {
                let right = plan_term(
                    pc,
                    inners.next().ok_or(anyhow!(
                        "parser error: multiplication / division without right term?"
                    ))?,
                )?;
                out = Expr::BinaryOp {
                    left: Box::new(out),
                    right: Box::new(right),
                    op: Op::from_str(op.as_str())?,
                }
            }
            Ok(out)
        }
        _ => panic!("({:?}): {}", item.as_rule(), item.as_str()),
    }
}

fn plan_term(pc: &mut PlanningContext, term: Pair<Rule>) -> Result<Expr> {
    match term.as_rule() {
        Rule::string => {
            let content = term
                .into_inner()
                .next()
                .expect("Strings should always have an inner value")
                .as_str();
            return Ok(Expr::String(String::from(content)));
        }
        Rule::id => {
            let tok = pc.tokenize(term.as_str());
            return Ok(Expr::Slot(pc.get_or_alloc_slot(tok)));
        }
        Rule::prop_lookup => {
            let mut prop_lookup = term.into_inner();
            let prop_lookup_expr = prop_lookup.next().unwrap();
            let base = match prop_lookup_expr.as_rule() {
                Rule::id => {
                    let tok = pc.tokenize(prop_lookup_expr.as_str());
                    Expr::Slot(pc.get_or_alloc_slot(tok))
                }
                _ => unreachable!(),
            };
            let mut props = Vec::new();
            for p_inner in prop_lookup {
                if let Rule::id = p_inner.as_rule() {
                    props.push(pc.tokenize(p_inner.as_str()));
                }
            }
            return Ok(Expr::Prop(Box::new(base), props));
        }
        Rule::func_call => {
            let mut func_call = term.into_inner();
            let func_name_item = func_call
                .next()
                .expect("All func_calls must start with an identifier");
            let name = pc.tokenize(&func_name_item.as_str().to_lowercase());
            // Parse args
            let mut args = Vec::new();
            for arg in func_call {
                args.push(plan_expr(pc, arg)?);
            }
            return Ok(Expr::FuncCall { name, args });
        }
        Rule::count_call => {
            let name = pc.tokenize("count");
            return Ok(Expr::FuncCall {
                name,
                args: Vec::new(),
            });
        }
        Rule::list => {
            let mut items = Vec::new();
            let exprs = term.into_inner();
            for exp in exprs {
                items.push(plan_expr(pc, exp)?);
            }
            return Ok(Expr::List(items));
        }
        Rule::map => return Ok(Expr::Map(parse_map_expression(pc, term)?)),
        Rule::int => {
            let v = term.as_str().parse::<i64>()?;
            return Ok(Expr::Int(v));
        }
        Rule::float => {
            let v = term.as_str().parse::<f64>()?;
            return Ok(Expr::Float(v));
        }
        Rule::science => {
            let v = term.as_str().parse::<f64>()?;
            return Ok(Expr::Float(v));
        }
        Rule::lit_true => return Ok(Expr::Bool(true)),
        Rule::lit_false => return Ok(Expr::Bool(false)),
        Rule::binary_op => {
            let mut parts = term.into_inner();
            let left = parts.next().expect("binary operators must have a left arg");
            let op = parts
                .next()
                .expect("binary operators must have an operator");
            let right = parts
                .next()
                .expect("binary operators must have a right arg");

            let left_expr = plan_term(pc, left)?;
            let right_expr = plan_term(pc, right)?;
            return Ok(Expr::BinaryOp {
                left: Box::new(left_expr),
                right: Box::new(right_expr),
                op: Op::from_str(op.as_str())?,
            });
        }
        Rule::expr => {
            // this happens when there are parenthetises forcing "full" expressions down here
            return plan_expr(pc, term);
        }
        _ => panic!("({:?}): {}", term.as_rule(), term.as_str()),
    }
}

pub fn parse_map_expression(
    pc: &mut PlanningContext,
    map_expr: Pair<Rule>,
) -> Result<Vec<MapEntryExpr>> {
    let mut out = Vec::new();
    for pair in map_expr.into_inner() {
        match pair.as_rule() {
            Rule::map_pair => {
                let mut pair_iter = pair.into_inner();
                let id_token = pair_iter
                    .next()
                    .expect("Map pair must contain an identifier");
                let identifier = pc.tokenize(id_token.as_str());

                let expr_token = pair_iter
                    .next()
                    .expect("Map pair must contain an expression");
                let expr = plan_expr(pc, expr_token)?;
                out.push(MapEntryExpr {
                    key: identifier,
                    val: expr,
                })
            }
            _ => unreachable!(),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{BackendDesc, FuncSignature, FuncType, Token, Tokens};
    use crate::frontend::{Frontend, LogicalPlan};
    use crate::Type;
    use anyhow::Result;
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::rc::Rc;

    // Outcome of testing planning; the plan plus other related items to do checks on
    #[derive(Debug)]
    struct PlanArtifacts {
        expr: Expr,
        slots: HashMap<Token, usize>,
        tokens: Rc<RefCell<Tokens>>,
    }

    fn plan(q: &str) -> Result<PlanArtifacts> {
        let tokens = Rc::new(RefCell::new(Tokens::new()));
        let tok_expr = tokens.borrow_mut().tokenize("expr");
        let fn_count = tokens.borrow_mut().tokenize("count");
        let backend_desc = BackendDesc::new(vec![FuncSignature {
            func_type: FuncType::Aggregating,
            name: fn_count,
            returns: Type::Integer,
            args: vec![(tok_expr, Type::Any)],
        }]);

        let frontend = Frontend {
            tokens: Rc::clone(&tokens),
            backend_desc: BackendDesc::new(vec![]),
        };
        let mut pc = PlanningContext::new(Rc::clone(&tokens), &backend_desc);
        let plan = frontend.plan_in_context(&format!("WITH {}", q), &mut pc);

        if let Ok(LogicalPlan::Project {
            src: _,
            projections,
        }) = plan
        {
            return Ok(PlanArtifacts {
                expr: projections[0].expr.clone(),
                slots: pc.slots,
                tokens: Rc::clone(&tokens),
            });
        } else {
            return Err(anyhow!("Expected RETURN plan, got: {:?}", plan?));
        }
    }

    #[test]
    fn plan_some_numbers() -> Result<()> {
        assert_eq!(plan("-1e-9")?.expr, Expr::Float(-1e-9));
        Ok(())
    }

    #[test]
    fn plan_arithmetic() -> Result<()> {
        assert_eq!(
            plan("12 / 4 * (3 - 2 * 4)")?.expr,
            Expr::BinaryOp {
                left: Box::new(Expr::BinaryOp {
                    left: Box::new(Expr::Int(12)),
                    right: Box::new(Expr::Int(4)),
                    op: Op::Div
                }),
                right: Box::new(Expr::BinaryOp {
                    left: Box::new(Expr::Int(3)),
                    right: Box::new(Expr::BinaryOp {
                        left: Box::new(Expr::Int(2)),
                        right: Box::new(Expr::Int(4)),
                        op: Op::Mul
                    }),
                    op: Op::Sub
                }),
                op: Op::Mul
            }
        );
        Ok(())
    }

    #[test]
    fn plan_boolean_logic() -> Result<()> {
        assert_eq!(plan("true")?.expr, Expr::Bool(true));
        assert_eq!(plan("false")?.expr, Expr::Bool(false));
        assert_eq!(
            plan("true and false")?.expr,
            Expr::And(vec![Expr::Bool(true), Expr::Bool(false)])
        );
        assert_eq!(
            plan("true and false and true")?.expr,
            Expr::And(vec![Expr::Bool(true), Expr::Bool(false), Expr::Bool(true)])
        );
        assert_eq!(
            plan("true or false")?.expr,
            Expr::Or(vec![Expr::Bool(true), Expr::Bool(false)])
        );
        assert_eq!(
            plan("true or false or true")?.expr,
            Expr::Or(vec![Expr::Bool(true), Expr::Bool(false), Expr::Bool(true)])
        );
        assert_eq!(
            plan("true and false or true")?.expr,
            Expr::Or(vec![
                Expr::And(vec![Expr::Bool(true), Expr::Bool(false)]),
                Expr::Bool(true)
            ])
        );
        assert_eq!(
            plan("true or false and true")?.expr,
            Expr::Or(vec![
                Expr::Bool(true),
                Expr::And(vec![Expr::Bool(false), Expr::Bool(true)])
            ])
        );
        Ok(())
    }

    #[test]
    fn plan_binary_operators() -> Result<()> {
        assert_eq!(
            plan("true = false")?.expr,
            Expr::BinaryOp {
                left: Box::new(Expr::Bool(true)),
                right: Box::new(Expr::Bool(false)),
                op: Op::Eq
            },
        );
        assert_eq!(
            plan("false = ( true = true )")?.expr,
            Expr::BinaryOp {
                left: Box::new(Expr::Bool(false)),
                right: Box::new(Expr::BinaryOp {
                    left: Box::new(Expr::Bool(true)),
                    right: Box::new(Expr::Bool(true)),
                    op: Op::Eq
                }),
                op: Op::Eq
            },
        );
        assert_eq!(
            plan("1 > 2")?.expr,
            Expr::BinaryOp {
                left: Box::new(Expr::Int(1)),
                right: Box::new(Expr::Int(2)),
                op: Op::Gt
            },
        );
        assert_eq!(
            plan("1 <> 2")?.expr,
            Expr::BinaryOp {
                left: Box::new(Expr::Int(1)),
                right: Box::new(Expr::Int(2)),
                op: Op::NotEq
            },
        );
        Ok(())
    }

    #[test]
    fn plan_maps() -> Result<()> {
        let p = plan("{name: {name2: 'baz'}}")?;
        let key_name = p.tokens.borrow_mut().tokenize("name");
        let key_name2 = p.tokens.borrow_mut().tokenize("name2");
        assert_eq!(
            p.expr,
            Expr::Map(vec![MapEntryExpr {
                key: key_name,
                val: Expr::Map(vec![MapEntryExpr {
                    key: key_name2,
                    val: Expr::String("baz".to_string())
                }])
            }]),
        );
        Ok(())
    }
}
