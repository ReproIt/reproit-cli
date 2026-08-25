use std::{
    collections::BTreeMap,
    io::ErrorKind,
    path::PathBuf,
    sync::{Arc, OnceLock},
};

use reproit_app::{
    agent::{
        AgentOperations, CheckReprosInput, CheckReprosResult, GetReproInput, ListReprosInput,
        RunReproInput, RunReproResult, TriageReproInput,
    },
    remove_kept,
};
use reproit_core::{
    Error, ErrorCode,
    contracts::{CLOUD_API_SCHEMAS, CORE_SCHEMAS, MCP_SCHEMAS},
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt as _, AsyncWrite, AsyncWriteExt as _, BufReader};
use tokio::{sync::Mutex, task::JoinSet};

use crate::{
    FilesystemRepository,
    agent::ProductionAgent,
    render::{PublicErrorContext, structured_error},
};

const MCP_PROTOCOL_VERSION: &str = "2025-06-18";
const MAX_REQUEST_BYTES: usize = 64 * 1024;
const MAX_RESULT_BYTES: usize = 1024 * 1024;
const MAX_METADATA_CALLS: usize = 32;
const MAX_EXECUTION_CALLS: usize = 2;

pub async fn serve(root: PathBuf) -> Result<(), Error> {
    let input = BufReader::new(tokio::io::stdin());
    let output = tokio::io::stdout();
    serve_io(ProductionAgent::new(root.clone()), root, input, output).await
}

// This function owns the MCP stream lifecycle and its bounded process state.
#[allow(clippy::too_many_lines)]
async fn serve_io<Agent, Input, Output>(
    agent: Agent,
    root: PathBuf,
    mut input: Input,
    output: Output,
) -> Result<(), Error>
where
    Agent: AgentOperations + 'static,
    Input: AsyncBufRead + Unpin,
    Output: AsyncWrite + Send + Unpin + 'static,
{
    let agent = Arc::new(agent);
    let root = Arc::new(root);
    let output = Arc::new(Mutex::new(output));
    let metadata_calls = Arc::new(tokio::sync::Semaphore::new(MAX_METADATA_CALLS));
    let execution_calls = Arc::new(tokio::sync::Semaphore::new(MAX_EXECUTION_CALLS));
    let cancellations = Arc::new(Mutex::new(BTreeMap::<
        String,
        tokio::sync::watch::Sender<bool>,
    >::new()));
    let mut tasks = JoinSet::new();
    let mut initialized = false;
    loop {
        let Some(line) = read_bounded_line(&mut input).await? else {
            break;
        };
        let request = match line {
            BoundedLine::Complete(line) => {
                let Ok(request) = serde_json::from_slice::<Value>(&line) else {
                    write_locked(
                        &output,
                        protocol_error(Value::Null, -32700, "Invalid JSON."),
                    )
                    .await?;
                    continue;
                };
                request
            }
            BoundedLine::OverLimit => {
                write_locked(
                    &output,
                    protocol_error(Value::Null, -32600, "The MCP request exceeds 64 KiB."),
                )
                .await?;
                continue;
            }
        };
        if let Some(request_id) = cancellation_identity(&request) {
            if let Some(cancelled) = cancellations.lock().await.get(&request_id) {
                let _ = cancelled.send(true);
            }
            continue;
        }
        if initialized && let Some((id, execution)) = tool_call_identity(&request) {
            let semaphore = if execution {
                Arc::clone(&execution_calls)
            } else {
                Arc::clone(&metadata_calls)
            };
            let Ok(permit) = semaphore.try_acquire_owned() else {
                let error = safe_error(Error::new(
                    ErrorCode::RateLimited,
                    "The MCP process has reached its active call limit.",
                ));
                write_locked(&output, success(id, tool_error(error))).await?;
                continue;
            };
            let request_id = request_identity(&id);
            let (cancelled, mut cancellation) = tokio::sync::watch::channel(false);
            if !register_active_request(
                &mut *cancellations.lock().await,
                request_id.clone(),
                cancelled,
            ) {
                write_locked(
                    &output,
                    protocol_error(id, -32600, "The MCP request ID is already active."),
                )
                .await?;
                continue;
            }
            let agent = Arc::clone(&agent);
            let root = Arc::clone(&root);
            let output = Arc::clone(&output);
            let cancellations = Arc::clone(&cancellations);
            tasks.spawn(async move {
                let mut task_initialized = true;
                let response = tokio::select! {
                    response = handle_request(
                        agent.as_ref(),
                        root.as_ref(),
                        &mut task_initialized,
                        request,
                    ) => response,
                    result = cancellation.changed() => {
                        let _ = result;
                        None
                    }
                };
                let write_result = if let Some(response) = response {
                    write_locked(&output, response).await
                } else {
                    Ok(())
                };
                cancellations.lock().await.remove(&request_id);
                drop(permit);
                write_result?;
                Ok::<(), Error>(())
            });
            continue;
        }
        if let Some(response) =
            handle_request(agent.as_ref(), root.as_ref(), &mut initialized, request).await
        {
            write_locked(&output, response).await?;
        }
    }
    while let Some(result) = tasks.join_next().await {
        result.map_err(|_| io_error(std::io::Error::other("MCP task stopped")))??;
    }
    output.lock().await.flush().await.map_err(io_error)
}

fn cancellation_identity(request: &Value) -> Option<String> {
    let object = request.as_object()?;
    if object.get("jsonrpc")?.as_str()? != "2.0"
        || object.get("id").is_some()
        || object.get("method")?.as_str()? != "notifications/cancelled"
    {
        return None;
    }
    let request_id = object.get("params")?.get("requestId")?;
    valid_request_id(request_id).then(|| request_identity(request_id))
}

fn request_identity(id: &Value) -> String {
    serde_json::to_string(id).expect("valid JSON-RPC request ID")
}

fn tool_call_identity(request: &Value) -> Option<(Value, bool)> {
    let object = request.as_object()?;
    if object.get("jsonrpc")?.as_str()? != "2.0" || object.get("method")?.as_str()? != "tools/call"
    {
        return None;
    }
    let id = object.get("id")?.clone();
    if !valid_request_id(&id) {
        return None;
    }
    let name = object.get("params")?.get("name")?.as_str()?;
    let execution = matches!(
        name,
        "triage_repro" | "run_repro" | "check_repros" | "keep_repro" | "remove_repro"
    );
    Some((id, execution))
}

fn register_active_request(
    active: &mut BTreeMap<String, tokio::sync::watch::Sender<bool>>,
    request_id: String,
    cancelled: tokio::sync::watch::Sender<bool>,
) -> bool {
    if active.contains_key(&request_id) {
        return false;
    }
    active.insert(request_id, cancelled);
    true
}

async fn write_locked(
    output: &Mutex<impl AsyncWrite + Unpin>,
    response: Value,
) -> Result<(), Error> {
    write_response(&mut *output.lock().await, response).await
}

async fn handle_request(
    agent: &impl AgentOperations,
    root: &std::path::Path,
    initialized: &mut bool,
    request: Value,
) -> Option<Value> {
    let Some(object) = request.as_object() else {
        return Some(protocol_error(Value::Null, -32600, "Invalid request."));
    };
    let id = object.get("id").cloned();
    let response_id = id.clone().unwrap_or(Value::Null);
    if object.get("jsonrpc") != Some(&Value::String("2.0".to_owned()))
        || !id.as_ref().is_none_or(valid_request_id)
    {
        return Some(protocol_error(response_id, -32600, "Invalid request."));
    }
    let Some(method) = object.get("method").and_then(Value::as_str) else {
        return Some(protocol_error(response_id, -32600, "Invalid request."));
    };
    if id.is_none() {
        if matches!(
            method,
            "notifications/initialized" | "notifications/cancelled"
        ) {
            return None;
        }
        return None;
    }
    match method {
        "initialize" => {
            *initialized = true;
            Some(success(
                response_id,
                json!({
                    "protocolVersion": MCP_PROTOCOL_VERSION,
                    "capabilities": {"tools": {"listChanged": false}},
                    "serverInfo": {
                        "name": "reproit",
                        "version": env!("CARGO_PKG_VERSION")
                    }
                }),
            ))
        }
        _ if !*initialized => Some(protocol_error(
            response_id,
            -32002,
            "Initialize the MCP session first.",
        )),
        "ping" => Some(success(response_id, json!({}))),
        "tools/list" => Some(success(response_id, json!({"tools": tools()}))),
        "tools/call" => {
            let params = object.get("params").cloned().unwrap_or(Value::Null);
            Some(success(response_id, call_tool(agent, root, params).await))
        }
        _ => Some(protocol_error(
            response_id,
            -32601,
            "The MCP method is not available.",
        )),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolCall {
    arguments: Value,
    name: String,
}

async fn call_tool(agent: &impl AgentOperations, root: &std::path::Path, params: Value) -> Value {
    let call: ToolCall = match serde_json::from_value(params) {
        Ok(call) => call,
        Err(_) => return tool_error(safe_error(Error::schema_invalid())),
    };
    match call.name.as_str() {
        "list_repros" => {
            let input = match validated_input::<ListReprosInput>(call.arguments, |input| {
                input.validate()
            }) {
                Ok(input) => input,
                Err(error) => return tool_error(error),
            };
            tool_result(agent.list_repros(input).await.map_err(safe_error))
        }
        "get_repro" => {
            let input = match parse_input(call.arguments) {
                Ok(input) => input,
                Err(error) => return tool_error(error),
            };
            tool_result(agent.get_repro(input).await.map_err(safe_error))
        }
        "triage_repro" => {
            let input = match validated_input::<TriageReproInput>(
                call.arguments,
                TriageReproInput::validate,
            ) {
                Ok(input) => input,
                Err(error) => return tool_error(error),
            };
            tool_result(agent.triage_repro(input).await.map_err(safe_error))
        }
        "run_repro" => {
            let input = match parse_input::<RunReproInput>(call.arguments) {
                Ok(input) => input,
                Err(error) => return tool_error(error),
            };
            match agent.run_repro(input).await.map_err(safe_error) {
                Ok(result) => tool_success(safe_run_result(result)),
                Err(error) => tool_error(error),
            }
        }
        "check_repros" => {
            let input = match validated_input::<CheckReprosInput>(
                call.arguments,
                CheckReprosInput::validate,
            ) {
                Ok(input) => input,
                Err(error) => return tool_error(error),
            };
            match agent.check_repros(input).await.map_err(safe_error) {
                Ok(result) => tool_success(safe_check_result(result)),
                Err(error) => tool_error(error),
            }
        }
        "keep_repro" => {
            let input = match parse_input(call.arguments) {
                Ok(input) => input,
                Err(error) => return tool_error(error),
            };
            tool_result(agent.keep_repro(input).await.map_err(safe_error))
        }
        "remove_repro" => {
            let input = match parse_input::<GetReproInput>(call.arguments) {
                Ok(input) => input,
                Err(error) => return tool_error(error),
            };
            let mut repository = FilesystemRepository::new(root);
            match remove_kept(&mut repository, input.repro_id).map_err(safe_error) {
                Ok(()) => tool_success(json!({"removed": true, "repro_id": input.repro_id})),
                Err(error) => tool_error(error),
            }
        }
        _ => tool_error(safe_error(Error::schema_invalid())),
    }
}

fn parse_input<Input: serde::de::DeserializeOwned>(value: Value) -> Result<Input, Error> {
    serde_json::from_value(value).map_err(|_| safe_error(Error::schema_invalid()))
}

fn validated_input<Input: serde::de::DeserializeOwned>(
    value: Value,
    validate: impl FnOnce(&Input) -> Result<(), Error>,
) -> Result<Input, Error> {
    let input = parse_input(value)?;
    validate(&input).map_err(safe_error)?;
    Ok(input)
}

fn tool_result(result: Result<impl serde::Serialize, Error>) -> Value {
    result.map_or_else(tool_error, tool_success)
}

fn tool_success(value: impl serde::Serialize) -> Value {
    let Ok(structured) = serde_json::to_value(value) else {
        return tool_error(safe_error(Error::schema_invalid()));
    };
    json!({
        "content": [{"type": "text", "text": "The tool call succeeded."}],
        "structuredContent": structured
    })
}

#[allow(clippy::needless_pass_by_value)]
fn tool_error(error: Error) -> Value {
    json!({
        "content": [{"type": "text", "text": error.message}],
        "isError": true,
        "structuredContent": {"error": error}
    })
}

#[allow(clippy::needless_pass_by_value)]
fn safe_error(error: Error) -> Error {
    structured_error(PublicErrorContext::General, &error)
}

fn safe_run_result(mut result: RunReproResult) -> RunReproResult {
    result.error = result.error.map(safe_error);
    if let Some(execution) = result.execution.as_mut() {
        execution.error = execution.error.take().map(safe_error);
    }
    result
}

fn safe_check_result(mut result: CheckReprosResult) -> CheckReprosResult {
    for check in result.errors.iter_mut().chain(&mut result.regressions) {
        check.error = check.error.take().map(safe_error);
    }
    result
}

fn tools() -> Vec<Value> {
    [
        (
            "list_repros",
            "List visible or kept Repros.",
            "list_repros_input",
            "list_repros_result",
        ),
        (
            "get_repro",
            "Get safe details for one Repro.",
            "get_repro_input",
            "get_repro_result",
        ),
        (
            "triage_repro",
            "Change the priority, assignee, or workflow for one Repro.",
            "triage_repro_input",
            "triage_repro_result",
        ),
        (
            "run_repro",
            "Run one Repro with the captured or developer program.",
            "run_repro_input",
            "run_repro_result",
        ),
        (
            "check_repros",
            "Check selected Repros or the complete kept set.",
            "check_repros_input",
            "check_repros_result",
        ),
        (
            "keep_repro",
            "Keep one Repro in the repository.",
            "keep_repro_input",
            "keep_repro_result",
        ),
        (
            "remove_repro",
            "Remove one kept Repro reference from the repository.",
            "remove_repro_input",
            "remove_repro_result",
        ),
    ]
    .into_iter()
    .map(|(name, description, input, output)| {
        json!({
            "name": name,
            "description": description,
            "inputSchema": schema(input),
            "outputSchema": schema(output)
        })
    })
    .collect()
}

fn schema(name: &str) -> Value {
    static SCHEMAS: OnceLock<BTreeMap<String, Value>> = OnceLock::new();
    SCHEMAS
        .get_or_init(|| {
            [
                "list_repros_input",
                "list_repros_result",
                "get_repro_input",
                "get_repro_result",
                "triage_repro_input",
                "triage_repro_result",
                "run_repro_input",
                "run_repro_result",
                "check_repros_input",
                "check_repros_result",
                "keep_repro_input",
                "keep_repro_result",
                "remove_repro_input",
                "remove_repro_result",
            ]
            .into_iter()
            .map(|name| (name.to_owned(), standalone_schema(name)))
            .collect()
        })
        .get(name)
        .cloned()
        .expect("named MCP schema")
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum SchemaSource {
    Mcp,
    Cloud,
    Core,
}

impl SchemaSource {
    fn document(self) -> &'static Value {
        static MCP_DOCUMENT: OnceLock<Value> = OnceLock::new();
        static CLOUD_DOCUMENT: OnceLock<Value> = OnceLock::new();
        static CORE_DOCUMENT: OnceLock<Value> = OnceLock::new();
        match self {
            Self::Mcp => MCP_DOCUMENT
                .get_or_init(|| serde_json::from_str(MCP_SCHEMAS).expect("valid MCP schemas")),
            Self::Cloud => CLOUD_DOCUMENT.get_or_init(|| {
                serde_json::from_str(CLOUD_API_SCHEMAS).expect("valid Cloud schemas")
            }),
            Self::Core => CORE_DOCUMENT
                .get_or_init(|| serde_json::from_str(CORE_SCHEMAS).expect("valid Core schemas")),
        }
    }

    const fn prefix(self) -> &'static str {
        match self {
            Self::Mcp => "mcp",
            Self::Cloud => "cloud",
            Self::Core => "core",
        }
    }
}

fn standalone_schema(name: &str) -> Value {
    let mut pending = vec![(SchemaSource::Mcp, name.to_owned())];
    let mut definitions = serde_json::Map::new();
    while let Some((source, definition_name)) = pending.pop() {
        let local_name = local_definition_name(source, &definition_name);
        if definitions.contains_key(&local_name) {
            continue;
        }
        let mut definition = source
            .document()
            .pointer(&format!("/$defs/{definition_name}"))
            .cloned()
            .expect("referenced normative schema definition");
        rewrite_schema_references(&mut definition, source, &mut pending);
        definitions.insert(local_name, definition);
    }
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$ref": format!("#/$defs/{}", local_definition_name(SchemaSource::Mcp, name)),
        "$defs": definitions
    })
}

fn rewrite_schema_references(
    value: &mut Value,
    current_source: SchemaSource,
    pending: &mut Vec<(SchemaSource, String)>,
) {
    match value {
        Value::Array(values) => {
            for value in values {
                rewrite_schema_references(value, current_source, pending);
            }
        }
        Value::Object(object) => {
            if let Some(reference) = object
                .get("$ref")
                .and_then(Value::as_str)
                .map(str::to_owned)
            {
                let (source, definition_name, suffix) =
                    resolve_schema_reference(current_source, &reference)
                        .expect("supported normative schema reference");
                let local_name = local_definition_name(source, definition_name);
                object.insert(
                    "$ref".to_owned(),
                    Value::String(format!("#/$defs/{local_name}{suffix}")),
                );
                pending.push((source, definition_name.to_owned()));
            }
            for (key, nested) in object.iter_mut() {
                if key != "$ref" {
                    rewrite_schema_references(nested, current_source, pending);
                }
            }
        }
        _ => {}
    }
}

fn resolve_schema_reference(
    current_source: SchemaSource,
    reference: &str,
) -> Option<(SchemaSource, &str, &str)> {
    let (source, pointer) = if let Some(pointer) = reference.strip_prefix('#') {
        (current_source, pointer)
    } else if let Some(pointer) =
        reference.strip_prefix("https://reproit.dev/spec/v1/cloud-api-schemas.json#")
    {
        (SchemaSource::Cloud, pointer)
    } else if let Some(pointer) =
        reference.strip_prefix("https://reproit.dev/spec/v1/schemas.json#")
    {
        (SchemaSource::Core, pointer)
    } else {
        let pointer = reference.strip_prefix("schemas.json#")?;
        (SchemaSource::Core, pointer)
    };
    let path = pointer.strip_prefix("/$defs/")?;
    let (definition_name, suffix) = path
        .split_once('/')
        .map_or((path, ""), |(name, _)| (name, &path[name.len()..]));
    (!definition_name.is_empty()).then_some((source, definition_name, suffix))
}

fn local_definition_name(source: SchemaSource, name: &str) -> String {
    format!("{}__{name}", source.prefix())
}

fn valid_request_id(value: &Value) -> bool {
    value.is_null() || value.is_string() || value.is_number()
}

#[allow(clippy::needless_pass_by_value)]
fn success(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

#[allow(clippy::needless_pass_by_value)]
fn protocol_error(id: Value, code: i32, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {"code": code, "message": message}
    })
}

async fn write_response(
    output: &mut (impl AsyncWrite + Unpin),
    mut response: Value,
) -> Result<(), Error> {
    let mut bytes = serde_json::to_vec(&response).map_err(|_| Error::schema_invalid())?;
    if bytes.len() > MAX_RESULT_BYTES {
        let id = response.get_mut("id").map_or(Value::Null, Value::take);
        response = protocol_error(id, -32603, "The MCP result exceeds 1 MiB.");
        bytes = serde_json::to_vec(&response).map_err(|_| Error::schema_invalid())?;
    }
    output.write_all(&bytes).await.map_err(io_error)?;
    output.write_all(b"\n").await.map_err(io_error)?;
    output.flush().await.map_err(io_error)
}

enum BoundedLine {
    Complete(Vec<u8>),
    OverLimit,
}

async fn read_bounded_line(
    input: &mut (impl AsyncBufRead + Unpin),
) -> Result<Option<BoundedLine>, Error> {
    let mut line = Vec::new();
    let mut over_limit = false;
    loop {
        let available = input.fill_buf().await.map_err(io_error)?;
        if available.is_empty() {
            if line.is_empty() && !over_limit {
                return Ok(None);
            }
            return Ok(Some(if over_limit {
                BoundedLine::OverLimit
            } else {
                BoundedLine::Complete(line)
            }));
        }
        let consumed = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |index| index + 1);
        let content = &available[..consumed];
        if !over_limit {
            let content = content.strip_suffix(b"\n").unwrap_or(content);
            if line.len().saturating_add(content.len()) > MAX_REQUEST_BYTES {
                over_limit = true;
                line.clear();
            } else {
                line.extend_from_slice(content);
            }
        }
        let ended = content.ends_with(b"\n");
        input.consume(consumed);
        if ended {
            if !over_limit && line.last() == Some(&b'\r') {
                line.pop();
            }
            return Ok(Some(if over_limit {
                BoundedLine::OverLimit
            } else {
                BoundedLine::Complete(line)
            }));
        }
    }
}

#[allow(clippy::needless_pass_by_value)]
fn io_error(error: std::io::Error) -> Error {
    let retryable = matches!(
        error.kind(),
        ErrorKind::Interrupted | ErrorKind::TimedOut | ErrorKind::WouldBlock
    );
    Error {
        code: ErrorCode::EvaluationError,
        message: "Repro It could not use the MCP input or output stream.".to_owned(),
        retryable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reproit_app::agent::{
        AgentFuture, CheckReprosResult, KeepReproResult, ListReprosResult, RunReproResult,
        TriageReproResult,
    };
    use reproit_cloud_api::ReproDetail;
    use tokio::io::AsyncReadExt as _;

    struct UnusedAgent;

    impl AgentOperations for UnusedAgent {
        fn check_repros(&self, _: CheckReprosInput) -> AgentFuture<'_, CheckReprosResult> {
            Box::pin(async { panic!("unexpected agent call") })
        }

        fn get_repro(&self, _: GetReproInput) -> AgentFuture<'_, ReproDetail> {
            Box::pin(async { panic!("unexpected agent call") })
        }

        fn keep_repro(&self, _: GetReproInput) -> AgentFuture<'_, KeepReproResult> {
            Box::pin(async { panic!("unexpected agent call") })
        }

        fn list_repros(&self, _: ListReprosInput) -> AgentFuture<'_, ListReprosResult> {
            Box::pin(async {
                Ok(ListReprosResult::Cloud {
                    next_cursor: None,
                    repros: Vec::new(),
                })
            })
        }

        fn run_repro(&self, _: RunReproInput) -> AgentFuture<'_, RunReproResult> {
            Box::pin(async { panic!("unexpected agent call") })
        }

        fn triage_repro(&self, _: TriageReproInput) -> AgentFuture<'_, TriageReproResult> {
            Box::pin(async { panic!("unexpected agent call") })
        }
    }

    #[tokio::test]
    async fn server_initializes_and_lists_the_seven_contract_tools() {
        let requests = concat!(
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n",
            "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n",
            "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\"}\n"
        );
        let input = BufReader::new(std::io::Cursor::new(requests.as_bytes()));
        let (writer, mut reader) = tokio::io::duplex(MAX_RESULT_BYTES * 2);
        serve_io(UnusedAgent, PathBuf::from("."), input, writer)
            .await
            .unwrap();
        let mut output = Vec::new();
        reader.read_to_end(&mut output).await.unwrap();
        let responses = output
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(responses.len(), 2);
        let tools = responses[1]["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 7);
        assert!(tools.iter().any(|tool| tool["name"] == "remove_repro"));
    }

    #[tokio::test]
    async fn oversized_request_is_rejected_without_unbounded_output() {
        let mut request = vec![b'a'; MAX_REQUEST_BYTES + 1];
        request.push(b'\n');
        let input = BufReader::new(std::io::Cursor::new(request));
        let (writer, mut reader) = tokio::io::duplex(4096);
        serve_io(UnusedAgent, PathBuf::from("."), input, writer)
            .await
            .unwrap();
        let mut output = Vec::new();
        reader.read_to_end(&mut output).await.unwrap();
        assert!(output.len() < 4096);
        let lines = output
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>();
        assert_eq!(lines.len(), 1);
        let response: Value = serde_json::from_slice(lines[0]).unwrap();
        assert_eq!(response["error"]["code"], -32600);
        assert_eq!(
            response["error"]["message"],
            "The MCP request exceeds 64 KiB."
        );
    }

    #[tokio::test]
    async fn list_tool_returns_the_typed_empty_result() {
        let result = call_tool(
            &UnusedAgent,
            std::path::Path::new("."),
            json!({
                "name": "list_repros",
                "arguments": {
                    "assignee_id": null,
                    "cursor": null,
                    "limit": 50,
                    "priority": [],
                    "scope": "cloud",
                    "workflow": ["OPEN", "REGRESSED"]
                }
            }),
        )
        .await;
        assert_eq!(result["structuredContent"]["scope"], "cloud");
        assert_eq!(result["structuredContent"]["repros"], json!([]));
        assert!(result.get("isError").is_none());
    }

    #[test]
    fn tool_errors_replace_internal_messages() {
        let error = Error::new(
            ErrorCode::EvaluationError,
            "The debugger credential and private route were exposed.",
        );
        let result = tool_error(safe_error(error));
        let encoded = serde_json::to_string(&result).unwrap();
        assert!(!encoded.contains("debugger credential"));
        assert!(!encoded.contains("private route"));
        assert!(result["isError"].as_bool().unwrap());
    }

    #[test]
    fn schema_tools_use_the_normative_machine_contract() {
        for tool in tools() {
            let name = tool["name"].as_str().unwrap();
            assert_eq!(tool["inputSchema"], schema(&format!("{name}_input")));
            assert_eq!(tool["outputSchema"], schema(&format!("{name}_result")));
        }
    }

    #[test]
    fn advertised_tool_schemas_resolve_every_reference_locally() {
        for tool in tools() {
            assert_local_schema(&tool["inputSchema"]);
            assert_local_schema(&tool["outputSchema"]);
        }
    }

    #[test]
    fn advertised_tools_fit_the_bounded_result() {
        let response = success(json!(1), json!({"tools": tools()}));
        assert!(serde_json::to_vec(&response).unwrap().len() <= MAX_RESULT_BYTES);
    }

    #[test]
    fn request_ids_reject_structured_values() {
        assert!(valid_request_id(&Value::Null));
        assert!(valid_request_id(&json!(1)));
        assert!(valid_request_id(&json!("one")));
        assert!(!valid_request_id(&json!({})));
    }

    #[test]
    fn cancellation_uses_the_exact_active_request_identity() {
        let cancellation = json!({
            "jsonrpc": "2.0",
            "method": "notifications/cancelled",
            "params": {"requestId": "call-1"}
        });
        assert_eq!(
            cancellation_identity(&cancellation).as_deref(),
            Some("\"call-1\"")
        );
        assert!(
            cancellation_identity(&json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "notifications/cancelled",
                "params": {"requestId": "call-1"}
            }))
            .is_none()
        );
    }

    #[test]
    fn duplicate_request_id_does_not_replace_the_active_cancellation() {
        let (original, original_receiver) = tokio::sync::watch::channel(false);
        let (duplicate, duplicate_receiver) = tokio::sync::watch::channel(false);
        let mut active = BTreeMap::new();
        assert!(register_active_request(
            &mut active,
            "1".to_owned(),
            original,
        ));
        assert!(!register_active_request(
            &mut active,
            "1".to_owned(),
            duplicate,
        ));
        active["1"].send(true).unwrap();
        assert!(*original_receiver.borrow());
        assert!(!*duplicate_receiver.borrow());
    }

    #[test]
    fn remove_uses_the_execution_call_limit() {
        let request = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {"name": "remove_repro", "arguments": {}}
        });
        assert_eq!(tool_call_identity(&request), Some((json!(1), true)));
    }

    fn assert_local_schema(schema: &Value) {
        fn visit(value: &Value, definitions: &serde_json::Map<String, Value>) {
            match value {
                Value::Array(values) => {
                    for value in values {
                        visit(value, definitions);
                    }
                }
                Value::Object(object) => {
                    if let Some(reference) = object.get("$ref").and_then(Value::as_str) {
                        let path = reference
                            .strip_prefix("#/$defs/")
                            .expect("advertised schema reference must be local");
                        let definition_name = path.split('/').next().unwrap();
                        assert!(
                            definitions.contains_key(definition_name),
                            "missing local schema definition {definition_name}"
                        );
                    }
                    for nested in object.values() {
                        visit(nested, definitions);
                    }
                }
                _ => {}
            }
        }
        let definitions = schema["$defs"].as_object().unwrap();
        visit(schema, definitions);
    }
}
