use super::*;

#[tokio::test]
async fn test_exec_approval_uses_available_decisions() -> anyhow::Result<()> {
    let session_id = SessionId::new("test");
    let client = Arc::new(StubClient::with_permission_responses(vec![
        RequestPermissionResponse::new(RequestPermissionOutcome::Selected(
            SelectedPermissionOutcome::new("denied"),
        )),
    ]));
    let session_client =
        SessionClient::with_client(session_id.clone(), client.clone(), Arc::default());
    let thread = Arc::new(StubCodexThread::new());
    let (response_tx, _response_rx) = tokio::sync::oneshot::channel();
    let (message_tx, mut message_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut prompt_state = PromptState::new(
        "submission-id".to_string(),
        session_id.clone(),
        thread.clone(),
        Arc::new(std::sync::Mutex::new(None)),
        None,
        None,
        message_tx,
        response_tx,
    );

    prompt_state.exec_approval(
        &session_client,
        ExecApprovalRequestEvent {
            call_id: "call-id".to_string(),
            approval_id: Some("approval-id".to_string()),
            turn_id: "turn-id".to_string(),
            started_at_ms: 0,
            command: vec!["echo".to_string(), "hi".to_string()],
            cwd: std::env::current_dir()?.try_into()?,
            reason: None,
            network_approval_context: None,
            proposed_execpolicy_amendment: None,
            proposed_network_policy_amendments: None,
            additional_permissions: None,
            available_decisions: Some(vec![ReviewDecision::Approved, ReviewDecision::Denied]),
            parsed_cmd: vec![ParsedCommand::Unknown {
                cmd: "echo hi".to_string(),
            }],
        },
    )?;

    let ThreadMessage::PermissionRequestResolved {
        submission_id,
        interaction_id,
        request_key,
        response,
    } = message_rx.recv().await.unwrap()
    else {
        panic!("expected permission resolution message");
    };
    assert_eq!(submission_id, "submission-id");
    prompt_state
        .handle_permission_request_resolved(
            &session_client,
            interaction_id,
            request_key,
            response,
        )
        .await?;

    let requests = client.permission_requests.lock().unwrap();
    let request = requests.last().unwrap();
    let option_ids = request
        .options
        .iter()
        .map(|option| option.option_id.0.to_string())
        .collect::<Vec<_>>();
    assert_eq!(option_ids, vec!["approved", "denied"]);

    let ops = thread.ops.lock().unwrap();
    assert!(matches!(
        ops.last(),
        Some(Op::ExecApproval {
            id,
            turn_id,
            decision: ReviewDecision::Denied,
        }) if id == "approval-id" && turn_id.as_deref() == Some("turn-id")
    ));

    Ok(())
}

#[tokio::test]
async fn test_mcp_tool_approval_elicitation_routes_to_permission_request() -> anyhow::Result<()>
{
    let session_id = SessionId::new("test");
    let client = Arc::new(StubClient::with_permission_responses(vec![
        RequestPermissionResponse::new(RequestPermissionOutcome::Selected(
            SelectedPermissionOutcome::new(MCP_TOOL_APPROVAL_ALLOW_SESSION_OPTION_ID),
        )),
    ]));
    let session_client =
        SessionClient::with_client(session_id.clone(), client.clone(), Arc::default());
    let thread = Arc::new(StubCodexThread::new());
    let (response_tx, _response_rx) = tokio::sync::oneshot::channel();
    let (message_tx, mut message_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut prompt_state = PromptState::new(
        "submission-id".to_string(),
        session_id.clone(),
        thread.clone(),
        Arc::new(std::sync::Mutex::new(None)),
        None,
        None,
        message_tx,
        response_tx,
    );

    let request_id = format!("{MCP_TOOL_APPROVAL_REQUEST_ID_PREFIX}call-123");
    prompt_state
        .mcp_elicitation(
            &session_client,
            ElicitationRequestEvent {
                turn_id: Some("turn-id".to_string()),
                server_name: "test-server".to_string(),
                id: codex_protocol::mcp::RequestId::String(request_id.clone()),
                request: ElicitationRequest::Form {
                    meta: Some(serde_json::json!({
                        "codex_approval_kind": "mcp_tool_call",
                        "persist": ["session", "always"],
                        "connector_name": "Docs",
                        "tool_title": "search_docs",
                        "tool_description": "Search project documentation",
                        "tool_params_display": [
                            {
                                "display_name": "Query",
                                "name": "query",
                                "value": "approval flow"
                            }
                        ]
                    })),
                    message: "Allow Docs to run tool \"search_docs\"?".to_string(),
                    requested_schema: serde_json::json!({
                        "type": "object",
                        "properties": {}
                    }),
                },
            },
        )
        .await?;

    let ThreadMessage::PermissionRequestResolved {
        submission_id,
        interaction_id,
        request_key,
        response,
    } = message_rx.recv().await.unwrap()
    else {
        panic!("expected permission resolution message");
    };
    assert_eq!(submission_id, "submission-id");

    {
        let requests = client.permission_requests.lock().unwrap();
        let request = requests.last().unwrap();
        assert_eq!(request.tool_call.tool_call_id.0.as_ref(), "call-123");
        assert_eq!(
            request
                .options
                .iter()
                .map(|option| option.option_id.0.to_string())
                .collect::<Vec<_>>(),
            vec![
                MCP_TOOL_APPROVAL_ALLOW_OPTION_ID.to_string(),
                MCP_TOOL_APPROVAL_ALLOW_SESSION_OPTION_ID.to_string(),
                MCP_TOOL_APPROVAL_ALLOW_ALWAYS_OPTION_ID.to_string(),
                MCP_TOOL_APPROVAL_CANCEL_OPTION_ID.to_string(),
            ]
        );
    }

    prompt_state
        .handle_permission_request_resolved(
            &session_client,
            interaction_id,
            request_key,
            response,
        )
        .await?;

    let op = thread.ops.lock().unwrap().last().cloned().unwrap();
    match op {
        Op::ResolveElicitation {
            server_name,
            request_id: codex_protocol::mcp::RequestId::String(id),
            decision,
            content,
            meta,
        } => {
            assert_eq!(server_name, "test-server");
            assert_eq!(id, request_id);
            assert_eq!(decision, ElicitationAction::Accept);
            assert!(content.is_none());
            assert_eq!(
                meta.as_ref()
                    .and_then(|value| value.get("persist"))
                    .and_then(serde_json::Value::as_str),
                Some(MCP_TOOL_APPROVAL_PERSIST_SESSION)
            );
        }
        other => panic!("unexpected op: {other:?}"),
    }

    Ok(())
}

#[tokio::test]
async fn test_request_user_input_routes_to_permission_request() -> anyhow::Result<()> {
    let session_id = SessionId::new("test");
    let client = Arc::new(StubClient::with_permission_responses(vec![
        RequestPermissionResponse::new(RequestPermissionOutcome::Selected(
            SelectedPermissionOutcome::new("answer:0:1"),
        )),
    ]));
    let session_client =
        SessionClient::with_client(session_id.clone(), client.clone(), Arc::default());
    let thread = Arc::new(StubCodexThread::new());
    let (response_tx, _response_rx) = tokio::sync::oneshot::channel();
    let (message_tx, mut message_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut prompt_state = PromptState::new(
        "submission-id".to_string(),
        session_id,
        thread.clone(),
        Arc::new(std::sync::Mutex::new(None)),
        None,
        None,
        message_tx,
        response_tx,
    );

    prompt_state
        .handle_event(
            &session_client,
            EventMsg::RequestUserInput(RequestUserInputEvent {
                call_id: "call-user-input".to_string(),
                turn_id: "turn-id".to_string(),
                questions: vec![RequestUserInputQuestion {
                    id: "approach".to_string(),
                    header: "Approach".to_string(),
                    question: "Which approach should I take?".to_string(),
                    is_other: false,
                    is_secret: false,
                    options: Some(vec![
                        codex_protocol::request_user_input::RequestUserInputQuestionOption {
                            label: "First".to_string(),
                            description: "Use the first path.".to_string(),
                        },
                        codex_protocol::request_user_input::RequestUserInputQuestionOption {
                            label: "Second".to_string(),
                            description: "Use the second path.".to_string(),
                        },
                    ]),
                }],
            }),
        )
        .await;

    let ThreadMessage::PermissionRequestResolved {
        submission_id,
        interaction_id,
        request_key,
        response,
    } = message_rx.recv().await.unwrap()
    else {
        panic!("expected permission resolution message");
    };
    assert_eq!(submission_id, "submission-id");

    {
        let requests = client.permission_requests.lock().unwrap();
        let request = requests.last().unwrap();
        assert_eq!(request.tool_call.tool_call_id.0.as_ref(), "call-user-input");
        assert_eq!(
            request
                .options
                .iter()
                .map(|option| option.option_id.0.to_string())
                .collect::<Vec<_>>(),
            vec!["answer:0:0", "answer:0:1", "cancel"]
        );
    }

    prompt_state
        .handle_permission_request_resolved(
            &session_client,
            interaction_id,
            request_key,
            response,
        )
        .await?;

    let ops = thread.ops.lock().unwrap();
    match ops.last() {
        Some(Op::UserInputAnswer { id, response }) => {
            assert_eq!(id, "turn-id");
            assert_eq!(
                response
                    .answers
                    .get("approach")
                    .map(|answer| &answer.answers),
                Some(&vec!["Second".to_string()])
            );
        }
        other => panic!("unexpected op: {other:?}"),
    }

    Ok(())
}

#[tokio::test]
async fn test_request_user_input_custom_answer_uses_permission_guidance() -> anyhow::Result<()>
{
    let session_id = SessionId::new("test");
    let client = Arc::new(StubClient::with_permission_responses(vec![
        RequestPermissionResponse::new(RequestPermissionOutcome::Selected(
            SelectedPermissionOutcome::new("answer:0:custom"),
        ))
        .meta(Meta::from_iter([(
            KODEX_PERMISSION_GUIDANCE_META_KEY.to_string(),
            json!("Use the smaller scoped refactor."),
        )])),
    ]));
    let session_client =
        SessionClient::with_client(session_id.clone(), client.clone(), Arc::default());
    let thread = Arc::new(StubCodexThread::new());
    let (response_tx, _response_rx) = tokio::sync::oneshot::channel();
    let (message_tx, mut message_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut prompt_state = PromptState::new(
        "submission-id".to_string(),
        session_id,
        thread.clone(),
        Arc::new(std::sync::Mutex::new(None)),
        None,
        None,
        message_tx,
        response_tx,
    );

    prompt_state
        .handle_event(
            &session_client,
            EventMsg::RequestUserInput(RequestUserInputEvent {
                call_id: "call-custom-input".to_string(),
                turn_id: "turn-id".to_string(),
                questions: vec![RequestUserInputQuestion {
                    id: "guidance".to_string(),
                    header: "Guidance".to_string(),
                    question: "Tell Codex what to do differently.".to_string(),
                    is_other: true,
                    is_secret: false,
                    options: None,
                }],
            }),
        )
        .await;

    let ThreadMessage::PermissionRequestResolved {
        interaction_id,
        request_key,
        response,
        ..
    } = message_rx.recv().await.unwrap()
    else {
        panic!("expected permission resolution message");
    };

    prompt_state
        .handle_permission_request_resolved(
            &session_client,
            interaction_id,
            request_key,
            response,
        )
        .await?;

    let ops = thread.ops.lock().unwrap();
    match ops.last() {
        Some(Op::UserInputAnswer { response, .. }) => {
            assert_eq!(
                response
                    .answers
                    .get("guidance")
                    .map(|answer| &answer.answers),
                Some(&vec!["Use the smaller scoped refactor.".to_string()])
            );
        }
        other => panic!("unexpected op: {other:?}"),
    }

    Ok(())
}

#[tokio::test]
async fn test_request_user_input_uses_structured_permission_answers() -> anyhow::Result<()> {
    let session_id = SessionId::new("test");
    let client = Arc::new(StubClient::with_permission_responses(vec![
        RequestPermissionResponse::new(RequestPermissionOutcome::Selected(
            SelectedPermissionOutcome::new("answer:0:0"),
        ))
        .meta(Meta::from_iter([(
            KODEX_USER_INPUT_ANSWERS_META_KEY.to_string(),
            json!({
                "answers": {
                    "approach": ["Careful"],
                    "checks": ["Unit", "Build"]
                }
            }),
        )])),
    ]));
    let session_client =
        SessionClient::with_client(session_id.clone(), client.clone(), Arc::default());
    let thread = Arc::new(StubCodexThread::new());
    let (response_tx, _response_rx) = tokio::sync::oneshot::channel();
    let (message_tx, mut message_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut prompt_state = PromptState::new(
        "submission-id".to_string(),
        session_id,
        thread.clone(),
        Arc::new(std::sync::Mutex::new(None)),
        None,
        None,
        message_tx,
        response_tx,
    );

    prompt_state
        .handle_event(
            &session_client,
            EventMsg::RequestUserInput(RequestUserInputEvent {
                call_id: "call-user-input".to_string(),
                turn_id: "turn-id".to_string(),
                questions: vec![
                    RequestUserInputQuestion {
                        id: "approach".to_string(),
                        header: "Approach".to_string(),
                        question: "Which approach should I take?".to_string(),
                        is_other: false,
                        is_secret: false,
                        options: Some(vec![
                            codex_protocol::request_user_input::RequestUserInputQuestionOption {
                                label: "Fast".to_string(),
                                description: "Use the fast path.".to_string(),
                            },
                            codex_protocol::request_user_input::RequestUserInputQuestionOption {
                                label: "Careful".to_string(),
                                description: "Use the careful path.".to_string(),
                            },
                        ]),
                    },
                    RequestUserInputQuestion {
                        id: "checks".to_string(),
                        header: "Checks".to_string(),
                        question: "Which checks should run?".to_string(),
                        is_other: true,
                        is_secret: false,
                        options: Some(vec![
                            codex_protocol::request_user_input::RequestUserInputQuestionOption {
                                label: "Unit".to_string(),
                                description: "Run unit tests.".to_string(),
                            },
                            codex_protocol::request_user_input::RequestUserInputQuestionOption {
                                label: "Build".to_string(),
                                description: "Run build.".to_string(),
                            },
                        ]),
                    },
                ],
            }),
        )
        .await;

    let ThreadMessage::PermissionRequestResolved {
        interaction_id,
        request_key,
        response,
        ..
    } = message_rx.recv().await.unwrap()
    else {
        panic!("expected permission resolution message");
    };

    prompt_state
        .handle_permission_request_resolved(
            &session_client,
            interaction_id,
            request_key,
            response,
        )
        .await?;

    let ops = thread.ops.lock().unwrap();
    match ops.last() {
        Some(Op::UserInputAnswer { id, response }) => {
            assert_eq!(id, "turn-id");
            assert_eq!(
                response
                    .answers
                    .get("approach")
                    .map(|answer| &answer.answers),
                Some(&vec!["Careful".to_string()])
            );
            assert_eq!(
                response.answers.get("checks").map(|answer| &answer.answers),
                Some(&vec!["Unit".to_string(), "Build".to_string()])
            );
        }
        other => panic!("unexpected op: {other:?}"),
    }

    Ok(())
}

#[tokio::test]
async fn test_mcp_elicitation_declines_unsupported_form_requests() -> anyhow::Result<()> {
    let session_id = SessionId::new("test");
    let client = Arc::new(StubClient::with_permission_responses(vec![
        RequestPermissionResponse::new(RequestPermissionOutcome::Selected(
            SelectedPermissionOutcome::new("decline"),
        )),
    ]));
    let session_client =
        SessionClient::with_client(session_id.clone(), client.clone(), Arc::default());
    let thread = Arc::new(StubCodexThread::new());
    let (response_tx, _response_rx) = tokio::sync::oneshot::channel();
    let (message_tx, _message_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut prompt_state = PromptState::new(
        "submission-id".to_string(),
        session_id.clone(),
        thread.clone(),
        Arc::new(std::sync::Mutex::new(None)),
        None,
        None,
        message_tx,
        response_tx,
    );

    prompt_state
        .mcp_elicitation(
            &session_client,
            ElicitationRequestEvent {
                turn_id: Some("turn-id".to_string()),
                server_name: "test-server".to_string(),
                id: codex_protocol::mcp::RequestId::String("request-id".to_string()),
                request: ElicitationRequest::Form {
                    meta: None,
                    message: "Need some structured input".to_string(),
                    requested_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "name": { "type": "string" }
                        }
                    }),
                },
            },
        )
        .await?;

    let requests = client.permission_requests.lock().unwrap();
    assert!(
        requests.is_empty(),
        "unsupported MCP elicitations should be auto-declined"
    );

    let ops = thread.ops.lock().unwrap();
    assert!(matches!(
        ops.last(),
        Some(Op::ResolveElicitation {
            server_name,
            request_id: codex_protocol::mcp::RequestId::String(request_id),
            decision: ElicitationAction::Decline,
            content: None,
            meta: None,
        }) if server_name == "test-server" && request_id == "request-id"
    ));

    Ok(())
}

#[tokio::test]
async fn test_blocked_approval_does_not_block_followup_events() -> anyhow::Result<()> {
    let session_id = SessionId::new("test");
    let notify = Arc::new(Notify::new());
    let client = Arc::new(StubClient::with_blocked_permission_requests(
        vec![],
        notify.clone(),
    ));
    let session_client =
        SessionClient::with_client(session_id.clone(), client.clone(), Arc::default());
    let thread = Arc::new(StubCodexThread::new());
    let (response_tx, _response_rx) = tokio::sync::oneshot::channel();
    let (message_tx, _message_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut prompt_state = PromptState::new(
        "submission-id".to_string(),
        session_id.clone(),
        thread,
        Arc::new(std::sync::Mutex::new(None)),
        None,
        None,
        message_tx,
        response_tx,
    );

    prompt_state
        .handle_event(
            &session_client,
            EventMsg::ExecApprovalRequest(ExecApprovalRequestEvent {
                call_id: "call-id".to_string(),
                approval_id: Some("approval-id".to_string()),
                turn_id: "turn-id".to_string(),
                started_at_ms: 0,
                command: vec!["echo".to_string(), "hi".to_string()],
                cwd: std::env::current_dir()?.try_into()?,
                reason: None,
                network_approval_context: None,
                proposed_execpolicy_amendment: None,
                proposed_network_policy_amendments: None,
                additional_permissions: None,
                available_decisions: Some(vec![
                    ReviewDecision::Approved,
                    ReviewDecision::Abort,
                ]),
                parsed_cmd: vec![ParsedCommand::Unknown {
                    cmd: "echo hi".to_string(),
                }],
            }),
        )
        .await;

    prompt_state
        .handle_event(
            &session_client,
            EventMsg::AgentMessage(AgentMessageEvent {
                message: "still flowing".to_string(),
                phase: None,
                memory_citation: None,
            }),
        )
        .await;

    let notifications = client.notifications.lock().unwrap();
    assert!(notifications.iter().any(|notification| {
        matches!(
            &notification.update,
            SessionUpdate::AgentMessageChunk(ContentChunk {
                content: ContentBlock::Text(TextContent { text, .. }),
                ..
            }) if text == "still flowing"
        )
    }));

    drop(notifications);
    prompt_state.detach_pending_interactions();
    notify.notify_one();

    Ok(())
}

#[tokio::test]
async fn test_detached_permission_request_drains_late_response() -> anyhow::Result<()> {
    let notify = Arc::new(Notify::new());
    let session_id = SessionId::new("test");
    let client = Arc::new(StubClient::with_blocked_permission_requests(
        vec![RequestPermissionResponse::new(
            RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new("approved")),
        )],
        notify.clone(),
    ));
    let session_client =
        SessionClient::with_client(session_id.clone(), client.clone(), Arc::default());
    let thread = Arc::new(StubCodexThread::new());
    let (response_tx, _response_rx) = tokio::sync::oneshot::channel();
    let (message_tx, mut message_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut prompt_state = PromptState::new(
        "submission-id".to_string(),
        session_id.clone(),
        thread.clone(),
        Arc::new(std::sync::Mutex::new(None)),
        None,
        None,
        message_tx,
        response_tx,
    );

    prompt_state
        .handle_event(
            &session_client,
            EventMsg::ExecApprovalRequest(ExecApprovalRequestEvent {
                call_id: "call-id".to_string(),
                approval_id: Some("approval-id".to_string()),
                turn_id: "turn-id".to_string(),
                started_at_ms: 0,
                command: vec!["echo".to_string(), "hi".to_string()],
                cwd: std::env::current_dir()?.try_into()?,
                reason: None,
                network_approval_context: None,
                proposed_execpolicy_amendment: None,
                proposed_network_policy_amendments: None,
                additional_permissions: None,
                available_decisions: Some(vec![
                    ReviewDecision::Approved,
                    ReviewDecision::Abort,
                ]),
                parsed_cmd: vec![ParsedCommand::Unknown {
                    cmd: "echo hi".to_string(),
                }],
            }),
        )
        .await;

    tokio::time::timeout(Duration::from_millis(100), async {
        loop {
            if !client.permission_requests.lock().unwrap().is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await?;

    prompt_state.detach_pending_interactions();
    notify.notify_one();

    let ThreadMessage::PermissionRequestResolved {
        submission_id,
        interaction_id,
        request_key,
        response,
    } = tokio::time::timeout(Duration::from_millis(100), message_rx.recv())
        .await?
        .expect("permission response should be drained")
    else {
        panic!("expected permission resolution message");
    };
    assert_eq!(submission_id, "submission-id");

    prompt_state
        .handle_permission_request_resolved(
            &session_client,
            interaction_id,
            request_key,
            response,
        )
        .await?;

    let ops = thread.ops.lock().unwrap();
    assert!(
        ops.is_empty(),
        "late permission response should not submit an approval: {ops:?}"
    );

    Ok(())
}

#[tokio::test]
async fn permission_abort_guidance_submits_followup_prompt() -> anyhow::Result<()> {
    let session_id = SessionId::new("test");
    let client = Arc::new(StubClient::with_permission_responses(vec![
        RequestPermissionResponse::new(RequestPermissionOutcome::Selected(
            SelectedPermissionOutcome::new("abort"),
        ))
        .meta(Meta::from_iter([(
            KODEX_PERMISSION_GUIDANCE_META_KEY.to_string(),
            json!("Explain the command first and avoid deleting files."),
        )])),
    ]));
    let session_client =
        SessionClient::with_client(session_id.clone(), client.clone(), Arc::default());
    let conversation = Arc::new(StubCodexThread::new());
    let models_manager = Arc::new(StubModelsManager);
    let config = Config::load_with_cli_overrides_and_harness_overrides(
        vec![],
        ConfigOverrides::default(),
    )
    .await?;
    let (message_tx, message_rx) = tokio::sync::mpsc::unbounded_channel();
    let (resolution_tx, resolution_rx) = tokio::sync::mpsc::unbounded_channel();
    let actor = ThreadActor::new(
        StubAuth,
        session_client,
        conversation.clone(),
        models_manager,
        config,
        None,
        message_rx,
        resolution_tx,
        resolution_rx,
    );
    let handle = tokio::spawn(actor.spawn());
    let thread = Thread {
        thread: conversation.clone(),
        message_tx,
        _handle: handle,
    };

    let (prompt_response_tx, prompt_response_rx) = tokio::sync::oneshot::channel();
    thread.message_tx.send(ThreadMessage::Prompt {
        request: PromptRequest::new(session_id, vec!["approval-block".into()]),
        response_tx: prompt_response_tx,
    })?;
    let _stop_reason_rx = prompt_response_rx.await??;

    tokio::time::timeout(Duration::from_millis(100), async {
        loop {
            if conversation.ops.lock().unwrap().len() >= 3 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await?;

    let ops = conversation.ops.lock().unwrap();
    assert!(matches!(
        ops.get(1),
        Some(Op::ExecApproval {
            id,
            turn_id: Some(_),
            decision: ReviewDecision::Abort,
        }) if id == "approval-id"
    ));
    assert!(matches!(
        ops.get(2),
        Some(Op::UserInput { items, .. })
            if prompt_text_from_items(items)
                .as_deref()
                == Some("Explain the command first and avoid deleting files.")
    ));

    Ok(())
}

#[tokio::test]
async fn test_thread_shutdown_bypasses_blocked_permission_request() -> anyhow::Result<()> {
    let session_id = SessionId::new("test");
    let notify = Arc::new(Notify::new());
    let client = Arc::new(StubClient::with_blocked_permission_requests(
        vec![RequestPermissionResponse::new(
            RequestPermissionOutcome::Cancelled,
        )],
        notify.clone(),
    ));
    let session_client =
        SessionClient::with_client(session_id.clone(), client.clone(), Arc::default());
    let conversation = Arc::new(StubCodexThread::new());
    let models_manager = Arc::new(StubModelsManager);
    let config = Config::load_with_cli_overrides_and_harness_overrides(
        vec![],
        ConfigOverrides::default(),
    )
    .await?;
    let (message_tx, message_rx) = tokio::sync::mpsc::unbounded_channel();
    let (resolution_tx, resolution_rx) = tokio::sync::mpsc::unbounded_channel();
    let actor = ThreadActor::new(
        StubAuth,
        session_client,
        conversation.clone(),
        models_manager,
        config,
        None,
        message_rx,
        resolution_tx,
        resolution_rx,
    );

    let handle = tokio::spawn(actor.spawn());
    let thread = Thread {
        thread: conversation.clone(),
        message_tx,
        _handle: handle,
    };

    let (prompt_response_tx, prompt_response_rx) = tokio::sync::oneshot::channel();
    thread.message_tx.send(ThreadMessage::Prompt {
        request: PromptRequest::new(session_id, vec!["approval-block".into()]),
        response_tx: prompt_response_tx,
    })?;
    let stop_reason_rx = prompt_response_rx.await??;

    tokio::time::timeout(Duration::from_millis(100), async {
        loop {
            if !client.permission_requests.lock().unwrap().is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await?;

    tokio::time::timeout(Duration::from_millis(100), thread.shutdown()).await??;
    let stop_reason =
        tokio::time::timeout(Duration::from_millis(100), stop_reason_rx).await??;
    assert_eq!(stop_reason?, StopReason::Cancelled);
    notify.notify_one();

    let ops = conversation.ops.lock().unwrap();
    assert!(matches!(ops.last(), Some(Op::Shutdown)));

    Ok(())
}
