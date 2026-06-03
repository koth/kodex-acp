//! Codex ACP - An Agent Client Protocol implementation for Codex.
#![deny(clippy::print_stdout, clippy::print_stderr)]

use agent_client_protocol::ByteStreams;
use agent_client_protocol::schema::SessionId;
use codex_core::config::{Config, ConfigOverrides};
use codex_utils_cli::CliConfigOverrides;
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::Arc;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use tracing_subscriber::EnvFilter;

mod codex_agent;
mod thread;

const TITLE_HELPER_ENV: &str = "KODEX_CODEX_ACP_TITLE_HELPER";

#[derive(Deserialize)]
struct TitleHelperRequest {
    session_id: String,
    prompt_text: String,
    response_text: Option<String>,
}

#[derive(Serialize)]
struct TitleHelperResponse {
    title: Option<String>,
}

/// Run the Codex ACP agent.
///
/// This sets up an ACP agent that communicates over stdio, bridging
/// the ACP protocol with the existing codex-rs infrastructure.
///
/// # Errors
///
/// If unable to parse the config or start the program.
pub async fn run_main(
    codex_linux_sandbox_exe: Option<PathBuf>,
    cli_config_overrides: CliConfigOverrides,
    port: Option<u16>,
) -> std::io::Result<()> {
    init_tracing();
    if std::env::var_os(TITLE_HELPER_ENV).is_some() {
        return run_title_helper(codex_linux_sandbox_exe, cli_config_overrides).await;
    }

    let config = load_config(codex_linux_sandbox_exe.clone(), cli_config_overrides).await?;

    let agent = Arc::new(codex_agent::CodexAgent::new(config, codex_linux_sandbox_exe).await?);

    if let Some(port) = port {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
            .await
            .map_err(|e| {
                std::io::Error::new(
                    e.kind(),
                    format!("failed to bind ACP TCP listener on 127.0.0.1:{port}: {e}"),
                )
            })?;
        let (stream, _) = listener.accept().await?;
        let (read, write) = stream.into_split();
        agent
            .serve(ByteStreams::new(write.compat_write(), read.compat()))
            .await
            .map_err(|e| std::io::Error::other(format!("ACP error: {e}")))?;
        return Ok(());
    }

    agent
        .serve(ByteStreams::new(
            tokio::io::stdout().compat_write(),
            tokio::io::stdin().compat(),
        ))
        .await
        .map_err(|e| std::io::Error::other(format!("ACP error: {e}")))?;

    Ok(())
}

fn init_tracing() {
    // Install a simple subscriber so `tracing` output is visible.
    // Users can control the log level with `RUST_LOG`.
    drop(
        tracing_subscriber::fmt()
            .with_writer(std::io::stderr)
            .with_env_filter(EnvFilter::from_default_env())
            .try_init(),
    );
}

async fn load_config(
    codex_linux_sandbox_exe: Option<PathBuf>,
    cli_config_overrides: CliConfigOverrides,
) -> std::io::Result<Config> {
    // Parse CLI overrides and load configuration
    let cli_kv_overrides = cli_config_overrides.parse_overrides().map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("error parsing -c overrides: {e}"),
        )
    })?;

    let config_overrides = ConfigOverrides {
        codex_linux_sandbox_exe: codex_linux_sandbox_exe.clone(),
        ..ConfigOverrides::default()
    };

    let config =
        Config::load_with_cli_overrides_and_harness_overrides(cli_kv_overrides, config_overrides)
            .await
            .map_err(|e| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("error loading config: {e}"),
                )
            })?;
    // Apply residency requirement so the HTTP client sends the
    // x-openai-internal-codex-residency header on all requests.
    codex_login::default_client::set_default_client_residency_requirement(
        config.enforce_residency.value(),
    );
    Ok(config)
}

async fn run_title_helper(
    codex_linux_sandbox_exe: Option<PathBuf>,
    cli_config_overrides: CliConfigOverrides,
) -> std::io::Result<()> {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;
    let request: TitleHelperRequest = serde_json::from_str(&input).map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("error parsing title helper request: {e}"),
        )
    })?;

    let config = load_config(codex_linux_sandbox_exe.clone(), cli_config_overrides).await?;
    let agent = codex_agent::CodexAgent::new(config, codex_linux_sandbox_exe).await?;
    let title = agent
        .generate_session_title(
            SessionId::from(request.session_id),
            &request.prompt_text,
            request.response_text.as_deref(),
        )
        .await
        .map_err(|e| std::io::Error::other(format!("title generation failed: {e}")))?;

    let output = serde_json::to_vec(&TitleHelperResponse { title }).map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("error serializing title helper response: {e}"),
        )
    })?;
    let mut stdout = std::io::stdout();
    stdout.write_all(&output)?;
    stdout.write_all(b"\n")?;

    Ok(())
}

// Re-export the MCP server types for compatibility
pub use codex_mcp_server::{
    CodexToolCallParam, CodexToolCallReplyParam, ExecApprovalElicitRequestParams,
    ExecApprovalResponse, PatchApprovalElicitRequestParams, PatchApprovalResponse,
};
