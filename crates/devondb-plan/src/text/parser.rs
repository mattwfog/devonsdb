//! Text-form parser (`docs/PLAN_IR.md` § Expressions / Queries /
//! Statements / Defaults, binding).

use std::collections::{HashMap, HashSet};

use crate::expr::{BinaryOp, DateTruncUnit, Expr, Metric};
use crate::ops::{
    AggregateFunction, AggregateItem, Direction, JoinKey, JoinType, KnnMode, KnnVectorSource,
    Operator, PLAN_VERSION, Plan, ProjectionItem, SortKey, SortOrder,
};
use crate::statement::{InterfaceColumn, RelRow, SetItem, Statement, StatementEnvelope};
use crate::text::lexer::{Keyword, Token, TokenKind, lex};
use crate::text::printer::print_expression;
use crate::typing::SchemaInput;
use devondb_types::{
    DevonError, DevonResult,
    logical_type::LogicalType,
    schema::{Column, fold, suggestion_suffix},
    value::Value,
};

const TOP_LEVEL_WORDS: [&str; 9] = [
    "let", "nodes", "knn", "create", "insert", "upsert", "copy", "update", "delete",
];
const STAGE_WORDS: [&str; 7] = [
    "expand",
    "filter",
    "project",
    "sort",
    "limit",
    "aggregate",
    "join",
];
const MAX_EXPRESSION_NESTING: usize = 128;
const DML_WHERE_REQUIRED: &str =
    "DML requires `where <pk> = <literal>`; predicate-driven bulk DML is a separate lane";

/// The query or statement produced by parsing DevonPlan text.
#[derive(Debug, Clone, PartialEq)]
pub enum Parsed {
    /// A versioned query plan.
    Query(Plan),
    /// A versioned DDL or DML statement.
    Statement(StatementEnvelope),
}

/// Parses one complete DevonPlan text query or statement.
pub fn parse(input: &str) -> DevonResult<Parsed> {
    parse_with_sources(input, None)
}

/// Parses DevonPlan text while resolving `nodes(name)` table-first against
/// node tables and then ontology interfaces.
pub fn parse_with_schema(input: &str, schema: &SchemaInput<'_>) -> DevonResult<Parsed> {
    parse_with_sources(input, Some(SourceResolver::new(schema)))
}

fn parse_with_sources(input: &str, sources: Option<SourceResolver>) -> DevonResult<Parsed> {
    let tokens = lex(input)?;
    let mut parser = Parser {
        tokens,
        index: 0,
        eof_position: input.chars().count() + 1,
        source_text: input.to_owned(),
        sources,
    };
    let parsed = parser.parse_top_level()?;
    parser.expect_end()?;
    Ok(parsed)
}

/// Parses exactly one text-language value literal.
///
/// CSV `COPY` uses this entry point for every non-String field so bulk input
/// and statement literals always share one grammar.
pub(crate) fn parse_value_literal(input: &str) -> DevonResult<Value> {
    let tokens = lex(input)?;
    let mut parser = Parser {
        tokens,
        index: 0,
        eof_position: input.chars().count() + 1,
        source_text: input.to_owned(),
        sources: None,
    };
    let value = parser.parse_value_literal()?;
    parser.expect_end()?;
    Ok(value)
}

struct Parser {
    tokens: Vec<Token>,
    index: usize,
    eof_position: usize,
    source_text: String,
    sources: Option<SourceResolver>,
}

#[derive(Clone)]
struct SourceResolver {
    node_tables: HashSet<String>,
    interfaces: HashSet<String>,
}

impl SourceResolver {
    fn new(schema: &SchemaInput<'_>) -> Self {
        Self {
            node_tables: schema
                .node_tables()
                .iter()
                .map(|table| fold(table.name()).into_owned())
                .collect(),
            interfaces: schema
                .interfaces()
                .iter()
                .map(|interface| fold(interface.name()).into_owned())
                .collect(),
        }
    }

    fn is_unshadowed_interface(&self, name: &str) -> bool {
        let name = fold(name);
        !self.node_tables.contains(name.as_ref()) && self.interfaces.contains(name.as_ref())
    }
}

struct SubplanDefinition {
    name: String,
    name_token: Token,
    tokens: Vec<Token>,
    eof_position: usize,
    expression_nesting: usize,
    resolving: bool,
    operator: Option<Operator>,
}

struct Subplans {
    definitions: Vec<SubplanDefinition>,
    by_name: HashMap<String, usize>,
    source_text: String,
    sources: Option<SourceResolver>,
}

impl Parser {
    fn parse_top_level(&mut self) -> DevonResult<Parsed> {
        match self.current_kind() {
            Some(TokenKind::Keyword(
                Keyword::Let | Keyword::Nodes | Keyword::Knn,
            )) => {
                self.parse_query().map(Parsed::Query)
            }
            // Contextual, like `geo`: `within` followed by `(` starts a
            // within-source query; `within` alone stays an identifier.
            Some(TokenKind::Ident(identifier))
                if matches!(identifier.as_str(), "within" | "textscan") && self.next_is_lparen() =>
            {
                self.parse_query().map(Parsed::Query)
            }
            Some(TokenKind::Keyword(
                Keyword::Create
                | Keyword::Insert
                | Keyword::Upsert
                | Keyword::Copy
                | Keyword::Update
                | Keyword::Delete,
            )) => {
                self.parse_statement().map(|stmt| {
                    Parsed::Statement(StatementEnvelope {
                        v: PLAN_VERSION,
                        stmt,
                    })
                })
            }
            Some(TokenKind::Ident(identifier)) if identifier == "pin" => {
                self.advance();
                self.parse_pin().map(|stmt| {
                    Parsed::Statement(StatementEnvelope {
                        v: PLAN_VERSION,
                        stmt,
                    })
                })
            }
            Some(TokenKind::Ident(identifier)) if identifier == "unpin" => {
                self.advance();
                self.parse_unpin().map(|stmt| {
                    Parsed::Statement(StatementEnvelope {
                        v: PLAN_VERSION,
                        stmt,
                    })
                })
            }
            Some(TokenKind::Ident(identifier)) if identifier == "detach" => {
                self.advance();
                self.parse_detach_delete().map(|stmt| {
                    Parsed::Statement(StatementEnvelope {
                        v: PLAN_VERSION,
                        stmt,
                    })
                })
            }
            Some(TokenKind::Ident(identifier)) => {
                let identifier = identifier.clone();
                if is_top_level_word(&identifier.to_ascii_lowercase())
                    || matches!(
                        identifier.to_ascii_lowercase().as_str(),
                        "pin" | "unpin" | "detach"
                    )
                {
                    Err(self.wrong_case_error(&identifier))
                } else {
                    Err(self.unknown_top_level_error(&identifier))
                }
            }
            _ => Err(self.error_here(
                "expected query source `nodes`/`knn` or statement `create`/`insert`/`upsert`/`copy`/`update`/`delete`",
            )),
        }
    }

    fn parse_query(&mut self) -> DevonResult<Plan> {
        self.parse_query_at(0)
    }

    fn parse_query_at(&mut self, expression_nesting: usize) -> DevonResult<Plan> {
        let mut subplans = Subplans::parse_prologue(self, expression_nesting)?;
        subplans.resolve_all()?;
        let (operator, _) = self.parse_pipeline(&mut subplans, expression_nesting)?;
        subplans.reject_binding_collisions(&operator)?;
        Ok(Plan {
            v: PLAN_VERSION,
            plan: operator,
        })
    }

    fn parse_pipeline(
        &mut self,
        subplans: &mut Subplans,
        expression_nesting: usize,
    ) -> DevonResult<(Operator, Option<String>)> {
        let (mut operator, mut nearest_binding) = self.parse_source(expression_nesting)?;
        while self.consume_kind(&TokenKind::Pipe) {
            let (next, introduced_binding) = self.parse_stage(
                operator,
                nearest_binding.as_deref(),
                subplans,
                expression_nesting,
            )?;
            operator = next;
            if let Some(binding) = introduced_binding {
                nearest_binding = Some(binding);
            }
        }
        Ok((operator, nearest_binding))
    }

    fn parse_source(
        &mut self,
        expression_nesting: usize,
    ) -> DevonResult<(Operator, Option<String>)> {
        if self.consume_keyword(Keyword::Nodes) {
            self.expect_kind(&TokenKind::LParen, "expected `(` after `nodes`")?;
            let table = self.take_identifier("node table name")?;
            self.expect_kind(&TokenKind::RParen, "expected `)` after node table name")?;
            self.expect_keyword(Keyword::As, "as")?;
            let binding = self.take_identifier("nodes binding")?;
            let nearest = Some(binding.clone());
            let source = if self
                .sources
                .as_ref()
                .is_some_and(|sources| sources.is_unshadowed_interface(&table))
            {
                Operator::ScanInterface {
                    interface: table,
                    binding,
                }
            } else {
                Operator::ScanNodes { table, binding }
            };
            return Ok((source, nearest));
        }
        if let Some(TokenKind::Ident(identifier)) = self.current_kind()
            && identifier == "textscan"
            && self.next_is_lparen()
        {
            let source = self.parse_textscan_source()?;
            let Operator::TextScan { binding, .. } = &source else {
                return Err(self.error_here("invalid textscan source"));
            };
            let nearest = Some(binding.clone());
            return Ok((source, nearest));
        }
        if let Some(TokenKind::Ident(identifier)) = self.current_kind()
            && identifier == "within"
            && self.next_is_lparen()
        {
            return self.parse_within_source().map(|source| (source, None));
        }
        self.expect_keyword(Keyword::Knn, "knn")?;
        self.parse_knn_source(expression_nesting)
            .map(|source| (source, None))
    }

    fn parse_textscan_source(&mut self) -> DevonResult<Operator> {
        self.advance();
        self.expect_kind(&TokenKind::LParen, "expected `(` after textscan")?;
        let table = self.take_identifier("textscan table")?;
        self.expect_kind(&TokenKind::Dot, "expected `.` before textscan column")?;
        let column = self.take_identifier("textscan column")?;
        self.expect_kind(&TokenKind::Comma, "expected `,` before query")?;
        let query = self.take_string()?;
        self.expect_kind(&TokenKind::Comma, "expected `,` before k")?;
        let key = self.take_identifier("textscan k")?;
        if key != "k" {
            return Err(self.error_here("textscan expects k=<count>"));
        }
        self.expect_kind(&TokenKind::Eq, "expected `=` after k")?;
        let k = self.parse_count("textscan k")?;
        self.expect_kind(&TokenKind::RParen, "expected `)` after textscan k")?;
        self.expect_keyword(Keyword::As, "as")?;
        let binding = self.take_identifier("textscan binding")?;
        Ok(Operator::TextScan {
            table,
            column,
            query,
            k,
            binding,
        })
    }

    fn parse_within_source(&mut self) -> DevonResult<Operator> {
        self.advance();
        self.expect_kind(&TokenKind::LParen, "expected `(` after `within`")?;
        let table = self.take_identifier("within table name")?;
        self.expect_kind(&TokenKind::Dot, "expected `.` in within GeoPoint column")?;
        let column = self.take_identifier("within GeoPoint column name")?;
        self.expect_kind(
            &TokenKind::Comma,
            "expected `,` after within GeoPoint column",
        )?;
        let center = match self.current_kind() {
            Some(TokenKind::Ident(identifier)) if identifier == "geo" && self.next_is_lparen() => {
                match self.parse_geo_literal()? {
                    Value::GeoPoint(point) => point,
                    _ => return Err(self.error_here("within center must be a geo literal")),
                }
            }
            _ => {
                return Err(self.error_here("expected `geo(<lat>, <lng>)` as the within center"));
            }
        };
        self.expect_kind(&TokenKind::Comma, "expected `,` after within center")?;
        let meters = self.parse_geo_component("radius")?;
        self.expect_kind(&TokenKind::RParen, "expected `)` after within radius")?;
        Ok(Operator::WithinScan {
            table,
            column,
            center,
            meters,
        })
    }

    fn parse_knn_source(&mut self, expression_nesting: usize) -> DevonResult<Operator> {
        self.expect_kind(&TokenKind::LParen, "expected `(` after `knn`")?;
        let table = self.take_identifier("KNN table name")?;
        self.expect_kind(&TokenKind::Dot, "expected `.` in KNN vector column")?;
        let column = self.take_identifier("KNN vector column name")?;
        self.expect_kind(&TokenKind::Comma, "expected `,` after KNN vector column")?;
        let query = match self.current_kind() {
            Some(TokenKind::LBracket) => KnnVectorSource::Literal(self.parse_vector()?),
            Some(TokenKind::Keyword(Keyword::Scalar)) => KnnVectorSource::Scalar {
                plan: self.parse_scalar_plan(expression_nesting)?,
            },
            _ => {
                return Err(self.error_here(
                    "expected a vector literal or `scalar(<query>)` as the KNN query vector",
                ));
            }
        };
        self.expect_kind(&TokenKind::Comma, "expected `,` after KNN query vector")?;
        let k = self.parse_count("KNN `k`")?;
        self.expect_kind(&TokenKind::Comma, "expected `,` after KNN `k`")?;
        let metric = self.parse_metric()?;
        let mode = self.parse_knn_mode()?;
        self.expect_kind(&TokenKind::RParen, "expected `)` after KNN metric")?;
        Ok(Operator::KnnScan {
            table,
            column,
            query,
            k,
            metric,
            mode,
        })
    }

    /// Parses the optional trailing `, approximate` of a `knn(...)` source.
    /// Absence spells the exact default (`docs/HNSW.md` §11).
    fn parse_knn_mode(&mut self) -> DevonResult<KnnMode> {
        if !matches!(self.current_kind(), Some(TokenKind::Comma)) {
            return Ok(KnnMode::Exact);
        }
        self.advance();
        let word = self.take_identifier("KNN mode")?;
        if word == "approximate" {
            Ok(KnnMode::Approximate)
        } else {
            Err(self.error_here("expected `approximate` after KNN metric"))
        }
    }

    fn parse_stage(
        &mut self,
        input: Operator,
        nearest_binding: Option<&str>,
        subplans: &mut Subplans,
        expression_nesting: usize,
    ) -> DevonResult<(Operator, Option<String>)> {
        match self.current_kind() {
            Some(TokenKind::Ident(word)) if word == "expand_rel" => {
                self.parse_expand_rel(input, nearest_binding)
            }
            Some(TokenKind::Keyword(Keyword::Expand)) => {
                self.advance();
                self.parse_expand(input, nearest_binding)
            }
            Some(TokenKind::Keyword(Keyword::Filter)) => {
                self.advance();
                let predicate = self.parse_expression_at(expression_nesting)?;
                Ok((
                    Operator::Filter {
                        predicate,
                        input: Box::new(input),
                    },
                    None,
                ))
            }
            Some(TokenKind::Keyword(Keyword::Project)) => {
                self.advance();
                self.parse_project(input, expression_nesting)
                    .map(|operator| (operator, None))
            }
            Some(TokenKind::Keyword(Keyword::Sort)) => {
                self.advance();
                self.parse_sort(input, expression_nesting)
                    .map(|operator| (operator, None))
            }
            Some(TokenKind::Keyword(Keyword::Limit)) => {
                self.advance();
                self.parse_limit(input).map(|operator| (operator, None))
            }
            Some(TokenKind::Keyword(Keyword::Aggregate)) => {
                self.advance();
                self.parse_aggregate(input, expression_nesting)
                    .map(|operator| (operator, None))
            }
            Some(TokenKind::Keyword(Keyword::Join)) => {
                self.advance();
                self.parse_join(input, JoinType::Inner, subplans, expression_nesting)
            }
            Some(TokenKind::Ident(word)) if word == "left" => {
                self.advance();
                self.expect_keyword(Keyword::Join, "join")?;
                self.parse_join(input, JoinType::Left, subplans, expression_nesting)
            }
            _ => Err(self.unknown_stage_error()),
        }
    }

    // Keep traversal-only Operator temporaries off the recursive Project /
    // scalar parsing stack: every scalar level passes through parse_stage.
    fn parse_expand_rel(
        &mut self,
        input: Operator,
        nearest_binding: Option<&str>,
    ) -> DevonResult<(Operator, Option<String>)> {
        self.advance();
        let (operator, introduced) = self.parse_expand(input, nearest_binding)?;
        let word = self.take_identifier("via")?;
        if word != "via" {
            return Err(self.error_here("expected via relationship binding"));
        }
        let rel_binding = self.take_identifier("relationship binding")?;
        let Operator::Expand {
            rel,
            direction,
            from_binding,
            binding,
            input,
        } = operator
        else {
            return Err(self.error_here("expected expand operator"));
        };
        Ok((
            Operator::ExpandRel {
                rel,
                direction,
                from_binding,
                binding,
                rel_binding,
                input,
            },
            introduced,
        ))
    }

    fn parse_join(
        &mut self,
        left: Operator,
        join: JoinType,
        subplans: &mut Subplans,
        expression_nesting: usize,
    ) -> DevonResult<(Operator, Option<String>)> {
        let name_token = self
            .current()
            .cloned()
            .ok_or_else(|| self.error_here("expected join subplan name"))?;
        let name = self.take_identifier("join subplan name")?;
        self.expect_keyword(Keyword::On, "on")?;
        let expression_token = self.current().cloned();
        let expression = self.parse_expression_at(expression_nesting)?;
        let right = subplans.resolve(&name, &name_token)?;
        let on = decompose_join_keys(expression, &left, &right).map_err(|problem| {
            expression_token.as_ref().map_or_else(
                || self.error_here(problem),
                |token| self.error_at(token, problem),
            )
        })?;
        Ok((
            Operator::HashJoin {
                join,
                on,
                left: Box::new(left),
                right: Box::new(right),
            },
            None,
        ))
    }

    fn parse_expand(
        &mut self,
        input: Operator,
        nearest_binding: Option<&str>,
    ) -> DevonResult<(Operator, Option<String>)> {
        let rel = self.take_identifier("relationship table name")?;
        let direction = self.parse_direction()?;
        let from_binding = if self.consume_optional_keyword(Keyword::From, "from")? {
            self.take_identifier("expand source binding")?
        } else {
            nearest_binding.map(str::to_owned).ok_or_else(|| {
                self.error_here(
                    "expand without `from` has no preceding nodes/expand binding to default to",
                )
            })?
        };
        self.expect_keyword(Keyword::As, "as")?;
        let binding = self.take_identifier("expand result binding")?;
        let introduced = Some(binding.clone());
        Ok((
            Operator::Expand {
                rel,
                direction,
                from_binding,
                binding,
                input: Box::new(input),
            },
            introduced,
        ))
    }

    fn parse_project(
        &mut self,
        input: Operator,
        expression_nesting: usize,
    ) -> DevonResult<Operator> {
        let mut expressions = Vec::new();
        loop {
            let expr = self.parse_expression_at(expression_nesting)?;
            let alias = if self.consume_optional_keyword(Keyword::As, "as")? {
                self.take_identifier("project alias")?
            } else {
                print_expression(&expr)?
            };
            expressions.push(ProjectionItem { expr, alias });
            if !self.consume_kind(&TokenKind::Comma) {
                break;
            }
        }
        Ok(Operator::Project {
            exprs: expressions,
            input: Box::new(input),
        })
    }

    fn parse_sort(&mut self, input: Operator, expression_nesting: usize) -> DevonResult<Operator> {
        let mut keys = Vec::new();
        loop {
            let expr = self.parse_expression_at(expression_nesting)?;
            let order = if self.consume_optional_keyword(Keyword::Asc, "asc")? {
                SortOrder::Asc
            } else if self.consume_optional_keyword(Keyword::Desc, "desc")? {
                SortOrder::Desc
            } else {
                SortOrder::Asc
            };
            keys.push(SortKey { expr, order });
            if !self.consume_kind(&TokenKind::Comma) {
                break;
            }
        }
        Ok(Operator::Sort {
            keys,
            input: Box::new(input),
        })
    }

    fn parse_limit(&mut self, input: Operator) -> DevonResult<Operator> {
        let count = self.parse_count("limit count")?;
        let offset = if self.consume_optional_keyword(Keyword::Offset, "offset")? {
            Some(self.parse_count("limit offset")?)
        } else {
            None
        };
        Ok(Operator::Limit {
            count,
            offset,
            input: Box::new(input),
        })
    }

    fn parse_aggregate(
        &mut self,
        input: Operator,
        expression_nesting: usize,
    ) -> DevonResult<Operator> {
        let mut aggs = vec![self.parse_aggregate_item(expression_nesting)?];
        while self.consume_kind(&TokenKind::Comma) {
            aggs.push(self.parse_aggregate_item(expression_nesting)?);
        }
        let mut group_by = Vec::new();
        if self.consume_optional_keyword(Keyword::By, "by")? {
            group_by.push(self.parse_expression_at(expression_nesting)?);
            while self.consume_kind(&TokenKind::Comma) {
                group_by.push(self.parse_expression_at(expression_nesting)?);
            }
        }
        Ok(Operator::Aggregate {
            group_by,
            aggs,
            input: Box::new(input),
        })
    }

    fn parse_aggregate_item(&mut self, expression_nesting: usize) -> DevonResult<AggregateItem> {
        let function = self.parse_aggregate_function()?;
        self.expect_kind(&TokenKind::LParen, "expected `(` after aggregate function")?;
        if function == AggregateFunction::PercentileCont {
            self.parse_percentile_fraction()?;
            self.expect_kind(
                &TokenKind::Comma,
                "expected `,` after `percentile_cont` fraction",
            )?;
        }
        let expr = self.parse_expression_at(expression_nesting)?;
        self.expect_kind(
            &TokenKind::RParen,
            "expected `)` after aggregate expression",
        )?;
        self.expect_keyword(Keyword::As, "as")?;
        let alias = self.take_identifier("aggregate alias")?;
        Ok(AggregateItem {
            function,
            expr,
            alias,
        })
    }

    fn parse_percentile_fraction(&mut self) -> DevonResult<()> {
        let Some(TokenKind::Ident(name)) = self.current_kind() else {
            return Err(
                self.error_here("`percentile_cont` fraction must be exactly `decimal(\"0.5\")`")
            );
        };
        if name != "decimal" {
            return Err(
                self.error_here("`percentile_cont` fraction must be exactly `decimal(\"0.5\")`")
            );
        }
        self.advance();
        self.expect_kind(
            &TokenKind::LParen,
            "expected `(` in `percentile_cont` fraction `decimal(\"0.5\")`",
        )?;
        let fraction = self.take_string().map_err(|_| {
            self.error_here("`percentile_cont` fraction must be exactly `decimal(\"0.5\")`")
        })?;
        if fraction != "0.5" {
            return Err(self.error_here(format!(
                "`percentile_cont` fraction must be exactly `decimal(\"0.5\")`, got `decimal({fraction:?})`"
            )));
        }
        self.expect_kind(
            &TokenKind::RParen,
            "expected `)` after `percentile_cont` fraction",
        )
    }

    fn parse_expression_at(&mut self, nesting: usize) -> DevonResult<Expr> {
        self.parse_or(nesting)
    }

    fn parse_or(&mut self, nesting: usize) -> DevonResult<Expr> {
        let mut expression = self.parse_and(nesting)?;
        let mut chain_nesting = nesting;
        while self.consume_keyword(Keyword::Or) {
            chain_nesting = self.expression_operand_nesting(chain_nesting)?;
            let right = self.parse_and(chain_nesting)?;
            expression = binary(BinaryOp::Or, expression, right);
        }
        Ok(expression)
    }

    fn parse_and(&mut self, nesting: usize) -> DevonResult<Expr> {
        let mut expression = self.parse_not(nesting)?;
        let mut chain_nesting = nesting;
        while self.consume_keyword(Keyword::And) {
            chain_nesting = self.expression_operand_nesting(chain_nesting)?;
            let right = self.parse_not(chain_nesting)?;
            expression = binary(BinaryOp::And, expression, right);
        }
        Ok(expression)
    }

    fn parse_not(&mut self, nesting: usize) -> DevonResult<Expr> {
        if matches!(self.current_kind(), Some(TokenKind::Keyword(Keyword::Not))) {
            let operand_nesting = self.expression_operand_nesting(nesting)?;
            self.advance();
            return self
                .parse_not(operand_nesting)
                .map(|expression| Expr::Not(Box::new(expression)));
        }
        self.parse_comparison(nesting)
    }

    fn parse_comparison(&mut self, nesting: usize) -> DevonResult<Expr> {
        let left = self.parse_additive(nesting)?;
        let Some(op) = self.take_comparison_operator() else {
            return Ok(left);
        };
        let operand_nesting = self.expression_operand_nesting(nesting)?;
        let right = self.parse_additive(operand_nesting)?;
        if self.comparison_operator_at_current() {
            return Err(
                self.error_here("comparison operators do not chain; parenthesize one comparison")
            );
        }
        Ok(binary(op, left, right))
    }

    fn parse_additive(&mut self, nesting: usize) -> DevonResult<Expr> {
        let mut expression = self.parse_multiplicative(nesting)?;
        let mut chain_nesting = nesting;
        loop {
            let op = if self.consume_kind(&TokenKind::Plus) {
                Some(BinaryOp::Add)
            } else if self.consume_kind(&TokenKind::Minus) {
                Some(BinaryOp::Sub)
            } else {
                None
            };
            let Some(op) = op else { break };
            chain_nesting = self.expression_operand_nesting(chain_nesting)?;
            let right = self.parse_multiplicative(chain_nesting)?;
            expression = binary(op, expression, right);
        }
        Ok(expression)
    }

    fn parse_multiplicative(&mut self, nesting: usize) -> DevonResult<Expr> {
        let mut expression = self.parse_primary(nesting)?;
        let mut chain_nesting = nesting;
        loop {
            let op = if self.consume_kind(&TokenKind::Star) {
                Some(BinaryOp::Mul)
            } else if self.consume_kind(&TokenKind::Slash) {
                Some(BinaryOp::Div)
            } else {
                None
            };
            let Some(op) = op else { break };
            chain_nesting = self.expression_operand_nesting(chain_nesting)?;
            let right = self.parse_primary(chain_nesting)?;
            expression = binary(op, expression, right);
        }
        Ok(expression)
    }

    fn parse_primary(&mut self, nesting: usize) -> DevonResult<Expr> {
        match self.current_kind() {
            Some(TokenKind::Minus | TokenKind::Number { .. }) => {
                self.parse_numeric_value().map(Expr::Lit)
            }
            Some(TokenKind::Keyword(Keyword::Null)) => {
                self.advance();
                Ok(Expr::Lit(Value::Null))
            }
            Some(TokenKind::Keyword(Keyword::True | Keyword::False)) => {
                self.parse_boolean_value().map(Expr::Lit)
            }
            Some(TokenKind::Str(_)) => self
                .take_string()
                .map(|value| Expr::Lit(Value::String(value))),
            Some(TokenKind::LBracket) => self
                .parse_vector()
                .map(|value| Expr::Lit(Value::Vector(value))),
            Some(TokenKind::Keyword(Keyword::Distance)) => self.parse_distance(nesting),
            Some(TokenKind::Keyword(Keyword::If)) => self.parse_if(nesting),
            Some(TokenKind::Keyword(Keyword::Coalesce)) => {
                self.parse_variadic_call(Keyword::Coalesce, "coalesce", nesting, Expr::Coalesce)
            }
            Some(TokenKind::Keyword(Keyword::Least)) => {
                self.parse_variadic_call(Keyword::Least, "least", nesting, Expr::Least)
            }
            Some(TokenKind::Keyword(Keyword::Greatest)) => {
                self.parse_variadic_call(Keyword::Greatest, "greatest", nesting, Expr::Greatest)
            }
            Some(TokenKind::Keyword(Keyword::DateTrunc)) => self.parse_date_trunc(nesting),
            Some(TokenKind::Keyword(Keyword::DateAdd)) => self.parse_date_add(nesting),
            Some(TokenKind::Keyword(Keyword::Round)) => self.parse_round(nesting),
            Some(TokenKind::Keyword(Keyword::RoundDiv)) => self.parse_round_div(nesting),
            Some(TokenKind::Keyword(Keyword::Scalar)) => self.parse_scalar(nesting),
            Some(TokenKind::Ident(_)) => self.parse_identifier_primary(),
            Some(TokenKind::LParen) => self.parse_parenthesized_expression(nesting),
            _ => Err(self.error_here("expected an expression operand")),
        }
    }

    fn parse_identifier_primary(&mut self) -> DevonResult<Expr> {
        let Some(TokenKind::Ident(identifier)) = self.current_kind() else {
            return Err(self.error_here("expected an identifier expression"));
        };
        // Contextual call words stay ordinary column-reference identifiers
        // unless immediately followed by `(`.
        if identifier == "geo" && self.next_is_lparen() {
            return self.parse_geo_literal().map(Expr::Lit);
        }
        if identifier == "scoreof" && self.next_is_lparen() {
            self.advance();
            self.expect_kind(&TokenKind::LParen, "expected `(` after scoreof")?;
            let binding = self.take_identifier("scoreof binding")?;
            self.expect_kind(&TokenKind::RParen, "expected `)` after scoreof binding")?;
            return Ok(Expr::ScoreOf(binding));
        }
        if identifier == "classof" && self.next_is_lparen() {
            return self.parse_classof();
        }
        if is_scalar_v2_literal(identifier) && self.next_is_lparen() {
            return self.parse_scalar_v2_literal().map(Expr::Lit);
        }
        let folded = identifier.to_ascii_lowercase();
        if self.next_is_lparen()
            && (is_a6_call_word(&folded)
                || matches!(folded.as_str(), "timestamp" | "bytes" | "decimal" | "json"))
        {
            return Err(self.wrong_case_error(identifier));
        }
        if matches!(
            folded.as_str(),
            "true" | "false" | "null" | "count" | "sum" | "min" | "max" | "avg" | "percentile_cont"
        ) {
            return Err(self.wrong_case_error(identifier));
        }
        self.parse_column_reference()
    }

    fn parse_classof(&mut self) -> DevonResult<Expr> {
        self.advance();
        self.expect_kind(&TokenKind::LParen, "expected `(` after `classof`")?;
        let binding = self.take_identifier("classof binding")?;
        self.expect_kind(&TokenKind::RParen, "expected `)` after classof binding")?;
        Ok(Expr::ClassOf(binding))
    }

    fn parse_parenthesized_expression(&mut self, nesting: usize) -> DevonResult<Expr> {
        let operand_nesting = self.expression_operand_nesting(nesting)?;
        self.advance();
        let expression = self.parse_or(operand_nesting)?;
        self.expect_kind(&TokenKind::RParen, "expected `)` after expression")?;
        Ok(expression)
    }

    fn parse_distance(&mut self, nesting: usize) -> DevonResult<Expr> {
        self.expect_keyword(Keyword::Distance, "distance")?;
        if !self.at_kind(&TokenKind::LParen) {
            return Err(self.error_here("expected `(` after `distance`"));
        }
        let operand_nesting = self.expression_operand_nesting(nesting)?;
        self.advance();
        let left = self.parse_or(operand_nesting)?;
        self.expect_kind(
            &TokenKind::Comma,
            "expected `,` after left distance operand",
        )?;
        let right = self.parse_or(operand_nesting)?;
        self.expect_kind(
            &TokenKind::Comma,
            "expected `,` after right distance operand",
        )?;
        let metric = self.parse_metric()?;
        self.expect_kind(&TokenKind::RParen, "expected `)` after distance metric")?;
        Ok(Expr::Distance {
            left: Box::new(left),
            right: Box::new(right),
            metric,
        })
    }

    fn parse_if(&mut self, nesting: usize) -> DevonResult<Expr> {
        let operand_nesting = self.begin_expression_call(Keyword::If, "if", nesting)?;
        let mut arguments = self.parse_call_arguments(operand_nesting)?;
        self.require_call_arity("if", arguments.len(), 3)?;
        self.expect_kind(&TokenKind::RParen, "expected `)` after `if` arguments")?;
        let else_expr = arguments
            .pop()
            .ok_or_else(|| self.error_here("missing `if` else"))?;
        let then_expr = arguments
            .pop()
            .ok_or_else(|| self.error_here("missing `if` then"))?;
        let cond = arguments
            .pop()
            .ok_or_else(|| self.error_here("missing `if` condition"))?;
        Ok(Expr::If {
            cond: Box::new(cond),
            then_expr: Box::new(then_expr),
            else_expr: Box::new(else_expr),
        })
    }

    fn parse_variadic_call(
        &mut self,
        keyword: Keyword,
        name: &str,
        nesting: usize,
        constructor: fn(Vec<Expr>) -> Expr,
    ) -> DevonResult<Expr> {
        let operand_nesting = self.begin_expression_call(keyword, name, nesting)?;
        let arguments = self.parse_call_arguments(operand_nesting)?;
        if arguments.len() < 2 {
            return Err(self.wrong_call_arity(name, "at least 2", arguments.len()));
        }
        self.expect_kind(
            &TokenKind::RParen,
            format!("expected `)` after `{name}` arguments"),
        )?;
        Ok(constructor(arguments))
    }

    fn parse_date_trunc(&mut self, nesting: usize) -> DevonResult<Expr> {
        let operand_nesting =
            self.begin_expression_call(Keyword::DateTrunc, "date_trunc", nesting)?;
        if self.at_kind(&TokenKind::RParen) {
            return Err(self.wrong_call_arity("date_trunc", "exactly 2", 0));
        }
        let unit = self.take_date_trunc_unit()?;
        if !self.consume_kind(&TokenKind::Comma) {
            return Err(self.wrong_call_arity("date_trunc", "exactly 2", 1));
        }
        if self.at_kind(&TokenKind::RParen) {
            return Err(self.wrong_call_arity("date_trunc", "exactly 2", 1));
        }
        let value = self.parse_or(operand_nesting)?;
        let extra = self.parse_additional_call_arguments(operand_nesting)?;
        self.require_call_arity("date_trunc", 2 + extra, 2)?;
        self.expect_kind(
            &TokenKind::RParen,
            "expected `)` after `date_trunc` arguments",
        )?;
        Ok(Expr::DateTrunc {
            unit,
            value: Box::new(value),
        })
    }

    fn parse_date_add(&mut self, nesting: usize) -> DevonResult<Expr> {
        let operand_nesting = self.begin_expression_call(Keyword::DateAdd, "date_add", nesting)?;
        if self.at_kind(&TokenKind::RParen) {
            return Err(self.wrong_call_arity("date_add", "exactly 3", 0));
        }
        let unit = self.take_day_unit("date_add")?;
        self.expect_call_comma("date_add", 1, 3)?;
        let value = self.parse_or(operand_nesting)?;
        self.expect_call_comma("date_add", 2, 3)?;
        let amount = self.parse_or(operand_nesting)?;
        let extra = self.parse_additional_call_arguments(operand_nesting)?;
        self.require_call_arity("date_add", 3 + extra, 3)?;
        self.expect_kind(
            &TokenKind::RParen,
            "expected `)` after `date_add` arguments",
        )?;
        Ok(Expr::DateAdd {
            unit,
            value: Box::new(value),
            amount: Box::new(amount),
        })
    }

    fn parse_round(&mut self, nesting: usize) -> DevonResult<Expr> {
        let operand_nesting = self.begin_expression_call(Keyword::Round, "round", nesting)?;
        if self.at_kind(&TokenKind::RParen) {
            return Err(self.wrong_call_arity("round", "exactly 2", 0));
        }
        let value = self.parse_or(operand_nesting)?;
        self.expect_call_comma("round", 1, 2)?;
        let places = self.parse_places("round")?;
        self.expect_places_end("round")?;
        self.expect_kind(&TokenKind::RParen, "expected `)` after `round` arguments")?;
        Ok(Expr::Round {
            value: Box::new(value),
            places,
        })
    }

    fn parse_round_div(&mut self, nesting: usize) -> DevonResult<Expr> {
        let operand_nesting =
            self.begin_expression_call(Keyword::RoundDiv, "round_div", nesting)?;
        if self.at_kind(&TokenKind::RParen) {
            return Err(self.wrong_call_arity("round_div", "exactly 3", 0));
        }
        let numerator = self.parse_or(operand_nesting)?;
        self.expect_call_comma("round_div", 1, 3)?;
        let denominator = self.parse_or(operand_nesting)?;
        self.expect_call_comma("round_div", 2, 3)?;
        let places = self.parse_places("round_div")?;
        self.expect_places_end("round_div")?;
        self.expect_kind(
            &TokenKind::RParen,
            "expected `)` after `round_div` arguments",
        )?;
        Ok(Expr::RoundDiv {
            numerator: Box::new(numerator),
            denominator: Box::new(denominator),
            places,
        })
    }

    fn parse_scalar(&mut self, nesting: usize) -> DevonResult<Expr> {
        self.parse_scalar_plan(nesting)
            .map(|plan| Expr::Scalar { plan })
    }

    fn parse_scalar_plan(&mut self, nesting: usize) -> DevonResult<Box<Operator>> {
        let query_nesting = self.begin_expression_call(Keyword::Scalar, "scalar", nesting)?;
        if self.at_kind(&TokenKind::RParen) {
            return Err(self.wrong_call_arity("scalar", "exactly 1 query", 0));
        }
        if self.scalar_argument_is_statement() {
            return Err(self.error_here("`scalar` accepts a query, not a statement"));
        }
        let plan = self.parse_query_at(query_nesting)?;
        self.expect_kind(
            &TokenKind::RParen,
            "expected the `)` matching the `scalar(` opener",
        )?;
        Ok(Box::new(plan.plan))
    }

    fn scalar_argument_is_statement(&self) -> bool {
        matches!(
            self.current_kind(),
            Some(TokenKind::Keyword(
                Keyword::Create
                    | Keyword::Insert
                    | Keyword::Upsert
                    | Keyword::Copy
                    | Keyword::Update
                    | Keyword::Delete
            ))
        ) || matches!(
            self.current_kind(),
            Some(TokenKind::Ident(word)) if matches!(word.as_str(), "pin" | "unpin" | "detach")
        )
    }

    fn expect_call_comma(&mut self, name: &str, actual: usize, expected: usize) -> DevonResult<()> {
        if self.consume_kind(&TokenKind::Comma) {
            if self.at_kind(&TokenKind::RParen) {
                return Err(self.wrong_call_arity(name, &format!("exactly {expected}"), actual));
            }
            return Ok(());
        }
        Err(self.wrong_call_arity(name, &format!("exactly {expected}"), actual))
    }

    fn parse_places(&mut self, name: &str) -> DevonResult<u8> {
        if self.at_kind(&TokenKind::Minus) {
            return Err(self.error_here(format!(
                "`{name}` places must be a nonnegative integer literal in 0..=38"
            )));
        }
        let token = self.current().cloned().ok_or_else(|| {
            self.error_here(format!(
                "`{name}` places must be a nonnegative integer literal in 0..=38"
            ))
        })?;
        let TokenKind::Number { text, is_float } = &token.kind else {
            return Err(self.error_at(
                &token,
                format!("`{name}` places must be a nonnegative integer literal, not an expression"),
            ));
        };
        if *is_float {
            return Err(self.error_at(
                &token,
                format!("`{name}` places must be an integer literal in 0..=38"),
            ));
        }
        let places = text
            .parse::<u8>()
            .map_err(|_| self.error_at(&token, format!("`{name}` places must be in 0..=38")))?;
        if places > 38 {
            return Err(self.error_at(&token, format!("`{name}` places must be in 0..=38")));
        }
        self.advance();
        Ok(places)
    }

    fn expect_places_end(&self, name: &str) -> DevonResult<()> {
        if self.at_kind(&TokenKind::RParen) {
            return Ok(());
        }
        Err(self.error_here(format!(
            "`{name}` places must be a nonnegative integer literal, not an expression"
        )))
    }

    fn begin_expression_call(
        &mut self,
        keyword: Keyword,
        name: &str,
        nesting: usize,
    ) -> DevonResult<usize> {
        self.expect_keyword(keyword, name)?;
        if !self.at_kind(&TokenKind::LParen) {
            return Err(self.error_here(format!("expected `(` after `{name}`")));
        }
        let operand_nesting = self.expression_operand_nesting(nesting)?;
        self.advance();
        Ok(operand_nesting)
    }

    fn parse_call_arguments(&mut self, nesting: usize) -> DevonResult<Vec<Expr>> {
        let mut arguments = Vec::new();
        if self.at_kind(&TokenKind::RParen) {
            return Ok(arguments);
        }
        loop {
            arguments.push(self.parse_expression_at(nesting)?);
            if !self.consume_kind(&TokenKind::Comma) {
                return Ok(arguments);
            }
        }
    }

    fn parse_additional_call_arguments(&mut self, nesting: usize) -> DevonResult<usize> {
        let mut count = 0;
        while self.consume_kind(&TokenKind::Comma) {
            if self.at_kind(&TokenKind::RParen) {
                return Err(self.error_here("expected an expression operand after `,`"));
            }
            self.parse_expression_at(nesting)?;
            count += 1;
        }
        Ok(count)
    }

    fn take_date_trunc_unit(&mut self) -> DevonResult<DateTruncUnit> {
        self.take_day_unit("date_trunc")
    }

    fn take_day_unit(&mut self, name: &str) -> DevonResult<DateTruncUnit> {
        let Some(TokenKind::Str(unit)) = self.current_kind() else {
            return Err(self.error_here(format!(
                "`{name}` unit must be the string literal `\"day\"`; accepted unit is `day`"
            )));
        };
        if unit != "day" {
            return Err(self.error_here(format!(
                "unknown `{name}` unit `{unit}`; accepted unit is `day`"
            )));
        }
        self.advance();
        Ok(DateTruncUnit::Day)
    }

    fn require_call_arity(&self, name: &str, actual: usize, expected: usize) -> DevonResult<()> {
        if actual != expected {
            return Err(self.wrong_call_arity(name, &format!("exactly {expected}"), actual));
        }
        Ok(())
    }

    fn wrong_call_arity(&self, name: &str, expected: &str, actual: usize) -> DevonError {
        self.error_here(format!(
            "`{name}` has wrong arity: expected {expected} arguments, got {actual}"
        ))
    }

    fn expression_operand_nesting(&self, nesting: usize) -> DevonResult<usize> {
        if nesting >= MAX_EXPRESSION_NESTING {
            return Err(self.error_here(format!(
                "expression nesting exceeds {MAX_EXPRESSION_NESTING} levels"
            )));
        }
        Ok(nesting + 1)
    }

    fn parse_column_reference(&mut self) -> DevonResult<Expr> {
        let binding = self.take_identifier("column binding")?;
        self.expect_kind(
            &TokenKind::Dot,
            "column reference requires `binding.column`",
        )?;
        let column = self.take_identifier("column name")?;
        Ok(Expr::Col(format!("{binding}.{column}")))
    }

    fn parse_statement(&mut self) -> DevonResult<Statement> {
        if self.consume_keyword(Keyword::Create) {
            return self.parse_create();
        }
        if self.consume_keyword(Keyword::Copy) {
            return self.parse_copy();
        }
        if self.consume_keyword(Keyword::Update) {
            return self.parse_update();
        }
        if self.consume_keyword(Keyword::Delete) {
            return self.parse_delete();
        }
        if self.consume_keyword(Keyword::Upsert) {
            return self.parse_upsert();
        }
        self.expect_keyword(Keyword::Insert, "insert")?;
        self.parse_insert()
    }

    fn parse_update(&mut self) -> DevonResult<Statement> {
        let table = self.take_identifier("update node table name")?;
        self.expect_keyword(Keyword::Set, "set")?;
        if self.current().is_none() || self.at_kind(&TokenKind::Keyword(Keyword::Where)) {
            return Err(self.error_here("an update requires at least one set item"));
        }

        let mut columns = HashSet::new();
        let mut set = Vec::new();
        loop {
            let token = self.current().cloned();
            let column = self.take_identifier("update set column")?;
            if !columns.insert(fold(&column).into_owned()) {
                return Err(token.as_ref().map_or_else(
                    || self.error_here(format!("duplicate set column `{column}`")),
                    |token| self.error_at(token, format!("duplicate set column `{column}`")),
                ));
            }
            self.expect_kind(&TokenKind::Eq, "expected `=` after update set column")?;
            set.push(SetItem {
                column,
                value: self.parse_dml_literal()?,
            });
            if !self.consume_kind(&TokenKind::Comma) {
                break;
            }
        }
        let (key_column, key) = self.parse_dml_key()?;
        Ok(Statement::UpdateNode {
            table,
            set,
            key_column,
            key,
        })
    }

    fn parse_delete(&mut self) -> DevonResult<Statement> {
        let (table, key_column, key) = self.parse_delete_target()?;
        Ok(Statement::DeleteNode {
            table,
            key_column,
            key,
        })
    }

    fn parse_detach_delete(&mut self) -> DevonResult<Statement> {
        self.expect_keyword(Keyword::Delete, "delete")?;
        let (table, key_column, key) = self.parse_delete_target()?;
        Ok(Statement::DetachDeleteNode {
            table,
            key_column,
            key,
        })
    }

    fn parse_delete_target(&mut self) -> DevonResult<(String, String, Value)> {
        self.expect_keyword(Keyword::From, "from")?;
        let table = self.take_identifier("delete node table name")?;
        let (key_column, key) = self.parse_dml_key()?;
        Ok((table, key_column, key))
    }

    fn parse_dml_key(&mut self) -> DevonResult<(String, Value)> {
        if !self.consume_optional_keyword(Keyword::Where, "where")? {
            return Err(self.error_here(DML_WHERE_REQUIRED));
        }
        let key_column = self.take_identifier("DML primary-key column")?;
        self.expect_kind(&TokenKind::Eq, "expected `=` after DML primary-key column")?;
        let key = self.parse_dml_literal()?;
        Ok((key_column, key))
    }

    fn parse_dml_literal(&mut self) -> DevonResult<Value> {
        let value = self.parse_value_literal()?;
        if matches!(
            self.current_kind(),
            Some(
                TokenKind::Eq
                    | TokenKind::Ne
                    | TokenKind::Lt
                    | TokenKind::Le
                    | TokenKind::Gt
                    | TokenKind::Ge
                    | TokenKind::Plus
                    | TokenKind::Minus
                    | TokenKind::Star
                    | TokenKind::Slash
                    | TokenKind::Dot
                    | TokenKind::Keyword(Keyword::And | Keyword::Or)
            )
        ) {
            return Err(self.error_here("DML values must be literals, not expressions"));
        }
        Ok(value)
    }

    fn parse_copy(&mut self) -> DevonResult<Statement> {
        let table = self.take_identifier("copy node table name")?;
        self.expect_keyword(Keyword::From, "from")?;
        let path = self.take_string()?;
        let sort_by = if self.consume_optional_keyword(Keyword::Sort, "sort")? {
            self.expect_keyword(Keyword::By, "by")?;
            Some(self.take_identifier("copy sort column")?)
        } else {
            None
        };
        Ok(Statement::CopyNode {
            table,
            path,
            sort_by,
        })
    }

    fn parse_pin(&mut self) -> DevonResult<Statement> {
        let name = self.take_string()?;
        self.expect_keyword(Keyword::As, "as")?;
        let plan = self.parse_query()?;
        Ok(Statement::PinPlan {
            name,
            text: self.source_text.clone(),
            plan,
        })
    }

    fn parse_unpin(&mut self) -> DevonResult<Statement> {
        Ok(Statement::UnpinPlan {
            name: self.take_string()?,
        })
    }

    fn parse_create(&mut self) -> DevonResult<Statement> {
        if let Some(TokenKind::Ident(identifier)) = self.current_kind() {
            match identifier.as_str() {
                "hnsw" => {
                    self.advance();
                    return self.parse_create_hnsw_index();
                }
                "interface" => {
                    self.advance();
                    return self.parse_create_interface();
                }
                "class" => {
                    self.advance();
                    return self.parse_create_class();
                }
                _ if matches!(
                    identifier.to_ascii_lowercase().as_str(),
                    "node" | "rel" | "hnsw" | "interface" | "class"
                ) =>
                {
                    return Err(self.wrong_case_error(identifier));
                }
                _ => {}
            }
        }
        if self.consume_keyword(Keyword::Node) {
            self.expect_keyword(Keyword::Table, "table")?;
            let name = self.take_identifier("node table name")?;
            self.expect_kind(&TokenKind::LParen, "node table requires a column list")?;
            let columns = self.parse_nonempty_columns()?;
            return Ok(Statement::CreateNodeTable { name, columns });
        }
        if !self.consume_keyword(Keyword::Rel) {
            return Err(self.error_here(
                "expected create kind `node`, `rel`, `hnsw`, `interface`, or `class`",
            ));
        }
        self.expect_keyword(Keyword::Table, "table")?;
        let name = self.take_identifier("relationship table name")?;
        self.expect_keyword(Keyword::From, "from")?;
        let from = self.take_identifier("relationship source table")?;
        self.expect_keyword(Keyword::To, "to")?;
        let to = self.take_identifier("relationship destination table")?;
        let columns = if self.consume_kind(&TokenKind::LParen) {
            self.parse_nonempty_columns()?
        } else {
            Vec::new()
        };
        Ok(Statement::CreateRelTable {
            name,
            from,
            to,
            columns,
        })
    }

    fn parse_create_interface(&mut self) -> DevonResult<Statement> {
        let name = self.take_identifier("interface name")?;
        self.expect_kind(
            &TokenKind::LParen,
            "interface declaration requires a column list",
        )?;
        if self.at_kind(&TokenKind::RParen) {
            return Err(self.error_here("an interface requires at least one column"));
        }
        let mut columns = vec![self.parse_interface_column()?];
        while self.consume_kind(&TokenKind::Comma) {
            columns.push(self.parse_interface_column()?);
        }
        self.expect_kind(
            &TokenKind::RParen,
            "expected `)` after interface column list",
        )?;
        Ok(Statement::CreateInterface { name, columns })
    }

    fn parse_interface_column(&mut self) -> DevonResult<InterfaceColumn> {
        Ok(InterfaceColumn {
            name: self.take_identifier("interface column name")?,
            ty: self.parse_logical_type()?,
        })
    }

    fn parse_create_class(&mut self) -> DevonResult<Statement> {
        self.expect_contextual_word("for")?;
        let table = self.take_identifier("class table name")?;
        let mut display = None;
        let mut plural = None;
        let mut label = None;
        let mut summary = Vec::new();
        let mut color = None;
        let mut description = None;
        let mut verb = None;
        let mut inverse = None;
        let mut implements = Vec::new();
        if self.consume_kind(&TokenKind::LParen) {
            self.parse_class_clauses(
                &mut display,
                &mut plural,
                &mut label,
                &mut summary,
                &mut color,
                &mut description,
                &mut verb,
                &mut inverse,
                &mut implements,
            )?;
        }
        Ok(Statement::CreateClass {
            table,
            display,
            plural,
            label,
            summary,
            color,
            description,
            verb,
            inverse,
            implements,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn parse_class_clauses(
        &mut self,
        display: &mut Option<String>,
        plural: &mut Option<String>,
        label: &mut Option<String>,
        summary: &mut Vec<String>,
        color: &mut Option<String>,
        description: &mut Option<String>,
        verb: &mut Option<String>,
        inverse: &mut Option<String>,
        implements: &mut Vec<String>,
    ) -> DevonResult<()> {
        if self.at_kind(&TokenKind::RParen) {
            return Err(self.error_here("a class clause list cannot be empty"));
        }
        let mut previous_order = 0;
        loop {
            let token = self.current().cloned();
            let clause = self.take_identifier("class clause")?;
            let order = class_clause_order(&clause).ok_or_else(|| {
                token.as_ref().map_or_else(
                    || self.error_here("unknown class clause"),
                    |token| self.error_at(token, format!("unknown class clause `{clause}`")),
                )
            })?;
            if order <= previous_order {
                return Err(self.error_here(format!(
                    "class clause `{clause}` is duplicated or out of canonical order"
                )));
            }
            previous_order = order;
            match clause.as_str() {
                "display" => *display = Some(self.take_string()?),
                "plural" => *plural = Some(self.take_string()?),
                "label" => *label = Some(self.take_identifier("label column")?),
                "summary" => *summary = self.parse_nonempty_identifier_list("summary columns")?,
                "color" => *color = Some(self.take_string()?),
                "description" => *description = Some(self.take_string()?),
                "verb" => *verb = Some(self.take_string()?),
                "inverse" => *inverse = Some(self.take_string()?),
                "implements" => {
                    *implements = self.parse_nonempty_identifier_list("implemented interfaces")?;
                }
                _ => return Err(self.error_here("unknown class clause")),
            }
            if !self.consume_kind(&TokenKind::Comma) {
                break;
            }
        }
        self.expect_kind(&TokenKind::RParen, "expected `)` after class clauses")
    }

    fn parse_nonempty_identifier_list(&mut self, description: &str) -> DevonResult<Vec<String>> {
        self.expect_kind(
            &TokenKind::LParen,
            format!("expected `(` before {description}"),
        )?;
        if self.at_kind(&TokenKind::RParen) {
            return Err(self.error_here(format!("{description} requires at least one name")));
        }
        let mut names = vec![self.take_identifier(description)?];
        while self.consume_kind(&TokenKind::Comma) {
            names.push(self.take_identifier(description)?);
        }
        self.expect_kind(
            &TokenKind::RParen,
            format!("expected `)` after {description}"),
        )?;
        Ok(names)
    }

    fn parse_nonempty_columns(&mut self) -> DevonResult<Vec<Column>> {
        if self.at_kind(&TokenKind::RParen) {
            return Err(self.error_here("a parenthesized column list requires at least one column"));
        }
        let mut columns = vec![self.parse_column_definition()?];
        while self.consume_kind(&TokenKind::Comma) {
            columns.push(self.parse_column_definition()?);
        }
        self.expect_kind(&TokenKind::RParen, "expected `)` after column list")?;
        Ok(columns)
    }

    fn parse_column_definition(&mut self) -> DevonResult<Column> {
        let name = self.take_identifier("column name")?;
        let ty = self.parse_logical_type()?;
        let primary_key = if self.consume_optional_keyword(Keyword::Primary, "primary")? {
            self.expect_keyword(Keyword::Key, "key")?;
            true
        } else {
            false
        };
        Ok(Column {
            name,
            ty,
            primary_key,
        })
    }

    fn parse_logical_type(&mut self) -> DevonResult<LogicalType> {
        let token = self
            .current()
            .cloned()
            .ok_or_else(|| self.error_here("expected a column type"))?;
        let TokenKind::Ident(name) = &token.kind else {
            return Err(self.error_at(
                &token,
                "expected column type Bool, Int64, Float64, String, Vector(dim), GeoPoint, Timestamp, Bytes, Decimal(p, s), or Json",
            ));
        };
        self.advance();
        match name.as_str() {
            "Bool" => Ok(LogicalType::Bool),
            "Int64" => Ok(LogicalType::Int64),
            "Float64" => Ok(LogicalType::Float64),
            "String" => Ok(LogicalType::String),
            "GeoPoint" => Ok(LogicalType::GeoPoint),
            "Timestamp" => Ok(LogicalType::Timestamp),
            "Bytes" => Ok(LogicalType::Bytes),
            "Json" => Ok(LogicalType::Json),
            "Decimal" => self.parse_decimal_type(&token),
            "Vector" => {
                self.expect_kind(&TokenKind::LParen, "expected `(` after type `Vector`")?;
                let dim = self.parse_count("Vector dimension")?;
                let dim = u32::try_from(dim)
                    .map_err(|_| self.error_at(&token, "Vector dimension exceeds u32"))?;
                self.expect_kind(&TokenKind::RParen, "expected `)` after Vector dimension")?;
                Ok(LogicalType::Vector { dim })
            }
            _ => Err(self.error_at(
                &token,
                "unknown column type; expected Bool, Int64, Float64, String, Vector(dim), GeoPoint, Timestamp, Bytes, Decimal(p, s), or Json",
            )),
        }
    }

    fn parse_decimal_type(&mut self, decimal_token: &Token) -> DevonResult<LogicalType> {
        self.expect_kind(&TokenKind::LParen, "expected `(` after type `Decimal`")?;
        let precision = self.parse_count("Decimal precision")?;
        self.expect_kind(
            &TokenKind::Comma,
            "expected `,` between Decimal precision and scale",
        )?;
        let scale = self.parse_count("Decimal scale")?;
        self.expect_kind(&TokenKind::RParen, "expected `)` after Decimal scale")?;
        let precision = u8::try_from(precision).map_err(|_| {
            self.error_at(decimal_token, "Decimal precision must satisfy 1 <= p <= 38")
        })?;
        let scale = u8::try_from(scale)
            .map_err(|_| self.error_at(decimal_token, "Decimal scale must satisfy s <= p"))?;
        if !(1..=38).contains(&precision) || scale > precision {
            return Err(self.error_at(
                decimal_token,
                format!(
                    "Decimal({precision}, {scale}) is outside Decimal bounds 1 <= p <= 38, s <= p"
                ),
            ));
        }
        Ok(LogicalType::Decimal { precision, scale })
    }

    fn parse_insert(&mut self) -> DevonResult<Statement> {
        let relationship = self.consume_optional_keyword(Keyword::Rel, "rel")?;
        self.expect_keyword(Keyword::Into, "into")?;
        let table = self.take_identifier("insert table name")?;
        self.expect_keyword(Keyword::Values, "values")?;
        if relationship {
            let rows = self.parse_rel_rows()?;
            Ok(Statement::InsertRel { table, rows })
        } else {
            let rows = self.parse_node_rows()?;
            Ok(Statement::InsertNode { table, rows })
        }
    }

    fn parse_upsert(&mut self) -> DevonResult<Statement> {
        let table = self.take_identifier("upsert node table name")?;
        self.expect_keyword(Keyword::Values, "values")?;
        if self.current().is_none() {
            return Err(self.error_here("upsert requires at least one row"));
        }
        let mut rows = vec![self.parse_upsert_row()?];
        while self.consume_kind(&TokenKind::Comma) {
            rows.push(self.parse_upsert_row()?);
        }
        Ok(Statement::UpsertNode { table, rows })
    }

    fn parse_upsert_row(&mut self) -> DevonResult<Vec<Value>> {
        self.expect_kind(&TokenKind::LParen, "expected `(` to start upsert row")?;
        if self.at_kind(&TokenKind::RParen) {
            return Err(self.error_here("upsert row requires at least one value"));
        }
        let mut values = vec![self.parse_dml_literal()?];
        while self.consume_kind(&TokenKind::Comma) {
            values.push(self.parse_dml_literal()?);
        }
        self.expect_kind(&TokenKind::RParen, "expected `)` after upsert row")?;
        Ok(values)
    }

    fn parse_node_rows(&mut self) -> DevonResult<Vec<Vec<Value>>> {
        let mut rows = vec![self.parse_node_row()?];
        while self.consume_kind(&TokenKind::Comma) {
            rows.push(self.parse_node_row()?);
        }
        Ok(rows)
    }

    fn parse_node_row(&mut self) -> DevonResult<Vec<Value>> {
        self.expect_kind(&TokenKind::LParen, "expected `(` to start node insert row")?;
        if self.at_kind(&TokenKind::RParen) {
            return Err(self.error_here("node insert row requires at least one value"));
        }
        let mut values = vec![self.parse_value_literal()?];
        while self.consume_kind(&TokenKind::Comma) {
            values.push(self.parse_value_literal()?);
        }
        self.expect_kind(&TokenKind::RParen, "expected `)` after node insert row")?;
        Ok(values)
    }

    fn parse_rel_rows(&mut self) -> DevonResult<Vec<RelRow>> {
        let mut rows = vec![self.parse_rel_row()?];
        while self.consume_kind(&TokenKind::Comma) {
            rows.push(self.parse_rel_row()?);
        }
        Ok(rows)
    }

    fn parse_rel_row(&mut self) -> DevonResult<RelRow> {
        self.expect_kind(
            &TokenKind::LParen,
            "expected `(` to start relationship insert row",
        )?;
        let from_key = self.parse_value_literal()?;
        self.expect_kind(
            &TokenKind::Arrow,
            "expected `->` between relationship endpoint keys",
        )?;
        let to_key = self.parse_value_literal()?;
        let mut values = Vec::new();
        while self.consume_kind(&TokenKind::Comma) {
            values.push(self.parse_value_literal()?);
        }
        self.expect_kind(
            &TokenKind::RParen,
            "expected `)` after relationship insert row",
        )?;
        Ok(RelRow {
            from_key,
            to_key,
            values,
        })
    }

    fn parse_value_literal(&mut self) -> DevonResult<Value> {
        match self.current_kind() {
            Some(TokenKind::Minus | TokenKind::Number { .. }) => self.parse_numeric_value(),
            Some(TokenKind::Keyword(Keyword::Null)) => {
                self.advance();
                Ok(Value::Null)
            }
            Some(TokenKind::Keyword(Keyword::True | Keyword::False)) => self.parse_boolean_value(),
            Some(TokenKind::Str(_)) => self.take_string().map(Value::String),
            Some(TokenKind::LBracket) => self.parse_vector().map(Value::Vector),
            Some(TokenKind::Ident(identifier)) if identifier == "geo" && self.next_is_lparen() => {
                self.parse_geo_literal()
            }
            Some(TokenKind::Ident(identifier))
                if is_scalar_v2_literal(identifier) && self.next_is_lparen() =>
            {
                self.parse_scalar_v2_literal()
            }
            _ => Err(self.error_here("expected a literal value")),
        }
    }

    /// Parses `geo(<lat>, <lng>)` into a canonical [`Value::GeoPoint`].
    ///
    /// The constructor surface normalizes the two canonical spellings
    /// (`+180` longitude, pole longitudes) per `docs/GEO.md` §5; anything
    /// else out of range is a parse error.
    fn parse_geo_literal(&mut self) -> DevonResult<Value> {
        let geo_token = self
            .current()
            .cloned()
            .ok_or_else(|| self.error_here("expected `geo`"))?;
        self.advance();
        self.expect_kind(&TokenKind::LParen, "expected `(` after `geo`")?;
        let lat = self.parse_geo_component("latitude")?;
        self.expect_kind(&TokenKind::Comma, "expected `,` between geo components")?;
        let lng = self.parse_geo_component("longitude")?;
        self.expect_kind(&TokenKind::RParen, "expected `)` after geo longitude")?;
        devondb_types::GeoPoint::new(lat, lng)
            .map(Value::GeoPoint)
            .map_err(|error| self.error_at(&geo_token, error.to_string()))
    }

    fn parse_scalar_v2_literal(&mut self) -> DevonResult<Value> {
        let constructor = self
            .current()
            .cloned()
            .ok_or_else(|| self.error_here("expected a scalar literal constructor"))?;
        let TokenKind::Ident(name) = &constructor.kind else {
            return Err(self.error_at(&constructor, "expected a scalar literal constructor"));
        };
        let name = name.clone();
        self.advance();
        self.expect_kind(&TokenKind::LParen, format!("expected `(` after `{name}`"))?;
        let text = self
            .take_string()
            .map_err(|_| self.error_here(format!("{name} literal requires a string argument")))?;
        self.expect_kind(
            &TokenKind::RParen,
            format!("expected `)` after {name} literal"),
        )?;
        match name.as_str() {
            "timestamp" => parse_timestamp_micros(&text)
                .map(Value::Timestamp)
                .map_err(|problem| self.error_at(&constructor, problem)),
            "bytes" => parse_lowercase_hex(&text)
                .map(Value::Bytes)
                .map_err(|problem| self.error_at(&constructor, problem)),
            "decimal" => text
                .parse::<devondb_types::Decimal128>()
                .map(Value::Decimal)
                .map_err(|error| {
                    self.error_at(&constructor, format!("invalid decimal literal: {error}"))
                }),
            "json" => canonical_json_text(&text)
                .map(Value::Json)
                .map_err(|problem| self.error_at(&constructor, problem)),
            _ => Err(self.error_at(&constructor, "unknown scalar literal constructor")),
        }
    }

    fn parse_geo_component(&mut self, component: &str) -> DevonResult<f64> {
        let minus = if self.at_kind(&TokenKind::Minus) {
            let token = self.current().cloned();
            self.advance();
            token
        } else {
            None
        };
        let token = self.take_number_token().map_err(|_| {
            minus.as_ref().map_or_else(
                || self.error_here(format!("geo {component} must be a number")),
                |minus| self.error_at(minus, "`-` in a geo literal must be followed by a number"),
            )
        })?;
        let position = minus
            .as_ref()
            .map_or(token.position, |token| token.position);
        match number_value(&token, minus.is_some(), position)? {
            Value::Int64(value) => Ok(value as f64),
            Value::Float64(value) => Ok(value),
            _ => Err(self.error_at(&token, "geo components must be numbers")),
        }
    }

    fn parse_numeric_value(&mut self) -> DevonResult<Value> {
        let minus = if self.at_kind(&TokenKind::Minus) {
            let token = self.current().cloned();
            self.advance();
            token
        } else {
            None
        };
        let token = self.take_number_token().map_err(|_| {
            minus.as_ref().map_or_else(
                || self.error_here("expected a number"),
                |minus| {
                    self.error_at(
                        minus,
                        "`-` in operand position must be followed by a number",
                    )
                },
            )
        })?;
        let position = minus
            .as_ref()
            .map_or(token.position, |token| token.position);
        number_value(&token, minus.is_some(), position)
    }

    fn parse_vector(&mut self) -> DevonResult<Vec<f32>> {
        self.expect_kind(&TokenKind::LBracket, "expected `[` to start vector")?;
        if self.consume_kind(&TokenKind::RBracket) {
            return Ok(Vec::new());
        }
        let mut values = vec![self.parse_vector_element(0)?];
        while self.consume_kind(&TokenKind::Comma) {
            let index = values.len();
            values.push(self.parse_vector_element(index)?);
        }
        self.expect_kind(&TokenKind::RBracket, "expected `]` after vector")?;
        Ok(values)
    }

    fn parse_vector_element(&mut self, index: usize) -> DevonResult<f32> {
        let minus = if self.at_kind(&TokenKind::Minus) {
            let token = self.current().cloned();
            self.advance();
            token
        } else {
            None
        };
        let token = self.take_number_token().map_err(|_| {
            minus.as_ref().map_or_else(
                || self.error_here("vector elements must be numbers"),
                |minus| self.error_at(minus, "`-` in a vector must be followed by a number"),
            )
        })?;
        let position = minus
            .as_ref()
            .map_or(token.position, |token| token.position);
        vector_number(&token, minus.is_some(), position, index)
    }

    fn parse_count(&mut self, argument: &str) -> DevonResult<u64> {
        if self.at_kind(&TokenKind::Minus) {
            return Err(self.error_here(format!("{argument} rejects negative numbers")));
        }
        let token = self.take_number_token()?;
        let TokenKind::Number { text, is_float } = &token.kind else {
            return Err(self.error_at(&token, "expected an integer"));
        };
        if *is_float {
            return Err(self.error_at(&token, format!("{argument} requires an integer")));
        }
        let value = text
            .parse::<u64>()
            .map_err(|_| self.error_at(&token, format!("{argument} is outside the Int64 range")))?;
        if value > i64::MAX as u64 {
            return Err(self.error_at(&token, format!("{argument} is outside the Int64 range")));
        }
        Ok(value)
    }

    fn parse_boolean_value(&mut self) -> DevonResult<Value> {
        if self.consume_keyword(Keyword::True) {
            Ok(Value::Bool(true))
        } else if self.consume_keyword(Keyword::False) {
            Ok(Value::Bool(false))
        } else {
            Err(self.error_here("expected `true` or `false`"))
        }
    }

    /// Parses `index <name> on <table>.<column> metric <metric>` after
    /// `create hnsw`. `index` and `metric` are contextual identifiers;
    /// `on` is shared with join grammar and therefore reserved.
    fn parse_create_hnsw_index(&mut self) -> DevonResult<Statement> {
        self.expect_contextual_word("index")?;
        let name = self.take_identifier("HNSW index name")?;
        self.expect_keyword(Keyword::On, "on")?;
        let table = self.take_identifier("HNSW index table name")?;
        self.expect_kind(&TokenKind::Dot, "expected `.` in HNSW index column")?;
        let column = self.take_identifier("HNSW index column name")?;
        self.expect_contextual_word("metric")?;
        let metric = self.parse_metric()?;
        Ok(Statement::CreateHnswIndex {
            name,
            table,
            column,
            metric,
        })
    }

    fn expect_contextual_word(&mut self, word: &str) -> DevonResult<()> {
        let found = self.take_identifier(word)?;
        if found == word {
            Ok(())
        } else {
            Err(self.error_here(format!("expected `{word}`, found `{found}`")))
        }
    }

    fn parse_metric(&mut self) -> DevonResult<Metric> {
        if self.consume_keyword(Keyword::Cosine) {
            Ok(Metric::Cosine)
        } else if self.consume_keyword(Keyword::L2) {
            Ok(Metric::L2)
        } else {
            Err(self.expected_keyword_choice(&["cosine", "l2"]))
        }
    }

    fn parse_direction(&mut self) -> DevonResult<Direction> {
        if self.consume_keyword(Keyword::Out) {
            Ok(Direction::Out)
        } else if self.consume_keyword(Keyword::In) {
            Ok(Direction::In)
        } else if self.consume_keyword(Keyword::Both) {
            Ok(Direction::Both)
        } else {
            Err(self.expected_keyword_choice(&["out", "in", "both"]))
        }
    }

    fn parse_aggregate_function(&mut self) -> DevonResult<AggregateFunction> {
        let function = match self.current_kind() {
            Some(TokenKind::Keyword(Keyword::Count)) => AggregateFunction::Count,
            Some(TokenKind::Keyword(Keyword::Sum)) => AggregateFunction::Sum,
            Some(TokenKind::Keyword(Keyword::Min)) => AggregateFunction::Min,
            Some(TokenKind::Keyword(Keyword::Max)) => AggregateFunction::Max,
            Some(TokenKind::Keyword(Keyword::Avg)) => AggregateFunction::Avg,
            Some(TokenKind::Keyword(Keyword::PercentileCont)) => AggregateFunction::PercentileCont,
            _ => {
                return Err(self.expected_keyword_choice(&[
                    "count",
                    "sum",
                    "min",
                    "max",
                    "avg",
                    "percentile_cont",
                ]));
            }
        };
        self.advance();
        Ok(function)
    }

    fn take_comparison_operator(&mut self) -> Option<BinaryOp> {
        let operator = match self.current_kind()? {
            TokenKind::Eq => BinaryOp::Eq,
            TokenKind::Ne => BinaryOp::Ne,
            TokenKind::Lt => BinaryOp::Lt,
            TokenKind::Le => BinaryOp::Le,
            TokenKind::Gt => BinaryOp::Gt,
            TokenKind::Ge => BinaryOp::Ge,
            _ => return None,
        };
        self.advance();
        Some(operator)
    }

    fn comparison_operator_at_current(&self) -> bool {
        matches!(
            self.current_kind(),
            Some(
                TokenKind::Eq
                    | TokenKind::Ne
                    | TokenKind::Lt
                    | TokenKind::Le
                    | TokenKind::Gt
                    | TokenKind::Ge
            )
        )
    }

    fn take_identifier(&mut self, description: &str) -> DevonResult<String> {
        let token = self
            .current()
            .cloned()
            .ok_or_else(|| self.error_here(format!("expected {description}")))?;
        let TokenKind::Ident(identifier) = token.kind else {
            return Err(self.error_at(&token, format!("expected {description}")));
        };
        self.advance();
        Ok(identifier)
    }

    fn take_string(&mut self) -> DevonResult<String> {
        let token = self
            .current()
            .cloned()
            .ok_or_else(|| self.error_here("expected a string"))?;
        let TokenKind::Str(value) = token.kind else {
            return Err(self.error_at(&token, "expected a string"));
        };
        self.advance();
        Ok(value)
    }

    fn take_number_token(&mut self) -> DevonResult<Token> {
        let token = self
            .current()
            .cloned()
            .ok_or_else(|| self.error_here("expected a number"))?;
        if !matches!(token.kind, TokenKind::Number { .. }) {
            return Err(self.error_at(&token, "expected a number"));
        }
        self.advance();
        Ok(token)
    }

    fn expect_keyword(&mut self, keyword: Keyword, spelling: &str) -> DevonResult<()> {
        if self.consume_keyword(keyword) {
            return Ok(());
        }
        if let Some(TokenKind::Ident(identifier)) = self.current_kind()
            && identifier.eq_ignore_ascii_case(spelling)
        {
            return Err(self.wrong_case_error(identifier));
        }
        Err(self.error_here(format!("expected keyword `{spelling}`")))
    }

    fn consume_optional_keyword(&mut self, keyword: Keyword, spelling: &str) -> DevonResult<bool> {
        if self.consume_keyword(keyword) {
            return Ok(true);
        }
        if let Some(TokenKind::Ident(identifier)) = self.current_kind()
            && identifier.eq_ignore_ascii_case(spelling)
        {
            return Err(self.wrong_case_error(identifier));
        }
        Ok(false)
    }

    fn expected_keyword_choice(&self, choices: &[&str]) -> DevonError {
        if let Some(TokenKind::Ident(identifier)) = self.current_kind()
            && choices
                .iter()
                .any(|choice| identifier.eq_ignore_ascii_case(choice))
        {
            return self.wrong_case_error(identifier);
        }
        self.error_here(format!("expected one of {}", choices.join(", ")))
    }

    fn expect_kind(&mut self, kind: &TokenKind, problem: impl Into<String>) -> DevonResult<()> {
        if self.consume_kind(kind) {
            Ok(())
        } else {
            Err(self.error_here(problem))
        }
    }

    fn expect_end(&self) -> DevonResult<()> {
        if self.current().is_some() {
            Err(self.error_here("unexpected trailing token"))
        } else {
            Ok(())
        }
    }

    fn consume_keyword(&mut self, keyword: Keyword) -> bool {
        if matches!(self.current_kind(), Some(TokenKind::Keyword(found)) if *found == keyword) {
            self.advance();
            true
        } else {
            false
        }
    }

    fn consume_kind(&mut self, kind: &TokenKind) -> bool {
        if self.at_kind(kind) {
            self.advance();
            true
        } else {
            false
        }
    }

    /// Whether the token AFTER the current one is `(` — the lookahead that
    /// keeps contextual call-form words (`geo`) unambiguous with column
    /// references.
    fn next_is_lparen(&self) -> bool {
        self.tokens
            .get(self.index + 1)
            .is_some_and(|token| token.kind == TokenKind::LParen)
    }

    fn at_kind(&self, kind: &TokenKind) -> bool {
        self.current_kind() == Some(kind)
    }

    fn current(&self) -> Option<&Token> {
        self.tokens.get(self.index)
    }

    fn current_kind(&self) -> Option<&TokenKind> {
        self.current().map(|token| &token.kind)
    }

    fn advance(&mut self) {
        self.index += 1;
    }

    fn unknown_stage_error(&self) -> DevonError {
        if let Some(TokenKind::Ident(identifier)) = self.current_kind()
            && (is_stage_word(&identifier.to_ascii_lowercase())
                || identifier.eq_ignore_ascii_case("left"))
        {
            return self.wrong_case_error(identifier);
        }
        let suffix = self
            .current_kind()
            .and_then(|kind| match kind {
                TokenKind::Ident(identifier) => Some(suggestion_suffix(identifier, STAGE_WORDS)),
                _ => None,
            })
            .unwrap_or_default();
        self.error_here(format!(
            "unknown pipeline stage; v0 stages are expand, expand_rel, filter, project, sort, limit, aggregate, join, left join{suffix}"
        ))
    }

    fn unknown_top_level_error(&self, identifier: &str) -> DevonError {
        self.error_here(format!(
            "expected query source `nodes`/`knn` or statement `create`/`insert`/`upsert`/`copy`/`update`/`delete`{}",
            suggestion_suffix(identifier, TOP_LEVEL_WORDS)
        ))
    }

    fn wrong_case_error(&self, identifier: &str) -> DevonError {
        self.error_here(format!(
            "keyword `{identifier}` must be lowercase; did you mean `{}`?",
            identifier.to_ascii_lowercase()
        ))
    }

    fn error_here(&self, problem: impl Into<String>) -> DevonError {
        let problem = problem.into();
        if let Some(token) = self.current() {
            self.error_at(token, problem)
        } else {
            parse_error(self.eof_position, "<end of input>", problem)
        }
    }

    fn error_at(&self, token: &Token, problem: impl Into<String>) -> DevonError {
        parse_error(token.position, &token_text(&token.kind), problem.into())
    }
}

fn matching_query_semicolon(tokens: &[Token], start: usize) -> Option<usize> {
    let mut parenthesis_depth = 0_usize;
    let mut bracket_depth = 0_usize;
    for (index, token) in tokens.iter().enumerate().skip(start) {
        match token.kind {
            TokenKind::LParen => parenthesis_depth += 1,
            TokenKind::RParen if parenthesis_depth == 0 => return None,
            TokenKind::RParen => parenthesis_depth -= 1,
            TokenKind::LBracket => bracket_depth += 1,
            TokenKind::RBracket if bracket_depth == 0 => return None,
            TokenKind::RBracket => bracket_depth -= 1,
            TokenKind::Semicolon if parenthesis_depth == 0 && bracket_depth == 0 => {
                return Some(index);
            }
            _ => {}
        }
    }
    None
}

impl Subplans {
    fn parse_prologue(parser: &mut Parser, expression_nesting: usize) -> DevonResult<Self> {
        let mut subplans = Self {
            definitions: Vec::new(),
            by_name: HashMap::new(),
            source_text: parser.source_text.clone(),
            sources: parser.sources.clone(),
        };
        while parser.consume_keyword(Keyword::Let) {
            let name_token = parser
                .current()
                .cloned()
                .ok_or_else(|| parser.error_here("expected let subplan name"))?;
            let name = parser.take_identifier("let subplan name")?;
            let lookup = fold(&name).into_owned();
            if subplans.by_name.contains_key(&lookup) {
                return Err(
                    parser.error_at(&name_token, format!("duplicate let subplan name `{name}`"))
                );
            }
            parser.expect_kind(&TokenKind::Eq, "expected `=` after let subplan name")?;
            let start = parser.index;
            let end = matching_query_semicolon(&parser.tokens, start)
                .ok_or_else(|| parser.error_here("expected `;` after let subplan pipeline"))?;
            let eof_position = parser.tokens[end].position;
            let tokens = parser.tokens[start..end].to_vec();
            parser.index = end + 1;
            let index = subplans.definitions.len();
            subplans.by_name.insert(lookup, index);
            subplans.definitions.push(SubplanDefinition {
                name,
                name_token,
                tokens,
                eof_position,
                expression_nesting,
                resolving: false,
                operator: None,
            });
        }
        Ok(subplans)
    }

    fn resolve_all(&mut self) -> DevonResult<()> {
        for index in 0..self.definitions.len() {
            self.resolve_index(index)?;
        }
        Ok(())
    }

    fn resolve(&mut self, name: &str, token: &Token) -> DevonResult<Operator> {
        let Some(index) = self.by_name.get(fold(name).as_ref()).copied() else {
            let defined = self
                .definitions
                .iter()
                .map(|definition| format!("`{}`", definition.name))
                .collect::<Vec<_>>();
            let names = if defined.is_empty() {
                "<none>".to_owned()
            } else {
                defined.join(", ")
            };
            return Err(parse_error(
                token.position,
                &token_text(&token.kind),
                format!("unknown subplan name `{name}`; defined names: {names}"),
            ));
        };
        self.resolve_index(index)
    }

    fn resolve_index(&mut self, index: usize) -> DevonResult<Operator> {
        if let Some(operator) = &self.definitions[index].operator {
            return Ok(operator.clone());
        }
        if self.definitions[index].resolving {
            let definition = &self.definitions[index];
            return Err(parse_error(
                definition.name_token.position,
                &token_text(&definition.name_token.kind),
                format!(
                    "cyclic let subplan reference involving `{}`",
                    definition.name
                ),
            ));
        }

        self.definitions[index].resolving = true;
        let mut parser = Parser {
            tokens: self.definitions[index].tokens.clone(),
            index: 0,
            eof_position: self.definitions[index].eof_position,
            source_text: self.source_text.clone(),
            sources: self.sources.clone(),
        };
        let expression_nesting = self.definitions[index].expression_nesting;
        let parsed = parser
            .parse_pipeline(self, expression_nesting)
            .and_then(|(operator, _)| parser.expect_end().map(|()| operator));
        self.definitions[index].resolving = false;
        let operator = parsed?;
        self.definitions[index].operator = Some(operator.clone());
        Ok(operator)
    }

    fn reject_binding_collisions(&self, main: &Operator) -> DevonResult<()> {
        let mut bindings = HashMap::new();
        collect_declared_bindings(main, &mut bindings);
        for definition in &self.definitions {
            if let Some(operator) = &definition.operator {
                collect_declared_bindings(operator, &mut bindings);
            }
        }
        for definition in &self.definitions {
            if let Some(binding) = bindings.get(fold(&definition.name).as_ref()) {
                return Err(parse_error(
                    definition.name_token.position,
                    &token_text(&definition.name_token.kind),
                    format!(
                        "let subplan name `{}` collides with binding `{binding}`",
                        definition.name
                    ),
                ));
            }
        }
        Ok(())
    }
}

fn decompose_join_keys(
    expression: Expr,
    left: &Operator,
    right: &Operator,
) -> Result<Vec<JoinKey>, &'static str> {
    let left_bindings = referenceable_bindings(left);
    let right_bindings = referenceable_bindings(right);
    let mut keys = Vec::new();
    push_join_keys(expression, &left_bindings, &right_bindings, &mut keys)?;
    Ok(keys)
}

fn push_join_keys(
    expression: Expr,
    left_bindings: &HashSet<String>,
    right_bindings: &HashSet<String>,
    keys: &mut Vec<JoinKey>,
) -> Result<(), &'static str> {
    match expression {
        Expr::Binary {
            op: BinaryOp::And,
            left,
            right,
        } => {
            push_join_keys(*left, left_bindings, right_bindings, keys)?;
            push_join_keys(*right, left_bindings, right_bindings, keys)
        }
        Expr::Binary {
            op: BinaryOp::Eq,
            left,
            right,
        } => {
            let (left, right) = normalize_join_key(*left, *right, left_bindings, right_bindings);
            keys.push(JoinKey { left, right });
            Ok(())
        }
        _ => Err("non-equi predicates belong in a following `filter` stage"),
    }
}

fn normalize_join_key(
    left: Expr,
    right: Expr,
    left_bindings: &HashSet<String>,
    right_bindings: &HashSet<String>,
) -> (Expr, Expr) {
    let left_refs = expression_bindings(&left);
    let right_refs = expression_bindings(&right);
    let normal = left_refs.is_subset(left_bindings) && right_refs.is_subset(right_bindings);
    let swapped = left_refs.is_subset(right_bindings) && right_refs.is_subset(left_bindings);
    if swapped && !normal {
        (right, left)
    } else {
        (left, right)
    }
}

fn expression_bindings(expression: &Expr) -> HashSet<String> {
    let mut bindings = HashSet::new();
    collect_expression_bindings(expression, &mut bindings);
    bindings
}

fn collect_expression_bindings(expression: &Expr, bindings: &mut HashSet<String>) {
    match expression {
        Expr::Col(reference) => {
            if let Some((binding, _)) = reference.split_once('.') {
                bindings.insert(fold(binding).into_owned());
            }
        }
        Expr::ClassOf(binding) | Expr::ScoreOf(binding) => {
            bindings.insert(fold(binding).into_owned());
        }
        Expr::Binary { left, right, .. } | Expr::Distance { left, right, .. } => {
            collect_expression_bindings(left, bindings);
            collect_expression_bindings(right, bindings);
        }
        Expr::If {
            cond,
            then_expr,
            else_expr,
        } => {
            collect_expression_bindings(cond, bindings);
            collect_expression_bindings(then_expr, bindings);
            collect_expression_bindings(else_expr, bindings);
        }
        Expr::Coalesce(expressions) | Expr::Least(expressions) | Expr::Greatest(expressions) => {
            for expression in expressions {
                collect_expression_bindings(expression, bindings);
            }
        }
        Expr::DateTrunc { value, .. } | Expr::Round { value, .. } => {
            collect_expression_bindings(value, bindings);
        }
        Expr::DateAdd { value, amount, .. } => {
            collect_expression_bindings(value, bindings);
            collect_expression_bindings(amount, bindings);
        }
        Expr::RoundDiv {
            numerator,
            denominator,
            ..
        } => {
            collect_expression_bindings(numerator, bindings);
            collect_expression_bindings(denominator, bindings);
        }
        Expr::Scalar { plan } => {
            let mut embedded = HashSet::new();
            collect_operator_expression_bindings(plan, &mut embedded);
            let declared = referenceable_bindings(plan);
            embedded.retain(|binding| !declared.contains(binding));
            bindings.extend(embedded);
        }
        Expr::Not(operand) => collect_expression_bindings(operand, bindings),
        Expr::Lit(_) => {}
    }
}

fn collect_operator_expression_bindings(operator: &Operator, bindings: &mut HashSet<String>) {
    collect_local_operator_expression_bindings(operator, bindings);
    match operator {
        Operator::ExpandRel { input, .. }
        | Operator::Expand { input, .. }
        | Operator::Filter { input, .. }
        | Operator::Project { input, .. }
        | Operator::Sort { input, .. }
        | Operator::Limit { input, .. }
        | Operator::Aggregate { input, .. } => {
            collect_operator_expression_bindings(input, bindings);
        }
        Operator::HashJoin { left, right, .. } => {
            collect_operator_expression_bindings(left, bindings);
            collect_operator_expression_bindings(right, bindings);
        }
        Operator::ScanNodes { .. }
        | Operator::ScanInterface { .. }
        | Operator::KnnScan { .. }
        | Operator::TextScan { .. }
        | Operator::WithinScan { .. } => {}
    }
}

fn collect_local_operator_expression_bindings(operator: &Operator, bindings: &mut HashSet<String>) {
    match operator {
        Operator::Filter { predicate, .. } => {
            collect_expression_bindings(predicate, bindings);
        }
        Operator::Project { exprs, .. } => {
            for item in exprs {
                collect_expression_bindings(&item.expr, bindings);
            }
        }
        Operator::Sort { keys, .. } => {
            for key in keys {
                collect_expression_bindings(&key.expr, bindings);
            }
        }
        Operator::Aggregate { group_by, aggs, .. } => {
            for expression in group_by {
                collect_expression_bindings(expression, bindings);
            }
            for aggregate in aggs {
                collect_expression_bindings(&aggregate.expr, bindings);
            }
        }
        Operator::HashJoin { on, .. } => {
            for key in on {
                collect_expression_bindings(&key.left, bindings);
                collect_expression_bindings(&key.right, bindings);
            }
        }
        Operator::ScanNodes { .. }
        | Operator::ScanInterface { .. }
        | Operator::KnnScan { .. }
        | Operator::TextScan { .. }
        | Operator::WithinScan { .. }
        | Operator::ExpandRel { .. }
        | Operator::Expand { .. }
        | Operator::Limit { .. } => {}
    }
}

fn referenceable_bindings(operator: &Operator) -> HashSet<String> {
    match operator {
        Operator::TextScan { binding, .. }
        | Operator::ScanNodes { binding, .. }
        | Operator::ScanInterface { binding, .. } => HashSet::from([fold(binding).into_owned()]),
        Operator::KnnScan { table, .. } | Operator::WithinScan { table, .. } => {
            HashSet::from([fold(table).into_owned()])
        }
        Operator::ExpandRel {
            binding,
            rel_binding,
            input,
            ..
        } => {
            let mut bindings = referenceable_bindings(input);
            bindings.insert(fold(binding).into_owned());
            bindings.insert(fold(rel_binding).into_owned());
            bindings
        }
        Operator::Expand { binding, input, .. } => {
            let mut bindings = referenceable_bindings(input);
            bindings.insert(fold(binding).into_owned());
            bindings
        }
        Operator::Filter { input, .. }
        | Operator::Sort { input, .. }
        | Operator::Limit { input, .. } => referenceable_bindings(input),
        Operator::Project { exprs, input } => {
            let mut bindings = score_bindings(input);
            for item in exprs {
                insert_reference_binding(&item.alias, &mut bindings);
                if let Expr::Col(reference) = &item.expr {
                    insert_reference_binding(reference, &mut bindings);
                }
            }
            bindings
        }
        Operator::Aggregate { group_by, aggs, .. } => {
            let mut bindings = HashSet::new();
            for expression in group_by {
                if let Ok(name) = print_expression(expression) {
                    insert_reference_binding(&name, &mut bindings);
                }
            }
            for aggregate in aggs {
                insert_reference_binding(&aggregate.alias, &mut bindings);
            }
            bindings
        }
        Operator::HashJoin { left, right, .. } => {
            let mut bindings = referenceable_bindings(left);
            bindings.extend(referenceable_bindings(right));
            bindings
        }
    }
}

fn score_bindings(operator: &Operator) -> HashSet<String> {
    match operator {
        Operator::TextScan { binding, .. } => HashSet::from([fold(binding).into_owned()]),
        Operator::Project { input, .. }
        | Operator::Filter { input, .. }
        | Operator::Sort { input, .. }
        | Operator::Limit { input, .. }
        | Operator::Expand { input, .. }
        | Operator::ExpandRel { input, .. } => score_bindings(input),
        Operator::HashJoin { left, right, .. } => {
            let mut bindings = score_bindings(left);
            bindings.extend(score_bindings(right));
            bindings
        }
        _ => HashSet::new(),
    }
}

fn insert_reference_binding(reference: &str, bindings: &mut HashSet<String>) {
    if let Some((binding, _)) = reference.split_once('.') {
        bindings.insert(fold(binding).into_owned());
    }
}

fn collect_declared_bindings(operator: &Operator, bindings: &mut HashMap<String, String>) {
    match operator {
        Operator::ExpandRel {
            binding,
            rel_binding,
            ..
        } => {
            bindings.insert(fold(binding).into_owned(), binding.clone());
            bindings.insert(fold(rel_binding).into_owned(), rel_binding.clone());
        }

        Operator::TextScan { binding, .. }
        | Operator::ScanNodes { binding, .. }
        | Operator::ScanInterface { binding, .. }
        | Operator::Expand { binding, .. } => {
            bindings.insert(fold(binding).into_owned(), binding.clone());
        }
        Operator::KnnScan { table, .. } | Operator::WithinScan { table, .. } => {
            bindings.insert(fold(table).into_owned(), table.clone());
        }
        _ => {}
    }
    match operator {
        Operator::ExpandRel { input, .. }
        | Operator::Expand { input, .. }
        | Operator::Filter { input, .. }
        | Operator::Project { input, .. }
        | Operator::Sort { input, .. }
        | Operator::Limit { input, .. }
        | Operator::Aggregate { input, .. } => collect_declared_bindings(input, bindings),
        Operator::HashJoin { left, right, .. } => {
            collect_declared_bindings(left, bindings);
            collect_declared_bindings(right, bindings);
        }
        Operator::ScanNodes { .. }
        | Operator::ScanInterface { .. }
        | Operator::KnnScan { .. }
        | Operator::TextScan { .. }
        | Operator::WithinScan { .. } => {}
    }
}

fn class_clause_order(clause: &str) -> Option<u8> {
    match clause {
        "display" => Some(1),
        "plural" => Some(2),
        "label" => Some(3),
        "summary" => Some(4),
        "color" => Some(5),
        "description" => Some(6),
        "verb" => Some(7),
        "inverse" => Some(8),
        "implements" => Some(9),
        _ => None,
    }
}

fn is_scalar_v2_literal(identifier: &str) -> bool {
    matches!(identifier, "timestamp" | "bytes" | "decimal" | "json")
}

fn is_a6_call_word(identifier: &str) -> bool {
    matches!(
        identifier,
        "if" | "coalesce"
            | "least"
            | "greatest"
            | "date_trunc"
            | "date_add"
            | "round"
            | "round_div"
            | "scalar"
    )
}

fn parse_timestamp_micros(text: &str) -> Result<i64, String> {
    let Some(body) = text.strip_suffix('Z') else {
        return Err(
            "timestamp literals must use UTC suffix `Z`; non-UTC offsets are not allowed"
                .to_owned(),
        );
    };
    let bytes = body.as_bytes();
    let (year, year_end) = parse_timestamp_year(bytes)?;
    validate_timestamp_layout(bytes, year_end)?;
    let month = timestamp_component(bytes, year_end + 1, year_end + 3, "month")?;
    let day = timestamp_component(bytes, year_end + 4, year_end + 6, "day")?;
    let hour = timestamp_component(bytes, year_end + 7, year_end + 9, "hour")?;
    let minute = timestamp_component(bytes, year_end + 10, year_end + 12, "minute")?;
    let second = timestamp_component(bytes, year_end + 13, year_end + 15, "second")?;
    let micros = parse_timestamp_fraction(bytes, year_end + 15)?;

    validate_timestamp_components(year, month, day, hour, minute, second)?;
    let days = i128::from(days_from_civil(year, month, day));
    let total = days * 86_400_000_000
        + i128::from(hour) * 3_600_000_000
        + i128::from(minute) * 60_000_000
        + i128::from(second) * 1_000_000
        + i128::from(micros);
    i64::try_from(total)
        .map_err(|_| "timestamp literal is outside the epoch-microseconds range".to_owned())
}

fn parse_timestamp_year(bytes: &[u8]) -> Result<(i64, usize), String> {
    match bytes.first() {
        Some(sign @ (b'+' | b'-')) => {
            if bytes.get(7) != Some(&b'-') {
                return Err("timestamp year sign must be followed by exactly six digits".to_owned());
            }
            let magnitude = i64::from(timestamp_component(bytes, 1, 7, "year")?);
            let year = if *sign == b'-' { -magnitude } else { magnitude };
            if year == 0 && *sign == b'-' {
                return Err("timestamp expanded year zero must use `+000000`".to_owned());
            }
            if (1..=9999).contains(&year) {
                return Err("timestamp years 0001 through 9999 must not carry a sign".to_owned());
            }
            Ok((year, 7))
        }
        _ => {
            let year = i64::from(timestamp_component(bytes, 0, 4, "year")?);
            if year == 0 {
                return Err("timestamp year zero must use expanded form `+000000`".to_owned());
            }
            Ok((year, 4))
        }
    }
}

fn validate_timestamp_layout(bytes: &[u8], year_end: usize) -> Result<(), String> {
    let time_end = year_end + 15;
    let separators = [
        (year_end, b'-'),
        (year_end + 3, b'-'),
        (year_end + 6, b'T'),
        (year_end + 9, b':'),
        (year_end + 12, b':'),
    ];
    let invalid = bytes.len() < time_end
        || separators
            .iter()
            .any(|(index, expected)| bytes.get(*index) != Some(expected))
        || (bytes.len() > time_end && bytes.get(time_end) != Some(&b'.'));
    if invalid {
        return Err(
            "timestamp literal must have form `YYYY-MM-DDThh:mm:ss[.ffffff]Z` or `[+-]YYYYYY-MM-DDThh:mm:ss[.ffffff]Z` in UTC"
                .to_owned(),
        );
    }
    Ok(())
}

fn parse_timestamp_fraction(bytes: &[u8], time_end: usize) -> Result<u32, String> {
    if bytes.len() == time_end {
        return Ok(0);
    }
    let fraction = &bytes[time_end + 1..];
    if !(1..=6).contains(&fraction.len()) {
        return Err("timestamp fractional seconds allow 1 through 6 digits".to_owned());
    }
    if !fraction.iter().all(u8::is_ascii_digit) {
        return Err("timestamp fractional seconds must contain only decimal digits".to_owned());
    }
    let micros = fraction
        .iter()
        .fold(0, |value, digit| value * 10 + u32::from(*digit - b'0'));
    Ok(micros * 10_u32.pow(6 - fraction.len() as u32))
}

fn timestamp_component(bytes: &[u8], start: usize, end: usize, field: &str) -> Result<u32, String> {
    let Some(component) = bytes.get(start..end) else {
        return Err(format!("timestamp {field} is missing"));
    };
    if !component.iter().all(u8::is_ascii_digit) {
        return Err(format!(
            "timestamp {field} must contain only decimal digits"
        ));
    }
    Ok(component
        .iter()
        .fold(0, |value, digit| value * 10 + u32::from(*digit - b'0')))
}

fn validate_timestamp_components(
    year: i64,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    second: u32,
) -> Result<(), String> {
    if !(1..=12).contains(&month) {
        return Err(format!("timestamp month {month} is out of range 1..=12"));
    }
    let month_days = days_in_month(year, month);
    if !(1..=month_days).contains(&day) {
        return Err(format!(
            "timestamp day {day} is out of range 1..={month_days} for month {month}"
        ));
    }
    if hour > 23 {
        return Err(format!("timestamp hour {hour} is out of range 0..=23"));
    }
    if minute > 59 {
        return Err(format!("timestamp minute {minute} is out of range 0..=59"));
    }
    if second > 59 {
        return Err(format!("timestamp second {second} is out of range 0..=59"));
    }
    Ok(())
}

fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => 31,
    }
}

fn is_leap_year(year: i64) -> bool {
    year.rem_euclid(4) == 0 && (year.rem_euclid(100) != 0 || year.rem_euclid(400) == 0)
}

/// `(year, month, day)` -> days since the Unix epoch, the exact inverse of
/// `devondb-types`' timestamp printer (`civil_from_days`).
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let adjusted_year = year - i64::from(month <= 2);
    let era = adjusted_year.div_euclid(400);
    let year_of_era = adjusted_year.rem_euclid(400);
    let shifted_month = i64::from(month) + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn parse_lowercase_hex(text: &str) -> Result<Vec<u8>, String> {
    if !text.len().is_multiple_of(2) {
        return Err("bytes literal requires an even-length hex string".to_owned());
    }
    if text.bytes().any(|byte| matches!(byte, b'A'..=b'F')) {
        return Err("bytes literal hex must be lowercase".to_owned());
    }
    if !text
        .bytes()
        .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err("bytes literal must contain only lowercase hex digits".to_owned());
    }

    text.as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            let high = hex_nibble(pair[0])
                .ok_or_else(|| "bytes literal contains invalid hex".to_owned())?;
            let low = hex_nibble(pair[1])
                .ok_or_else(|| "bytes literal contains invalid hex".to_owned())?;
            Ok((high << 4) | low)
        })
        .collect()
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

fn canonical_json_text(text: &str) -> Result<String, String> {
    crate::expr::canonical_json_preserving_key_order(text)
        .map_err(|error| format!("json literal must contain valid JSON: {error}"))
}

fn number_value(token: &Token, negative: bool, position: usize) -> DevonResult<Value> {
    let TokenKind::Number { text, is_float } = &token.kind else {
        return Err(parse_error(
            token.position,
            &token_text(&token.kind),
            "expected a number",
        ));
    };
    if *is_float {
        let signed = signed_number_text(text, negative);
        let value = signed
            .parse::<f64>()
            .map_err(|_| parse_error(position, &signed, "invalid Float64 literal"))?;
        if !value.is_finite() {
            return Err(parse_error(
                position,
                &signed,
                "Float64 literal must be finite",
            ));
        }
        return Ok(Value::Float64(value));
    }
    parse_integer(position, text, negative).map(Value::Int64)
}

fn vector_number(token: &Token, negative: bool, position: usize, index: usize) -> DevonResult<f32> {
    let TokenKind::Number { text, is_float } = &token.kind else {
        return Err(parse_error(
            token.position,
            &token_text(&token.kind),
            "expected a number",
        ));
    };
    if !is_float {
        return parse_integer(position, text, negative).map(|value| value as f32);
    }
    let signed = signed_number_text(text, negative);
    let value = signed
        .parse::<f32>()
        .map_err(|_| parse_error(position, &signed, "invalid f32 vector element"))?;
    if !value.is_finite() {
        return Err(parse_error(
            position,
            &signed,
            format!("vector element {index} must be a finite f32 number"),
        ));
    }
    Ok(value)
}

fn parse_integer(position: usize, text: &str, negative: bool) -> DevonResult<i64> {
    let signed = signed_number_text(text, negative);
    let magnitude = text.parse::<u64>().map_err(|_| {
        parse_error(
            position,
            &signed,
            "integer literal is outside the Int64 range",
        )
    })?;
    if negative {
        if magnitude == (i64::MAX as u64) + 1 {
            return Ok(i64::MIN);
        }
        if magnitude <= i64::MAX as u64 {
            return Ok(-(magnitude as i64));
        }
    } else if magnitude <= i64::MAX as u64 {
        return Ok(magnitude as i64);
    }
    Err(parse_error(
        position,
        &signed,
        "integer literal is outside the Int64 range",
    ))
}

fn signed_number_text(text: &str, negative: bool) -> String {
    if negative {
        format!("-{text}")
    } else {
        text.to_owned()
    }
}

fn binary(op: BinaryOp, left: Expr, right: Expr) -> Expr {
    Expr::Binary {
        op,
        left: Box::new(left),
        right: Box::new(right),
    }
}

fn parse_error(position: usize, offending: &str, problem: impl AsRef<str>) -> DevonError {
    DevonError::InvalidArgument {
        context: format!(
            "DevonPlan text parse error at position {position}: {}; offending token {offending:?}",
            problem.as_ref()
        ),
    }
}

fn is_top_level_word(word: &str) -> bool {
    TOP_LEVEL_WORDS.contains(&word)
}

fn is_stage_word(word: &str) -> bool {
    STAGE_WORDS.contains(&word)
}

fn token_text(kind: &TokenKind) -> String {
    match kind {
        TokenKind::Pipe => "|".into(),
        TokenKind::LParen => "(".into(),
        TokenKind::RParen => ")".into(),
        TokenKind::LBracket => "[".into(),
        TokenKind::RBracket => "]".into(),
        TokenKind::Comma => ",".into(),
        TokenKind::Semicolon => ";".into(),
        TokenKind::Dot => ".".into(),
        TokenKind::Arrow => "->".into(),
        TokenKind::Eq => "=".into(),
        TokenKind::Ne => "!=".into(),
        TokenKind::Lt => "<".into(),
        TokenKind::Le => "<=".into(),
        TokenKind::Gt => ">".into(),
        TokenKind::Ge => ">=".into(),
        TokenKind::Plus => "+".into(),
        TokenKind::Minus => "-".into(),
        TokenKind::Star => "*".into(),
        TokenKind::Slash => "/".into(),
        TokenKind::Keyword(keyword) => keyword_text(*keyword).into(),
        TokenKind::Ident(identifier) => identifier.clone(),
        TokenKind::Number { text, .. } => text.clone(),
        TokenKind::Str(value) => format!("{value:?}"),
    }
}

fn keyword_text(keyword: Keyword) -> &'static str {
    match keyword {
        Keyword::And => "and",
        Keyword::Or => "or",
        Keyword::Not => "not",
        Keyword::True => "true",
        Keyword::False => "false",
        Keyword::Null => "null",
        Keyword::As => "as",
        Keyword::By => "by",
        Keyword::Asc => "asc",
        Keyword::Desc => "desc",
        Keyword::Out => "out",
        Keyword::In => "in",
        Keyword::Both => "both",
        Keyword::From => "from",
        Keyword::To => "to",
        Keyword::Nodes => "nodes",
        Keyword::Expand => "expand",
        Keyword::Filter => "filter",
        Keyword::Project => "project",
        Keyword::Sort => "sort",
        Keyword::Limit => "limit",
        Keyword::Offset => "offset",
        Keyword::Aggregate => "aggregate",
        Keyword::Knn => "knn",
        Keyword::Distance => "distance",
        Keyword::If => "if",
        Keyword::Coalesce => "coalesce",
        Keyword::Least => "least",
        Keyword::Greatest => "greatest",
        Keyword::DateTrunc => "date_trunc",
        Keyword::DateAdd => "date_add",
        Keyword::Round => "round",
        Keyword::RoundDiv => "round_div",
        Keyword::Scalar => "scalar",
        Keyword::Cosine => "cosine",
        Keyword::L2 => "l2",
        Keyword::Count => "count",
        Keyword::Sum => "sum",
        Keyword::Min => "min",
        Keyword::Max => "max",
        Keyword::Avg => "avg",
        Keyword::PercentileCont => "percentile_cont",
        Keyword::Create => "create",
        Keyword::Insert => "insert",
        Keyword::Upsert => "upsert",
        Keyword::Copy => "copy",
        Keyword::Update => "update",
        Keyword::Set => "set",
        Keyword::Delete => "delete",
        Keyword::Where => "where",
        Keyword::Into => "into",
        Keyword::Values => "values",
        Keyword::Node => "node",
        Keyword::Rel => "rel",
        Keyword::Table => "table",
        Keyword::Primary => "primary",
        Keyword::Key => "key",
        Keyword::Let => "let",
        Keyword::Join => "join",
        Keyword::On => "on",
    }
}

#[cfg(test)]
mod tests {
    use super::{Parsed, parse, parse_value_literal};
    use crate::expr::{BinaryOp, DateTruncUnit, Expr};
    use crate::ops::{AggregateFunction, Direction, JoinType, Operator, PLAN_VERSION, SortOrder};
    use crate::statement::Statement;
    use crate::text::printer::{print_plan, print_statement};
    use devondb_types::{DevonError, logical_type::LogicalType, value::Value};

    fn query(input: &str) -> crate::ops::Plan {
        let Parsed::Query(plan) = parse(input).unwrap() else {
            panic!("expected query");
        };
        plan
    }

    /// The geo literal (`docs/GEO.md` §5, PLAN_IR § Literals): parse both
    /// component spellings, print canonically, and round-trip exactly.
    #[test]
    fn geo_literal_parses_prints_and_round_trips() {
        let plan = query("nodes(T) as t | project geo(45.5, -122.625) as place");
        let printed = print_plan(&plan).unwrap();
        assert_eq!(
            printed,
            "nodes(T) as t | project geo(45.5, -122.625) as place"
        );
        assert_eq!(parse(&printed).unwrap(), Parsed::Query(plan));

        // Integer components and the two canonical foldings normalize at
        // construction; the printed form is float-spelled and stable.
        let normalized = query("nodes(T) as t | project geo(90, 55) as pole");
        assert_eq!(
            print_plan(&normalized).unwrap(),
            "nodes(T) as t | project geo(90.0, 0.0) as pole"
        );

        // `geo` stays an ordinary identifier when not followed by `(`.
        let column = query("nodes(T) as geo | project geo.x as x");
        assert!(print_plan(&column).unwrap().contains("geo.x"));

        // Out-of-range components are parse errors with the canonical rule.
        let error = parse("nodes(T) as t | project geo(95.0, 0.0) as p").unwrap_err();
        assert!(error.to_string().contains("latitude"), "{error}");
    }

    #[test]
    fn geo_point_type_name_parses_in_ddl() {
        let envelope = statement("create node table Place (id Int64 primary key, loc GeoPoint)");
        let printed = print_statement(&envelope).unwrap();
        assert!(printed.contains("loc GeoPoint"), "{printed}");
        assert_eq!(parse(&printed).unwrap(), Parsed::Statement(envelope));
    }

    #[test]
    fn scalar_v2_ddl_names_parse_print_and_preserve_syntax_only_parameters() {
        let text = concat!(
            "create node table Scalars (id Int64 primary key, happened Timestamp, ",
            "payload Bytes, amount Decimal(38, 38), document Json)"
        );
        let envelope = statement(text);
        let Statement::CreateNodeTable { columns, .. } = &envelope.stmt else {
            panic!("expected create node table");
        };
        assert_eq!(columns[1].ty, LogicalType::Timestamp);
        assert_eq!(columns[2].ty, LogicalType::Bytes);
        assert_eq!(
            columns[3].ty,
            LogicalType::Decimal {
                precision: 38,
                scale: 38,
            }
        );
        assert_eq!(columns[4].ty, LogicalType::Json);
        assert_eq!(print_statement(&envelope).unwrap(), text);
        assert_eq!(parse(text).unwrap(), Parsed::Statement(envelope));
    }

    #[test]
    fn scalar_v2_literals_parse_print_parse_and_plan_json_round_trip() {
        let cases = [
            (
                r#"nodes(T) as t | project timestamp("1970-01-01T00:00:00Z") as value"#,
                r#"nodes(T) as t | project timestamp("1970-01-01T00:00:00Z") as value"#,
            ),
            (
                r#"nodes(T) as t | project timestamp("2024-08-09T00:00:00.5Z") as value"#,
                r#"nodes(T) as t | project timestamp("2024-08-09T00:00:00.500000Z") as value"#,
            ),
            (
                r#"nodes(T) as t | project timestamp("1969-12-31T23:59:59Z") as value"#,
                r#"nodes(T) as t | project timestamp("1969-12-31T23:59:59Z") as value"#,
            ),
            (
                r#"nodes(T) as t | project bytes("") as value"#,
                r#"nodes(T) as t | project bytes("") as value"#,
            ),
            (
                r#"nodes(T) as t | project bytes("00ff1a") as value"#,
                r#"nodes(T) as t | project bytes("00ff1a") as value"#,
            ),
            (
                r#"nodes(T) as t | project decimal("99999999999999999999999999999999999999") as value"#,
                r#"nodes(T) as t | project decimal("99999999999999999999999999999999999999") as value"#,
            ),
            (
                r#"nodes(T) as t | project decimal("-99999999999999999999999999999999999999") as value"#,
                r#"nodes(T) as t | project decimal("-99999999999999999999999999999999999999") as value"#,
            ),
            (
                r#"nodes(T) as t | project json("{ \"k\": 1 }") as value"#,
                r#"nodes(T) as t | project json("{\"k\":1}") as value"#,
            ),
            (
                r#"nodes(T) as t | project json("[1, true, null]") as value"#,
                r#"nodes(T) as t | project json("[1,true,null]") as value"#,
            ),
            (
                r#"nodes(T) as t | project json("true") as value"#,
                r#"nodes(T) as t | project json("true") as value"#,
            ),
            (
                r#"nodes(T) as t | project json("\"devon\"") as value"#,
                r#"nodes(T) as t | project json("\"devon\"") as value"#,
            ),
        ];

        for (input, canonical) in cases {
            let plan = query(input);
            let printed = print_plan(&plan).unwrap();
            assert_eq!(printed, canonical);
            assert_eq!(parse(&printed).unwrap(), Parsed::Query(plan.clone()));
            let json = plan.to_json().unwrap();
            assert_eq!(crate::ops::Plan::from_json(&json).unwrap(), plan);
        }

        assert_eq!(
            parse_value_literal(r#"timestamp("1970-01-01T00:00:00Z")"#).unwrap(),
            Value::Timestamp(0)
        );
        assert_eq!(
            parse_value_literal(r#"timestamp("1969-12-31T23:59:59Z")"#).unwrap(),
            Value::Timestamp(-1_000_000)
        );
        assert_eq!(
            parse_value_literal(r#"timestamp("2024-08-09T00:00:00.5Z")"#).unwrap(),
            Value::Timestamp(1_723_161_600_500_000)
        );
        let seven_digits = error_context(
            r#"nodes(T) as t | project timestamp("2024-08-09T00:00:00.1234567Z") as value"#,
        );
        assert!(
            seven_digits.contains("1 through 6 digits"),
            "{seven_digits}"
        );
    }

    #[test]
    fn decimal_type_bounds_reject_out_of_range_precision_and_scale() {
        for (text, precision, scale) in [("Decimal(1, 0)", 1_u8, 0_u8), ("Decimal(38, 38)", 38, 38)]
        {
            let text = format!("create node table T (amount {text})");
            let envelope = statement(&text);
            let Statement::CreateNodeTable { columns, .. } = &envelope.stmt else {
                panic!("expected create node table");
            };
            assert_eq!(columns[0].ty, LogicalType::Decimal { precision, scale });
            assert_eq!(print_statement(&envelope).unwrap(), text);
            assert_eq!(parse(&text).unwrap(), Parsed::Statement(envelope));
        }

        for text in ["Decimal(0, 0)", "Decimal(39, 0)", "Decimal(10, 11)"] {
            let error = error_context(&format!("create node table T (amount {text})"));
            assert!(error.contains("1 <= p <= 38, s <= p"), "{text}: {error}");
        }
    }

    #[test]
    fn json_literals_reject_numbers_that_cannot_preserve_value_fidelity() {
        for number in [
            "1e999",
            "-1e999",
            "1e-999",
            "123456789012345678901234567890",
        ] {
            let error = error_context(&format!(
                r#"nodes(T) as t | project json("{number}") as value"#
            ));
            assert!(
                error.contains("finite JSON number range"),
                "{number}: {error}"
            );
            assert!(error.contains(&format!("`{number}`")), "{number}: {error}");
        }

        // `-2e2` keeps its value (-200.0) through canonical serialization.
        let canonicalized = query(r#"nodes(T) as t | project json("[1.25,-2e2]") as value"#);
        assert_eq!(
            print_plan(&canonicalized).unwrap(),
            r#"nodes(T) as t | project json("[1.25,-200.0]") as value"#
        );

        let stable = query(r#"nodes(T) as t | project json("[1.25,-200.0,3,0]") as value"#);
        assert_eq!(
            print_plan(&stable).unwrap(),
            r#"nodes(T) as t | project json("[1.25,-200.0,3,0]") as value"#
        );
    }

    #[test]
    fn scalar_v2_insert_and_upsert_rows_round_trip_through_text_and_json() {
        let rows = concat!(
            "(timestamp(\"1970-01-01T00:00:00Z\"), bytes(\"\"), decimal(\"19.99\"), ",
            "json(\"{\\\"k\\\":1}\")), ",
            "(timestamp(\"1969-12-31T23:59:59Z\"), bytes(\"00ff1a\"), ",
            "decimal(\"-99999999999999999999999999999999999999\"), ",
            "json(\"[1,true,null]\"))"
        );
        for text in [
            format!("insert into Scalars values {rows}"),
            format!("upsert Scalars values {rows}"),
        ] {
            let envelope = statement(&text);
            assert_eq!(print_statement(&envelope).unwrap(), text);
            assert_eq!(parse(&text).unwrap(), Parsed::Statement(envelope.clone()));
            let json = envelope.to_json().unwrap();
            assert_eq!(
                crate::statement::StatementEnvelope::from_json(&json).unwrap(),
                envelope
            );
        }
    }

    #[test]
    fn upsert_pinned_spelling_and_error_gates() {
        let text = r#"upsert People values (1, "ada")"#;
        let envelope = statement(text);
        assert!(matches!(&envelope.stmt, Statement::UpsertNode { .. }));
        assert_eq!(print_statement(&envelope).unwrap(), text);
        assert_eq!(parse(text).unwrap(), Parsed::Statement(envelope));

        for input in ["upsert People values", "upsert People values ()"] {
            let context = error_context(input);
            assert!(context.contains("at least one"), "{context}");
            assert!(context.contains("position"), "{context}");
            assert!(context.contains("offending token"), "{context}");
        }
        let expression = error_context("upsert People values (1 + 2)");
        assert!(
            expression.contains("DML values must be literals, not expressions"),
            "{expression}"
        );
    }

    #[test]
    fn scalar_v2_literal_errors_name_the_binding_rule() {
        let cases = [
            (r#"insert into T values (bytes("abc"))"#, "even-length"),
            (r#"insert into T values (bytes("00FF"))"#, "lowercase"),
            (
                r#"insert into T values (timestamp("2026-01-01T00:00:00+00:00"))"#,
                "UTC",
            ),
            (
                r#"insert into T values (timestamp("2026-13-01T00:00:00Z"))"#,
                "month",
            ),
            (r#"insert into T values (decimal("1e5"))"#, "decimal"),
            (r#"insert into T values (json("{"))"#, "valid JSON"),
        ];
        for (input, required) in cases {
            let context = error_context(input);
            assert!(context.contains(required), "{input}: {context}");
            assert!(context.contains("position"), "{input}: {context}");
            assert!(context.contains("offending token"), "{input}: {context}");
        }
    }

    fn statement(input: &str) -> crate::statement::StatementEnvelope {
        let Parsed::Statement(envelope) = parse(input).unwrap() else {
            panic!("expected statement");
        };
        envelope
    }

    fn error_context(input: &str) -> String {
        let DevonError::InvalidArgument { context } = parse(input).unwrap_err() else {
            panic!("expected InvalidArgument");
        };
        context
    }

    #[test]
    fn spec_query_and_m2_pipeline_parse_and_print_canonically() {
        let examples = [
            "nodes(Person) as p | filter p.age > 30 | project p.name as name",
            concat!(
                "nodes(Person) as p | expand Knows out as f | filter f.age > 30 | ",
                "project p.name, f.name"
            ),
        ];
        for text in examples {
            let plan = query(text);
            assert_eq!(plan.v, PLAN_VERSION);
            assert_eq!(print_plan(&plan).unwrap(), text);
        }
    }

    #[test]
    fn join_text_parses_normalizes_and_round_trips_inner_and_left() {
        let cases = [
            (
                concat!(
                    "let recent = nodes(Commit) as c | filter c.pushed = false; ",
                    "nodes(Repo) as r | join recent on c.repo_pk = r.pk and r.tenant = c.tenant"
                ),
                JoinType::Inner,
            ),
            (
                concat!(
                    "let cities = nodes(City) as c; ",
                    "nodes(Person) as p | left join cities on c.id = p.city_id"
                ),
                JoinType::Left,
            ),
        ];

        for (input, expected_join) in cases {
            let plan = query(input);
            let Operator::HashJoin { join, on, .. } = &plan.plan else {
                panic!("expected hash join");
            };
            assert_eq!(*join, expected_join);
            assert_eq!(
                on[0].left,
                Expr::Col(if expected_join == JoinType::Inner {
                    "r.pk".into()
                } else {
                    "p.city_id".into()
                })
            );
            let canonical = print_plan(&plan).unwrap();
            assert_eq!(parse(&canonical).unwrap(), Parsed::Query(plan));
            let reparsed = query(&canonical);
            assert_eq!(print_plan(&reparsed).unwrap(), canonical);
        }
    }

    #[test]
    fn joins_support_forward_references_nested_chains_and_repeated_subplans() {
        let nested = concat!(
            "let owners = nodes(Person) as p; ",
            "let repos = nodes(Repo) as r | join owners on r.owner_id = p.id; ",
            "nodes(Org) as o | join repos on o.id = r.org_id"
        );
        let plan = query(nested);
        let canonical = print_plan(&plan).unwrap();
        assert!(canonical.starts_with(concat!(
            "let j1 = nodes(Repo) as r | join j2 on r.owner_id = p.id;\n",
            "let j2 = nodes(Person) as p;\n"
        )));
        assert_eq!(parse(&canonical).unwrap(), Parsed::Query(plan));

        let repeated = concat!(
            "let commits = nodes(Commit) as c; ",
            "nodes(Repo) as r | join commits on r.pk = c.repo_id | ",
            "join commits on r.mirror_pk = c.repo_id"
        );
        let plan = query(repeated);
        let text = print_plan(&plan).unwrap();
        assert_eq!(text.matches("nodes(Commit) as c").count(), 2);
        assert_eq!(parse(&text).unwrap(), Parsed::Query(plan));
    }

    #[test]
    fn join_parse_errors_obey_name_shape_and_position_contracts() {
        let unknown = concat!(
            "let known = nodes(City) as c; ",
            "nodes(Person) as p | join missing on p.id = c.id"
        );
        let context = error_context(unknown);
        assert!(
            context.contains("unknown subplan name `missing`"),
            "{context}"
        );
        assert!(context.contains("defined names: `known`"), "{context}");
        assert!(
            context.contains(&format!(
                "position {}",
                unknown.find("missing").unwrap() + 1
            )),
            "{context}"
        );

        let duplicate = concat!(
            "let right = nodes(City) as c; ",
            "let RIGHT = nodes(Place) as q; nodes(Person) as p"
        );
        let context = error_context(duplicate);
        assert!(
            context.contains("duplicate let subplan name `RIGHT`"),
            "{context}"
        );
        assert!(context.contains("position"), "{context}");
        assert!(context.contains("offending token \"RIGHT\""), "{context}");

        let collision = concat!(
            "let p = nodes(City) as c; ",
            "nodes(Person) as P | join p on P.id = c.id"
        );
        let context = error_context(collision);
        assert!(context.contains("collides with binding `P`"), "{context}");
        assert!(context.contains("position 5"), "{context}");

        for predicate in [
            "p.id > c.id",
            "p.id = c.id or p.name = c.name",
            "p.id = c.id and p.name != c.name",
        ] {
            let input = format!(
                "let city = nodes(City) as c; nodes(Person) as p | join city on {predicate}"
            );
            let context = error_context(&input);
            assert!(
                context.contains("non-equi predicates belong in a following `filter` stage"),
                "{context}"
            );
            assert!(context.contains("position"), "{context}");
            assert!(context.contains("offending token"), "{context}");
        }
    }

    #[test]
    fn sugar_defaults_normalize_to_shortest_spelling() {
        let text = concat!(
            "nodes(Person) as p | expand Knows out from p as f | ",
            "project f.age as `f.age` | sort f.age asc"
        );
        let canonical = concat!(
            "nodes(Person) as p | expand Knows out as f | ",
            "project f.age | sort f.age"
        );
        assert_eq!(print_plan(&query(text)).unwrap(), canonical);
    }

    #[test]
    fn expression_precedence_and_parentheses_build_expected_trees() {
        let left = query("nodes(T) as p | filter p.a - p.b - p.c = p.d or p.e and not p.f");
        let Operator::Filter { predicate, .. } = left.plan else {
            panic!("expected filter");
        };
        let Expr::Binary {
            op: BinaryOp::Or,
            left,
            right,
        } = predicate
        else {
            panic!("expected or");
        };
        assert!(matches!(
            *left,
            Expr::Binary {
                op: BinaryOp::Eq,
                ..
            }
        ));
        assert!(matches!(
            *right,
            Expr::Binary {
                op: BinaryOp::And,
                ..
            }
        ));

        let plan = query("nodes(T) as p | filter p.a - (p.b - p.c) = p.d");
        assert_eq!(
            print_plan(&plan).unwrap(),
            "nodes(T) as p | filter p.a - (p.b - p.c) = p.d"
        );
    }

    #[test]
    fn a6_primary_calls_parse_with_exact_shapes() {
        let plan = query(concat!(
            "nodes(T) as p | project if(p.active, coalesce(null, p.a, p.b), ",
            "greatest(least(p.c, p.d), p.e)) as selected, ",
            "date_trunc(\"day\", p.created_at) as day"
        ));
        let Operator::Project { exprs, .. } = &plan.plan else {
            panic!("expected project");
        };
        assert!(matches!(&exprs[0].expr, Expr::If { .. }));
        assert_eq!(
            &exprs[1].expr,
            &Expr::DateTrunc {
                unit: DateTruncUnit::Day,
                value: Box::new(Expr::Col("p.created_at".into())),
            }
        );
        let text = print_plan(&plan).unwrap();
        assert_eq!(parse(&text).unwrap(), Parsed::Query(plan));
    }

    #[test]
    fn a6_call_diagnostics_name_arity_unit_and_lowercase_spelling() {
        let arity_cases = [
            ("if(true, 1)", "if", "exactly 3", "got 2"),
            ("coalesce(1)", "coalesce", "at least 2", "got 1"),
            ("least()", "least", "at least 2", "got 0"),
            ("greatest(1)", "greatest", "at least 2", "got 1"),
            (
                "date_trunc(\"day\", p.created_at, p.updated_at)",
                "date_trunc",
                "exactly 2",
                "got 3",
            ),
        ];
        for (expression, name, expected, actual) in arity_cases {
            let context = error_context(&format!("nodes(T) as p | project {expression}"));
            assert!(context.contains("wrong arity"), "{context}");
            assert!(context.contains(name), "{context}");
            assert!(context.contains(expected), "{context}");
            assert!(context.contains(actual), "{context}");
            assert!(context.contains("position"), "{context}");
        }

        for expression in [
            "date_trunc(\"hour\", p.created_at)",
            "date_trunc(p.unit, p.created_at)",
        ] {
            let context = error_context(&format!("nodes(T) as p | project {expression}"));
            assert!(context.contains("accepted unit is `day`"), "{context}");
        }

        for (wrong, lowercase) in [
            ("If", "if"),
            ("Coalesce", "coalesce"),
            ("Least", "least"),
            ("Greatest", "greatest"),
            ("Date_Trunc", "date_trunc"),
        ] {
            let context = error_context(&format!("nodes(T) as p | project {wrong}(1, 2, 3)"));
            assert!(context.contains("must be lowercase"), "{context}");
            assert!(
                context.contains(&format!("did you mean `{lowercase}`")),
                "{context}"
            );
        }
    }

    #[test]
    fn a6b_forms_parse_print_and_reach_a_canonical_fixpoint() {
        let input = concat!(
            "nodes(T) as p | project date_add(\"day\", p.created_at, -6) as shifted, ",
            "round(p.cost, 0) as rounded, round_div(p.cost, p.baseline, 1) as ratio, ",
            "scalar(nodes(U) as u | filter u.owner = p.id | project u.value) as lookup | ",
            "aggregate percentile_cont(decimal(\"0.5\"), p.cost) as median"
        );
        let plan = query(input);
        let Operator::Aggregate {
            aggs,
            input: aggregate_input,
            ..
        } = &plan.plan
        else {
            panic!("expected aggregate");
        };
        assert_eq!(aggs[0].function, AggregateFunction::PercentileCont);
        let Operator::Project { exprs, .. } = aggregate_input.as_ref() else {
            panic!("expected project");
        };
        assert!(matches!(exprs[0].expr, Expr::DateAdd { .. }));
        assert!(matches!(exprs[1].expr, Expr::Round { places: 0, .. }));
        assert!(matches!(exprs[2].expr, Expr::RoundDiv { places: 1, .. }));
        assert!(matches!(exprs[3].expr, Expr::Scalar { .. }));

        let canonical = print_plan(&plan).unwrap();
        assert_eq!(canonical, input);
        let reparsed = query(&canonical);
        assert_eq!(reparsed, plan);
        assert_eq!(print_plan(&reparsed).unwrap(), canonical);
    }

    #[test]
    fn scalar_uses_the_matching_parenthesis_and_prints_nested_canonical_queries() {
        let input = concat!(
            "nodes(Outer) as o | project scalar(let matches = nodes(Inner) as i; ",
            "nodes(Other) as x | join matches on x.id = i.id | ",
            "project round(i.value, 0)) + o.delta as result"
        );
        let plan = query(input);
        let canonical = print_plan(&plan).unwrap();
        assert!(canonical.contains("scalar(let j1 = nodes(Inner) as i;\n"));
        assert!(canonical.ends_with(") + o.delta as result"));
        assert_eq!(parse(&canonical).unwrap(), Parsed::Query(plan.clone()));
        assert_eq!(print_plan(&query(&canonical)).unwrap(), canonical);
    }

    #[test]
    fn scalar_join_key_collection_removes_inner_bindings_and_keeps_correlation() {
        let plan = query(concat!(
            "let rights = nodes(Right) as r; nodes(Left) as l | join rights on ",
            "r.id = scalar(nodes(Inner) as i | filter i.owner = l.id | project i.id)"
        ));
        let Operator::HashJoin { on, .. } = &plan.plan else {
            panic!("expected hash join");
        };
        assert!(matches!(on[0].left, Expr::Scalar { .. }));
        assert_eq!(on[0].right, Expr::Col("r.id".into()));
        let canonical = print_plan(&plan).unwrap();
        assert_eq!(parse(&canonical).unwrap(), Parsed::Query(plan));
    }

    #[test]
    fn a6b_text_refuses_open_units_places_fractions_and_statements() {
        for expression in [
            "date_add(\"hour\", p.created_at, 1)",
            "date_add(p.unit, p.created_at, 1)",
        ] {
            let context = error_context(&format!("nodes(T) as p | project {expression}"));
            assert!(context.contains("accepted unit is `day`"), "{context}");
        }

        for expression in [
            "round(p.cost, -1)",
            "round(p.cost, 1.0)",
            "round(p.cost, p.places)",
            "round(p.cost, 1 + 1)",
            "round(p.cost, 39)",
            "round_div(p.cost, p.baseline, p.places)",
        ] {
            let context = error_context(&format!("nodes(T) as p | project {expression}"));
            assert!(context.contains("places"), "{context}");
            assert!(
                context.contains("integer") || context.contains("0..=38"),
                "{context}"
            );
        }

        for fraction in [
            "decimal(\"0.25\")",
            "decimal(\"0.50\")",
            "p.fraction",
            "0.5",
        ] {
            let input =
                format!("nodes(T) as p | aggregate percentile_cont({fraction}, p.cost) as median");
            let context = error_context(&input);
            assert!(context.contains("exactly `decimal(\"0.5\")`"), "{context}");
        }

        for statement in [
            "create node table X (id Int64 primary key)",
            "insert into X values (1)",
            "delete from X where id = 1",
            "pin \"x\" as nodes(X) as x",
        ] {
            let input = format!("nodes(T) as p | project scalar({statement})");
            let context = error_context(&input);
            assert!(
                context.contains("accepts a query, not a statement"),
                "{context}"
            );
        }
    }

    #[test]
    fn a6b_reserved_names_require_lowercase_calls_or_quoted_identifiers() {
        for (wrong, lowercase) in [
            ("Date_Add", "date_add"),
            ("Round", "round"),
            ("Round_Div", "round_div"),
            ("Scalar", "scalar"),
        ] {
            let context = error_context(&format!("nodes(T) as p | project {wrong}(p.x, 0)"));
            assert!(context.contains("must be lowercase"), "{context}");
            assert!(
                context.contains(&format!("did you mean `{lowercase}`")),
                "{context}"
            );
        }
        let percentile = error_context(concat!(
            "nodes(T) as p | aggregate ",
            "Percentile_Cont(decimal(\"0.5\"), p.cost) as median"
        ));
        assert!(percentile.contains("did you mean `percentile_cont`"));

        let quoted = concat!(
            "nodes(`date_add`) as `scalar` | project `scalar`.`round` as `round_div` | ",
            "aggregate count(`scalar`.`round`) as `percentile_cont`"
        );
        assert_eq!(print_plan(&query(quoted)).unwrap(), quoted);
    }

    #[test]
    fn scalar_expression_nesting_accepts_128_and_rejects_129() {
        fn nested_scalar_query(depth: usize) -> String {
            let mut expression = "outer.value".to_owned();
            for level in 0..depth {
                expression = format!("scalar(nodes(T{level}) as b{level} | project {expression})");
            }
            format!("nodes(Outer) as outer | project {expression} as result")
        }

        let accepted = nested_scalar_query(128);
        let plan = query(&accepted);
        let canonical = print_plan(&plan).unwrap();
        assert_eq!(parse(&canonical).unwrap(), Parsed::Query(plan));

        let rejected = error_context(&nested_scalar_query(129));
        assert!(
            rejected.contains("expression nesting exceeds 128 levels"),
            "{rejected}"
        );
    }

    #[test]
    fn binary_operator_chains_nest_and_reject_at_129_operators() {
        fn or_chain(terms: usize) -> String {
            let expression = (0..terms)
                .map(|index| format!("p.value{index} = {index}"))
                .collect::<Vec<_>>()
                .join(" or ");
            format!("nodes(T) as p | filter {expression}")
        }

        let accepted = query(&or_chain(128));
        let canonical = print_plan(&accepted).unwrap();

        // Typing and validation walk the 128-deep chain without exhausting
        // the default test-thread stack.
        let columns = (0..128)
            .map(|index| devondb_types::schema::Column {
                name: format!("value{index}"),
                ty: LogicalType::Int64,
                primary_key: index == 0,
            })
            .collect();
        let table = devondb_types::schema::NodeTableSchema::new("T".to_owned(), columns).unwrap();
        let node_tables = [table];
        let schema = crate::typing::SchemaInput::tables_only(&node_tables, &[]);
        crate::validate::validate_with_schema(&accepted, &schema).unwrap();

        assert_eq!(parse(&canonical).unwrap(), Parsed::Query(accepted));

        let rejected = error_context(&or_chain(129));
        assert!(
            rejected.contains("expression nesting exceeds 128 levels"),
            "{rejected}"
        );
    }

    #[test]
    fn negative_literals_include_i64_min_and_subtraction_of_negative() {
        let plan = query(concat!(
            "nodes(T) as p | filter p.x = -9223372036854775808 | ",
            "project p.x - -42"
        ));
        assert_eq!(
            print_plan(&plan).unwrap(),
            concat!(
                "nodes(T) as p | filter p.x = -9223372036854775808 | ",
                "project p.x - -42"
            )
        );
    }

    #[test]
    fn knn_aggregate_distance_and_all_sort_orders_parse() {
        let text = concat!(
            "knn(Document.embedding, [0.25, -0.5], 3, cosine) | ",
            "filter distance(d.embedding, [0.0, 1], l2) < 2.0 | ",
            "sort d.score desc, d.id | aggregate count(d.id) as n, ",
            "sum(d.score) as total by d.kind"
        );
        let plan = query(text);
        let Operator::Aggregate { input, .. } = &plan.plan else {
            panic!("expected aggregate");
        };
        let Operator::Sort { keys, .. } = input.as_ref() else {
            panic!("expected sort");
        };
        assert_eq!(keys[0].order, SortOrder::Desc);
        assert_eq!(keys[1].order, SortOrder::Asc);
        assert_eq!(
            print_plan(&plan).unwrap(),
            text.replace("[0.0, 1]", "[0, 1]")
        );
    }

    #[test]
    fn every_statement_example_parses_and_prints() {
        let examples = [
            "create node table Person (id Int64 primary key, name String, bio Vector(768))",
            "create rel table Knows from Person to Person (since Int64)",
            "create rel table Likes from Person to Person",
            "insert into Person values (1, \"Ada\", [0.1, 0.2]), (2, \"Grace\", [0.3, 0.4])",
            "insert rel into Knows values (1 -> 2, 1843), (2 -> 1, 1957)",
            "update Person set name = \"Ada\" where id = 7",
            "update Person set name = \"Ada\", age = 37, active = true where id = 7",
            "delete from Person where id = 7",
            "copy Person from \"/tmp/people.csv\"",
            "copy Person from \"/tmp/people.csv\" sort by id",
            "create interface Nameable (name String)",
            concat!(
                "create class for Person (display \"Person\", plural \"people\", label name, ",
                "summary (name, role), color \"#7aa2ff\", description \"a human\", ",
                "implements (Nameable))"
            ),
            "create class for Knows (verb \"knows\", inverse \"is known by\")",
            "create class for Person",
            concat!(
                "pin \"ada friends\" as nodes(Person) as person | ",
                "filter person.name = \"ada\""
            ),
            "unpin \"ada friends\"",
        ];
        for text in examples {
            let envelope = statement(text);
            assert_eq!(envelope.v, PLAN_VERSION);
            assert_eq!(print_statement(&envelope).unwrap(), text);
        }
    }

    #[test]
    fn update_and_delete_parse_print_parse_and_json_round_trip() {
        for (text, set_items) in [
            ("update Person set name = \"Ada\" where id = 7", Some(1)),
            (
                "update Person set name = \"Ada\", age = 37, active = true where id = 7",
                Some(3),
            ),
            ("delete from Person where id = 7", None),
        ] {
            let envelope = statement(text);
            match (&envelope.stmt, set_items) {
                (Statement::UpdateNode { set, .. }, Some(expected)) => {
                    assert_eq!(set.len(), expected)
                }
                (Statement::DeleteNode { .. }, None) => {}
                _ => panic!("unexpected DML statement for {text}"),
            }

            let printed = print_statement(&envelope).unwrap();
            assert_eq!(printed, text);
            assert_eq!(
                parse(&printed).unwrap(),
                Parsed::Statement(envelope.clone())
            );
            let json = envelope.to_json().unwrap();
            assert_eq!(
                crate::statement::StatementEnvelope::from_json(&json).unwrap(),
                envelope
            );
        }
    }

    #[test]
    fn update_and_delete_reject_non_pk_addressed_or_malformed_forms() {
        let required =
            "DML requires `where <pk> = <literal>`; predicate-driven bulk DML is a separate lane";
        for input in ["update Person set name = \"Ada\"", "delete from Person"] {
            let context = error_context(input);
            assert!(context.contains(required), "{context}");
        }

        let missing_set = error_context("update Person where id = 7");
        assert!(
            missing_set.contains("expected keyword `set`"),
            "{missing_set}"
        );

        let empty_set = error_context("update Person set where id = 7");
        assert!(
            empty_set.contains("an update requires at least one set item"),
            "{empty_set}"
        );

        let duplicate =
            error_context("update Person set name = \"Ada\", Name = \"A\" where id = 7");
        assert!(
            duplicate.contains("duplicate set column `Name`"),
            "{duplicate}"
        );

        let column_expression = error_context("update Person set age = p.age where id = 7");
        assert!(
            column_expression.contains("expected a literal value"),
            "{column_expression}"
        );

        let arithmetic_expression = error_context("update Person set age = 18 + 19 where id = 7");
        assert!(
            arithmetic_expression.contains("DML values must be literals, not expressions"),
            "{arithmetic_expression}"
        );

        let trailing = error_context("delete from Person where id = 7 garbage");
        assert!(trailing.contains("unexpected trailing token"), "{trailing}");

        let missing_from = error_context("delete Person where id = 7");
        assert!(
            missing_from.contains("expected keyword `from`"),
            "{missing_from}"
        );
    }

    #[test]
    fn copy_statement_rejects_missing_from_unquoted_path_and_trailing_input() {
        let missing_from = error_context("copy Person \"people.csv\"");
        assert!(
            missing_from.contains("expected keyword `from`"),
            "{missing_from}"
        );

        let unquoted_path = error_context("copy Person from people.csv");
        assert!(
            unquoted_path.contains("expected a string"),
            "{unquoted_path}"
        );

        let trailing = error_context("copy Person from \"people.csv\" garbage");
        assert!(trailing.contains("unexpected trailing token"), "{trailing}");
    }

    #[test]
    fn ontology_clause_words_remain_contextual_identifiers() {
        let interface = statement("create interface interface (class String, display Int64)");
        let Statement::CreateInterface { name, columns } = interface.stmt else {
            panic!("expected create interface");
        };
        assert_eq!(name, "interface");
        assert_eq!(columns[0].name, "class");
        assert_eq!(columns[1].name, "display");

        let class = statement("create class for class (label display, implements (interface))");
        assert_eq!(
            print_statement(&class).unwrap(),
            "create class for class (label display, implements (interface))"
        );
    }

    #[test]
    fn pin_heads_are_contextual_and_capture_original_query_text() {
        let envelope =
            statement("pin \"all people\" as nodes(Person) as person | sort person.name asc");
        let Statement::PinPlan { name, text, plan } = envelope.stmt else {
            panic!("expected pin plan");
        };
        assert_eq!(name, "all people");
        assert_eq!(
            text,
            "pin \"all people\" as nodes(Person) as person | sort person.name asc"
        );
        assert_eq!(
            print_plan(&plan).unwrap(),
            "nodes(Person) as person | sort person.name"
        );

        let unpin = statement("unpin \"all people\"");
        assert!(matches!(
            unpin.stmt,
            Statement::UnpinPlan { name } if name == "all people"
        ));

        let table = statement("create node table pin (unpin String)");
        assert!(matches!(table.stmt, Statement::CreateNodeTable { .. }));
    }

    #[test]
    fn detach_head_is_contextual_and_identifiers_remain_bare() {
        let envelope = statement("detach delete from detach where detach = 7");
        assert!(matches!(
            &envelope.stmt,
            Statement::DetachDeleteNode {
                table,
                key_column,
                key: Value::Int64(7),
            } if table == "detach" && key_column == "detach"
        ));
        assert_eq!(
            print_statement(&envelope).unwrap(),
            "detach delete from detach where detach = 7"
        );

        let table = statement("create node table detach (detach Int64 primary key)");
        assert_eq!(
            print_statement(&table).unwrap(),
            "create node table detach (detach Int64 primary key)"
        );
        let plan = query("nodes(detach) as detach | project detach.detach");
        assert_eq!(
            print_plan(&plan).unwrap(),
            "nodes(detach) as detach | project detach.detach"
        );
    }

    #[test]
    fn create_types_and_literal_variants_are_preserved() {
        let envelope =
            statement("create node table T (a Bool, b Int64, c Float64, d String, e Vector(0))");
        let Statement::CreateNodeTable { columns, .. } = envelope.stmt else {
            panic!("expected create node");
        };
        assert_eq!(columns[0].ty, LogicalType::Bool);
        assert_eq!(columns[4].ty, LogicalType::Vector { dim: 0 });

        let envelope = statement("insert into T values (null, true, false, -1, 2.0, \"x\", [])");
        let Statement::InsertNode { rows, .. } = envelope.stmt else {
            panic!("expected insert node");
        };
        assert_eq!(rows[0][0], Value::Null);
        assert_eq!(rows[0][4], Value::Float64(2.0));
    }

    #[test]
    fn comparison_chaining_wrong_case_and_unknown_stage_obey_error_contract() {
        let comparison = error_context("nodes(T) as p | filter p.a < p.b < p.c");
        assert!(comparison.contains("parenthesize"));
        assert!(comparison.contains("position"));
        assert!(comparison.contains("\"<\""));

        let wrong_case = error_context("nodes(T) as p | Filter p.a = 1");
        assert!(wrong_case.contains("Filter"));
        assert!(wrong_case.contains("filter"));

        for (wrong, lowercase) in [
            ("True", "true"),
            ("Null", "null"),
            ("Timestamp(", "timestamp"),
            ("Count(", "count"),
        ] {
            let (wrong_word, expression) = if let Some(word) = wrong.strip_suffix('(') {
                (
                    word,
                    format!("nodes(T) as p | project {wrong}p.x) as value"),
                )
            } else {
                (wrong, format!("nodes(T) as p | project {wrong} as value"))
            };
            let error = error_context(&expression);
            assert!(error.contains(wrong_word), "{wrong}: {error}");
            assert!(
                error.contains(&format!("did you mean `{lowercase}`")),
                "{wrong}: {error}"
            );
        }

        let unknown = error_context("nodes(T) as p | teleport p.a");
        assert!(unknown.contains("teleport"));
        for stage in ["expand", "filter", "project", "sort", "limit", "aggregate"] {
            assert!(unknown.contains(stage));
        }
    }

    #[test]
    fn did_you_mean_parser_stage_and_statement_keyword_typos() {
        let stage = error_context("nodes(T) as p | projct p.name");
        assert!(stage.contains("position"));
        assert!(stage.contains("offending token \"projct\""));
        assert!(stage.contains(" (did you mean `project`?)"));

        let statement = error_context("craete node table T (id Int64 primary key)");
        assert!(statement.contains("position 1"));
        assert!(statement.contains("offending token \"craete\""));
        assert!(statement.contains(" (did you mean `create`?)"));
    }

    #[test]
    fn did_you_mean_parser_name_beyond_budget_has_no_suffix() {
        let context = error_context("nodes(T) as p | zzzzzz p.name");
        assert!(context.contains("unknown pipeline stage"));
        assert!(context.contains("offending token \"zzzzzz\""));
        assert!(!context.contains("did you mean"));
    }

    #[test]
    fn invalid_minus_counts_and_expand_default_are_rejected() {
        let minus = error_context("nodes(T) as p | filter -p.a = 1");
        assert!(minus.contains("followed by a number"));
        assert!(minus.contains("\"-\""));

        for (text, argument) in [
            ("nodes(T) as p | limit -1", "limit count"),
            ("nodes(T) as p | limit 1 offset -2", "limit offset"),
            ("knn(T.v, [], -1, l2)", "KNN `k`"),
            ("create node table T (v Vector(-1))", "Vector dimension"),
        ] {
            assert!(error_context(text).contains(argument));
        }
        let expand = error_context("knn(T.v, [], 1, l2) | expand R out as r");
        assert!(expand.contains("no preceding"));
    }

    #[test]
    fn out_of_range_and_non_finite_numbers_are_rejected() {
        assert!(error_context("insert into T values (9223372036854775808)").contains("Int64"));
        assert!(error_context("insert into T values (-9223372036854775809)").contains("Int64"));
        assert!(error_context("insert into T values (1e999)").contains("finite"));
        assert!(error_context("insert into T values ([1e999])").contains("finite"));

        // The text-side vector guard names the offending element's index.
        let error = error_context("knn(T.v, [0.5, 1e999], 2, cosine)");
        assert!(error.contains("vector element 1"), "{error}");
        assert!(error.contains("finite"), "{error}");
    }

    #[test]
    fn json_literal_canonicalization_preserves_document_key_order() {
        // PLAN_IR's canonical-form law for Json requires preserved key
        // order; the workspace's serde_json build (no `preserve_order`)
        // would alphabetize keys.
        let plan = query(r#"nodes(T) as t | project json("{ \"b\": 1, \"a\": 2 }") as value"#);
        assert_eq!(
            print_plan(&plan).unwrap(),
            r#"nodes(T) as t | project json("{\"b\":1,\"a\":2}") as value"#
        );
        let json = plan.to_json().unwrap();
        assert_eq!(crate::ops::Plan::from_json(&json).unwrap(), plan);

        let nested = query(
            r#"nodes(T) as t | project json("{\"z\": {\"y\": 1, \"x\": 2}, \"a\": 3}") as value"#,
        );
        assert_eq!(
            print_plan(&nested).unwrap(),
            r#"nodes(T) as t | project json("{\"z\":{\"y\":1,\"x\":2},\"a\":3}") as value"#
        );
    }

    #[test]
    fn text_plan_matches_binding_canonical_json_example() {
        let plan = query("nodes(Person) as p | filter p.age > 30 | project p.name as name");
        assert_eq!(
            plan.to_json().unwrap(),
            r#"{"v":0,"plan":{"op":"Project","exprs":[{"expr":{"col":"p.name"},"as":"name"}],"input":{"op":"Filter","predicate":{"gt":[{"col":"p.age"},{"lit":30}]},"input":{"op":"ScanNodes","table":"Person","binding":"p"}}}}"#
        );
    }

    #[test]
    fn explicit_expand_from_after_knn_round_trips() {
        let plan = query("knn(T.v, [], 1, l2) | expand R in from seed as reached");
        let Operator::Expand {
            direction,
            from_binding,
            ..
        } = &plan.plan
        else {
            panic!("expected expand");
        };
        assert_eq!(*direction, Direction::In);
        assert_eq!(from_binding, "seed");
        assert_eq!(
            print_plan(&plan).unwrap(),
            "knn(T.v, [], 1, l2) | expand R in from seed as reached"
        );
    }
}
