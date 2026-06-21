use super::*;

#[tokio::test]
async fn test_prompt() -> anyhow::Result<()> {
    let (session_id, client, _, message_tx, _handle) = setup().await?;
    let (prompt_response_tx, prompt_response_rx) = tokio::sync::oneshot::channel();

    message_tx.send(ThreadMessage::Prompt {
        request: PromptRequest::new(session_id.clone(), vec!["Hi".into()]),
        response_tx: prompt_response_tx,
    })?;

    let stop_reason = prompt_response_rx.await??.await??;
    assert_eq!(stop_reason, StopReason::EndTurn);
    drop(message_tx);

    let notifications = client.notifications.lock().unwrap();
    assert_eq!(notifications.len(), 1);
    assert!(matches!(
        &notifications[0].update,
        SessionUpdate::AgentMessageChunk(ContentChunk {
            content: ContentBlock::Text(TextContent { text, .. }),
            ..
        }) if text == "Hi"
    ));

    Ok(())
}

#[tokio::test]
async fn commentary_phase_agent_message_is_sent_as_chat() -> anyhow::Result<()> {
    let (session_id, client, _, message_tx, _handle) = setup().await?;
    let (prompt_response_tx, prompt_response_rx) = tokio::sync::oneshot::channel();

    message_tx.send(ThreadMessage::Prompt {
        request: PromptRequest::new(session_id.clone(), vec!["commentary-only".into()]),
        response_tx: prompt_response_tx,
    })?;

    let stop_reason = prompt_response_rx.await??.await??;
    assert_eq!(stop_reason, StopReason::EndTurn);
    drop(message_tx);

    let notifications = client.notifications.lock().unwrap();
    assert!(notifications.iter().any(|notification| {
        matches!(
            &notification.update,
            SessionUpdate::AgentMessageChunk(ContentChunk {
                content: ContentBlock::Text(TextContent { text, .. }),
                ..
            }) if text.contains("Need patch")
        )
    }));

    Ok(())
}

#[tokio::test]
async fn commentary_phase_deltas_do_not_suppress_final_answer() -> anyhow::Result<()> {
    let (session_id, client, _, message_tx, _handle) = setup().await?;
    let (prompt_response_tx, prompt_response_rx) = tokio::sync::oneshot::channel();

    message_tx.send(ThreadMessage::Prompt {
        request: PromptRequest::new(
            session_id.clone(),
            vec!["commentary-delta-then-final".into()],
        ),
        response_tx: prompt_response_tx,
    })?;

    let stop_reason = prompt_response_rx.await??.await??;
    assert_eq!(stop_reason, StopReason::EndTurn);
    drop(message_tx);

    let notifications = client.notifications.lock().unwrap();
    assert!(notifications.iter().any(|notification| {
        matches!(
            &notification.update,
            SessionUpdate::AgentMessageChunk(ContentChunk {
                content: ContentBlock::Text(TextContent { text, .. }),
                ..
            }) if text.contains("Need internal note")
        )
    }));
    assert!(notifications.iter().any(|notification| {
        matches!(
            &notification.update,
            SessionUpdate::AgentMessageChunk(ContentChunk {
                content: ContentBlock::Text(TextContent { text, .. }),
                ..
            }) if text == "Final answer."
        )
    }));

    Ok(())
}

#[tokio::test]
async fn test_thread_goal_updated_is_sent_as_agent_message() -> anyhow::Result<()> {
    let (session_id, client, _, message_tx, _handle) = setup().await?;
    let (prompt_response_tx, prompt_response_rx) = tokio::sync::oneshot::channel();

    message_tx.send(ThreadMessage::Prompt {
        request: PromptRequest::new(session_id.clone(), vec!["thread-goal-update".into()]),
        response_tx: prompt_response_tx,
    })?;

    let stop_reason = prompt_response_rx.await??.await??;
    assert_eq!(stop_reason, StopReason::EndTurn);
    drop(message_tx);

    let notifications = client.notifications.lock().unwrap();
    assert!(notifications.iter().any(|notification| {
        matches!(
            &notification.update,
            SessionUpdate::AgentMessageChunk(ContentChunk {
                content: ContentBlock::Text(TextContent { text, .. }),
                ..
            }) if text == "Goal updated (active): Ship the goal update"
        )
    }));

    Ok(())
}

#[tokio::test]
async fn prompt_during_active_regular_turn_is_submitted_as_user_input() -> anyhow::Result<()> {
    let (session_id, _client, thread, message_tx, _handle) = setup().await?;
    let (first_response_tx, first_response_rx) = tokio::sync::oneshot::channel();

    message_tx.send(ThreadMessage::Prompt {
        request: PromptRequest::new(session_id.clone(), vec!["long-exec".into()]),
        response_tx: first_response_tx,
    })?;
    let first_stop_rx = first_response_rx.await??;

    let (steer_response_tx, steer_response_rx) = tokio::sync::oneshot::channel();
    message_tx.send(ThreadMessage::Prompt {
        request: PromptRequest::new(session_id.clone(), vec!["steer while running".into()]),
        response_tx: steer_response_tx,
    })?;

    let stop_reason = steer_response_rx.await??.await??;
    assert_eq!(stop_reason, StopReason::EndTurn);

    let ops = thread.ops.lock().unwrap();
    assert_eq!(ops.len(), 2);
    assert!(matches!(
        &ops[0],
        Op::UserInput { items, .. }
            if prompt_text_from_items(items).as_deref() == Some("long-exec")
    ));
    assert!(matches!(
        &ops[1],
        Op::UserInput { items, .. }
            if prompt_text_from_items(items).as_deref() == Some("steer while running")
    ));
    drop(ops);
    drop(first_stop_rx);
    drop(message_tx);

    Ok(())
}

#[tokio::test]
async fn prompt_during_non_steerable_turn_returns_clear_error() -> anyhow::Result<()> {
    let (session_id, _client, _thread, message_tx, _handle) = setup().await?;
    let (first_response_tx, first_response_rx) = tokio::sync::oneshot::channel();

    message_tx.send(ThreadMessage::Prompt {
        request: PromptRequest::new(session_id.clone(), vec!["long-exec".into()]),
        response_tx: first_response_tx,
    })?;
    let first_stop_rx = first_response_rx.await??;

    let (steer_response_tx, steer_response_rx) = tokio::sync::oneshot::channel();
    message_tx.send(ThreadMessage::Prompt {
        request: PromptRequest::new(session_id.clone(), vec!["steer-reject".into()]),
        response_tx: steer_response_tx,
    })?;

    let error = steer_response_rx
        .await?
        .expect_err("steer should be rejected");
    assert!(
        error.to_string().contains("active turn is not steerable")
            || format!("{error:?}").contains("active turn is not steerable"),
        "unexpected error: {error:?}",
    );
    drop(first_stop_rx);
    drop(message_tx);

    Ok(())
}

#[tokio::test]
async fn test_image_generation_emits_image_content() -> anyhow::Result<()> {
    let (session_id, client, _, message_tx, _handle) = setup().await?;
    let (prompt_response_tx, prompt_response_rx) = tokio::sync::oneshot::channel();
    let expected_uri = image_generation_test_saved_path()
        .to_string_lossy()
        .into_owned();

    message_tx.send(ThreadMessage::Prompt {
        request: PromptRequest::new(session_id.clone(), vec!["image-generation".into()]),
        response_tx: prompt_response_tx,
    })?;

    let stop_reason = prompt_response_rx.await??.await??;
    assert_eq!(stop_reason, StopReason::EndTurn);
    drop(message_tx);

    let notifications = client.notifications.lock().unwrap();
    let tool_call = notifications
        .iter()
        .find_map(|notification| match &notification.update {
            SessionUpdate::ToolCall(tool_call) if tool_call.tool_call_id.0.as_ref() == "ig-1" => {
                Some(tool_call)
            }
            _ => None,
        })
        .expect("image generation tool call should be sent");
    assert_eq!(tool_call.title, "Image generation");
    assert_eq!(tool_call.status, ToolCallStatus::InProgress);

    let update = notifications
        .iter()
        .find_map(|notification| match &notification.update {
            SessionUpdate::ToolCallUpdate(update) if update.tool_call_id.0.as_ref() == "ig-1" => {
                Some(update)
            }
            _ => None,
        })
        .expect("image generation tool call update should be sent");
    assert_eq!(update.fields.status, Some(ToolCallStatus::Completed));
    let content = update
        .fields
        .content
        .as_ref()
        .expect("image generation update should include content");
    assert_eq!(content.len(), 2);
    assert!(matches!(
        &content[0],
        ToolCallContent::Content(Content {
            content: ContentBlock::Text(TextContent { text, .. }),
            ..
        }) if text == "Revised prompt: A tiny blue square"
    ));
    assert!(matches!(
        &content[1],
        ToolCallContent::Content(Content {
            content: ContentBlock::Image(ImageContent {
                data,
                mime_type,
                uri,
                ..
            }),
            ..
        }) if data == "Zm9v" && mime_type == "image/png" && uri.as_deref() == Some(expected_uri.as_str())
    ));

    Ok(())
}

#[tokio::test]
async fn test_compact() -> anyhow::Result<()> {
    let (session_id, client, thread, message_tx, _handle) = setup().await?;
    let (prompt_response_tx, prompt_response_rx) = tokio::sync::oneshot::channel();

    message_tx.send(ThreadMessage::Prompt {
        request: PromptRequest::new(session_id.clone(), vec!["/compact".into()]),
        response_tx: prompt_response_tx,
    })?;

    let stop_reason = prompt_response_rx.await??.await??;
    assert_eq!(stop_reason, StopReason::EndTurn);
    drop(message_tx);

    let notifications = client.notifications.lock().unwrap();
    assert!(notifications.iter().any(|notification| {
        matches!(
            &notification.update,
            SessionUpdate::AgentMessageChunk(ContentChunk {
                content: ContentBlock::Text(TextContent { text, .. }),
                ..
            }) if text == "Compact task completed"
        )
    }));
    assert!(notifications.iter().any(|notification| {
        notification
            .meta
            .as_ref()
            .is_some_and(|meta| meta.contains_key(KODEX_CONTEXT_COMPACTED_META_KEY))
    }));
    assert!(notifications.iter().any(|notification| {
        notification.meta.as_ref().is_some_and(|meta| {
            meta.get(KODEX_CONTEXT_COMPACTION_META_KEY)
                .and_then(|value| value.get("phase"))
                .and_then(serde_json::Value::as_str)
                == Some("started")
        })
    }));
    assert!(notifications.iter().any(|notification| {
        notification.meta.as_ref().is_some_and(|meta| {
            meta.get(KODEX_CONTEXT_COMPACTION_META_KEY)
                .and_then(|value| value.get("phase"))
                .and_then(serde_json::Value::as_str)
                == Some("completed")
        })
    }));
    let ops = thread.ops.lock().unwrap();
    assert_eq!(ops.as_slice(), &[Op::Compact]);

    Ok(())
}

#[test]
fn test_guardian_execve_summary_uses_argv_without_duplication() -> anyhow::Result<()> {
    let action = GuardianAssessmentAction::Execve {
        source: GuardianCommandSource::UnifiedExec,
        program: "/bin/ls".to_string(),
        argv: vec!["/bin/ls".to_string(), "-l".to_string()],
        cwd: std::env::current_dir()?.try_into()?,
    };

    assert_eq!(
        guardian_action_summary(&action),
        Some("exec /bin/ls -l".to_string())
    );

    Ok(())
}

#[test]
fn auto_mode_preset_enables_network_access() {
    let preset = APPROVAL_PRESETS
        .iter()
        .find(|preset| preset.id == "auto")
        .expect("auto mode preset should exist");

    assert_eq!(
        preset.permission_profile.network_sandbox_policy(),
        NetworkSandboxPolicy::Enabled
    );
    assert!(preset.description.contains("access the internet"));
}

#[tokio::test]
async fn modes_match_augmented_workspace_permission_profile() -> anyhow::Result<()> {
    let mut config =
        Config::load_with_cli_overrides_and_harness_overrides(vec![], ConfigOverrides::default())
            .await?;
    config
        .permissions
        .approval_policy
        .set(codex_protocol::protocol::AskForApproval::OnRequest)?;

    let workspace_profile = PermissionProfile::workspace_write();
    let extra_roots = vec![config.codex_home.as_path().join("memories").try_into()?];
    let file_system_policy = workspace_profile
        .file_system_sandbox_policy()
        .with_additional_writable_roots(config.cwd.as_path(), &extra_roots);
    let augmented_profile = PermissionProfile::from_runtime_permissions(
        &file_system_policy,
        workspace_profile.network_sandbox_policy(),
    );
    assert_ne!(augmented_profile, workspace_profile);

    config
        .permissions
        .set_permission_profile(augmented_profile)?;

    let mode_id = current_session_mode_id(&config).expect("mode should be recognized");
    assert_eq!(mode_id.0.as_ref(), "auto");

    Ok(())
}

#[tokio::test]
async fn modes_match_legacy_augmented_workspace_permission_profile() -> anyhow::Result<()> {
    let mut config =
        Config::load_with_cli_overrides_and_harness_overrides(vec![], ConfigOverrides::default())
            .await?;
    config
        .permissions
        .approval_policy
        .set(codex_protocol::protocol::AskForApproval::OnRequest)?;

    let workspace_profile = PermissionProfile::workspace_write();
    let extra_roots = vec![config.codex_home.as_path().join("memories").try_into()?];
    let file_system_policy = workspace_profile
        .file_system_sandbox_policy()
        .with_additional_writable_roots(config.cwd.as_path(), &extra_roots);
    let augmented_profile = PermissionProfile::from_runtime_permissions(
        &file_system_policy,
        workspace_profile.network_sandbox_policy(),
    );
    assert_ne!(augmented_profile, workspace_profile);

    config
        .permissions
        .set_permission_profile(augmented_profile)?;
    assert!(config.permissions.active_permission_profile().is_none());

    let mode_id = current_session_mode_id(&config).expect("mode should be recognized");
    assert_eq!(mode_id.0.as_ref(), "auto");

    Ok(())
}

#[test]
fn read_only_mode_does_not_trust_project() {
    assert!(!mode_trusts_project("read-only"));
    assert!(mode_trusts_project("auto"));
    assert!(mode_trusts_project("full-access"));
}

#[tokio::test]
async fn config_options_do_not_expose_separate_codex_collaboration_mode() -> anyhow::Result<()> {
    let (_, _, _, message_tx, _handle) = setup().await?;
    let (response_tx, response_rx) = tokio::sync::oneshot::channel();

    message_tx.send(ThreadMessage::GetConfigOptions { response_tx })?;
    let options = response_rx.await??;

    assert!(
        options
            .iter()
            .all(|option| option.id.0.as_ref() != "codex_collaboration_mode")
    );

    Ok(())
}

#[tokio::test]
async fn setting_read_only_mode_updates_collaboration_mode_to_plan() -> anyhow::Result<()> {
    let (_, _, thread, message_tx, _handle) = setup().await?;
    let (response_tx, response_rx) = tokio::sync::oneshot::channel();

    message_tx.send(ThreadMessage::SetMode {
        mode: SessionModeId::new("read-only"),
        response_tx,
    })?;
    response_rx.await??;

    let ops = thread.ops.lock().unwrap();
    let Some(Op::ThreadSettings { thread_settings }) = ops.last() else {
        panic!("expected thread settings update");
    };
    let collaboration_mode = thread_settings
        .collaboration_mode
        .as_ref()
        .expect("collaboration mode should be set");
    assert_eq!(collaboration_mode.mode, ModeKind::Plan);
    assert!(!collaboration_mode.model().is_empty());

    Ok(())
}

#[tokio::test]
async fn setting_auto_mode_updates_collaboration_mode_to_default() -> anyhow::Result<()> {
    let (_, _, thread, message_tx, _handle) = setup().await?;
    let (response_tx, response_rx) = tokio::sync::oneshot::channel();

    message_tx.send(ThreadMessage::SetMode {
        mode: SessionModeId::new("auto"),
        response_tx,
    })?;
    response_rx.await??;

    let ops = thread.ops.lock().unwrap();
    let Some(Op::ThreadSettings { thread_settings }) = ops.last() else {
        panic!("expected thread settings update");
    };
    let collaboration_mode = thread_settings
        .collaboration_mode
        .as_ref()
        .expect("collaboration mode should be set");
    assert_eq!(collaboration_mode.mode, ModeKind::Default);
    let permission_profile = thread_settings
        .permission_profile
        .as_ref()
        .expect("permission profile should be set");
    assert_eq!(
        permission_profile.network_sandbox_policy(),
        NetworkSandboxPolicy::Enabled
    );

    Ok(())
}

#[tokio::test]
async fn test_init() -> anyhow::Result<()> {
    let (session_id, client, thread, message_tx, _handle) = setup().await?;
    let (prompt_response_tx, prompt_response_rx) = tokio::sync::oneshot::channel();

    message_tx.send(ThreadMessage::Prompt {
        request: PromptRequest::new(session_id.clone(), vec!["/init".into()]),
        response_tx: prompt_response_tx,
    })?;

    let stop_reason = prompt_response_rx.await??.await??;
    assert_eq!(stop_reason, StopReason::EndTurn);
    drop(message_tx);

    let notifications = client.notifications.lock().unwrap();
    assert!(
        notifications.iter().any(|notification| {
            matches!(
                &notification.update,
                SessionUpdate::AgentMessageChunk(ContentChunk {
                    content: ContentBlock::Text(TextContent { text, .. }), ..
                }) if text == INIT_COMMAND_PROMPT // we echo the prompt
            )
        }),
        "notifications don't match {notifications:?}"
    );
    let ops = thread.ops.lock().unwrap();
    assert_eq!(
        ops.as_slice(),
        &[Op::UserInput {
            items: vec![UserInput::Text {
                text: INIT_COMMAND_PROMPT.to_string(),
                text_elements: vec![]
            }],
            final_output_json_schema: None,
            environments: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        }],
        "ops don't match {ops:?}"
    );

    Ok(())
}

#[tokio::test]
async fn test_review() -> anyhow::Result<()> {
    let (session_id, client, thread, message_tx, _handle) = setup().await?;
    let (prompt_response_tx, prompt_response_rx) = tokio::sync::oneshot::channel();

    message_tx.send(ThreadMessage::Prompt {
        request: PromptRequest::new(session_id.clone(), vec!["/review".into()]),
        response_tx: prompt_response_tx,
    })?;

    let stop_reason = prompt_response_rx.await??.await??;
    assert_eq!(stop_reason, StopReason::EndTurn);
    drop(message_tx);

    let notifications = client.notifications.lock().unwrap();
    assert_eq!(notifications.len(), 1);
    assert!(
        matches!(
            &notifications[0].update,
            SessionUpdate::AgentMessageChunk(ContentChunk {
                content: ContentBlock::Text(TextContent { text, .. }),
                ..
            }) if text == "current changes" // we echo the prompt
        ),
        "notifications don't match {notifications:?}"
    );

    let ops = thread.ops.lock().unwrap();
    assert_eq!(
        ops.as_slice(),
        &[Op::Review {
            review_request: ReviewRequest {
                user_facing_hint: Some(user_facing_hint(&ReviewTarget::UncommittedChanges)),
                target: ReviewTarget::UncommittedChanges,
            }
        }],
        "ops don't match {ops:?}"
    );

    Ok(())
}

#[tokio::test]
async fn test_custom_review() -> anyhow::Result<()> {
    let (session_id, client, thread, message_tx, _handle) = setup().await?;
    let (prompt_response_tx, prompt_response_rx) = tokio::sync::oneshot::channel();
    let instructions = "Review what we did in agents.md";

    message_tx.send(ThreadMessage::Prompt {
        request: PromptRequest::new(
            session_id.clone(),
            vec![format!("/review {instructions}").into()],
        ),
        response_tx: prompt_response_tx,
    })?;

    let stop_reason = prompt_response_rx.await??.await??;
    assert_eq!(stop_reason, StopReason::EndTurn);
    drop(message_tx);

    let notifications = client.notifications.lock().unwrap();
    assert_eq!(notifications.len(), 1);
    assert!(
        matches!(
            &notifications[0].update,
            SessionUpdate::AgentMessageChunk(ContentChunk {
                content: ContentBlock::Text(TextContent { text, .. }),
                ..
            }) if text == "Review what we did in agents.md" // we echo the prompt
        ),
        "notifications don't match {notifications:?}"
    );

    let ops = thread.ops.lock().unwrap();
    assert_eq!(
        ops.as_slice(),
        &[Op::Review {
            review_request: ReviewRequest {
                user_facing_hint: Some(user_facing_hint(&ReviewTarget::Custom {
                    instructions: instructions.to_owned()
                })),
                target: ReviewTarget::Custom {
                    instructions: instructions.to_owned()
                },
            }
        }],
        "ops don't match {ops:?}"
    );

    Ok(())
}

#[tokio::test]
async fn test_commit_review() -> anyhow::Result<()> {
    let (session_id, client, thread, message_tx, _handle) = setup().await?;
    let (prompt_response_tx, prompt_response_rx) = tokio::sync::oneshot::channel();

    message_tx.send(ThreadMessage::Prompt {
        request: PromptRequest::new(session_id.clone(), vec!["/review-commit 123456".into()]),
        response_tx: prompt_response_tx,
    })?;

    let stop_reason = prompt_response_rx.await??.await??;
    assert_eq!(stop_reason, StopReason::EndTurn);
    drop(message_tx);

    let notifications = client.notifications.lock().unwrap();
    assert_eq!(notifications.len(), 1);
    assert!(
        matches!(
            &notifications[0].update,
            SessionUpdate::AgentMessageChunk(ContentChunk {
                content: ContentBlock::Text(TextContent { text, .. }),
                ..
            }) if text == "commit 123456" // we echo the prompt
        ),
        "notifications don't match {notifications:?}"
    );

    let ops = thread.ops.lock().unwrap();
    assert_eq!(
        ops.as_slice(),
        &[Op::Review {
            review_request: ReviewRequest {
                user_facing_hint: Some(user_facing_hint(&ReviewTarget::Commit {
                    sha: "123456".to_owned(),
                    title: None
                })),
                target: ReviewTarget::Commit {
                    sha: "123456".to_owned(),
                    title: None
                },
            }
        }],
        "ops don't match {ops:?}"
    );

    Ok(())
}

#[tokio::test]
async fn test_branch_review() -> anyhow::Result<()> {
    let (session_id, client, thread, message_tx, _handle) = setup().await?;
    let (prompt_response_tx, prompt_response_rx) = tokio::sync::oneshot::channel();

    message_tx.send(ThreadMessage::Prompt {
        request: PromptRequest::new(session_id.clone(), vec!["/review-branch feature".into()]),
        response_tx: prompt_response_tx,
    })?;

    let stop_reason = prompt_response_rx.await??.await??;
    assert_eq!(stop_reason, StopReason::EndTurn);
    drop(message_tx);

    let notifications = client.notifications.lock().unwrap();
    assert_eq!(notifications.len(), 1);
    assert!(
        matches!(
            &notifications[0].update,
            SessionUpdate::AgentMessageChunk(ContentChunk {
                content: ContentBlock::Text(TextContent { text, .. }),
                ..
            }) if text == "changes against 'feature'" // we echo the prompt
        ),
        "notifications don't match {notifications:?}"
    );

    let ops = thread.ops.lock().unwrap();
    assert_eq!(
        ops.as_slice(),
        &[Op::Review {
            review_request: ReviewRequest {
                user_facing_hint: Some(user_facing_hint(&ReviewTarget::BaseBranch {
                    branch: "feature".to_owned()
                })),
                target: ReviewTarget::BaseBranch {
                    branch: "feature".to_owned()
                },
            }
        }],
        "ops don't match {ops:?}"
    );

    Ok(())
}

#[tokio::test]
async fn test_delta_deduplication() -> anyhow::Result<()> {
    let (session_id, client, _, message_tx, _handle) = setup().await?;
    let (prompt_response_tx, prompt_response_rx) = tokio::sync::oneshot::channel();

    message_tx.send(ThreadMessage::Prompt {
        request: PromptRequest::new(session_id.clone(), vec!["test delta".into()]),
        response_tx: prompt_response_tx,
    })?;

    let stop_reason = prompt_response_rx.await??.await??;
    assert_eq!(stop_reason, StopReason::EndTurn);
    drop(message_tx);

    // We should only get ONE notification, not duplicates from both delta and non-delta
    let notifications = client.notifications.lock().unwrap();
    assert_eq!(
        notifications.len(),
        1,
        "Should only receive delta event, not duplicate non-delta. Got: {notifications:?}"
    );
    assert!(matches!(
        &notifications[0].update,
        SessionUpdate::AgentMessageChunk(ContentChunk {
            content: ContentBlock::Text(TextContent { text, .. }),
            ..
        }) if text == "test delta"
    ));

    Ok(())
}
