use super::*;

pub(super) enum PendingPermissionRequest {
    Exec {
        approval_id: String,
        turn_id: String,
        option_map: HashMap<String, ReviewDecision>,
    },
    Patch {
        call_id: String,
        option_map: HashMap<String, ReviewDecision>,
    },
    RequestPermissions {
        call_id: String,
        permissions: RequestPermissionProfile,
    },
    McpElicitation {
        server_name: String,
        request_id: codex_protocol::mcp::RequestId,
        option_map: HashMap<String, ResolvedMcpElicitation>,
    },
    UserInput {
        id: String,
        option_map: HashMap<String, ResolvedUserInputAnswer>,
    },
}

pub(super) struct PendingPermissionInteraction {
    pub(super) id: u64,
    pub(super) request: PendingPermissionRequest,
}

#[derive(Clone)]
pub(super) struct ResolvedMcpElicitation {
    pub(super) action: ElicitationAction,
    pub(super) content: Option<serde_json::Value>,
    pub(super) meta: Option<serde_json::Value>,
}

impl ResolvedMcpElicitation {
    fn accept() -> Self {
        Self {
            action: ElicitationAction::Accept,
            content: None,
            meta: None,
        }
    }

    fn accept_with_persist(persist: &'static str) -> Self {
        Self {
            action: ElicitationAction::Accept,
            content: None,
            meta: Some(serde_json::json!({ "persist": persist })),
        }
    }

    pub(super) fn cancel() -> Self {
        Self {
            action: ElicitationAction::Cancel,
            content: None,
            meta: None,
        }
    }
}

#[derive(Clone)]
pub(super) struct ResolvedUserInputAnswer {
    question_id: String,
    answer: Option<String>,
    use_guidance: bool,
}

pub(super) fn exec_request_key(call_id: &str) -> String {
    format!("exec:{call_id}")
}

pub(super) fn patch_request_key(call_id: &str) -> String {
    format!("patch:{call_id}")
}

pub(super) fn permissions_request_key(call_id: &str) -> String {
    format!("permissions:{call_id}")
}

pub(super) fn mcp_elicitation_request_key(
    server_name: &str,
    request_id: &codex_protocol::mcp::RequestId,
) -> String {
    format!("mcp-elicitation:{server_name}:{request_id}")
}

pub(super) fn user_input_request_key(id: &str, call_id: &str) -> String {
    format!("user-input:{id}:{call_id}")
}

const MCP_TOOL_APPROVAL_KIND_KEY: &str = "codex_approval_kind";
const MCP_TOOL_APPROVAL_KIND_MCP_TOOL_CALL: &str = "mcp_tool_call";
const MCP_TOOL_APPROVAL_PERSIST_KEY: &str = "persist";
pub(in crate::thread) const MCP_TOOL_APPROVAL_PERSIST_SESSION: &str = "session";
const MCP_TOOL_APPROVAL_PERSIST_ALWAYS: &str = "always";
const MCP_TOOL_APPROVAL_TOOL_TITLE_KEY: &str = "tool_title";
const MCP_TOOL_APPROVAL_TOOL_DESCRIPTION_KEY: &str = "tool_description";
const MCP_TOOL_APPROVAL_CONNECTOR_NAME_KEY: &str = "connector_name";
const MCP_TOOL_APPROVAL_CONNECTOR_DESCRIPTION_KEY: &str = "connector_description";
const MCP_TOOL_APPROVAL_TOOL_PARAMS_KEY: &str = "tool_params";
const MCP_TOOL_APPROVAL_TOOL_PARAMS_DISPLAY_KEY: &str = "tool_params_display";
pub(in crate::thread) const MCP_TOOL_APPROVAL_REQUEST_ID_PREFIX: &str = "mcp_tool_call_approval_";
pub(in crate::thread) const MCP_TOOL_APPROVAL_ALLOW_OPTION_ID: &str = "approved";
pub(in crate::thread) const MCP_TOOL_APPROVAL_ALLOW_SESSION_OPTION_ID: &str =
    "approved-for-session";
pub(in crate::thread) const MCP_TOOL_APPROVAL_ALLOW_ALWAYS_OPTION_ID: &str = "approved-always";
pub(in crate::thread) const MCP_TOOL_APPROVAL_CANCEL_OPTION_ID: &str = "cancel";

pub(super) struct SupportedMcpElicitationPermissionRequest {
    pub(super) request_key: String,
    pub(super) tool_call: ToolCallUpdate,
    pub(super) options: Vec<PermissionOption>,
    pub(super) option_map: HashMap<String, ResolvedMcpElicitation>,
}

pub(super) fn build_supported_mcp_elicitation_permission_request(
    server_name: &str,
    request_id: &codex_protocol::mcp::RequestId,
    request: &ElicitationRequest,
    raw_input: serde_json::Value,
) -> Option<SupportedMcpElicitationPermissionRequest> {
    let ElicitationRequest::Form {
        meta: Some(meta),
        message,
        requested_schema: _,
    } = request
    else {
        return None;
    };
    let meta = meta.as_object()?;
    if meta
        .get(MCP_TOOL_APPROVAL_KIND_KEY)
        .and_then(serde_json::Value::as_str)
        != Some(MCP_TOOL_APPROVAL_KIND_MCP_TOOL_CALL)
    {
        return None;
    }

    let (allow_session_remember, allow_persistent_approval) = mcp_tool_approval_persist_modes(meta);
    let mut options = vec![PermissionOption::new(
        MCP_TOOL_APPROVAL_ALLOW_OPTION_ID,
        "Allow",
        PermissionOptionKind::AllowOnce,
    )];
    let mut option_map = HashMap::from([(
        MCP_TOOL_APPROVAL_ALLOW_OPTION_ID.to_string(),
        ResolvedMcpElicitation::accept(),
    )]);

    if allow_session_remember {
        options.push(PermissionOption::new(
            MCP_TOOL_APPROVAL_ALLOW_SESSION_OPTION_ID,
            "Allow for this session",
            PermissionOptionKind::AllowAlways,
        ));
        option_map.insert(
            MCP_TOOL_APPROVAL_ALLOW_SESSION_OPTION_ID.to_string(),
            ResolvedMcpElicitation::accept_with_persist(MCP_TOOL_APPROVAL_PERSIST_SESSION),
        );
    }

    if allow_persistent_approval {
        options.push(PermissionOption::new(
            MCP_TOOL_APPROVAL_ALLOW_ALWAYS_OPTION_ID,
            "Allow and don't ask again",
            PermissionOptionKind::AllowAlways,
        ));
        option_map.insert(
            MCP_TOOL_APPROVAL_ALLOW_ALWAYS_OPTION_ID.to_string(),
            ResolvedMcpElicitation::accept_with_persist(MCP_TOOL_APPROVAL_PERSIST_ALWAYS),
        );
    }

    options.push(PermissionOption::new(
        MCP_TOOL_APPROVAL_CANCEL_OPTION_ID,
        "Cancel",
        PermissionOptionKind::RejectOnce,
    ));
    option_map.insert(
        MCP_TOOL_APPROVAL_CANCEL_OPTION_ID.to_string(),
        ResolvedMcpElicitation::cancel(),
    );

    let tool_call_id = mcp_tool_approval_call_id(request_id)
        .unwrap_or_else(|| format!("mcp-elicitation:{request_id}"));
    let title = meta
        .get(MCP_TOOL_APPROVAL_TOOL_TITLE_KEY)
        .and_then(serde_json::Value::as_str)
        .filter(|title| !title.trim().is_empty())
        .map(|title| format!("Approve {title}"))
        .unwrap_or_else(|| "Approve MCP tool call".to_string());
    let content = format_mcp_tool_approval_content(server_name, message, meta);

    Some(SupportedMcpElicitationPermissionRequest {
        request_key: mcp_elicitation_request_key(server_name, request_id),
        tool_call: ToolCallUpdate::new(
            ToolCallId::new(tool_call_id),
            ToolCallUpdateFields::new()
                .status(ToolCallStatus::Pending)
                .title(title)
                .content(vec![ToolCallContent::Content(Content::new(
                    ContentBlock::Text(TextContent::new(content)),
                ))])
                .raw_input(raw_input),
        ),
        options,
        option_map,
    })
}

fn mcp_tool_approval_persist_modes(
    meta: &serde_json::Map<String, serde_json::Value>,
) -> (bool, bool) {
    match meta.get(MCP_TOOL_APPROVAL_PERSIST_KEY) {
        Some(serde_json::Value::String(persist)) => (
            persist == MCP_TOOL_APPROVAL_PERSIST_SESSION,
            persist == MCP_TOOL_APPROVAL_PERSIST_ALWAYS,
        ),
        Some(serde_json::Value::Array(values)) => (
            values
                .iter()
                .any(|value| value.as_str() == Some(MCP_TOOL_APPROVAL_PERSIST_SESSION)),
            values
                .iter()
                .any(|value| value.as_str() == Some(MCP_TOOL_APPROVAL_PERSIST_ALWAYS)),
        ),
        _ => (false, false),
    }
}

fn mcp_tool_approval_call_id(request_id: &codex_protocol::mcp::RequestId) -> Option<String> {
    match request_id {
        codex_protocol::mcp::RequestId::String(value) => value
            .strip_prefix(MCP_TOOL_APPROVAL_REQUEST_ID_PREFIX)
            .map(ToString::to_string),
        codex_protocol::mcp::RequestId::Integer(_) => None,
    }
}

fn format_mcp_tool_approval_content(
    server_name: &str,
    message: &str,
    meta: &serde_json::Map<String, serde_json::Value>,
) -> String {
    let mut sections = vec![message.trim().to_string()];

    let source = meta
        .get(MCP_TOOL_APPROVAL_CONNECTOR_NAME_KEY)
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(|value| format!("Source: {value}"))
        .unwrap_or_else(|| format!("Server: {server_name}"));
    sections.push(source);

    if let Some(description) = meta
        .get(MCP_TOOL_APPROVAL_CONNECTOR_DESCRIPTION_KEY)
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.trim().is_empty())
    {
        sections.push(description.to_string());
    }

    if let Some(description) = meta
        .get(MCP_TOOL_APPROVAL_TOOL_DESCRIPTION_KEY)
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.trim().is_empty())
    {
        sections.push(description.to_string());
    }

    if let Some(params) = format_mcp_tool_approval_params(meta) {
        sections.push(format!("Arguments:\n{params}"));
    }

    sections.join("\n\n")
}

fn format_mcp_tool_approval_params(
    meta: &serde_json::Map<String, serde_json::Value>,
) -> Option<String> {
    if let Some(serde_json::Value::Array(params)) =
        meta.get(MCP_TOOL_APPROVAL_TOOL_PARAMS_DISPLAY_KEY)
    {
        let params = params
            .iter()
            .filter_map(|param| {
                let object = param.as_object()?;
                let name = object
                    .get("display_name")
                    .and_then(serde_json::Value::as_str)
                    .or_else(|| object.get("name").and_then(serde_json::Value::as_str))?;
                let value = object.get("value")?;
                Some(format!(
                    "- {name}: {}",
                    format_mcp_tool_approval_value(value)
                ))
            })
            .collect::<Vec<_>>();
        if !params.is_empty() {
            return Some(params.join("\n"));
        }
    }

    meta.get(MCP_TOOL_APPROVAL_TOOL_PARAMS_KEY).map(|params| {
        serde_json::to_string_pretty(params)
            .unwrap_or_else(|_| format_mcp_tool_approval_value(params))
    })
}

fn format_mcp_tool_approval_value(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(value) => value.clone(),
        _ => serde_json::to_string(value).unwrap_or_else(|_| value.to_string()),
    }
}

pub(super) fn format_thread_goal_update(event: &ThreadGoalUpdatedEvent) -> String {
    let status = match event.goal.status {
        ThreadGoalStatus::Active => "active",
        ThreadGoalStatus::Paused => "paused",
        ThreadGoalStatus::BudgetLimited => "budget limited",
        ThreadGoalStatus::Blocked => "blocked",
        ThreadGoalStatus::UsageLimited => "usage limited",
        ThreadGoalStatus::Complete => "complete",
    };

    let objective = event.goal.objective.trim();
    if objective.contains('\n') {
        format!("Goal updated ({status}):\n{objective}")
    } else {
        format!("Goal updated ({status}): {objective}")
    }
}

pub(super) fn permission_guidance_from_response(
    response: &RequestPermissionResponse,
) -> Option<String> {
    response
        .meta
        .as_ref()
        .and_then(permission_guidance_from_meta)
        .or_else(|| match &response.outcome {
            RequestPermissionOutcome::Selected(SelectedPermissionOutcome { meta, .. }) => {
                meta.as_ref().and_then(permission_guidance_from_meta)
            }
            RequestPermissionOutcome::Cancelled | _ => None,
        })
}

pub(super) fn permission_guidance_from_meta(meta: &Meta) -> Option<String> {
    meta.get(KODEX_PERMISSION_GUIDANCE_META_KEY)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|guidance| !guidance.is_empty())
        .map(str::to_string)
}

pub(super) fn user_input_response_from_permission_response(
    response: &RequestPermissionResponse,
) -> Option<RequestUserInputResponse> {
    response
        .meta
        .as_ref()
        .and_then(user_input_response_from_meta)
        .or_else(|| match &response.outcome {
            RequestPermissionOutcome::Selected(SelectedPermissionOutcome { meta, .. }) => {
                meta.as_ref().and_then(user_input_response_from_meta)
            }
            RequestPermissionOutcome::Cancelled | _ => None,
        })
}

pub(super) fn user_input_response_from_meta(meta: &Meta) -> Option<RequestUserInputResponse> {
    let value = meta.get(KODEX_USER_INPUT_ANSWERS_META_KEY)?;
    let answers = value
        .get("answers")
        .and_then(serde_json::Value::as_object)
        .or_else(|| value.as_object())?;
    let answers = answers
        .iter()
        .filter_map(|(question_id, value)| {
            let values = value
                .as_array()?
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|answer| !answer.is_empty())
                .map(str::to_string)
                .collect::<Vec<_>>();
            (!values.is_empty()).then(|| {
                (
                    question_id.clone(),
                    RequestUserInputAnswer { answers: values },
                )
            })
        })
        .collect::<HashMap<_, _>>();
    (!answers.is_empty()).then_some(RequestUserInputResponse { answers })
}

pub(super) fn empty_user_input_response() -> RequestUserInputResponse {
    RequestUserInputResponse {
        answers: HashMap::new(),
    }
}

pub(super) fn user_input_response_from_answer(
    answer: &ResolvedUserInputAnswer,
    guidance: Option<String>,
) -> RequestUserInputResponse {
    let selected_answer = if answer.use_guidance {
        guidance
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .or_else(|| answer.answer.clone())
            .unwrap_or_default()
    } else {
        answer.answer.clone().unwrap_or_default()
    };

    RequestUserInputResponse {
        answers: HashMap::from([(
            answer.question_id.clone(),
            RequestUserInputAnswer {
                answers: vec![selected_answer],
            },
        )]),
    }
}

pub(super) fn permission_guidance_followup(
    decision: &ReviewDecision,
    guidance: Option<String>,
) -> Option<String> {
    if matches!(decision, ReviewDecision::Abort | ReviewDecision::TimedOut) {
        guidance
    } else {
        None
    }
}

#[derive(Clone)]
pub(super) struct ExecPermissionOption {
    pub(super) option_id: &'static str,
    pub(super) permission_option: PermissionOption,
    pub(super) decision: ReviewDecision,
}

pub(super) fn build_user_input_permission_request(
    questions: &[RequestUserInputQuestion],
) -> (
    String,
    String,
    Vec<PermissionOption>,
    HashMap<String, ResolvedUserInputAnswer>,
) {
    let title = questions
        .first()
        .map(user_input_question_label)
        .filter(|label| !label.is_empty())
        .map(|label| format!("Ask user: {}", truncate_chars(&label, 80)))
        .unwrap_or_else(|| "Ask user".to_string());
    let multiple_questions = questions.len() > 1;
    let mut content = Vec::new();
    let mut options = Vec::new();
    let mut option_map = HashMap::new();

    for (question_index, question) in questions.iter().enumerate() {
        let question_label = user_input_question_label(question);
        if !question_label.is_empty() {
            content.push(format!("Question {}: {question_label}", question_index + 1));
        }
        if !question.question.trim().is_empty() && question.question.trim() != question_label.trim()
        {
            content.push(question.question.trim().to_string());
        }

        let mut has_fixed_options = false;
        if let Some(question_options) = question.options.as_ref() {
            for (option_index, option) in question_options.iter().enumerate() {
                has_fixed_options = true;
                let option_id = format!("answer:{question_index}:{option_index}");
                let label = if multiple_questions {
                    format!(
                        "{}: {}",
                        truncate_chars(&question_label, 28),
                        option.label.trim()
                    )
                } else {
                    option.label.trim().to_string()
                };
                let description = option.description.trim();
                if description.is_empty() {
                    content.push(format!("- {}", option.label.trim()));
                } else {
                    content.push(format!("- {}: {description}", option.label.trim()));
                }
                options.push(PermissionOption::new(
                    option_id.clone(),
                    label,
                    PermissionOptionKind::AllowOnce,
                ));
                option_map.insert(
                    option_id,
                    ResolvedUserInputAnswer {
                        question_id: question.id.clone(),
                        answer: Some(option.label.clone()),
                        use_guidance: false,
                    },
                );
            }
        }

        if question.is_other || !has_fixed_options {
            let option_id = format!("answer:{question_index}:custom");
            let label = if multiple_questions {
                format!("{}: Submit response", truncate_chars(&question_label, 28))
            } else {
                "Submit response".to_string()
            };
            content.push("- Custom response: use the supplemental note field.".to_string());
            options.push(PermissionOption::new(
                option_id.clone(),
                label,
                PermissionOptionKind::AllowOnce,
            ));
            option_map.insert(
                option_id,
                ResolvedUserInputAnswer {
                    question_id: question.id.clone(),
                    answer: None,
                    use_guidance: true,
                },
            );
        }

        content.push(String::new());
    }

    if !options.is_empty() {
        options.push(PermissionOption::new(
            "cancel",
            "Cancel",
            PermissionOptionKind::RejectOnce,
        ));
    }

    let content = content
        .into_iter()
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string();

    (title, content, options, option_map)
}

pub(super) fn user_input_permission_meta(questions: &[RequestUserInputQuestion]) -> Meta {
    Meta::from_iter([(
        KODEX_PERMISSION_INPUT_META_KEY.to_string(),
        serde_json::json!({ "questions": questions }),
    )])
}

pub(super) fn user_input_question_label(question: &RequestUserInputQuestion) -> String {
    let header = question.header.trim();
    if !header.is_empty() {
        return header.to_string();
    }
    question.question.trim().to_string()
}

pub(super) fn build_exec_permission_options(
    available_decisions: &[ReviewDecision],
    network_approval_context: Option<&NetworkApprovalContext>,
    additional_permissions: Option<&AdditionalPermissionProfile>,
) -> Vec<ExecPermissionOption> {
    available_decisions
        .iter()
        .map(|decision| match decision {
            ReviewDecision::Approved => ExecPermissionOption {
                option_id: "approved",
                permission_option: PermissionOption::new(
                    "approved",
                    if network_approval_context.is_some() {
                        "Yes, just this once"
                    } else {
                        "Yes, proceed"
                    },
                    PermissionOptionKind::AllowOnce,
                ),
                decision: ReviewDecision::Approved,
            },
            ReviewDecision::ApprovedExecpolicyAmendment {
                proposed_execpolicy_amendment,
            } => {
                let command_prefix = proposed_execpolicy_amendment.command().join(" ");
                let label = if command_prefix.contains('\n')
                    || command_prefix.contains('\r')
                    || command_prefix.is_empty()
                {
                    "Yes, and remember this command pattern".to_string()
                } else {
                    format!(
                        "Yes, and don't ask again for commands that start with `{command_prefix}`"
                    )
                };
                ExecPermissionOption {
                    option_id: "approved-execpolicy-amendment",
                    permission_option: PermissionOption::new(
                        "approved-execpolicy-amendment",
                        label,
                        PermissionOptionKind::AllowAlways,
                    ),
                    decision: ReviewDecision::ApprovedExecpolicyAmendment {
                        proposed_execpolicy_amendment: proposed_execpolicy_amendment.clone(),
                    },
                }
            }
            ReviewDecision::ApprovedForSession => ExecPermissionOption {
                option_id: "approved-for-session",
                permission_option: PermissionOption::new(
                    "approved-for-session",
                    if network_approval_context.is_some() {
                        "Yes, and allow this host for this session"
                    } else if additional_permissions.is_some() {
                        "Yes, and allow these permissions for this session"
                    } else {
                        "Yes, and don't ask again for this command in this session"
                    },
                    PermissionOptionKind::AllowAlways,
                ),
                decision: ReviewDecision::ApprovedForSession,
            },
            ReviewDecision::NetworkPolicyAmendment {
                network_policy_amendment,
            } => {
                let (option_id, label, kind) = match network_policy_amendment.action {
                    NetworkPolicyRuleAction::Allow => (
                        "network-policy-amendment-allow",
                        "Yes, and allow this host in the future",
                        PermissionOptionKind::AllowAlways,
                    ),
                    NetworkPolicyRuleAction::Deny => (
                        "network-policy-amendment-deny",
                        "No, and block this host in the future",
                        PermissionOptionKind::RejectAlways,
                    ),
                };
                ExecPermissionOption {
                    option_id,
                    permission_option: PermissionOption::new(option_id, label, kind),
                    decision: ReviewDecision::NetworkPolicyAmendment {
                        network_policy_amendment: network_policy_amendment.clone(),
                    },
                }
            }
            ReviewDecision::Denied => ExecPermissionOption {
                option_id: "denied",
                permission_option: PermissionOption::new(
                    "denied",
                    "No, continue without running it",
                    PermissionOptionKind::RejectOnce,
                ),
                decision: ReviewDecision::Denied,
            },
            ReviewDecision::Abort => ExecPermissionOption {
                option_id: "abort",
                permission_option: PermissionOption::new(
                    "abort",
                    "No, and tell Codex what to do differently",
                    PermissionOptionKind::RejectOnce,
                ),
                decision: ReviewDecision::Abort,
            },
            ReviewDecision::TimedOut => ExecPermissionOption {
                option_id: "timed_out",
                permission_option: PermissionOption::new(
                    "timed_out",
                    "Time out, tell Codex what to do differently",
                    PermissionOptionKind::RejectOnce,
                ),
                decision: ReviewDecision::TimedOut,
            },
        })
        .collect()
}

pub(super) struct ParseCommandToolCall {
    pub(super) title: String,
    pub(super) file_extension: Option<String>,
    pub(super) terminal_output: bool,
    pub(super) locations: Vec<ToolCallLocation>,
    pub(super) kind: ToolKind,
}

pub(super) fn parse_command_tool_call(
    parsed_cmd: Vec<ParsedCommand>,
    cwd: &Path,
) -> ParseCommandToolCall {
    let mut titles = Vec::new();
    let mut locations = Vec::new();
    let mut file_extension = None;
    let mut terminal_output = false;
    let mut kind = ToolKind::Execute;

    for cmd in parsed_cmd {
        let mut cmd_path = None;
        match cmd {
            ParsedCommand::Read { cmd: _, name, path } => {
                titles.push(format!("Read {name}"));
                file_extension = path
                    .extension()
                    .map(|ext| ext.to_string_lossy().to_string());
                cmd_path = Some(path);
                kind = ToolKind::Read;
            }
            ParsedCommand::ListFiles { cmd: _, path } => {
                let dir = if let Some(path) = path.as_ref() {
                    &cwd.join(path)
                } else {
                    cwd
                };
                titles.push(format!("List {}", dir.display()));
                cmd_path = path.map(PathBuf::from);
                kind = ToolKind::Search;
            }
            ParsedCommand::Search { cmd, query, path } => {
                titles.push(match (query, path.as_ref()) {
                    (Some(query), Some(path)) => format!("Search {query} in {path}"),
                    (Some(query), None) => format!("Search {query}"),
                    _ => format!("Search {cmd}"),
                });
                kind = ToolKind::Search;
            }
            ParsedCommand::Unknown { cmd } => {
                titles.push(cmd);
                terminal_output = true;
            }
        }

        if let Some(path) = cmd_path {
            locations.push(ToolCallLocation::new(if path.is_relative() {
                cwd.join(&path)
            } else {
                path
            }));
        }
    }

    ParseCommandToolCall {
        title: titles.join(", "),
        file_extension,
        terminal_output,
        locations,
        kind,
    }
}
