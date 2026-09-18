//! JSON API handlers over a shared [`Mutex`](std::sync::Mutex) database.

use std::collections::HashSet;
use std::io;
use std::net::TcpListener;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use devondb::text::parser::{Parsed, parse};
use devondb::text::printer::{print_plan, print_statement};
use devondb::{
    Database, DevonError, NodeTableSummary, Plan, QueryResult, RelTableSummary, SchemaSummary,
    StatementEnvelope, Value, fold, suggestion_suffix,
};
use devondb_nl::{Compiled, CompiledStatement, DeterministicCompiler, IntentCompiler, NoParse};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value as JsonValue, json};

use crate::http::{ApiHandler, Request, Response};

const QUERY_ROW_LIMIT: usize = 10_000;
const GRAPH_NODE_LIMIT: u64 = 500;
const DAY_SECONDS: i64 = 86_400;

/// The built-in UI API backed by one serialized database handle.
pub struct Api {
    database: Mutex<Database>,
}

impl Api {
    /// Creates an API over `database`.
    #[must_use]
    pub fn new(database: Database) -> Self {
        Self {
            database: Mutex::new(database),
        }
    }

    fn dispatch(&self, request: &Request) -> ApiResult<JsonValue> {
        match request.path.as_str() {
            "/api/schema" => self.schema(),
            "/api/ask" => self.ask(request),
            "/api/explain" => self.explain(request),
            "/api/query" => self.query(request),
            "/api/statement" => self.statement(request),
            "/api/pin" => self.pin(request),
            "/api/unpin" => self.unpin(request),
            "/api/run-pin" => self.run_pin(request),
            "/api/graph" => self.graph(request),
            _ => unreachable!("dispatch is called only for known routes"),
        }
    }

    fn schema(&self) -> ApiResult<JsonValue> {
        serde_json::to_value(lock(&self.database).schema_summary()).map_err(ApiFailure::internal)
    }

    fn ask(&self, request: &Request) -> ApiResult<JsonValue> {
        let request: TextRequest = parse_body(request)?;
        // Row-backed compilation, never the schema-only `compile`: row-backed
        // grounding is what resolves a value to its STORED spelling (`ada` ->
        // `"Ada"`, `docs/NL.md` § entity value slots) and a place name to a geo
        // literal (§ 9). The schema-only call silently degrades both, so the
        // browser's ask box would compile a filter that matches nothing.
        // Supply time at the API boundary so the compiler remains deterministic.
        let reference_date = utc_reference_date();
        let mut database = lock(&self.database);
        match DeterministicCompiler.compile_at_with_database(
            &request.text,
            &mut database,
            reference_date,
        ) {
            Compiled::Plan(plan) => explain_plan(&plan),
            Compiled::NoParse(mut question_report) => {
                match DeterministicCompiler
                    .compile_statement(&request.text, &database.schema_summary())
                {
                    CompiledStatement::Statement(statement) => {
                        // `/api/ask` explains only; `/api/statement` owns the
                        // confirmed-plan execution path (`docs/UI.md` §12.6).
                        explain_statement(&StatementEnvelope {
                            v: 0,
                            stmt: statement,
                        })
                    }
                    CompiledStatement::NoParse(statement_report) => {
                        append_statement_hints(&mut question_report, &statement_report);
                        Ok(json!({"noparse": no_parse_response(&question_report)}))
                    }
                }
            }
        }
    }

    fn explain(&self, request: &Request) -> ApiResult<JsonValue> {
        match parse_body::<ExplainRequest>(request)? {
            ExplainRequest::Text(request) => explain_parsed(parse(&request.text)?),
            ExplainRequest::Plan(request) => {
                explain_plan(&Plan::from_json(&request.plan.to_string())?)
            }
            ExplainRequest::Statement(request) => explain_statement(&StatementEnvelope::from_json(
                &request.statement.to_string(),
            )?),
        }
    }

    fn query(&self, request: &Request) -> ApiResult<JsonValue> {
        let plan = match parse_body::<QueryRequest>(request)? {
            QueryRequest::Text(request) => query_from_text(&request.text)?,
            QueryRequest::Plan(request) => Plan::from_json(&request.plan.to_string())?,
        };
        let result = lock(&self.database).run(&plan)?;
        query_result_json(result)
    }

    fn statement(&self, request: &Request) -> ApiResult<JsonValue> {
        let statement = match parse_body::<StatementRequest>(request)? {
            StatementRequest::Text(request) => statement_from_text(&request.text)?,
            StatementRequest::Statement(request) => {
                StatementEnvelope::from_json(&request.statement.to_string())?
            }
        };
        lock(&self.database).execute(&statement.stmt)?;
        Ok(json!({"ok": true}))
    }

    fn pin(&self, request: &Request) -> ApiResult<JsonValue> {
        let request: PinRequest = parse_body(request)?;
        let plan = Plan::from_json(&request.plan.to_string())?;
        lock(&self.database).pin(&request.name, &request.text, &plan)?;
        Ok(json!({"ok": true}))
    }

    fn unpin(&self, request: &Request) -> ApiResult<JsonValue> {
        let request: PinNameRequest = parse_body(request)?;
        lock(&self.database).unpin(&request.name)?;
        Ok(json!({"ok": true}))
    }

    fn run_pin(&self, request: &Request) -> ApiResult<JsonValue> {
        let request: PinNameRequest = parse_body(request)?;
        let result = lock(&self.database).run_pin(&request.name)?;
        query_result_json(result)
    }

    fn graph(&self, request: &Request) -> ApiResult<JsonValue> {
        let request: GraphRequest = parse_body(request)?;
        let mut database = lock(&self.database);
        graph_neighborhood(&mut database, request)
    }
}

fn utc_reference_date() -> i64 {
    let epoch_secs = match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_secs() as i64,
        Err(error) => -(error.duration().as_secs() as i64),
    };
    epoch_secs - epoch_secs.rem_euclid(DAY_SECONDS)
}

impl ApiHandler for Api {
    fn handle(&self, request: &Request) -> Response {
        let Some(method) = route_method(&request.path) else {
            return Response::not_found();
        };
        if request.method != method {
            return Response::method_not_allowed();
        }
        api_response(self.dispatch(request))
    }
}

/// Serves the built-in UI and JSON API until the listener fails permanently.
pub fn serve(database: Database, listener: TcpListener) -> io::Result<()> {
    let api = Api::new(database);
    crate::http::serve(listener, &api)
}

fn route_method(path: &str) -> Option<&'static str> {
    match path {
        "/api/schema" => Some("GET"),
        "/api/ask" | "/api/explain" | "/api/query" | "/api/statement" | "/api/pin"
        | "/api/unpin" | "/api/run-pin" | "/api/graph" => Some("POST"),
        _ => None,
    }
}

fn lock(database: &Mutex<Database>) -> MutexGuard<'_, Database> {
    database.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TextRequest {
    text: String,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ExplainRequest {
    Text(TextRequest),
    Plan(PlanRequest),
    Statement(StatementPlanRequest),
}

#[derive(Deserialize)]
#[serde(untagged)]
enum QueryRequest {
    Text(TextRequest),
    Plan(PlanRequest),
}

#[derive(Deserialize)]
#[serde(untagged)]
enum StatementRequest {
    Text(TextRequest),
    Statement(StatementPlanRequest),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PlanRequest {
    plan: JsonValue,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StatementPlanRequest {
    statement: JsonValue,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PinRequest {
    name: String,
    plan: JsonValue,
    text: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PinNameRequest {
    name: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GraphRequest {
    table: String,
    key: JsonValue,
    limit: Option<u64>,
}

#[derive(Serialize)]
struct NoParseResponse<'a> {
    recognized: Vec<GroundedResponse<'a>>,
    unrecognized: Vec<UngroundedResponse<'a>>,
    nearest: Vec<TemplateHintResponse<'a>>,
}

#[derive(Serialize)]
struct GroundedResponse<'a> {
    token: &'a str,
    target: &'a str,
}

#[derive(Serialize)]
struct UngroundedResponse<'a> {
    token: &'a str,
    suggestion: Option<&'a str>,
}

#[derive(Serialize)]
struct TemplateHintResponse<'a> {
    example: &'a str,
}

fn no_parse_response(report: &NoParse) -> NoParseResponse<'_> {
    NoParseResponse {
        recognized: report
            .recognized
            .iter()
            .map(|item| GroundedResponse {
                token: &item.token,
                target: &item.target,
            })
            .collect(),
        unrecognized: report
            .unrecognized
            .iter()
            .map(|item| UngroundedResponse {
                token: &item.token,
                suggestion: item.suggestion.as_deref(),
            })
            .collect(),
        nearest: report
            .nearest
            .iter()
            .map(|item| TemplateHintResponse {
                example: &item.example,
            })
            .collect(),
    }
}

pub(crate) fn append_statement_hints(question: &mut NoParse, statement: &NoParse) {
    if statement_grounded_more(question, statement) {
        question.nearest.extend(statement.nearest.iter().cloned());
    }
}

fn statement_grounded_more(question: &NoParse, statement: &NoParse) -> bool {
    const STATEMENT_HEADS: [&str; 4] = ["set", "change", "delete", "remove"];
    // The shared closed vocabulary makes the question refusal label mutation
    // heads as recognized even though no question template grounds them.
    let has_statement_head = statement
        .recognized
        .first()
        .is_some_and(|item| STATEMENT_HEADS.contains(&item.target.as_str()));
    let question_grounded = question
        .recognized
        .iter()
        .filter(|item| !STATEMENT_HEADS.contains(&item.target.as_str()))
        .count();
    has_statement_head && statement.recognized.len() > question_grounded
}

fn parse_body<T: DeserializeOwned>(request: &Request) -> ApiResult<T> {
    serde_json::from_slice(&request.body).map_err(ApiFailure::bad_request)
}

fn query_from_text(text: &str) -> ApiResult<Plan> {
    match parse(text)? {
        Parsed::Query(plan) => Ok(plan),
        Parsed::Statement(_) => Err(DevonError::InvalidArgument {
            context: "expected a query, found a statement".to_owned(),
        }
        .into()),
    }
}

fn statement_from_text(text: &str) -> ApiResult<StatementEnvelope> {
    match parse(text)? {
        Parsed::Statement(statement) => Ok(statement),
        Parsed::Query(_) => Err(DevonError::InvalidArgument {
            context: "expected a statement, found a query".to_owned(),
        }
        .into()),
    }
}

fn explain_parsed(parsed: Parsed) -> ApiResult<JsonValue> {
    match parsed {
        Parsed::Query(plan) => explain_plan(&plan),
        Parsed::Statement(statement) => explain_statement(&statement),
    }
}

fn explain_plan(plan: &Plan) -> ApiResult<JsonValue> {
    Ok(json!({
        "kind": "query",
        "canonical": print_plan(plan)?,
        "plan": canonical_plan_json(plan)?,
    }))
}

fn explain_statement(statement: &StatementEnvelope) -> ApiResult<JsonValue> {
    Ok(json!({
        "kind": "statement",
        "canonical": print_statement(statement)?,
        "plan": canonical_statement_json(statement)?,
    }))
}

fn canonical_plan_json(plan: &Plan) -> ApiResult<JsonValue> {
    let json = plan.to_json()?;
    serde_json::from_str(&json).map_err(ApiFailure::internal)
}

fn canonical_statement_json(statement: &StatementEnvelope) -> ApiResult<JsonValue> {
    let json = statement.to_json()?;
    serde_json::from_str(&json).map_err(ApiFailure::internal)
}

fn query_result_json(result: QueryResult) -> ApiResult<JsonValue> {
    let truncated = result.rows.len() > QUERY_ROW_LIMIT;
    let rows = result
        .rows
        .iter()
        .take(QUERY_ROW_LIMIT)
        .map(|row| row.iter().map(natural_value).collect::<ApiResult<Vec<_>>>())
        .collect::<ApiResult<Vec<_>>>()?;
    let row_count = rows.len();
    Ok(json!({
        "columns": result.columns,
        "rows": rows,
        "row_count": row_count,
        "truncated": truncated,
    }))
}

/// Converts one typed database value to its natural JSON form. Non-finite
/// floats are tagged from the typed value, because `serde_json::to_value`
/// would silently collapse them into the same `null` as SQL NULL — matching
/// the C FFI's `{"f64":"NaN"|"inf"|"-inf"}` spelling.
fn natural_value(value: &Value) -> ApiResult<JsonValue> {
    match value {
        Value::Float64(number) if !number.is_finite() => Ok(nonfinite_float(*number)),
        Value::Vector(elements) => Ok(natural_vector(elements)),
        _ => {
            let tagged = serde_json::to_value(value).map_err(ApiFailure::internal)?;
            match tagged {
                JsonValue::String(tag) if tag == "Null" => Ok(JsonValue::Null),
                JsonValue::Object(object) => natural_tagged_value(object),
                other => Err(ApiFailure::Internal(format!(
                    "unexpected serialized database value: {other}"
                ))),
            }
        }
    }
}

/// Serializes a non-finite float so it cannot collide with SQL NULL:
/// `{"f64":"NaN"}` / `{"f64":"inf"}` / `{"f64":"-inf"}`.
fn nonfinite_float(number: f64) -> JsonValue {
    let text = if number.is_nan() {
        "NaN"
    } else if number == f64::INFINITY {
        "inf"
    } else {
        "-inf"
    };
    JsonValue::Object(Map::from_iter([(
        "f64".to_owned(),
        JsonValue::String(text.to_owned()),
    )]))
}

/// Serializes a Vector, tagging any non-finite element with the same
/// `{"f64":...}` spelling so it cannot collapse into JSON null.
fn natural_vector(elements: &[f32]) -> JsonValue {
    JsonValue::Array(
        elements
            .iter()
            .map(|element| {
                if element.is_finite() {
                    JsonValue::from(*element)
                } else {
                    nonfinite_float(f64::from(*element))
                }
            })
            .collect(),
    )
}

fn natural_tagged_value(object: Map<String, JsonValue>) -> ApiResult<JsonValue> {
    let mut entries = object.into_iter();
    let Some((variant, value)) = entries.next() else {
        return Err(ApiFailure::Internal(
            "serialized database value has no variant".to_owned(),
        ));
    };
    if entries.next().is_some() {
        return Err(ApiFailure::Internal(
            "serialized database value has multiple variants".to_owned(),
        ));
    }
    match variant.as_str() {
        // Non-finite floats never reach these arms: `natural_value` tags
        // them from the typed value before serde can collapse them to null.
        "Bool" | "Int64" | "String" | "Vector" | "Float64" => Ok(value),
        // GeoPoint's natural API form is the tagged `geo` object — the same
        // spelling the plan language admits as its one object literal.
        "GeoPoint" => Ok(JsonValue::Object(Map::from_iter([(
            "geo".to_owned(),
            value,
        )]))),
        _ => Err(ApiFailure::Internal(format!(
            "unknown serialized database value variant `{variant}`"
        ))),
    }
}

fn graph_neighborhood(database: &mut Database, request: GraphRequest) -> ApiResult<JsonValue> {
    let summary = database.schema_summary();
    let table = node_table(&summary, &request.table)?.clone();
    let primary_key = primary_key(&table)?;
    let center_result = database.run(&node_plan(&table, primary_key, &request.key)?)?;
    let center = single_node(center_result, &table, &request.key)?;
    let limit = usize::try_from(request.limit.unwrap_or(GRAPH_NODE_LIMIT)).unwrap_or(usize::MAX);
    let mut graph = Graph::new(limit);
    if !graph.add_center(center) {
        return graph.to_json();
    }
    add_incident_neighbors(database, &summary, &table, &request.key, &mut graph)?;
    graph.to_json()
}

fn node_table<'a>(summary: &'a SchemaSummary, name: &str) -> ApiResult<&'a NodeTableSummary> {
    summary
        .node_tables
        .iter()
        .find(|table| fold(&table.name) == fold(name))
        .ok_or_else(|| {
            DevonError::NotFound {
                what: format!(
                    "node table `{name}`{}",
                    suggestion_suffix(
                        name,
                        summary.node_tables.iter().map(|table| table.name.as_str())
                    )
                ),
            }
            .into()
        })
}

fn primary_key(table: &NodeTableSummary) -> ApiResult<&str> {
    table
        .columns
        .iter()
        .find(|column| column.primary_key)
        .map(|column| column.name.as_str())
        .ok_or_else(|| {
            DevonError::InvalidArgument {
                context: format!("node table `{}` has no primary key", table.name),
            }
            .into()
        })
}

fn single_node(
    result: QueryResult,
    table: &NodeTableSummary,
    key: &JsonValue,
) -> ApiResult<GraphNode> {
    let Some(row) = result.rows.first() else {
        return Err(DevonError::NotFound {
            what: format!("node `{}` with primary key {key}", table.name),
        }
        .into());
    };
    row_to_node(table, row)
}

fn add_incident_neighbors(
    database: &mut Database,
    summary: &SchemaSummary,
    center: &NodeTableSummary,
    key: &JsonValue,
    graph: &mut Graph,
) -> ApiResult<()> {
    'relationships: for rel in &summary.rel_tables {
        for traversal in traversals(rel, &center.name) {
            let neighbor = node_table(summary, traversal.neighbor_table)?;
            let plan = expand_plan(center, key, rel, neighbor, traversal.direction)?;
            let result = database.run(&plan)?;
            for row in &result.rows {
                let node = row_to_node(neighbor, row)?;
                let edge = traversal.edge(rel, center, neighbor, key.clone(), node.key.clone());
                if !graph.add_neighbor(node, edge) {
                    break 'relationships;
                }
            }
        }
    }
    Ok(())
}

fn traversals<'a>(rel: &'a RelTableSummary, table: &str) -> Vec<Traversal<'a>> {
    let mut traversals = Vec::with_capacity(2);
    if fold(&rel.from) == fold(table) {
        traversals.push(Traversal {
            direction: "out",
            neighbor_table: &rel.to,
        });
    }
    if fold(&rel.to) == fold(table) {
        traversals.push(Traversal {
            direction: "in",
            neighbor_table: &rel.from,
        });
    }
    traversals
}

struct Traversal<'a> {
    direction: &'static str,
    neighbor_table: &'a str,
}

impl Traversal<'_> {
    fn edge(
        &self,
        rel: &RelTableSummary,
        from_table: &NodeTableSummary,
        to_table: &NodeTableSummary,
        center: JsonValue,
        neighbor: JsonValue,
    ) -> GraphEdge {
        let (from_key, to_key) = if self.direction == "out" {
            (center, neighbor)
        } else {
            (neighbor, center)
        };
        GraphEdge {
            rel: rel.name.clone(),
            from: GraphEndpoint {
                table: from_table.name.clone(),
                key: from_key,
            },
            to: GraphEndpoint {
                table: to_table.name.clone(),
                key: to_key,
            },
        }
    }
}

fn node_plan(table: &NodeTableSummary, primary_key: &str, key: &JsonValue) -> ApiResult<Plan> {
    let input = filtered_scan(&table.name, primary_key, key);
    plan_from_operator(project(&table.columns, "n", input))
}

fn expand_plan(
    center: &NodeTableSummary,
    key: &JsonValue,
    rel: &RelTableSummary,
    neighbor: &NodeTableSummary,
    direction: &str,
) -> ApiResult<Plan> {
    let input = filtered_scan(&center.name, primary_key(center)?, key);
    let expand = json!({
        "op": "Expand",
        "rel": rel.name,
        "direction": direction,
        "from_binding": "n",
        "binding": "m",
        "input": input,
    });
    plan_from_operator(project(&neighbor.columns, "m", expand))
}

fn filtered_scan(table: &str, primary_key: &str, key: &JsonValue) -> JsonValue {
    json!({
        "op": "Filter",
        "predicate": {"eq": [
            {"col": format!("n.{primary_key}")},
            {"lit": key},
        ]},
        "input": {"op": "ScanNodes", "table": table, "binding": "n"},
    })
}

fn project(columns: &[devondb::ColumnSummary], binding: &str, input: JsonValue) -> JsonValue {
    let exprs = columns
        .iter()
        .map(|column| {
            json!({
                "expr": {"col": format!("{binding}.{}", column.name)},
                "as": column.name,
            })
        })
        .collect::<Vec<_>>();
    json!({"op": "Project", "exprs": exprs, "input": input})
}

fn plan_from_operator(operator: JsonValue) -> ApiResult<Plan> {
    Plan::from_json(&json!({"v": 0, "plan": operator}).to_string()).map_err(Into::into)
}

fn row_to_node(table: &NodeTableSummary, row: &[Value]) -> ApiResult<GraphNode> {
    if row.len() != table.columns.len() {
        return Err(ApiFailure::Internal(format!(
            "query row for node table `{}` has {} values for {} columns",
            table.name,
            row.len(),
            table.columns.len()
        )));
    }
    let mut key = None;
    let mut props = Map::new();
    for (column, value) in table.columns.iter().zip(row) {
        let value = natural_value(value)?;
        if column.primary_key {
            key = Some(value);
        } else {
            props.insert(column.name.clone(), value);
        }
    }
    let key = key.ok_or_else(|| {
        ApiFailure::Internal(format!(
            "query row for node table `{}` has no primary-key value",
            table.name
        ))
    })?;
    Ok(GraphNode {
        table: table.name.clone(),
        key,
        props,
    })
}

#[derive(Serialize)]
struct GraphNode {
    table: String,
    key: JsonValue,
    props: Map<String, JsonValue>,
}

/// A table-qualified edge endpoint.
///
/// Bare primary-key endpoints are ambiguous across tables (`Person:1` vs
/// `Company:1` on the same edge), so every endpoint names its table —
/// pinned in `docs/UI.md` § 4 and relied on by the SPA's edge resolver.
#[derive(Serialize)]
struct GraphEndpoint {
    table: String,
    key: JsonValue,
}

#[derive(Serialize)]
struct GraphEdge {
    rel: String,
    from: GraphEndpoint,
    to: GraphEndpoint,
}

struct Graph {
    limit: usize,
    nodes: Vec<GraphNode>,
    edges: Vec<GraphEdge>,
    node_ids: HashSet<(String, String)>,
    edge_ids: HashSet<(String, String, String)>,
    truncated: bool,
}

impl Graph {
    fn new(limit: usize) -> Self {
        Self {
            limit,
            nodes: Vec::new(),
            edges: Vec::new(),
            node_ids: HashSet::new(),
            edge_ids: HashSet::new(),
            truncated: false,
        }
    }

    fn add_center(&mut self, node: GraphNode) -> bool {
        if self.limit == 0 {
            self.truncated = true;
            return false;
        }
        self.insert_node(node);
        true
    }

    fn add_neighbor(&mut self, node: GraphNode, edge: GraphEdge) -> bool {
        let node_id = node_identity(&node);
        if !self.node_ids.contains(&node_id) && self.nodes.len() == self.limit {
            self.truncated = true;
            return false;
        }
        if !self.node_ids.contains(&node_id) {
            self.node_ids.insert(node_id);
            self.nodes.push(node);
        }
        let edge_id = edge_identity(&edge);
        if self.edge_ids.insert(edge_id) {
            self.edges.push(edge);
        }
        true
    }

    fn insert_node(&mut self, node: GraphNode) {
        self.node_ids.insert(node_identity(&node));
        self.nodes.push(node);
    }

    fn to_json(&self) -> ApiResult<JsonValue> {
        serde_json::to_value(json!({
            "nodes": self.nodes,
            "edges": self.edges,
            "truncated": self.truncated,
        }))
        .map_err(ApiFailure::internal)
    }
}

fn node_identity(node: &GraphNode) -> (String, String) {
    (node.table.clone(), node.key.to_string())
}

fn edge_identity(edge: &GraphEdge) -> (String, String, String) {
    (
        edge.rel.clone(),
        format!("{}\u{0}{}", edge.from.table, edge.from.key),
        format!("{}\u{0}{}", edge.to.table, edge.to.key),
    )
}

type ApiResult<T> = Result<T, ApiFailure>;

enum ApiFailure {
    BadRequest(String),
    Engine(DevonError),
    Internal(String),
}

impl ApiFailure {
    fn bad_request(error: serde_json::Error) -> Self {
        Self::BadRequest(error.to_string())
    }

    fn internal(error: serde_json::Error) -> Self {
        Self::Internal(error.to_string())
    }
}

impl From<DevonError> for ApiFailure {
    fn from(error: DevonError) -> Self {
        Self::Engine(error)
    }
}

fn api_response(result: ApiResult<JsonValue>) -> Response {
    match result {
        Ok(value) => match serde_json::to_vec(&value) {
            Ok(body) => Response::json(200, body),
            Err(error) => Response::error(500, &error.to_string()),
        },
        Err(ApiFailure::BadRequest(message)) => Response::error(400, &message),
        Err(ApiFailure::Engine(error)) => Response::error(422, &error.to_string()),
        Err(ApiFailure::Internal(message)) => Response::error(500, &message),
    }
}

#[cfg(test)]
mod tests {
    use super::natural_value;
    use devondb::Value;
    use serde_json::{Value as JsonValue, json};

    fn natural(value: &Value) -> JsonValue {
        natural_value(value).unwrap_or_else(|_| panic!("natural_value failed"))
    }

    #[test]
    fn non_finite_float64_is_tagged_distinctly_from_null() {
        assert_eq!(natural(&Value::Null), JsonValue::Null);
        assert_eq!(natural(&Value::Float64(2.5)), json!(2.5));
        assert_eq!(natural(&Value::Float64(f64::NAN)), json!({"f64": "NaN"}));
        assert_eq!(
            natural(&Value::Float64(f64::INFINITY)),
            json!({"f64": "inf"})
        );
        assert_eq!(
            natural(&Value::Float64(f64::NEG_INFINITY)),
            json!({"f64": "-inf"})
        );
    }

    #[test]
    fn non_finite_vector_elements_are_tagged_like_scalars() {
        // Value ingress rejects non-finite floats, so no end-to-end path can
        // produce this vector — the tagging is defense against a storage
        // layer that ever admits one, mirrored from the C FFI.
        assert_eq!(
            natural(&Value::Vector(vec![
                1.5,
                f32::INFINITY,
                f32::NAN,
                f32::NEG_INFINITY
            ])),
            json!([1.5, {"f64": "inf"}, {"f64": "NaN"}, {"f64": "-inf"}])
        );
    }
}
