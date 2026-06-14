use super::*;
use super::InitializeResponse;
use super::{
    KODEX_FILE_EDITING_DEVELOPER_INSTRUCTIONS, ProtocolVersion, build_agent_capabilities,
    distinct_session_title, merge_kodex_developer_instructions,
};

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
    let response = InitializeResponse::new(ProtocolVersion::V1)
        .agent_capabilities(build_agent_capabilities());
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

    assert_eq!(merged, KODEX_FILE_EDITING_DEVELOPER_INSTRUCTIONS);
}

#[test]
fn kodex_developer_instructions_preserve_existing_text() {
    let merged = merge_kodex_developer_instructions(Some("Existing rule.".to_string()))
        .expect("instructions");

    assert!(merged.starts_with("Existing rule.\n\n"));
    assert!(merged.contains(KODEX_FILE_EDITING_DEVELOPER_INSTRUCTIONS));
}

#[test]
fn kodex_developer_instructions_are_not_duplicated() {
    let existing = format!(
        "Existing rule.\n\n{}",
        KODEX_FILE_EDITING_DEVELOPER_INSTRUCTIONS
    );

    let merged =
        merge_kodex_developer_instructions(Some(existing.clone())).expect("instructions");

    assert_eq!(merged, existing);
}
