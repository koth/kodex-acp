use super::InitializeResponse;
use super::{
    KODEX_ENGINEERING_DEVELOPER_RULES, KODEX_FILE_EDITING_DEVELOPER_INSTRUCTIONS,
    KODEX_WEB_TOOLS_MCP_SERVER_NAME, ProtocolVersion,
    build_agent_capabilities, client_mcp_server_config, distinct_session_title,
    merge_kodex_developer_instructions,
};
use agent_client_protocol::schema::{McpServer, McpServerHttp};
use codex_config::McpServerTransportConfig;
use std::path::Path;

#[test]
fn distinct_session_title_ignores_first_user_message() {
    assert_eq!(
        distinct_session_title("  Fix the parser  ", Some("Fix the parser")),
        None
    );
}

#[test]
fn distinct_session_title_keeps_saved_title() {
    assert_eq!(
        distinct_session_title("Parser cleanup", Some("Fix the parser")),
        Some("Parser cleanup".to_string())
    );
}

#[test]
fn initialize_response_advertises_session_list_capability() {
    let response =
        InitializeResponse::new(ProtocolVersion::V1).agent_capabilities(build_agent_capabilities());
    let value = serde_json::to_value(response).expect("serialize initialize response");

    assert_eq!(
        value.pointer("/agentCapabilities/sessionCapabilities/list"),
        Some(&serde_json::json!({}))
    );
    assert_eq!(
        value.pointer("/agentCapabilities/sessionCapabilities/close"),
        Some(&serde_json::json!({}))
    );
}

#[test]
fn kodex_developer_instructions_are_added_when_missing() {
    let merged = merge_kodex_developer_instructions(None).expect("instructions");

    assert!(merged.contains(KODEX_FILE_EDITING_DEVELOPER_INSTRUCTIONS));
    assert!(merged.contains(KODEX_ENGINEERING_DEVELOPER_RULES));
    assert!(merged.contains("Do not guess APIs; consult the documentation first."));
}

#[test]
fn kodex_developer_instructions_preserve_existing_text() {
    let merged = merge_kodex_developer_instructions(Some("Existing rule.".to_string()))
        .expect("instructions");

    assert!(merged.starts_with("Existing rule.\n\n"));
    assert!(merged.contains(KODEX_FILE_EDITING_DEVELOPER_INSTRUCTIONS));
    assert!(merged.contains(KODEX_ENGINEERING_DEVELOPER_RULES));
}

#[test]
fn kodex_developer_instructions_are_not_duplicated() {
    let existing = format!(
        "Existing rule.\n\n{}\n\n{}",
        KODEX_FILE_EDITING_DEVELOPER_INSTRUCTIONS, KODEX_ENGINEERING_DEVELOPER_RULES
    );

    let merged = merge_kodex_developer_instructions(Some(existing.clone())).expect("instructions");

    assert_eq!(merged, existing);
}

#[test]
fn kodex_web_tools_mcp_server_is_required() {
    let server = McpServer::Http(McpServerHttp::new(
        KODEX_WEB_TOOLS_MCP_SERVER_NAME,
        "http://127.0.0.1:34567/mcp",
    ));

    let (name, config) =
        client_mcp_server_config(server, Path::new("/tmp/project")).expect("supported mcp server");

    assert_eq!(name, KODEX_WEB_TOOLS_MCP_SERVER_NAME);
    assert!(config.required);
    assert!(matches!(
        config.transport,
        McpServerTransportConfig::StreamableHttp { .. }
    ));
}

#[test]
fn generic_client_mcp_server_remains_optional() {
    let server = McpServer::Http(McpServerHttp::new(
        "docs search",
        "http://127.0.0.1:34567/mcp",
    ));

    let (name, config) =
        client_mcp_server_config(server, Path::new("/tmp/project")).expect("supported mcp server");

    assert_eq!(name, "docs_search");
    assert!(!config.required);
}
