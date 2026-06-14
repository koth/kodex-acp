use super::*;

#[tokio::test]
async fn first_turn_persists_and_publishes_llm_session_title() -> anyhow::Result<()> {
    let title_generator = Arc::new(StubSessionTitleGenerator::new(Some("LLM Session Title")));
    let title_generator_trait: Arc<dyn SessionTitleGenerator> = title_generator.clone();
    let (session_id, client, thread, message_tx, _handle) =
        setup_with_title_generator(Some(title_generator_trait)).await?;
    let (prompt_response_tx, prompt_response_rx) = tokio::sync::oneshot::channel();

    message_tx.send(ThreadMessage::Prompt {
        request: PromptRequest::new(session_id.clone(), vec!["title-sync".into()]),
        response_tx: prompt_response_tx,
    })?;

    let stop_reason = prompt_response_rx.await??.await??;
    assert_eq!(stop_reason, StopReason::EndTurn);
    drop(message_tx);

    assert_eq!(
        thread.thread_name.lock().unwrap().as_deref(),
        Some("LLM Session Title")
    );
    assert_eq!(title_generator.calls.load(Ordering::SeqCst), 1);

    let notifications = client.notifications.lock().unwrap();
    assert!(
        notifications.iter().any(|notification| {
            matches!(
                &notification.update,
                SessionUpdate::SessionInfoUpdate(update)
                    if update.title.value().map(String::as_str)
                        == Some("LLM Session Title")
            )
        }),
        "missing session info title update: {notifications:?}"
    );

    Ok(())
}

#[tokio::test]
async fn first_turn_falls_back_to_hidden_thread_title_when_generator_fails()
-> anyhow::Result<()> {
    let title_generator = Arc::new(FailingSessionTitleGenerator::new());
    let title_generator_trait: Arc<dyn SessionTitleGenerator> = title_generator.clone();
    let (session_id, client, thread, message_tx, _handle) =
        setup_with_title_generator(Some(title_generator_trait)).await?;
    let (prompt_response_tx, prompt_response_rx) = tokio::sync::oneshot::channel();

    message_tx.send(ThreadMessage::Prompt {
        request: PromptRequest::new(session_id.clone(), vec!["title-sync".into()]),
        response_tx: prompt_response_tx,
    })?;

    let stop_reason = prompt_response_rx.await??.await??;
    assert_eq!(stop_reason, StopReason::EndTurn);
    drop(message_tx);

    assert_eq!(
        thread.thread_name.lock().unwrap().as_deref(),
        Some("WOA Title Fix")
    );
    assert_eq!(title_generator.calls.load(Ordering::SeqCst), 1);

    let ops = thread.ops.lock().unwrap();
    assert!(ops.iter().any(|op| matches!(
        op,
        Op::UserInput { items, .. }
            if prompt_text_from_items(items)
                .is_some_and(|text| text.starts_with(SESSION_TITLE_INSTRUCTIONS))
    )));
    assert!(
        ops.iter()
            .any(|op| matches!(op, Op::ThreadRollback { num_turns } if *num_turns == 1))
    );
    drop(ops);

    let notifications = client.notifications.lock().unwrap();
    assert!(
        notifications.iter().any(|notification| {
            matches!(
                &notification.update,
                SessionUpdate::SessionInfoUpdate(update)
                    if update.title.value().map(String::as_str) == Some("WOA Title Fix")
            )
        }),
        "missing fallback session info title update: {notifications:?}"
    );

    Ok(())
}

#[tokio::test]
async fn first_turn_does_not_use_agent_reply_as_session_title() -> anyhow::Result<()> {
    let (session_id, client, thread, message_tx, _handle) = setup().await?;
    let (prompt_response_tx, prompt_response_rx) = tokio::sync::oneshot::channel();

    message_tx.send(ThreadMessage::Prompt {
        request: PromptRequest::new(session_id.clone(), vec!["title-sync".into()]),
        response_tx: prompt_response_tx,
    })?;

    let stop_reason = prompt_response_rx.await??.await??;
    assert_eq!(stop_reason, StopReason::EndTurn);
    drop(message_tx);

    assert_eq!(thread.thread_name.lock().unwrap().as_deref(), None);

    let notifications = client.notifications.lock().unwrap();
    assert!(
        !notifications.iter().any(|notification| matches!(
            &notification.update,
            SessionUpdate::SessionInfoUpdate(update)
                if update.title.value().is_some()
        )),
        "agent reply should not be reused as a title: {notifications:?}"
    );

    Ok(())
}

