use super::*;

impl PromptState {
pub(super) async fn maybe_publish_session_title(
    &self,
    client: &SessionClient,
    response_text: Option<&str>,
) {
    if self.session_title.lock().unwrap().is_some() {
        return;
    }

    let mut title_prompt = self.prompt_text.clone();
    match self.thread.read_thread_title_state().await {
        Ok(state) => {
            if state.first_user_message.is_some() {
                title_prompt = state.first_user_message.clone();
            }
            if let Some(title) = state.name.as_deref().and_then(|name| {
                normalize_session_title(name, state.first_user_message.as_deref())
            }) {
                publish_session_title(&self.session_title, client, title);
                return;
            }
        }
        Err(err) => warn!("Failed to read thread name before title generation: {err}"),
    }

    let (Some(generator), Some(prompt_text)) = (
        self.title_generator.as_ref(),
        title_prompt
            .as_deref()
            .filter(|text| !text.trim().is_empty()),
    ) else {
        return;
    };

    let title = match tokio::time::timeout(
        SESSION_TITLE_GENERATION_TIMEOUT,
        generator.generate_title(&self.session_id, prompt_text, response_text),
    )
    .await
    {
        Ok(Ok(Some(title))) => title,
        Ok(Ok(None)) => return,
        Ok(Err(err)) => {
            warn!("Failed to generate session title: {err}");
            match self
                .generate_session_title_via_hidden_turn(prompt_text, response_text)
                .await
            {
                Ok(Some(title)) => title,
                Ok(None) => return,
                Err(err) => {
                    warn!("Failed to generate session title via hidden turn: {err}");
                    return;
                }
            }
        }
        Err(_) => {
            warn!("Timed out generating session title");
            match self
                .generate_session_title_via_hidden_turn(prompt_text, response_text)
                .await
            {
                Ok(Some(title)) => title,
                Ok(None) => return,
                Err(err) => {
                    warn!("Failed to generate session title via hidden turn: {err}");
                    return;
                }
            }
        }
    };

    if let Err(err) = self.thread.set_thread_name(title.clone()).await {
        warn!("Failed to persist generated thread name: {err}");
    }

    publish_session_title(&self.session_title, client, title);
}

async fn generate_session_title_via_hidden_turn(
    &self,
    prompt_text: &str,
    response_text: Option<&str>,
) -> anyhow::Result<Option<String>> {
    let title_prompt = build_session_title_prompt(prompt_text, response_text);
    let submission_id = self
        .thread
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: title_prompt,
                text_elements: vec![],
            }],
            final_output_json_schema: None,
            environments: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;

    let title = match tokio::time::timeout(
        SESSION_TITLE_GENERATION_TIMEOUT,
        self.collect_hidden_title_turn(&submission_id, prompt_text),
    )
    .await
    {
        Ok(result) => result?,
        Err(_) => {
            warn!("Timed out generating session title via hidden turn");
            if let Err(err) = self.thread.submit(Op::Interrupt).await {
                warn!("Failed to interrupt timed out hidden session title turn: {err}");
            }
            return Ok(None);
        }
    };

    if !self.rollback_hidden_title_turn().await {
        warn!(
            "Hidden session title turn was not rolled back; publishing title anyway because rollback failure already leaves the thread state unchanged"
        );
    }

    Ok(title)
}

async fn collect_hidden_title_turn(
    &self,
    submission_id: &str,
    prompt_text: &str,
) -> anyhow::Result<Option<String>> {
    let mut title = String::new();

    loop {
        let Event { id, msg } = self.thread.next_event().await?;
        if id != submission_id {
            warn!(
                "Ignoring event for unrelated submission while collecting hidden session title: {id}"
            );
            continue;
        }

        match msg {
            EventMsg::AgentMessageContentDelta(AgentMessageContentDeltaEvent {
                delta, ..
            }) => title.push_str(&delta),
            EventMsg::AgentMessage(AgentMessageEvent { message, .. }) => {
                if title.trim().is_empty() {
                    title.push_str(&message);
                }
            }
            EventMsg::TurnComplete(TurnCompleteEvent {
                last_agent_message, ..
            }) => {
                if title.trim().is_empty()
                    && let Some(last_agent_message) = last_agent_message
                {
                    title.push_str(&last_agent_message);
                }
                break;
            }
            EventMsg::TurnAborted(event) => {
                warn!(
                    "Hidden session title turn aborted: turn_id={:?}, reason={:?}",
                    event.turn_id, event.reason
                );
                return Ok(None);
            }
            EventMsg::Error(ErrorEvent { message, .. }) => {
                return Err(anyhow::anyhow!("hidden title turn failed: {message}"));
            }
            EventMsg::StreamError(StreamErrorEvent { message, .. }) => {
                return Err(anyhow::anyhow!(
                    "hidden title turn stream failed: {message}"
                ));
            }
            EventMsg::ExecApprovalRequest(..)
            | EventMsg::RequestPermissions(..)
            | EventMsg::DynamicToolCallRequest(..)
            | EventMsg::McpToolCallBegin(..)
            | EventMsg::ApplyPatchApprovalRequest(..)
            | EventMsg::PatchApplyBegin(..) => {
                warn!(
                    "Hidden session title turn requested tool or permission; aborting title generation"
                );
                if let Err(err) = self.thread.submit(Op::Interrupt).await {
                    warn!("Failed to interrupt hidden session title turn: {err}");
                }
                return Ok(None);
            }
            _ => {}
        }
    }

    Ok(normalize_session_title(&title, Some(prompt_text)))
}

async fn rollback_hidden_title_turn(&self) -> bool {
    let rollback_id = match self
        .thread
        .submit(Op::ThreadRollback { num_turns: 1 })
        .await
    {
        Ok(id) => id,
        Err(err) => {
            warn!("Failed to submit hidden session title rollback: {err}");
            return false;
        }
    };

    let rollback = async {
        loop {
            let Event { id, msg } = self.thread.next_event().await?;
            if id != rollback_id {
                warn!(
                    "Ignoring event for unrelated submission while rolling back hidden session title turn: {id}"
                );
                continue;
            }

            match msg {
                EventMsg::ThreadRolledBack(..) => return Ok::<bool, CodexErr>(true),
                EventMsg::Error(ErrorEvent { message, .. }) => {
                    warn!("Hidden session title rollback failed: {message}");
                    return Ok(false);
                }
                EventMsg::StreamError(StreamErrorEvent { message, .. }) => {
                    warn!("Hidden session title rollback stream failed: {message}");
                    return Ok(false);
                }
                _ => {}
            }
        }
    };

    match tokio::time::timeout(SESSION_TITLE_ROLLBACK_TIMEOUT, rollback).await {
        Ok(Ok(rolled_back)) => rolled_back,
        Ok(Err(err)) => {
            warn!("Failed to drain hidden session title rollback: {err}");
            false
        }
        Err(_) => {
            warn!("Timed out rolling back hidden session title turn");
            false
        }
    }
}

}
