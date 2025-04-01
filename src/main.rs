use extism::*;
use extism::{Manifest, Wasm};
use rpc_router::{
    Error, Handler, HandlerResult, Request, Router as RpcRouter, RouterBuilder, RpcResource,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::io;
use std::{collections::HashMap, sync::Arc};
use tokio::sync::RwLock;

mod config;
mod r#mod;
mod oci;
mod prompts;
mod resources;
mod tools;
mod types;

use r#mod::*;
use oci::*;
use prompts::{prompts_get, prompts_list};
use resources::{resource_read, resources_list};
use tools::{tools_call, tools_list};
use types::*;

use clap::Parser;
use std::path::PathBuf;

#[derive(Parser)]
#[command(author = "Tuan Anh Tran <me@tuananh.org>", version, about, long_about = None)]
struct Cli {
    /// Path to a config file
    #[arg(short = 'c', long, value_name = "FILE")]
    config_file: Option<PathBuf>,
    
    /// JSON string containing configuration
    #[arg(long, value_name = "JSON")]
    config_json: Option<String>,

    /// Log output file path
    #[arg(
        short = 'l',
        long = "log-file",
        value_name = "PATH",
        env = "HYPER_MCP_LOG_FILE"
    )]
    log_file: Option<String>,

    #[arg(
        long = "log-level",
        value_name = "LEVEL",
        env = "HYPER_MCP_LOG_LEVEL",
        default_value = "info"
    )]
    log_level: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Config {
    plugins: Vec<PluginConfig>,
}

#[derive(Debug, Serialize, Deserialize)]
struct RuntimeConfig {
    allowed_host: Option<String>,
    allowed_paths: Option<Vec<String>>,
}

#[derive(Debug, Serialize, Deserialize)]
struct PluginConfig {
    name: String,
    path: String,
    runtime_config: Option<RuntimeConfig>,
}

#[derive(Clone, RpcResource)]
pub struct PluginManager {
    plugins: Arc<RwLock<HashMap<String, Plugin>>>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // Get default config path in the user's config directory
    let default_config_path = dirs::config_dir()
        .map(|mut path| {
            path.push("hyper-mcp");
            path.push("config.json");
            path
        })
        .unwrap();

    if let Some(parent) = default_config_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Setup default log file path in user's data directory
    let default_log_path = dirs::state_dir()
        .or_else(dirs::data_local_dir)
        .map(|mut path| {
            path.push("hyper-mcp");
            path.push("logs");
            path.push("hyper-mcp.log");
            path
        })
        .unwrap();

    let log_file = cli
        .log_file
        .unwrap_or_else(|| default_log_path.to_str().unwrap().to_string());

    // Create log directory if it doesn't exist
    if let Some(log_dir) = PathBuf::from(&log_file).parent() {
        std::fs::create_dir_all(log_dir)?;
    }

    // Initialize logging
    config::init_logger(Some(&log_file), cli.log_level.as_deref())?;
    log::info!("Logging initialized to: {}", log_file);

    // We will print this so user know how to debug. Everything else will be logged to the log file to ensure clean stdio communication.
    println!("hyper-mcp started. Logs will be written to: {}", log_file);

    // Handle JSON string input
    let config: Config = if let Some(config_json) = &cli.config_json {
        log::info!("Using config from JSON string");
        match serde_json::from_str(config_json) {
            Ok(config) => config,
            Err(e) => {
                log::error!("Failed to parse JSON config: {}", e);
                return Err(anyhow::anyhow!("Failed to parse JSON config: {}", e));
            }
        }
    } 
    // Handle config file input
    else if let Some(config_file) = &cli.config_file {
        log::info!("Using config file at {}", config_file.display());
        let config_content = tokio::fs::read_to_string(config_file).await.map_err(|e| {
            log::error!("Failed to read config file at {:?}: {}", config_file, e);
            e
        })?;
        serde_json::from_str(&config_content)?
    }
    // Use default config path
    else {
        log::info!("Using default config file at {}", default_config_path.display());
        let config_content = tokio::fs::read_to_string(&default_config_path).await.map_err(|e| {
            log::error!("Failed to read default config file at {:?}: {}", default_config_path, e);
            e
        })?;
        serde_json::from_str(&config_content)?
    };

    let plugins = Arc::new(RwLock::new(HashMap::new()));

    for plugin_cfg in &config.plugins {
        let wasm_content = if plugin_cfg.path.starts_with("http") {
            reqwest::get(&plugin_cfg.path)
                .await?
                .bytes()
                .await?
                .to_vec()
        } else if plugin_cfg.path.starts_with("oci") {
            // ref should be like oci://tuananh/qr-code
            let image_reference = plugin_cfg.path.strip_prefix("oci://").unwrap();
            let target_file_path = "/plugin.wasm";
            let mut hasher = Sha256::new();
            hasher.update(image_reference);
            let hash = hasher.finalize();
            let short_hash = &hex::encode(hash)[..7];
            let cache_dir = dirs::cache_dir()
                .map(|mut path| {
                    path.push("hyper-mcp");
                    path
                })
                .unwrap();
            std::fs::create_dir_all(&cache_dir)?;

            let local_output_path =
                cache_dir.join(format!("{}-{}.wasm", plugin_cfg.name, short_hash));
            let local_output_path = local_output_path.to_str().unwrap();

            if let Err(e) =
                pull_and_extract_oci_image(image_reference, target_file_path, local_output_path)
                    .await
            {
                eprintln!("Error pulling oci plugin: {}", e);
            }
            log::info!(
                "cache plugin `{}` to : {}",
                plugin_cfg.name,
                local_output_path
            );
            tokio::fs::read(local_output_path).await?
        } else {
            tokio::fs::read(&plugin_cfg.path).await?
        };

        let mut manifest = Manifest::new([Wasm::data(wasm_content)]);
        if let Some(runtime_cfg) = &plugin_cfg.runtime_config {
            log::info!("runtime_cfg: {:?}", runtime_cfg);
            if let Some(host) = &runtime_cfg.allowed_host {
                manifest = manifest.with_allowed_host(host);
            }
            if let Some(paths) = &runtime_cfg.allowed_paths {
                for path in paths {
                    // path will be available in the plugin with exact same path
                    manifest = manifest.with_allowed_path(path.clone(), path.clone());
                }
            }
        }
        let plugin = Plugin::new(&manifest, [], true).unwrap();

        plugins
            .write()
            .await
            .insert(plugin_cfg.name.clone(), plugin);

        log::info!("Loaded plugin {}", plugin_cfg.name);
    }

    // setup router
    let rpc_router = build_rpc_router(plugins.clone());
    let input = io::stdin();
    let mut line = String::new();

    while input.read_line(&mut line).unwrap() != 0 {
        let line = std::mem::take(&mut line);
        log::debug!("received line: {}", line);
        if !line.is_empty() {
            if let Ok(json_value) = serde_json::from_str::<serde_json::Value>(&line) {
                // notifications, no response required
                if json_value.is_object() && json_value.get("id").is_none() {
                    if let Some(method) = json_value.get("method") {
                        if method == "notifications/initialized" {
                            notifications_initialized();
                        } else if method == "notifications/cancelled" {
                            let params_value = json_value.get("params").unwrap();
                            let cancel_params: CancelledNotification =
                                serde_json::from_value(params_value.clone()).unwrap();
                            notifications_cancelled(cancel_params);
                        }
                    }
                } else if let Ok(mut rpc_request) = Request::from_value(json_value) {
                    // NOTE: because params is not required in ping but we need it in json-rpc
                    // https://github.com/modelcontextprotocol/specification/blob/ce55bba19fc1f5a343e45ef1b47f9ccf1801d318/docs/specification/2024-11-05/basic/utilities/ping.md#message-format
                    if rpc_request.method == "ping" {
                        rpc_request.params =
                            Some(serde_json::Value::Object(serde_json::Map::new()));
                    }

                    let id = rpc_request.id.clone();
                    match rpc_router.call(rpc_request).await {
                        Ok(call_response) => {
                            if !call_response.value.is_null() {
                                let response =
                                    JsonRpcResponse::new(id, call_response.value.clone());
                                let response_json = serde_json::to_string(&response).unwrap();
                                log::debug!("ok: {}", response_json);
                                println!("{}", response_json);
                            }
                        }
                        Err(error) => match &error.error {
                            Error::Handler(handler) => {
                                if let Some(error_value) = handler.get::<serde_json::Value>() {
                                    let json_error = json!({
                                        "jsonrpc": "2.0",
                                        "error": error_value,
                                        "id": id
                                    });
                                    let response = serde_json::to_string(&json_error).unwrap();
                                    log::error!("error: {}", response);
                                    println!("{}", response);
                                }
                            }
                            _ => {
                                log::error!("Unexpected error {:?}", error);
                                let json_error = JsonRpcError::new(id, -1, "Invalid json-rpc call");
                                let response = serde_json::to_string(&json_error).unwrap();
                                println!("{}", response);
                            }
                        },
                    }
                }
            }
        }
    }
    Ok(())
}

fn build_rpc_router(plugins: Arc<RwLock<HashMap<String, Plugin>>>) -> RpcRouter {
    let plugins_clone = plugins.clone();

    RouterBuilder::default()
        .append_resource(PluginManager {
            plugins: plugins_clone,
        })
        .append_dyn("initialize", initialize.into_dyn())
        .append_dyn("ping", ping.into_dyn())
        .append_dyn("logging/setLevel", logging_set_level.into_dyn())
        .append_dyn("roots/list", roots_list.into_dyn())
        .append_dyn("prompts/list", prompts_list.into_dyn())
        .append_dyn("prompts/get", prompts_get.into_dyn())
        .append_dyn("resources/list", resources_list.into_dyn())
        .append_dyn("resources/read", resource_read.into_dyn())
        .append_dyn("tools/list", tools_list.into_dyn())
        .append_dyn("tools/call", tools_call.into_dyn())
        .build()
}

pub fn notifications_initialized() {}
pub fn notifications_cancelled(_params: CancelledNotification) {}

pub async fn initialize(_request: InitializeRequest) -> HandlerResult<InitializeResponse> {
    let result = InitializeResponse {
        protocol_version: PROTOCOL_VERSION.to_string(),
        server_info: Implementation {
            name: SERVER_NAME.to_string(),
            version: SERVER_VERSION.to_string(),
        },
        capabilities: ServerCapabilities {
            experimental: None,
            prompts: Some(PromptCapabilities::default()),
            resources: None,
            tools: Some(json!({})),
            roots: None,
            sampling: None,
            logging: None,
        },
        instructions: None,
    };
    Ok(result)
}

pub async fn ping(_request: PingRequest) -> HandlerResult<EmptyResult> {
    Ok(EmptyResult {})
}

pub async fn logging_set_level(_request: SetLevelRequest) -> HandlerResult<LoggingResponse> {
    Ok(LoggingResponse {})
}

pub async fn roots_list(_request: Option<ListRootsRequest>) -> HandlerResult<ListRootsResult> {
    let response = ListRootsResult {
        roots: vec![Root {
            name: "my project".to_string(),
            url: "file:///home/user/projects/my-project".to_string(),
        }],
    };
    Ok(response)
}
