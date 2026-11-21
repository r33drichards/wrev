use minimal_mcp_agent::agent;
#[cfg(unix)]
mod auth;
#[cfg(not(unix))]
#[path = "auth_headless.rs"]
mod auth;
mod config;
mod model;
#[cfg(feature = "quickjs-tools")]
use minimal_mcp_agent::quickjs_tools::QuickJsTools;
mod server;

use anyhow::{Context, Result, bail};
use rmcp::{
    ClientLifecycleMode, ClientServiceExt,
    model::ProtocolVersion,
    transport::{
        StreamableHttpClientTransport, streamable_http_client::StreamableHttpClientTransportConfig,
    },
};
use serde_json::Value;
use std::{
    env,
    io::{self, Write},
    time::Duration,
};
#[cfg(not(feature = "quickjs-tools"))]
use {
    rmcp::model::CallToolRequestParams,
    serde_json::json,
    sha2::{Digest, Sha256},
    std::collections::BTreeMap,
};

fn main() -> Result<()> {
    match env::args().nth(1).as_deref() {
        Some("login") => return auth::login(),
        Some("logout") => return auth::logout(),
        Some("models") => {
            for m in model::models(&config::chatgpt_token()?)? {
                println!(
                    "{}\t{}",
                    m["slug"].as_str().unwrap_or(""),
                    m["display_name"].as_str().unwrap_or("")
                );
            }
            return Ok(());
        }
        Some("migrate") => {
            #[cfg(feature = "mysql-backend")]
            {
                minimal_mcp_agent::durable_mysql::MySqlBackend::from_env()?.migrate()?;
                println!("MySQL durability schema ready");
                return Ok(());
            }
            #[cfg(not(feature = "mysql-backend"))]
            bail!("Rebuild with --features mysql-backend to migrate");
        }
        Some("tools" | "serve") => {}
        #[cfg(feature = "quickjs-tools")]
        Some("run-js") => {}
        Some("--help" | "-h") => {
            println!(
                "minimal-mcp-agent [login|logout|models|tools|serve|migrate]\nSet OPENAI_MODEL and MCP_URL, then run with no arguments.\nOptional MCP_TOKEN: bearer token for your MCP server (not your ChatGPT token).\nTool calls execute automatically; /quit exits."
            );
            #[cfg(feature = "quickjs-tools")]
            println!("run-js '<code>' executes JavaScript against the configured MCP servers.");
            return Ok(());
        }
        Some(other) => bail!("Unknown command: {other}"),
        None => {}
    }
    let hosted = env::args().nth(1).as_deref() == Some("serve");
    let durable = match env::var("DURABLE_BACKEND").as_deref() {
        Ok("mysql") => true,
        Ok("ephemeral") | Err(env::VarError::NotPresent) => false,
        _ => bail!("DURABLE_BACKEND must be ephemeral or mysql"),
    };
    if durable && !hosted {
        bail!("DURABLE_BACKEND=mysql requires serve mode with stable session_id/request_id");
    }
    #[cfg(not(feature = "mysql-backend"))]
    if durable {
        bail!("Rebuild with --features mysql-backend for durable serving");
    }
    #[cfg(feature = "mysql-backend")]
    let backend = if durable {
        Some(minimal_mcp_agent::durable_mysql::MySqlBackend::from_env()?)
    } else {
        None
    };
    if hosted {
        server::validate_environment()?;
    }
    #[cfg(feature = "quickjs-tools")]
    let mut quickjs = {
        let mut tools = QuickJsTools::default();
        register_rust_tools(&mut tools)?;
        tools
    };
    #[cfg(feature = "quickjs-tools")]
    let servers = if quickjs.rust_tool_names().next().is_some()
        && env::var_os("MCP_SERVERS_JSON").is_none()
        && env::var_os("MCP_URL").is_none()
    {
        Vec::new()
    } else {
        config::mcp_servers()?
    };
    #[cfg(not(feature = "quickjs-tools"))]
    let servers = config::mcp_servers()?;
    let runtime = tokio::runtime::Runtime::new()?;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let mut connections = Vec::new();
    let mut discovered = Vec::new();
    for (server_index, server) in servers.iter().enumerate() {
        let mut transport_config =
            StreamableHttpClientTransportConfig::with_uri(server.url.clone());
        if let Some(header) = config::bearer(server)? {
            transport_config = transport_config.auth_header(header);
        }
        if let Some(key) = &server.session_id_env {
            let session =
                env::var(key).with_context(|| format!("Missing session ID variable {key}"))?;
            if session.is_empty() {
                bail!("MCP session ID must not be empty");
            }
            transport_config.custom_headers.insert(
                reqwest::header::HeaderName::from_static("x-mcp-session-id"),
                reqwest::header::HeaderValue::from_str(&session)
                    .context("Invalid MCP session ID")?,
            );
        }
        let mcp = runtime.block_on(async {
            tokio::time::timeout(
                Duration::from_secs(30),
                ().serve_with_lifecycle(
                    StreamableHttpClientTransport::with_client(client.clone(), transport_config),
                    ClientLifecycleMode::Auto {
                        preferred_versions: vec![ProtocolVersion::LATEST],
                        legacy_version: Some(ProtocolVersion::LATEST_WITH_INITIALIZE),
                    },
                ),
            )
            .await
        })??;
        let tools = runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(30), mcp.list_all_tools()).await
        })??;
        #[cfg(feature = "quickjs-tools")]
        for tool in &tools {
            quickjs.register_mcp(&server.name, tool, mcp.peer().clone())?;
        }
        discovered.extend(tools.into_iter().map(|tool| (server_index, tool)));
        connections.push(mcp);
    }
    if env::args().nth(1).as_deref() == Some("tools") {
        #[cfg(feature = "quickjs-tools")]
        for tool in quickjs.rust_tool_names() {
            println!("rust: {tool}");
        }
        for (server_index, tool) in &discovered {
            if servers.len() == 1 {
                println!("{}", tool.name);
            } else {
                println!("{}: {}", servers[*server_index].name, tool.name);
            }
        }
        for mcp in connections {
            runtime.block_on(mcp.cancel())?;
        }
        return Ok(());
    }
    #[cfg(feature = "quickjs-tools")]
    if env::args().nth(1).as_deref() == Some("run-js") {
        let code = env::args()
            .nth(2)
            .context("Usage: minimal-mcp-agent run-js '<code>'")?;
        let arguments = serde_json::json!({"code": code}).to_string();
        let output = runtime.block_on(quickjs.call("run_js", &arguments, false));
        drop(quickjs);
        for mcp in connections {
            runtime.block_on(mcp.cancel())?;
        }
        print!(
            "{}",
            output?.as_str().context("Expected run_js text output")?
        );
        return Ok(());
    }
    let selected = env::var("OPENAI_MODEL")
        .context("Set OPENAI_MODEL (run `minimal-mcp-agent models` to list choices)")?;
    let token = config::chatgpt_token()?;
    if !model::models(&token)?.iter().any(|m| m["slug"] == selected) {
        bail!("OPENAI_MODEL is not listed for this account");
    }
    // Stable aliases keep arbitrary MCP names compatible with model tool-name restrictions.
    #[cfg(not(feature = "quickjs-tools"))]
    let registry: BTreeMap<String, _> = discovered
        .into_iter()
        .map(|tool| (tool_alias(&servers[tool.0].name, &tool.1.name), tool))
        .collect();
    #[cfg(not(feature = "quickjs-tools"))]
    let definitions: Vec<_> = registry.iter().map(|(alias, (server_index, tool))|json!({
        "type":"function", "name":alias, "description":format!("{}/{}: {}", servers[*server_index].name, tool.name, tool.description.as_deref().unwrap_or("")),
        "parameters":tool.input_schema, "strict":false })).collect();
    #[cfg(feature = "quickjs-tools")]
    let definitions = QuickJsTools::definitions();
    let mut prompt_llm = |messages: &[Value]| {
        model::prompt(&config::chatgpt_token()?, &selected, messages, &definitions)
    };
    #[cfg(not(feature = "quickjs-tools"))]
    let mut call_tool = |alias: &str, args: &str| -> Result<Value> {
        let (server_index, tool) = registry.get(alias).context("Unknown MCP tool")?;
        let args: Value = serde_json::from_str(args).context("Invalid tool arguments")?;
        let arguments = args
            .as_object()
            .context("Tool arguments must be an object")?
            .clone();
        if !hosted {
            println!("Tool: {}\nArguments: {}", tool.name, args);
        }
        let result = runtime.block_on(async {
            tokio::time::timeout(
                Duration::from_secs(120),
                connections[*server_index].call_tool(
                    CallToolRequestParams::new(tool.name.to_string()).with_arguments(arguments),
                ),
            )
            .await
        })??;
        Ok(serde_json::to_value(result)?)
    };
    #[cfg(feature = "quickjs-tools")]
    let mut call_tool =
        |name: &str, args: &str| runtime.block_on(quickjs.call(name, args, !hosted));
    let mut print_text = |text: &str| {
        if !hosted {
            println!("Agent: {text}");
        }
    };
    #[cfg(feature = "mysql-backend")]
    if let Some(backend) = backend {
        server::serve_durable(|session_id, request_id, input| {
            minimal_mcp_agent::durable::run_durable_turn(
                &backend,
                minimal_mcp_agent::durable::Turn {
                    session_id,
                    request_id,
                    input,
                    max_rounds: 20,
                },
                &mut prompt_llm,
                &mut call_tool,
                &mut print_text,
                &mut |_, _: &agent::Checkpoint| Ok(()),
            )
        })?;
        return Ok(());
    }
    let mut run = |input: &str, messages: &mut Vec<Value>| -> Result<()> {
        agent::run_turn(
            messages,
            input,
            20,
            &mut prompt_llm,
            &mut call_tool,
            &mut print_text,
        )
    };
    if hosted {
        server::serve(&mut run)?;
    } else {
        println!(
            "Connected: {} MCP tools. /quit exits. Tool calls execute automatically.",
            definitions.len()
        );
        let mut messages = vec![];
        while let Some(input) = read_line("You: ")? {
            if input == "/quit" {
                break;
            }
            if input.is_empty() {
                continue;
            }
            if let Err(error) = run(&input, &mut messages) {
                eprintln!("Error: {error}");
            }
        }
    }
    for mcp in connections {
        runtime.block_on(mcp.cancel())?;
    }
    Ok(())
}

/// Register application-specific Rust tools before opening MCP connections.
#[cfg(feature = "quickjs-tools")]
fn register_rust_tools(_tools: &mut QuickJsTools) -> Result<()> {
    Ok(())
}

fn read_line(prompt: &str) -> Result<Option<String>> {
    print!("{prompt}");
    io::stdout().flush()?;
    let mut line = String::new();
    if io::stdin().read_line(&mut line)? == 0 {
        return Ok(None);
    }
    Ok(Some(line.trim_end().to_string()))
}

#[cfg(not(feature = "quickjs-tools"))]
fn tool_alias(server: &str, tool: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(server.as_bytes());
    digest.update([0]);
    digest.update(tool.as_bytes());
    // Stable across discovery ordering, additions and process restarts.
    format!("mcp_{:x}", digest.finalize())[..64].to_string()
}

#[cfg(all(test, not(feature = "quickjs-tools")))]
mod tool_identity_tests {
    use super::tool_alias;
    #[test]
    fn aliases_are_stable_namespaced_and_bounded() {
        assert_eq!(tool_alias("one", "exec"), tool_alias("one", "exec"));
        assert_ne!(tool_alias("one", "exec"), tool_alias("two", "exec"));
        assert_ne!(tool_alias("ab", "c"), tool_alias("a", "bc"));
        assert_eq!(tool_alias("one", &"a".repeat(500)).len(), 64);
    }
}
