use anda_core::{BoxError, ModelEffort, Principal};
use anda_db::{database::DBConfig, storage::StorageConfig};
use anda_engine::{
    management::{BaseManagement, Visibility},
    model::{ModelConfig, Models, Proxy, request_client_builder, reqwest},
};
use anda_object_store::MetaStoreBuilder;
use axum::{Router, error_handling::HandleErrorLayer, routing};
use clap::{Parser, Subcommand};
use http::StatusCode;
use mimalloc::MiMalloc;
use object_store::{
    ObjectStore,
    aws::{AmazonS3Builder, S3CopyIfNotExists},
    local::LocalFileSystem,
    memory::InMemory,
};
use std::{collections::BTreeSet, net::SocketAddr, sync::Arc, time::Duration};
use structured_logger::{Builder, async_json::new_writer, get_env_level};
use tokio::{signal, task::JoinHandle};
use tokio_util::sync::CancellationToken;
use tower::{ServiceBuilder, limit::GlobalConcurrencyLimitLayer, load_shed::LoadShedLayer};
use tower_http::{
    compression::CompressionLayer,
    cors::{AllowHeaders, AllowMethods, CorsLayer},
};

use anda_brain::{
    agents::SELF_USER_ID,
    handler::*,
    mcp::{McpHttpServerConfig, McpServerConfig, build_streamable_http_service, run_stdio_server},
    parse_ed25519_pubkeys,
    space::AppState,
    types::ModelConfig as BrainModelConfig,
};

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

const APP_NAME: &str = env!("CARGO_PKG_NAME");
const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Parser, Clone)]
#[command(author, version, about, long_about = None)]
struct Cli {
    /// Path to a versioned JSON runtime configuration; selects compiled adapters.
    #[arg(long, env = "BRAIN_RUNTIME_CONFIG")]
    runtime_config: Option<std::path::PathBuf>,
    /// Port to listen on
    #[clap(long, env = "LISTEN_ADDR", default_value = "127.0.0.1:8042")]
    addr: String,

    /// API key
    #[arg(long, env = "ED25519_PUBKEYS", default_value = "")]
    ed25519_pubkeys: String,

    /// AI model family (e.g., "gemini", "anthropic", "openai")
    #[arg(long, env = "MODEL_FAMILY", default_value = "anthropic")]
    model_family: String,

    /// AI model name (e.g., "gemini-3-flash-preview", "claude-sonnet-4-6")
    #[arg(long, env = "MODEL_NAME", default_value = "deepseek-v4-pro")]
    model_name: String,

    /// API key for AI model
    #[arg(long, env = "MODEL_API_KEY", default_value = "")]
    model_api_key: String,

    #[arg(long, env = "MODEL_CONTEXT_WINDOW", default_value_t = 400000)]
    model_context_window: usize,

    #[arg(long, env = "MODEL_MAX_OUTPUT", default_value_t = 384000)]
    model_max_output: usize,

    /// API base URL for AI model
    #[arg(
        long,
        env = "MODEL_API_BASE",
        default_value = "https://api.deepseek.com/anthropic"
    )]
    model_api_base: String,

    /// Optional HTTPS proxy URL (e.g., "http://localhost:8080")
    #[arg(long, env = "HTTPS_PROXY")]
    https_proxy: Option<String>,

    #[arg(long, env = "SHARDING_IDX", default_value_t = 0)]
    sharding_idx: u32,

    /// Manager principal IDs, separated by comma
    #[arg(long, env = "MANAGERS", default_value = "")]
    managers: String,

    /// CORS allowed origins, separated by comma. Use "*" to allow all
    /// origins — note that "*" maps to CorsLayer::very_permissive(), which
    /// answers any origin with Access-Control-Allow-Credentials: true, i.e.
    /// any website may issue credentialed cross-site requests to this API.
    /// The API carries credentials in Bearer headers (never cookies), so
    /// browsers won't attach them automatically, but prefer an explicit
    /// origin list on deployments that don't need fully open CORS.
    #[arg(long, env = "CORS_ORIGINS", default_value = "")]
    cors_origins: String,

    /// Global cap on in-flight HTTP requests; excess requests are shed with
    /// 503 instead of queueing without bound. The default is deliberately
    /// generous so normal multi-tenant traffic never hits it; it only bounds
    /// pathological floods.
    #[arg(long, env = "HTTP_MAX_CONCURRENCY", default_value_t = 1024)]
    http_max_concurrency: usize,

    /// Cap on actual model calls across Spaces, including background work and
    /// compaction. Separately caps admitted HTTP/MCP model-driving requests;
    /// excess requests receive 429, admitted calls wait for a model slot.
    #[arg(long, env = "LLM_MAX_CONCURRENCY", default_value_t = 64)]
    llm_max_concurrency: usize,

    /// Enable the Streamable HTTP MCP endpoint mounted with the HTTP service
    #[arg(
        long,
        env = "MCP_HTTP_ENABLED",
        default_value_t = true,
        action = clap::ArgAction::Set
    )]
    mcp_http_enabled: bool,

    /// HTTP path prefix for remote MCP clients. Clients connect to {prefix}/{space_id}
    #[arg(long, env = "MCP_HTTP_PATH_PREFIX", default_value = "/mcp")]
    mcp_http_path_prefix: String,

    /// Allowed Host values for remote MCP requests, separated by comma. Use "*" to allow all.
    #[arg(long, env = "MCP_HTTP_ALLOWED_HOSTS", default_value = "")]
    mcp_http_allowed_hosts: String,

    /// Allowed browser Origin values for remote MCP requests, separated by comma. Use "*" to allow all.
    #[arg(long, env = "MCP_HTTP_ALLOWED_ORIGINS", default_value = "")]
    mcp_http_allowed_origins: String,

    /// Create remote MCP spaces on first use when they do not exist
    #[arg(long, env = "MCP_HTTP_AUTO_CREATE_SPACE", default_value_t = false)]
    mcp_http_auto_create_space: bool,

    /// Tier used when remote MCP auto-creates a memory space
    #[arg(long, env = "MCP_HTTP_AUTO_CREATE_TIER", default_value_t = 1)]
    mcp_http_auto_create_tier: u32,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand, Clone)]
pub enum Commands {
    Local {
        #[clap(long, env = "LOCAL_DB_PATH", default_value = "./db")]
        db: String,
    },
    Aws {
        #[arg(long, env = "AWS_BUCKET")]
        bucket: String,

        #[arg(long, env = "AWS_REGION")]
        region: String,
    },
    Mcp {
        /// Memory space exposed through MCP tools
        #[arg(long, env = "MCP_SPACE_ID")]
        space_id: String,

        /// Optional CWT or space token used to authorize MCP tool calls
        #[arg(long = "mcp-auth-token", env = "MCP_AUTH_TOKEN")]
        auth_token: Option<String>,

        /// Create the MCP memory space if it does not exist
        #[arg(
            long = "mcp-auto-create-space",
            env = "MCP_AUTO_CREATE_SPACE",
            default_value_t = false
        )]
        auto_create_space: bool,

        /// Tier used when --mcp-auto-create-space creates the memory space
        #[arg(
            long = "mcp-auto-create-tier",
            env = "MCP_AUTO_CREATE_TIER",
            default_value_t = 1
        )]
        auto_create_tier: u32,

        #[command(subcommand)]
        storage: Option<StorageCommand>,
    },
}

#[derive(Subcommand, Clone)]
pub enum StorageCommand {
    Local {
        #[clap(long, env = "LOCAL_DB_PATH", default_value = "./db")]
        db: String,
    },
    Aws {
        #[arg(long, env = "AWS_BUCKET")]
        bucket: String,

        #[arg(long, env = "AWS_REGION")]
        region: String,
    },
}

#[cfg(test)]
#[derive(Clone, Copy, Debug)]
struct AnyHost;

#[cfg(test)]
impl PartialEq<&str> for AnyHost {
    fn eq(&self, _other: &&str) -> bool {
        true
    }
}

fn build_http_client(cli: &Cli) -> Result<reqwest::Client, BoxError> {
    let mut http_client = request_client_builder()
        .https_only(false)
        .timeout(Duration::from_secs(600));
    // grcov-excl-stop
    if let Some(proxy) = &cli.https_proxy {
        http_client = http_client.proxy(Proxy::all(proxy)?);
    }
    Ok(http_client.build()?)
}

fn parse_managers(input: &str) -> Result<BTreeSet<Principal>, BoxError> {
    let mut managers = BTreeSet::new();
    // Tolerate whitespace around entries and stray commas ("a, b," would
    // otherwise fail on " b" and ""); a malformed id still fails startup.
    for id in input.split(',').map(str::trim).filter(|id| !id.is_empty()) {
        managers.insert(Principal::from_text(id)?);
    }
    Ok(managers)
}

fn model_config_from_cli(cli: &Cli) -> ModelConfig {
    ModelConfig {
        family: cli.model_family.clone(),
        model: cli.model_name.clone(),
        api_key: cli.model_api_key.clone(),
        api_base: cli.model_api_base.clone(),
        context_window: cli.model_context_window,
        max_output: cli.model_max_output,
        disabled: cli.model_api_key.is_empty(),
        labels: vec![],
        bearer_auth: false,
        stream: false,
        effort: Some(ModelEffort::High),
    }
}

fn default_db_config() -> DBConfig {
    DBConfig {
        name: "test".to_string(), // This is placeholder. The real name is space_id.
        description: "Anda Brain database".to_string(),
        storage: StorageConfig {
            cache_max_capacity: 100000,
            cache_max_bytes: None,
            compress_level: 3,
            object_chunk_size: 256 * 1024,
            bucket_overload_size: 1024 * 1024,
            max_small_object_size: 1024 * 1024 * 10,
        },
        lock: None,
    }
}

fn split_csv_values(input: &str) -> Vec<String> {
    input
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .collect()
}

fn normalize_http_path_prefix(prefix: &str) -> String {
    let trimmed = prefix.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        "/mcp".to_string()
    } else if trimmed.starts_with('/') {
        trimmed.to_string()
    } else {
        format!("/{trimmed}")
    }
}

fn mcp_http_config_from_cli(cli: &Cli) -> McpHttpServerConfig {
    McpHttpServerConfig {
        path_prefix: normalize_http_path_prefix(&cli.mcp_http_path_prefix),
        auto_create_space: cli.mcp_http_auto_create_space,
        auto_create_tier: cli.mcp_http_auto_create_tier,
        allowed_hosts: split_csv_values(&cli.mcp_http_allowed_hosts),
        allowed_origins: split_csv_values(&cli.mcp_http_allowed_origins),
        stateful_mode: true,
        json_response: false,
        sse_keep_alive_secs: Some(15),
    }
}

/// Wraps `router` in a shared in-flight request cap: at most `max_in_flight`
/// requests run at once and excess requests are shed immediately with
/// `shed_status` instead of queueing without bound.
///
/// `GlobalConcurrencyLimitLayer` shares one semaphore across every route of
/// the router (axum applies a layer to each route separately, so the plain
/// `ConcurrencyLimitLayer` would be a per-route limit). `LoadShedLayer`
/// turns "no permit available" into an error and `HandleErrorLayer` maps
/// that error to an HTTP response, which axum requires: its services must
/// be infallible.
fn with_concurrency_limit<S>(
    router: Router<S>,
    max_in_flight: usize,
    shed_status: StatusCode,
) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    // `max(1)` keeps a misconfigured `0` from shedding every request.
    with_shared_concurrency_limit(
        router,
        Arc::new(tokio::sync::Semaphore::new(max_in_flight.max(1))),
        shed_status,
    )
}

/// [`with_concurrency_limit`] over a caller-owned semaphore, so the same
/// budget can be drained by non-router work (the MCP LLM tools share the
/// LLM routes' semaphore).
fn with_shared_concurrency_limit<S>(
    router: Router<S>,
    semaphore: Arc<tokio::sync::Semaphore>,
    shed_status: StatusCode,
) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    router.layer(
        ServiceBuilder::new()
            .layer(HandleErrorLayer::new(
                move |err: tower::BoxError| async move {
                    if err.is::<tower::load_shed::error::Overloaded>() {
                        (
                            shed_status,
                            "too many concurrent requests, retry later".to_string(),
                        )
                    } else {
                        (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            format!("unhandled middleware error: {err}"),
                        )
                    }
                },
            ))
            .layer(LoadShedLayer::new())
            .layer(GlobalConcurrencyLimitLayer::with_semaphore(semaphore)),
    )
}

// grcov-excl-start: route registration is verified through direct handler tests; axum's builder chain gives low-value line coverage.
fn build_router(
    app_state: AppState,
    cli: &Cli,
    cancel_token: CancellationToken,
) -> Router<AppState> {
    // Request admission is separate from model permits: Formation and
    // Maintenance return before their background completions finish.
    let llm_router = with_shared_concurrency_limit(
        Router::new()
            .route("/v1/{space_id}/formation", routing::post(post_formation))
            .route("/v1/{space_id}/recall", routing::post(post_recall))
            .route(
                "/v1/{space_id}/recall_structured",
                routing::post(post_recall_structured),
            )
            .route(
                "/v1/{space_id}/maintenance",
                routing::post(post_maintenance),
            )
            .route(
                "/v1/{space_id}/management/shadow_eval",
                routing::post(post_shadow_eval),
            )
            .route(
                "/v1/{space_id}/wiki/digest",
                routing::post(post_wiki_digest),
            ),
        app_state.llm_request_semaphore().clone(),
        StatusCode::TOO_MANY_REQUESTS,
    );

    let mut router = Router::new()
        .route("/favicon.ico", routing::get(favicon))
        .route("/apple-touch-icon.webp", routing::get(apple_touch_icon))
        .route("/info", routing::get(get_information))
        .route("/SKILL.md", routing::get(get_skill))
        .route("/v1/{space_id}/info", routing::get(get_info))
        .route("/v1/{space_id}/status", routing::get(get_info))
        .route("/v1/{space_id}/attention", routing::get(get_attention))
        .route(
            "/v1/{space_id}/attention/{id}/responses",
            routing::post(post_attention_response),
        )
        .route("/v1/{space_id}/outcomes", routing::post(post_outcome))
        .route(
            "/v1/{space_id}/runtime/status",
            routing::get(get_runtime_status),
        )
        .route(
            "/v1/{space_id}/formation_status",
            routing::get(get_formation_status),
        )
        .route("/v1/{space_id}/probe", routing::post(post_probe))
        .route("/v1/{space_id}/memory/pin", routing::post(post_memory_pin))
        .route(
            "/v1/{space_id}/memory/forget",
            routing::post(post_memory_forget),
        )
        .route(
            "/v1/{space_id}/memory_status",
            routing::get(get_memory_status),
        )
        .route(
            "/v1/{space_id}/wiki/docs",
            routing::post(post_wiki_commit).get(list_wiki_docs),
        )
        .route(
            "/v1/{space_id}/wiki/docs/{doc_id}",
            routing::get(get_wiki_doc),
        )
        .route(
            "/v1/{space_id}/wiki/docs/{doc_id}/content",
            routing::get(get_wiki_content),
        )
        .route(
            "/v1/{space_id}/wiki/docs/{doc_id}/versions",
            routing::get(list_wiki_versions),
        )
        .route(
            "/v1/{space_id}/wiki/docs/{doc_id}/archive",
            routing::post(post_wiki_archive),
        )
        .route(
            "/v1/{space_id}/wiki/docs/{doc_id}/restore",
            routing::post(post_wiki_restore),
        )
        .route(
            "/v1/{space_id}/wiki/search",
            routing::post(post_wiki_search),
        )
        .route(
            "/v1/{space_id}/wiki/verify",
            routing::post(post_wiki_verify),
        )
        .route("/v1/{space_id}/wiki/events", routing::get(list_wiki_events))
        .route(
            "/v1/{space_id}/wiki/import",
            routing::post(post_wiki_import),
        )
        .route("/v1/{space_id}/wiki/export", routing::get(get_wiki_export))
        .route(
            "/v1/{space_id}/execute_kip_readonly",
            routing::post(execute_kip_readonly),
        )
        .route(
            "/v1/{space_id}/get_or_init_user",
            routing::post(get_or_init_user),
        )
        .route(
            "/v1/{space_id}/conversations/{conversation_id}",
            routing::get(get_conversation),
        )
        .route(
            "/v1/{space_id}/conversations/{conversation_id}/delta",
            routing::get(get_conversation_delta),
        )
        .route(
            "/v1/{space_id}/conversations",
            routing::get(list_conversations),
        )
        .route(
            "/v1/{space_id}/management/space_tokens",
            routing::get(list_space_tokens),
        )
        .route(
            "/v1/{space_id}/management/add_space_token",
            routing::post(add_space_token),
        )
        .route(
            "/v1/{space_id}/management/revoke_space_token",
            routing::post(revoke_space_token),
        )
        .route(
            "/v1/{space_id}/management/update_space",
            routing::patch(update_space),
        )
        .route(
            "/v1/{space_id}/management/restart_formation",
            routing::patch(restart_formation),
        )
        .route(
            "/v1/{space_id}/management/space_byok",
            routing::patch(update_byok),
        )
        .route(
            "/v1/{space_id}/management/space_byok",
            routing::get(get_byok),
        )
        .route(
            "/admin/{space_id}/update_space_tier",
            routing::post(update_space_tier),
        )
        .route("/admin/create_space", routing::post(create_space))
        .merge(llm_router)
        // Error bodies are always JSON; only success bodies follow the
        // Accept header (content negotiation happens in `AppResponse`).
        .layer(CompressionLayer::new());

    if cli.mcp_http_enabled {
        let mcp_config = mcp_http_config_from_cli(cli);
        let path_prefix = mcp_config.path_prefix.clone();
        let mcp_service =
            build_streamable_http_service(app_state, mcp_config, cancel_token.child_token());
        router = router.nest_service(&path_prefix, mcp_service);
    }

    // Global backstop over every route, the nested MCP endpoint included
    // (MCP tool calls are only covered by this outer cap): beyond
    // `HTTP_MAX_CONCURRENCY` in-flight requests the service sheds with 503
    // instead of accumulating unbounded queued work.
    with_concurrency_limit(
        router,
        cli.http_max_concurrency,
        StatusCode::SERVICE_UNAVAILABLE,
    )
}
// grcov-excl-stop

fn build_cors(cors_origins: &str) -> Result<CorsLayer, BoxError> {
    if cors_origins.trim().is_empty() {
        Ok(CorsLayer::new())
    } else if cors_origins.trim() == "*" {
        Ok(CorsLayer::very_permissive())
    } else {
        // A silently dropped origin would ship a service that "has CORS
        // configured" but rejects the intended frontend; fail startup loudly
        // instead.
        let mut origins: Vec<http::HeaderValue> = Vec::new();
        for origin in cors_origins
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            origins.push(
                origin
                    .parse()
                    .map_err(|err| format!("invalid CORS origin {origin:?}: {err}"))?,
            );
        }
        if origins.is_empty() {
            return Err(format!("CORS_ORIGINS contains no valid origin: {cors_origins:?}").into());
        }
        Ok(CorsLayer::new()
            .allow_origin(origins)
            .allow_credentials(true)
            .max_age(Duration::from_secs(86400))
            .allow_headers(AllowHeaders::mirror_request())
            .allow_methods(AllowMethods::mirror_request()))
    }
}

fn object_store_from_command(
    command: Option<Commands>,
) -> Result<(Arc<dyn ObjectStore>, String), BoxError> {
    let command = match command {
        Some(Commands::Local { db }) => Some(StorageCommand::Local { db }),
        Some(Commands::Aws { bucket, region }) => Some(StorageCommand::Aws { bucket, region }),
        Some(Commands::Mcp { storage, .. }) => storage,
        None => None,
    };

    object_store_from_storage_command(command)
}

fn object_store_from_storage_command(
    command: Option<StorageCommand>,
) -> Result<(Arc<dyn ObjectStore>, String), BoxError> {
    match command {
        Some(StorageCommand::Local { db }) => {
            let os = LocalFileSystem::new_with_prefix(db)?;
            let os = MetaStoreBuilder::new(os, 100000).build();
            Ok((Arc::new(os), "local".to_string()))
        }
        Some(StorageCommand::Aws { bucket, region }) => {
            let os = AmazonS3Builder::from_env()
                .with_bucket_name(bucket)
                .with_region(region)
                .with_copy_if_not_exists(S3CopyIfNotExists::Multipart)
                .build()?;
            Ok((Arc::new(os), "aws".to_string()))
        }
        None => Ok((Arc::new(InMemory::new()), "memory".to_string())),
    }
}

struct ServiceRuntime {
    app_state: AppState,
    app: Router,
    addr: SocketAddr,
    db_type: String,
    sharding_idx: u32,
    managers: String,
    model_name: String,
}

fn build_app_state(cli: &Cli) -> Result<(AppState, String), BoxError> {
    let http_client = build_http_client(cli)?;
    let managers = parse_managers(&cli.managers)?;
    let management = Arc::new(BaseManagement {
        controller: SELF_USER_ID,
        managers,
        visibility: Visibility::Public,
    });

    let models = Models::default();
    let model_config = model_config_from_cli(cli);
    models.set_model(model_config.model(http_client.clone())?);

    let (object_store, db_type) = object_store_from_command(cli.command.clone())?;
    let db_config = default_db_config();
    let ed25519_pubkeys = parse_ed25519_pubkeys(&cli.ed25519_pubkeys)?;

    let app_state = AppState::new(
        object_store,
        Arc::new(db_config),
        management,
        http_client,
        Arc::new(models),
        Arc::new(ed25519_pubkeys),
        APP_NAME.to_string(),
        APP_VERSION.to_string(),
        cli.sharding_idx,
    )
    .with_judge_model(judge_model_from_env())
    // One LLM budget for the whole process: the HTTP LLM routes and the MCP
    // LLM tools (HTTP and stdio alike) drain this same semaphore.
    .with_llm_concurrency(cli.llm_max_concurrency);

    let app_state = if let Some(path) = &cli.runtime_config {
        let metadata = std::fs::metadata(path)?;
        if metadata.len() > 1_048_576 {
            return Err("BRAIN_RUNTIME_CONFIG exceeds 1 MiB".into());
        }
        let config: anda_brain::runtime_api::config::RuntimeConfig =
            serde_json::from_slice(&std::fs::read(path)?)?;
        if cli.ed25519_pubkeys.trim().is_empty() {
            log::warn!(target:"brain","Runtime configuration loaded without a signed CWT verifier: independent HTTP outcome ingestion is disabled; verified Space tokens can only read/respond through explicit mappings");
        }
        app_state.with_runtime_config(config, |name| std::env::var(name).ok())?
    } else {
        app_state
    };

    Ok((app_state, db_type))
}

/// Independent judge model for the service path (plan M9), from the same
/// `JUDGE_MODEL_*` environment variables. Without this, shadow diagnostic
/// verdicts in service mode always fall back to the
/// evaluated space's own model — a self-grading blind spot.
fn judge_model_from_env() -> Option<BrainModelConfig> {
    let api_key = std::env::var("JUDGE_MODEL_API_KEY").unwrap_or_default();
    if api_key.is_empty() {
        return None;
    }
    Some(BrainModelConfig {
        family: std::env::var("JUDGE_MODEL_FAMILY").unwrap_or_else(|_| "openai".to_string()),
        model: std::env::var("JUDGE_MODEL_NAME").unwrap_or_default(),
        api_base: std::env::var("JUDGE_MODEL_API_BASE").unwrap_or_default(),
        api_key,
        ..Default::default()
    })
}

fn build_service_runtime(
    cli: &Cli,
    cancel_token: CancellationToken,
) -> Result<ServiceRuntime, BoxError> {
    let (app_state, db_type) = build_app_state(cli)?;
    let app = build_router(app_state.clone(), cli, cancel_token)
        .layer(build_cors(&cli.cors_origins)?)
        .with_state(app_state.clone());
    let addr: SocketAddr = cli.addr.parse()?;

    Ok(ServiceRuntime {
        app_state,
        app,
        addr,
        db_type,
        sharding_idx: cli.sharding_idx,
        managers: cli.managers.clone(),
        model_name: cli.model_name.clone(),
    })
}

async fn run_service(
    runtime: ServiceRuntime,
    global_cancel_token: CancellationToken,
) -> Result<(), BoxError> {
    let ServiceRuntime {
        app_state,
        app,
        addr,
        db_type,
        sharding_idx,
        managers,
        model_name,
    } = runtime;

    let listener = create_reuse_port_listener(addr).await?;
    let shutdown_token = global_cancel_token.clone();
    let server_handle = tokio::spawn(
        axum::serve(listener, app)
            .with_graceful_shutdown(shutdown_signal(shutdown_token))
            .into_future(),
    );

    let cancel_token = global_cancel_token.clone();
    let spaces_handle = tokio::spawn(async move {
        app_state.start_background_tasks(cancel_token).await;
    });

    log::warn!(
        target: "brain",
        "start service {}@{} on {:?}, sharding: {}, managers: {}, DB type: {}, Model: {}.",
        APP_NAME,
        APP_VERSION,
        addr,
        sharding_idx,
        managers,
        db_type,
        model_name
    );
    if db_type == "memory" {
        log::warn!(
            target: "brain",
            "WARNING: in-memory storage is active (no `local` or `aws` storage subcommand); ALL long-term memory will be LOST when the process exits. Configure persistent storage for production."
        );
    }

    join_service_tasks(server_handle, spaces_handle, global_cancel_token).await
}

/// Awaits both service tasks. Neither should finish on its own: the server
/// runs until the graceful-shutdown signal and the background tasks run
/// until the global cancel token fires. If one exits early (server accept
/// loop error, or a panic in either task), cancel the token so the peer
/// shuts down too, and propagate the failure so the process exits non-zero.
/// Without this the process would keep running with no listener — a zombie
/// an orchestrator never restarts.
async fn join_service_tasks(
    mut server_handle: JoinHandle<std::io::Result<()>>,
    mut spaces_handle: JoinHandle<()>,
    cancel_token: CancellationToken,
) -> Result<(), BoxError> {
    let (server_res, spaces_res) = tokio::select! {
        res = &mut server_handle => {
            if !cancel_token.is_cancelled() {
                log::error!(target: "brain", "server task exited before shutdown was requested; stopping background tasks");
            }
            cancel_token.cancel();
            (res, spaces_handle.await)
        }
        res = &mut spaces_handle => {
            if !cancel_token.is_cancelled() {
                log::error!(target: "brain", "space background task exited before shutdown was requested; stopping server");
            }
            cancel_token.cancel();
            (server_handle.await, res)
        }
    };

    server_res
        .map_err(|err| format!("server task failed: {err}"))?
        .map_err(|err| format!("server exited with error: {err}"))?;
    spaces_res.map_err(|err| format!("space background task failed: {err}"))?;
    Ok(())
}

/// ```bash
/// cargo run -p anda_brain
/// ```
// grcov-excl-start: main is a thin CLI/logging wrapper; build_service_runtime and run_service are unit-tested.
#[tokio::main]
async fn main() -> Result<(), BoxError> {
    dotenv::dotenv().ok();
    let cli = Cli::parse();

    match &cli.command {
        // MCP stdio reserves stdout for JSON-RPC; diagnostic logs use stderr.
        Some(Commands::Mcp { .. }) => {
            Builder::with_level(&get_env_level().to_string())
                .with_target_writer("*", new_writer(tokio::io::stderr()))
                .init();
        }
        _ => {
            // Structured JSON logging on stdout for the HTTP service.
            Builder::with_level(&get_env_level().to_string())
                .with_target_writer("*", new_writer(tokio::io::stdout()))
                .init();
        }
    }

    // Create global cancellation token for graceful shutdown
    let global_cancel_token = CancellationToken::new();
    match cli.command.clone() {
        Some(Commands::Mcp {
            space_id,
            auth_token,
            auto_create_space,
            auto_create_tier,
            ..
        }) => {
            let (app_state, _) = build_app_state(&cli)?;
            let mut mcp_config =
                McpServerConfig::stdio(space_id, auth_token.filter(|token| !token.is_empty()));
            mcp_config.auto_create_space = auto_create_space;
            mcp_config.auto_create_tier = auto_create_tier;
            run_stdio_server(app_state, mcp_config).await
        }
        _ => {
            let runtime = build_service_runtime(&cli, global_cancel_token.child_token())?;
            run_service(runtime, global_cancel_token).await
        }
    }
}
// grcov-excl-stop

async fn shutdown_signal(cancel_token: CancellationToken) {
    let external_cancel = cancel_token.cancelled();
    // grcov-excl-start: OS signal futures require process-level signals; cancellation-driven shutdown is tested.
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install signal handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    // grcov-excl-stop

    tokio::select! {
        _ = external_cancel => {},
        _ = ctrl_c => {},
        _ = terminate => {},
    }

    log::warn!(target: "brain", "received termination signal, starting graceful shutdown");
    cancel_token.cancel();
}

async fn create_reuse_port_listener(addr: SocketAddr) -> Result<tokio::net::TcpListener, BoxError> {
    let socket = match &addr {
        SocketAddr::V4(_) => tokio::net::TcpSocket::new_v4()?,
        SocketAddr::V6(_) => tokio::net::TcpSocket::new_v6()?,
    };

    #[cfg(unix)]
    let _ = socket.set_reuseport(true);

    socket.bind(addr)?;
    let listener = socket.listen(1024)?;
    Ok(listener)
}

#[cfg(test)]
mod tests {
    use super::{
        AnyHost, Cli, Commands, StorageCommand, build_cors, build_http_client, build_router,
        build_service_runtime, create_reuse_port_listener, default_db_config, join_service_tasks,
        mcp_http_config_from_cli, model_config_from_cli, normalize_http_path_prefix,
        object_store_from_command, parse_ed25519_pubkeys, parse_managers, run_service,
        split_csv_values, with_concurrency_limit,
    };
    use anda_brain::agents::SELF_USER_ID;
    use clap::{CommandFactory, Parser};
    use cose2::{Key as CoseKey, iana};
    use ic_auth_types::ByteBufB64;
    use std::time::{SystemTime, UNIX_EPOCH};
    use tokio::time::{Duration, sleep, timeout};
    use tokio_util::sync::CancellationToken;

    fn test_cli() -> Cli {
        Cli {
            runtime_config: None,
            addr: "127.0.0.1:0".to_string(),
            ed25519_pubkeys: String::new(),
            model_family: "openai".to_string(),
            model_name: "gpt-test".to_string(),
            model_api_key: "test-key".to_string(),
            model_context_window: 128,
            model_max_output: 64,
            model_api_base: "https://api.example.test".to_string(),
            https_proxy: None,
            sharding_idx: 7,
            managers: String::new(),
            cors_origins: String::new(),
            http_max_concurrency: 1024,
            llm_max_concurrency: 64,
            mcp_http_enabled: true,
            mcp_http_path_prefix: "/mcp".to_string(),
            mcp_http_allowed_hosts: String::new(),
            mcp_http_allowed_origins: String::new(),
            mcp_http_auto_create_space: false,
            mcp_http_auto_create_tier: 1,
            command: None,
        }
    }

    fn ed25519_basepoint_bytes() -> [u8; 32] {
        let mut bytes = [0x66; 32];
        bytes[0] = 0x58;
        bytes
    }

    #[test]
    fn any_host_matches_every_host_name() {
        assert_eq!(AnyHost, "api.example.com");
        assert_eq!(AnyHost, "localhost");
        assert_eq!(AnyHost, "");
    }

    #[test]
    fn cli_helpers_build_runtime_configuration() {
        let mut cli = test_cli();

        let model = model_config_from_cli(&cli);
        assert_eq!(model.family, "openai");
        assert_eq!(model.model, "gpt-test");
        assert_eq!(model.context_window, 128);
        assert_eq!(model.max_output, 64);
        assert!(!model.disabled);

        cli.model_api_key.clear();
        assert!(model_config_from_cli(&cli).disabled);

        let db = default_db_config();
        assert_eq!(db.name, "test");
        assert_eq!(db.storage.cache_max_capacity, 100000);
        assert_eq!(db.storage.object_chunk_size, 256 * 1024);

        let (app_state, _) = super::build_app_state(&test_cli()).unwrap();
        let _ = build_router(app_state, &test_cli(), CancellationToken::new());
        let _ = build_cors("").unwrap();
        let _ = build_cors("  ").unwrap();
        let _ = build_cors("*").unwrap();
        let _ = build_cors("https://example.test, https://app.example.test,").unwrap();
        // An origin that fails HeaderValue parsing (embedded DEL byte) must
        // abort startup instead of being silently dropped.
        assert!(build_cors("https://example.test,bad\u{7f}origin").is_err());
        // Only separators and whitespace is a configuration mistake too.
        assert!(build_cors(",").is_err());

        assert_eq!(normalize_http_path_prefix("mcp/"), "/mcp");
        assert_eq!(
            split_csv_values("localhost, brain.example.com, "),
            vec!["localhost", "brain.example.com"]
        );
        cli.mcp_http_path_prefix = "brain-mcp/".to_string();
        cli.mcp_http_allowed_hosts = "brain.example.com,127.0.0.1".to_string();
        cli.mcp_http_allowed_origins = "https://agents.example.com".to_string();
        cli.mcp_http_auto_create_space = true;
        let mcp = mcp_http_config_from_cli(&cli);
        assert_eq!(mcp.path_prefix, "/brain-mcp");
        assert_eq!(mcp.allowed_hosts.len(), 2);
        assert_eq!(mcp.allowed_origins, vec!["https://agents.example.com"]);
        assert!(mcp.auto_create_space);
    }

    #[test]
    fn build_service_runtime_wires_cli_into_app_state_and_router() {
        let mut cli = test_cli();
        cli.managers = SELF_USER_ID.to_string();
        cli.cors_origins = "*".to_string();

        let runtime = build_service_runtime(&cli, CancellationToken::new()).unwrap();

        assert_eq!(runtime.addr, "127.0.0.1:0".parse().unwrap());
        assert_eq!(runtime.db_type, "memory");
        assert_eq!(runtime.sharding_idx, 7);
        assert_eq!(runtime.managers, SELF_USER_ID.to_string());
        assert_eq!(runtime.model_name, "gpt-test");
        assert_eq!(runtime.app_state.app_name, "anda_brain");
        assert_eq!(runtime.app_state.sharding, 7);
        let _ = runtime.app;

        let mut invalid_addr = cli;
        invalid_addr.addr = "not an address".to_string();
        assert!(build_service_runtime(&invalid_addr, CancellationToken::new()).is_err());
    }

    #[tokio::test]
    async fn r4_startup_configuration_installs_the_compiled_adapter_and_rejects_unknowns() {
        let path =
            std::env::temp_dir().join(format!("brain-r4-config-{}.json", rand::random::<u64>()));
        let mut cli = test_cli();
        cli.runtime_config = Some(path.clone());
        let mut config = serde_json::json!({"format":"anda-brain:runtime-api-v1","spaces":{"r4_startup":{
            "bootstrap":true,"subjects":[{"credential":{"kind":"cwt_subject","subject":SELF_USER_ID.to_string()},"principal":"kip:principal:startup-reader","observer":false,"audit_recipients":false}],
            "audience":["kip:principal:startup-reader"],"observers":[],
            "adapter":{"id":"attention_inbox_v1","controller_principal":"kip:principal:startup-controller","recipient_principal":"kip:principal:startup-reader","message":"Memory attention","question":null,"reply_timeout_ms":60000,"context":null,"limits":anda_brain::action::ActionLimits::default()}
        }}});
        std::fs::write(&path, serde_json::to_vec(&config).unwrap()).unwrap();
        let runtime = build_service_runtime(&cli, CancellationToken::new()).unwrap();
        runtime
            .app_state
            .admin_create_space(
                SELF_USER_ID,
                SELF_USER_ID,
                "r4_startup".into(),
                1,
                anda_engine::unix_ms(),
            )
            .await
            .unwrap();
        let space = runtime
            .app_state
            .load_space("r4_startup", false)
            .await
            .unwrap();
        assert!(space.memory_runtime().is_some());
        assert!(space.attention().actions().is_some());
        space.close().await.unwrap();
        config["spaces"]["r4_startup"]["adapter"]["id"] = serde_json::json!("unknown-adapter");
        std::fs::write(&path, serde_json::to_vec(&config).unwrap()).unwrap();
        let error = match build_service_runtime(&cli, CancellationToken::new()) {
            Ok(_) => panic!("unknown adapter accepted"),
            Err(e) => e,
        };
        assert!(error.to_string().contains("unknown runtime adapter"));
        config["spaces"]["r4_startup"]["adapter"]["id"] = serde_json::json!("attention_inbox_v1");
        config["spaces"]["r4_startup"]["subjects"][0]["credential"] = serde_json::json!({"kind":"space_token_env","variable":format!("BRAIN_R4_MISSING_{}",rand::random::<u64>())});
        std::fs::write(&path, serde_json::to_vec(&config).unwrap()).unwrap();
        let error = match build_service_runtime(&cli, CancellationToken::new()) {
            Ok(_) => panic!("missing secret accepted"),
            Err(e) => e,
        };
        assert!(
            error.to_string().contains("runtime secret")
                && error.to_string().contains("unavailable")
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn parse_managers_accepts_empty_and_rejects_invalid_ids() {
        assert!(parse_managers("").unwrap().is_empty());

        let managers = parse_managers(&SELF_USER_ID.to_string()).unwrap();
        assert_eq!(managers.len(), 1);
        assert!(managers.contains(&SELF_USER_ID));

        // Whitespace around ids and stray commas are tolerated.
        let managers = parse_managers(&format!(" {SELF_USER_ID} , ,{SELF_USER_ID},")).unwrap();
        assert_eq!(managers.len(), 1);
        assert!(managers.contains(&SELF_USER_ID));
        assert!(parse_managers(" , ").unwrap().is_empty());

        assert!(parse_managers("not a principal").is_err());
        assert!(parse_managers(&format!("{SELF_USER_ID},not a principal")).is_err());
    }

    #[test]
    fn build_http_client_accepts_default_config_and_rejects_bad_proxy() {
        let cli = test_cli();
        let _ = build_http_client(&cli).unwrap();

        let mut cli = test_cli();
        cli.https_proxy = Some("not a proxy url".to_string());
        assert!(build_http_client(&cli).is_err());
    }

    #[test]
    fn object_store_helper_builds_memory_and_local_backends() {
        let (_, db_type) = object_store_from_command(None).unwrap();
        assert_eq!(db_type, "memory");

        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("anda-brain-local-store-{suffix}"));
        std::fs::create_dir_all(&path).unwrap();
        let (_, db_type) = object_store_from_command(Some(Commands::Local {
            db: path.to_string_lossy().to_string(),
        }))
        .unwrap();
        assert_eq!(db_type, "local");

        let (_, db_type) = object_store_from_command(Some(Commands::Mcp {
            space_id: "mcp_test".into(),
            auth_token: None,
            auto_create_space: false,
            auto_create_tier: 1,
            storage: Some(StorageCommand::Local {
                db: path.to_string_lossy().into(),
            }),
        }))
        .unwrap();
        assert_eq!(db_type, "local");

        let aws = object_store_from_command(Some(Commands::Aws {
            bucket: "anda-brain-test-bucket".to_string(),
            region: "us-east-1".to_string(),
        }));
        if let Ok((_, db_type)) = aws {
            assert_eq!(db_type, "aws");
        }
    }

    #[test]
    fn retired_offline_commands_are_rejected_before_service_setup() {
        let command = Cli::command();
        assert!(!command.get_subcommands().any(|c| c.get_name() == "eval"));
        for retired in [
            "eval",
            "--optimize",
            "--mine",
            "--scenario",
            "--validate-only",
        ] {
            assert!(
                Cli::try_parse_from(["anda_brain", retired]).is_err(),
                "{retired}"
            );
        }
        assert!(
            Cli::try_parse_from(["anda_brain", "local", "--db", "/tmp/p7-unused-cli-path"]).is_ok()
        );
        assert!(Cli::try_parse_from(["anda_brain", "mcp", "--space-id", "fixture"]).is_ok());
    }

    #[test]
    fn parse_ed25519_pubkeys_accepts_comma_separated_raw_keys() {
        let key_bytes = ed25519_basepoint_bytes();
        let encoded = ByteBufB64(key_bytes.to_vec()).to_string();
        let keys = parse_ed25519_pubkeys(&format!("{encoded}, {encoded}")).unwrap();

        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0].to_bytes(), key_bytes);
        assert_eq!(keys[1].to_bytes(), key_bytes);
    }

    #[test]
    fn parse_ed25519_pubkeys_accepts_cose_key_entries() {
        let key_bytes = ed25519_basepoint_bytes();
        let mut cose_key = CoseKey::new();
        cose_key.set_kty(iana::KeyTypeOKP);
        cose_key.insert(iana::OKPKeyParameterX, key_bytes.to_vec());
        let encoded = ByteBufB64(cose_key.to_vec().unwrap()).to_string();

        let keys = parse_ed25519_pubkeys(&encoded).unwrap();

        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].to_bytes(), key_bytes);
    }

    #[test]
    fn parse_ed25519_pubkeys_rejects_bad_binary_config() {
        let short_key = ByteBufB64(vec![1, 2, 3]).to_string();

        assert!(parse_ed25519_pubkeys("bad key").is_err());
        assert!(parse_ed25519_pubkeys(&short_key).is_err());
    }

    #[tokio::test]
    async fn create_reuse_port_listener_binds_ephemeral_port() {
        let listener = create_reuse_port_listener("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();

        assert_eq!(addr.ip().to_string(), "127.0.0.1");
        assert_ne!(addr.port(), 0);
    }

    #[tokio::test]
    async fn run_service_exits_when_cancelled() {
        let cancel = CancellationToken::new();
        let runtime = build_service_runtime(&test_cli(), cancel.child_token()).unwrap();
        let cancel_after_start = cancel.clone();
        tokio::spawn(async move {
            sleep(Duration::from_millis(50)).await;
            cancel_after_start.cancel();
        });

        timeout(Duration::from_secs(2), run_service(runtime, cancel))
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn join_service_tasks_cancels_peer_and_propagates_server_error() {
        let cancel = CancellationToken::new();
        let server: tokio::task::JoinHandle<std::io::Result<()>> =
            tokio::spawn(async { Err(std::io::Error::other("accept loop died")) });
        // Models `start_background_tasks`: runs until the token fires.
        let peer_token = cancel.clone();
        let spaces = tokio::spawn(async move { peer_token.cancelled().await });

        let err = timeout(
            Duration::from_secs(2),
            join_service_tasks(server, spaces, cancel.clone()),
        )
        .await
        .expect("must not hang once the server task is gone")
        .unwrap_err();

        assert!(cancel.is_cancelled(), "peer task must be cancelled");
        assert!(err.to_string().contains("accept loop died"));
    }

    #[tokio::test]
    async fn join_service_tasks_cancels_server_when_background_task_dies() {
        let cancel = CancellationToken::new();
        let server_token = cancel.clone();
        let server: tokio::task::JoinHandle<std::io::Result<()>> = tokio::spawn(async move {
            server_token.cancelled().await;
            Ok(())
        });
        let spaces = tokio::spawn(async { panic!("background task crashed") });

        let err = timeout(
            Duration::from_secs(2),
            join_service_tasks(server, spaces, cancel.clone()),
        )
        .await
        .expect("must not hang once the background task is gone")
        .unwrap_err();

        assert!(cancel.is_cancelled(), "server must be told to shut down");
        assert!(err.to_string().contains("space background task failed"));
    }

    #[tokio::test]
    async fn join_service_tasks_is_clean_on_graceful_shutdown() {
        let cancel = CancellationToken::new();
        let server_token = cancel.clone();
        let server: tokio::task::JoinHandle<std::io::Result<()>> = tokio::spawn(async move {
            server_token.cancelled().await;
            Ok(())
        });
        let spaces_token = cancel.clone();
        let spaces = tokio::spawn(async move { spaces_token.cancelled().await });
        cancel.cancel();

        timeout(
            Duration::from_secs(2),
            join_service_tasks(server, spaces, cancel),
        )
        .await
        .unwrap()
        .unwrap();
    }

    #[tokio::test]
    async fn with_concurrency_limit_sheds_excess_requests() {
        use axum::body::Body;
        use tower::ServiceExt;

        let started = std::sync::Arc::new(tokio::sync::Notify::new());
        let handler_started = started.clone();
        let app = with_concurrency_limit(
            axum::Router::new()
                .route(
                    "/hang",
                    super::routing::get(move || {
                        let started = handler_started.clone();
                        async move {
                            started.notify_one();
                            // Hold the single permit forever.
                            std::future::pending::<String>().await
                        }
                    }),
                )
                .route("/other", super::routing::get(|| async { "ok" })),
            1,
            http::StatusCode::SERVICE_UNAVAILABLE,
        );

        let hanging = app.clone();
        let first = tokio::spawn(async move {
            let _ = hanging
                .oneshot(http::Request::get("/hang").body(Body::empty()).unwrap())
                .await;
        });
        // The permit is acquired before the handler body runs, so once the
        // handler has signalled, the sole permit is provably taken.
        timeout(Duration::from_secs(2), started.notified())
            .await
            .unwrap();

        let res = app
            .clone()
            .oneshot(http::Request::get("/hang").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), http::StatusCode::SERVICE_UNAVAILABLE);

        // Requests to other routes are shed too: the cap is shared across
        // the whole router, not per route.
        let res = app
            .clone()
            .oneshot(http::Request::get("/other").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), http::StatusCode::SERVICE_UNAVAILABLE);

        first.abort();
    }
}
