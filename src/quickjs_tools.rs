use anyhow::{Context, Result, anyhow, ensure};
use rmcp::{RoleClient, model::CallToolRequestParams, service::Peer};
use rquickjs::{
    AsyncContext, AsyncRuntime, CatchResultExt, Function, Promise,
    function::{Async, Func},
};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, btree_map::Entry},
    future::{Future, ready},
    pin::Pin,
    rc::Rc,
    time::Duration,
};
use tokio::time::{Instant, timeout_at};

const TIME_LIMIT: Duration = Duration::from_secs(120);
const MAX_CODE_BYTES: usize = 64 * 1024;
const MAX_OUTPUT_BYTES: usize = 64 * 1024;
const MAX_CALLS: usize = 32;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RunJs {
    code: String,
}

type Arguments = Map<String, Value>;
type RustToolFuture = Pin<Box<dyn Future<Output = Result<Value>>>>;
type RustToolHandler = dyn Fn(Arguments) -> RustToolFuture;

#[derive(Clone)]
enum ToolHandler {
    Mcp(Peer<RoleClient>),
    Rust(Rc<RustToolHandler>),
}

const RUST_NAMESPACE: &str = "rust";

/// Tool catalog and dispatch for the QuickJS backend.
#[derive(Default)]
pub struct QuickJsTools {
    catalog: Vec<Value>,
    handlers: Rc<BTreeMap<(String, String), ToolHandler>>,
}

impl QuickJsTools {
    /// Register a discovered MCP tool and the connection that serves it.
    pub fn register_mcp(
        &mut self,
        server: &str,
        tool: &rmcp::model::Tool,
        peer: Peer<RoleClient>,
    ) -> Result<()> {
        self.register(
            server,
            &tool.name,
            json!({
                "server": server, "name": tool.name,
                "description": tool.description, "inputSchema": tool.input_schema,
            }),
            ToolHandler::Mcp(peer),
        )
    }

    /// Register a synchronous in-process function under the `rust` namespace.
    /// Functions should return promptly. Use `register_async_rust` for async I/O.
    pub fn register_rust(
        &mut self,
        name: &str,
        description: &str,
        input_schema: Value,
        handler: impl Fn(Arguments) -> Result<Value> + 'static,
    ) -> Result<()> {
        self.register_async_rust(name, description, input_schema, move |args| {
            ready(handler(args))
        })
    }

    /// Register an async in-process function. Validate untrusted arguments in
    /// the handler. The schema is advertised to callers for discovery.
    /// Errors from the handler stop the script's tool dispatch conservatively.
    pub fn register_async_rust<F>(
        &mut self,
        name: &str,
        description: &str,
        input_schema: Value,
        handler: impl Fn(Arguments) -> F + 'static,
    ) -> Result<()>
    where
        F: Future<Output = Result<Value>> + 'static,
    {
        ensure!(
            input_schema.get("type").and_then(Value::as_str) == Some("object"),
            "Rust tool schema must declare an object"
        );
        self.register(
            RUST_NAMESPACE,
            name,
            json!({
                "server": RUST_NAMESPACE, "name": name,
                "description": description, "inputSchema": input_schema,
            }),
            ToolHandler::Rust(Rc::new(move |args| Box::pin(handler(args)))),
        )
    }

    fn register(
        &mut self,
        server: &str,
        name: &str,
        metadata: Value,
        handler: ToolHandler,
    ) -> Result<()> {
        ensure!(
            !server.trim().is_empty() && !name.trim().is_empty(),
            "Tool namespace and name must be nonempty"
        );
        if server == RUST_NAMESPACE {
            let is_rust = matches!(handler, ToolHandler::Rust(_));
            ensure!(
                !self.handlers.iter().any(|((namespace, _), registered)| {
                    namespace == RUST_NAMESPACE
                        && matches!(registered, ToolHandler::Rust(_)) != is_rust
                }),
                "The rust namespace cannot mix Rust tools with an MCP server"
            );
        }
        match Rc::make_mut(&mut self.handlers).entry((server.to_owned(), name.to_owned())) {
            Entry::Occupied(_) => anyhow::bail!("Duplicate tool {server}/{name}"),
            Entry::Vacant(entry) => {
                entry.insert(handler);
            }
        }
        self.catalog.push(metadata);
        Ok(())
    }

    /// Names of tools registered as in-process Rust functions.
    pub fn rust_tool_names(&self) -> impl Iterator<Item = &str> {
        self.handlers.iter().filter_map(|((_, name), handler)| {
            matches!(handler, ToolHandler::Rust(_)).then_some(name.as_str())
        })
    }

    pub fn definitions() -> Vec<Value> {
        vec![json!({
            "type": "function",
            "name": "run_js",
            "description": "Run JavaScript in a fresh sandbox. Use mcp.listTools() or mcp.listTools(server) for schemas, then await mcp.callTool(server, name, arguments). Use console.log to return results. Await all calls before finishing.",
            "parameters": {
                "type": "object", "properties": {"code": {"type": "string"}},
                "required": ["code"], "additionalProperties": false
            },
            "strict": true
        })]
    }

    pub async fn call(&self, name: &str, arguments: &str, report: bool) -> Result<Value> {
        ensure!(name == "run_js", "Unknown tool");
        let RunJs { code } = serde_json::from_str(arguments).context("Invalid run_js arguments")?;
        ensure!(
            !code.trim().is_empty() && code.len() <= MAX_CODE_BYTES,
            "Invalid run_js code length"
        );
        if report {
            println!("Tool: run_js");
        }

        let deadline = Instant::now() + TIME_LIMIT;
        let runtime = AsyncRuntime::new()?;
        runtime.set_memory_limit(32 * 1024 * 1024).await;
        runtime.set_max_stack_size(256 * 1024).await;
        runtime
            .set_interrupt_handler(Some(Box::new(move || Instant::now() >= deadline)))
            .await;
        let context = AsyncContext::full(&runtime).await?;
        let calls = Rc::new(Cell::new(0usize));
        let pending = Rc::new(Cell::new(0usize));
        let uncertain = Rc::new(Cell::new(false));
        let output = Rc::new(RefCell::new(String::new()));
        let handlers = self.handlers.clone();
        let run = context.async_with(async |ctx| -> Result<()> {
            let list = ctx.json_parse(serde_json::to_vec(&self.catalog)?)?;
            let pending_calls = pending.clone();
            let failed = uncertain.clone();
            let call = Func::from(Async(
                move |server: String, name: String, arguments: String| {
                    let handlers = handlers.clone();
                    let calls = calls.clone();
                    let pending = pending_calls.clone();
                    let uncertain = failed.clone();
                    async move {
                        let handler = handlers
                            .get(&(server, name.clone()))
                            .ok_or_else(|| js_error("Unknown tool"))?;
                        let args: Map<String, Value> = serde_json::from_str(&arguments)
                            .map_err(|_| js_error("Tool arguments must be an object"))?;
                        if uncertain.get() {
                            return Err(js_error("An earlier tool call has an unknown outcome"));
                        }
                        if calls.get() >= MAX_CALLS || Instant::now() >= deadline {
                            return Err(js_error("Tool call limit reached"));
                        }
                        calls.set(calls.get() + 1);
                        pending.set(pending.get() + 1);
                        let result = match handler {
                            ToolHandler::Mcp(peer) => match peer
                                .call_tool(CallToolRequestParams::new(name).with_arguments(args))
                                .await
                            {
                                Ok(result) => serde_json::to_string(&result)
                                    .map_err(|_| js_error("Cannot encode MCP result")),
                                Err(_) => {
                                    uncertain.set(true);
                                    Err(js_error("MCP call failed with an unknown outcome"))
                                }
                            },
                            ToolHandler::Rust(handler) => match handler(args).await {
                                Ok(value) => {
                                    Ok(json!({"content": [], "structuredContent": value})
                                        .to_string())
                                }
                                Err(_) => {
                                    uncertain.set(true);
                                    Err(js_error("Rust tool failed"))
                                }
                            },
                        };
                        pending.set(pending.get() - 1);
                        result
                    }
                },
            ));
            let captured_output = output.clone();
            let write = Func::from(move |line: String| -> rquickjs::Result<()> {
                let mut output = captured_output.borrow_mut();
                if line.len() >= MAX_OUTPUT_BYTES - output.len() {
                    return Err(js_error("Console output limit exceeded"));
                }
                output.push_str(&line);
                output.push('\n');
                Ok(())
            });
            let install: Function = ctx
                .eval(
                    r#"
                (catalog, call, write) => {
                    const parse = JSON.parse;
                    const stringify = JSON.stringify;
                    const toString = String;
                    globalThis.mcp = {
                        listTools: (server) => parse(stringify(catalog.filter(tool =>
                            server === undefined || tool.server === server))),
                        callTool: async (server, name, args = {}) => {
                            const result = parse(await call(server, name, stringify(args)));
                            if (result.isError) {
                                const error = new Error("MCP tool returned an error");
                                error.result = result;
                                throw error;
                            }
                            return result;
                        },
                    };
                    globalThis.console = {log: (...args) => write(args.map(value =>
                        typeof value === "string" ? value : stringify(value) ?? toString(value)
                    ).join(" "))};
                }
            "#,
                )
                .catch(&ctx)
                .map_err(|error| anyhow!("{error}"))?;
            install
                .call::<_, ()>((list, call, write))
                .catch(&ctx)
                .map_err(|error| anyhow!("{error}"))?;
            let promise: Promise = ctx
                .eval(format!("(async () => {{\n{code}\n}})()"))
                .catch(&ctx)
                .map_err(|error| anyhow!("{error}"))?;
            promise
                .into_future::<()>()
                .await
                .catch(&ctx)
                .map_err(|error| anyhow!("{error}"))
        });
        timeout_at(deadline, run)
            .await
            .context("run_js timed out")??;
        ensure!(
            pending.get() == 0,
            "run_js finished with pending tool calls"
        );
        ensure!(!uncertain.get(), "A tool call has an unknown outcome");
        let output = output.take();
        Ok(Value::String(output))
    }
}

fn js_error(message: &str) -> rquickjs::Error {
    rquickjs::Error::new_from_js_message("script", "tool bridge", message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn javascript_discovers_and_calls_registered_rust_functions() -> Result<()> {
        let mut tools = QuickJsTools::default();
        let schema = json!({
            "type": "object",
            "properties": {"value": {"type": "integer"}},
            "required": ["value"]
        });
        tools.register_rust("increment", "Add one", schema.clone(), |args| {
            let value = args
                .get("value")
                .and_then(Value::as_i64)
                .context("Expected integer")?;
            Ok(json!({"value": value + 1}))
        })?;
        tools.register_async_rust(
            "double",
            "Multiply by two",
            schema.clone(),
            |args| async move {
                tokio::task::yield_now().await;
                let value = args
                    .get("value")
                    .and_then(Value::as_i64)
                    .context("Expected integer")?;
                Ok(json!({"value": value * 2}))
            },
        )?;
        assert!(
            tools
                .register_rust("increment", "Duplicate", schema, |_| Ok(Value::Null))
                .is_err()
        );

        let code = r#"
            const names = mcp.listTools("rust").map(tool => tool.name);
            if (names.join(",") !== "increment,double") throw new Error("Wrong catalog");
            const first = await mcp.callTool("rust", "increment", {value: 20});
            const second = await mcp.callTool("rust", "double", first.structuredContent);
            console.log(second.structuredContent.value);
        "#;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let result =
            runtime.block_on(tools.call("run_js", &json!({"code": code}).to_string(), false))?;
        assert_eq!(result, json!("42\n"));
        Ok(())
    }
}
