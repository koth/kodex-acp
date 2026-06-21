use super::*;

mod config;

pub(super) struct ThreadActor<A> {
    /// Allows for logging out from slash commands
    auth: A,
    /// Used for sending messages back to the client.
    client: SessionClient,
    /// The thread associated with this task.
    thread: Arc<dyn CodexThreadImpl>,
    /// The configuration for the thread.
    config: Config,
    /// The models available for this thread.
    models_manager: Arc<dyn ModelsManagerImpl>,
    /// Internal message sender used to route spawned interaction results back to the actor.
    resolution_tx: mpsc::UnboundedSender<ThreadMessage>,
    /// A sender for each interested `Op` submission that needs events routed.
    submissions: HashMap<String, SubmissionState>,
    /// A receiver for incoming thread messages.
    message_rx: mpsc::UnboundedReceiver<ThreadMessage>,
    /// A receiver for spawned interaction results.
    resolution_rx: mpsc::UnboundedReceiver<ThreadMessage>,
    /// Last config options state we emitted to the client, used for deduping updates.
    last_sent_config_options: Option<Vec<SessionConfigOption>>,
    /// Last protocol-visible session title, if one has been provided.
    session_title: Arc<Mutex<Option<String>>>,
    /// Hidden LLM title generator used when Codex has not yet persisted a title.
    title_generator: Option<Arc<dyn SessionTitleGenerator>>,
}

impl<A: Auth> ThreadActor<A> {
    #[expect(clippy::too_many_arguments)]
    pub(super) fn new(
        auth: A,
        client: SessionClient,
        thread: Arc<dyn CodexThreadImpl>,
        models_manager: Arc<dyn ModelsManagerImpl>,
        config: Config,
        title_generator: Option<Arc<dyn SessionTitleGenerator>>,
        message_rx: mpsc::UnboundedReceiver<ThreadMessage>,
        resolution_tx: mpsc::UnboundedSender<ThreadMessage>,
        resolution_rx: mpsc::UnboundedReceiver<ThreadMessage>,
    ) -> Self {
        Self {
            auth,
            client,
            thread,
            config,
            models_manager,
            resolution_tx,
            submissions: HashMap::new(),
            message_rx,
            resolution_rx,
            last_sent_config_options: None,
            session_title: Arc::new(Mutex::new(None)),
            title_generator,
        }
    }

    pub(super) async fn spawn(mut self) {
        let mut message_rx_open = true;
        loop {
            tokio::select! {
                biased;
                message = self.message_rx.recv(), if message_rx_open => match message {
                    Some(message) => self.handle_message(message).await,
                    None => message_rx_open = false,
                },
                message = self.resolution_rx.recv() => if let Some(message) = message {
                    self.handle_message(message).await
                },
                event = self.thread.next_event() => match event {
                    Ok(event) => self.handle_event(event).await,
                    Err(e) => {
                        error!("Error getting next event: {:?}", e);
                        break;
                    }
                }
            }
            // Litter collection of senders with no receivers
            self.submissions
                .retain(|_, submission| submission.is_active());

            if !message_rx_open && self.submissions.is_empty() {
                break;
            }
        }
    }

    async fn handle_message(&mut self, message: ThreadMessage) {
        match message {
            ThreadMessage::Load { response_tx } => {
                let result = self.handle_load().await;
                drop(response_tx.send(result));
                let client = self.client.clone();
                // Have this happen after the session is loaded by putting it
                // in a separate task
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    client.send_notification(SessionUpdate::AvailableCommandsUpdate(
                        AvailableCommandsUpdate::new(Self::builtin_commands()),
                    ));
                });
            }
            ThreadMessage::GetConfigOptions { response_tx } => {
                let result = self.config_options().await;
                drop(response_tx.send(result));
            }
            ThreadMessage::Prompt {
                request,
                response_tx,
            } => {
                let result = self.handle_prompt(request).await;
                drop(response_tx.send(result));
            }
            ThreadMessage::SetMode { mode, response_tx } => {
                let result = self.handle_set_mode(mode).await;
                drop(response_tx.send(result));
                self.maybe_emit_config_options_update().await;
            }
            ThreadMessage::SetModel { model, response_tx } => {
                let result = self.handle_set_model(model).await;
                drop(response_tx.send(result));
                self.maybe_emit_config_options_update().await;
            }
            ThreadMessage::SetConfigOption {
                config_id,
                value,
                response_tx,
            } => {
                let result = self.handle_set_config_option(config_id, value).await;
                drop(response_tx.send(result));
            }
            ThreadMessage::Cancel { response_tx } => {
                let result = self.handle_cancel().await;
                drop(response_tx.send(result));
            }
            ThreadMessage::StopTool {
                tool_call_id,
                response_tx,
            } => {
                let result = self.handle_stop_tool(tool_call_id).await;
                drop(response_tx.send(result));
            }
            ThreadMessage::Shutdown { response_tx } => {
                let result = self.handle_shutdown().await;
                drop(response_tx.send(result));
            }
            ThreadMessage::ReplayHistory {
                history,
                response_tx,
            } => {
                let result = self.handle_replay_history(history);
                drop(response_tx.send(result));
            }
            ThreadMessage::PermissionRequestResolved {
                submission_id,
                interaction_id,
                request_key,
                response,
            } => {
                let result = {
                    let Some(submission) = self.submissions.get_mut(&submission_id) else {
                        warn!(
                            "Ignoring permission response for unknown submission ID: {submission_id}"
                        );
                        return;
                    };

                    submission
                        .handle_permission_request_resolved(
                            &self.client,
                            interaction_id,
                            request_key,
                            response,
                        )
                        .await
                };

                match result {
                    Ok(Some(guidance)) => {
                        if let Err(err) = self.submit_permission_guidance_followup(guidance).await {
                            warn!("Failed to submit permission guidance follow-up: {err:?}");
                        }
                    }
                    Ok(None) => {}
                    Err(err) => {
                        if let Some(submission) = self.submissions.get_mut(&submission_id) {
                            submission.detach_pending_interactions();
                            submission.fail(err);
                        }
                    }
                }
            }
        }
    }

    async fn handle_load(&mut self) -> Result<LoadSessionResponse, Error> {
        Ok(LoadSessionResponse::new()
            .models(self.models().await?)
            .modes(self.modes())
            .config_options(self.config_options().await?))
    }

    async fn handle_prompt(
        &mut self,
        request: PromptRequest,
    ) -> Result<oneshot::Receiver<Result<StopReason, Error>>, Error> {
        let (response_tx, response_rx) = oneshot::channel();

        let session_id = request.session_id.clone();
        let items = build_prompt_items(request.prompt);
        let prompt_text = prompt_text_from_items(&items);
        let op;
        if let Some((name, rest)) = extract_slash_command(&items) {
            match name {
                "compact" => {
                    op = Op::Compact;
                    self.client.send_context_compaction_started();
                }
                "init" => {
                    op = Op::UserInput {
                        items: vec![UserInput::Text {
                            text: INIT_COMMAND_PROMPT.into(),
                            text_elements: vec![],
                        }],
                        final_output_json_schema: None,
                        environments: None,
                        responsesapi_client_metadata: None,
                        additional_context: Default::default(),
                        thread_settings: Default::default(),
                    }
                }
                "review" => {
                    let instructions = rest.trim();
                    let target = if instructions.is_empty() {
                        ReviewTarget::UncommittedChanges
                    } else {
                        ReviewTarget::Custom {
                            instructions: instructions.to_owned(),
                        }
                    };

                    op = Op::Review {
                        review_request: ReviewRequest {
                            user_facing_hint: Some(user_facing_hint(&target)),
                            target,
                        },
                    }
                }
                "review-branch" if !rest.is_empty() => {
                    let target = ReviewTarget::BaseBranch {
                        branch: rest.trim().to_owned(),
                    };
                    op = Op::Review {
                        review_request: ReviewRequest {
                            user_facing_hint: Some(user_facing_hint(&target)),
                            target,
                        },
                    }
                }
                "review-commit" if !rest.is_empty() => {
                    let target = ReviewTarget::Commit {
                        sha: rest.trim().to_owned(),
                        title: None,
                    };
                    op = Op::Review {
                        review_request: ReviewRequest {
                            user_facing_hint: Some(user_facing_hint(&target)),
                            target,
                        },
                    }
                }
                "logout" => {
                    self.auth.logout().await?;
                    return Err(Error::auth_required());
                }
                _ => {
                    op = Op::UserInput {
                        items,
                        final_output_json_schema: None,
                        environments: None,
                        responsesapi_client_metadata: None,
                        additional_context: Default::default(),
                        thread_settings: Default::default(),
                    }
                }
            }
        } else {
            op = Op::UserInput {
                items,
                final_output_json_schema: None,
                environments: None,
                responsesapi_client_metadata: None,
                additional_context: Default::default(),
                thread_settings: Default::default(),
            }
        }

        let submission_id = self
            .thread
            .submit(op.clone())
            .await
            .map_err(|e| Error::internal_error().data(e.to_string()))?;

        info!("Submitted prompt with submission_id: {submission_id}");
        info!("Starting to wait for conversation events for submission_id: {submission_id}");

        let state = SubmissionState::Prompt(PromptState::new(
            submission_id.clone(),
            session_id,
            self.thread.clone(),
            self.session_title.clone(),
            self.title_generator.clone(),
            prompt_text,
            self.resolution_tx.clone(),
            response_tx,
        ));

        self.submissions.insert(submission_id, state);

        Ok(response_rx)
    }

    async fn submit_permission_guidance_followup(&mut self, guidance: String) -> Result<(), Error> {
        let guidance = guidance.trim();
        if guidance.is_empty() {
            return Ok(());
        }

        let items = vec![UserInput::Text {
            text: guidance.to_string(),
            text_elements: vec![],
        }];
        let submission_id = self
            .thread
            .submit(Op::UserInput {
                items: items.clone(),
                final_output_json_schema: None,
                environments: None,
                responsesapi_client_metadata: None,
                additional_context: Default::default(),
                thread_settings: Default::default(),
            })
            .await
            .map_err(|e| Error::internal_error().data(e.to_string()))?;

        info!("Submitted permission guidance follow-up with submission_id: {submission_id}");

        let (response_tx, _response_rx) = oneshot::channel();
        let state = SubmissionState::Prompt(PromptState::new(
            submission_id.clone(),
            self.client.session_id.clone(),
            self.thread.clone(),
            self.session_title.clone(),
            self.title_generator.clone(),
            prompt_text_from_items(&items),
            self.resolution_tx.clone(),
            response_tx,
        ));
        self.submissions.insert(submission_id, state);

        Ok(())
    }

    async fn handle_set_mode(&mut self, mode: SessionModeId) -> Result<(), Error> {
        let preset = APPROVAL_PRESETS
            .iter()
            .find(|preset| mode.0.as_ref() == preset.id)
            .ok_or_else(Error::invalid_params)?;
        let collaboration_mode = self.collaboration_mode_for_session_mode(preset.id).await;

        self.thread
            .submit(Op::ThreadSettings {
                thread_settings: ThreadSettingsOverrides {
                    approval_policy: Some(preset.approval),
                    permission_profile: Some(preset.permission_profile.clone()),
                    active_permission_profile: active_profile_id_for_session_mode(preset.id)
                        .map(ActivePermissionProfile::new),
                    collaboration_mode: Some(collaboration_mode),
                    ..Default::default()
                },
            })
            .await
            .map_err(|e| Error::from(anyhow::anyhow!(e)))?;

        self.config
            .permissions
            .approval_policy
            .set(preset.approval)
            .map_err(|e| Error::from(anyhow::anyhow!(e)))?;
        self.config
            .permissions
            .set_permission_profile(preset.permission_profile.clone())
            .map_err(|e| Error::from(anyhow::anyhow!(e)))?;

        if mode_trusts_project(preset.id) {
            set_project_trust_level(
                &self.config.codex_home,
                &self.config.cwd,
                TrustLevel::Trusted,
            )?;
        }

        Ok(())
    }

    async fn get_current_model(&self) -> String {
        self.models_manager.get_model(&self.config.model).await
    }

    async fn handle_set_model(&mut self, model: ModelId) -> Result<(), Error> {
        let (selected_provider, model_id) = Self::decode_provider_value(model.0.as_ref());
        if let Some(provider) = selected_provider.as_deref() {
            self.set_active_model_provider(provider)?;
        }

        // Try parsing as preset format, otherwise use as-is, fallback to config
        let decoded_model = ModelId::new(model_id);
        let (model_to_use, effort_to_use) =
            if let Some((m, e)) = Self::parse_model_id(&decoded_model) {
                (m, Some(e))
            } else {
                let model_str = decoded_model.0.to_string();
                let fallback = if !model_str.is_empty() {
                    model_str
                } else {
                    self.get_current_model().await
                };
                (fallback, self.config.model_reasoning_effort)
            };

        if model_to_use.is_empty() {
            return Err(Error::invalid_params().data("No model parsed or configured"));
        }

        self.thread
            .submit(Op::ThreadSettings {
                thread_settings: ThreadSettingsOverrides {
                    model: Some(model_to_use.clone()),
                    effort: Some(effort_to_use),
                    ..Default::default()
                },
            })
            .await
            .map_err(|e| Error::from(anyhow::anyhow!(e)))?;

        self.config.model = Some(model_to_use);
        self.config.model_reasoning_effort = effort_to_use;

        Ok(())
    }

    async fn handle_cancel(&mut self) -> Result<(), Error> {
        self.detach_pending_interactions();
        self.thread
            .submit(Op::Interrupt)
            .await
            .map_err(|e| Error::from(anyhow::anyhow!(e)))?;
        Ok(())
    }

    async fn handle_stop_tool(&mut self, tool_call_id: String) -> Result<bool, Error> {
        let mut stopped = false;
        for submission in self.submissions.values_mut() {
            if submission.stop_tool(&self.client, &tool_call_id) {
                stopped = true;
            }
        }

        if stopped {
            self.thread
                .submit(Op::Interrupt)
                .await
                .map_err(|e| Error::from(anyhow::anyhow!(e)))?;
        }

        Ok(stopped)
    }

    async fn handle_shutdown(&mut self) -> Result<(), Error> {
        self.detach_pending_interactions();
        self.thread
            .submit(Op::Shutdown)
            .await
            .map_err(|e| Error::from(anyhow::anyhow!(e)))?;
        Ok(())
    }

    fn detach_pending_interactions(&mut self) {
        for submission in self.submissions.values_mut() {
            submission.detach_pending_interactions();
        }
    }

    /// Replay conversation history to the client via session/update notifications.
    /// This is called when loading a session to stream all prior messages.
    ///
    /// We process both `EventMsg` and `ResponseItem`:
    /// - `EventMsg` for user/agent messages and reasoning (like the TUI does)
    /// - `ResponseItem` for tool calls only (not persisted as EventMsg)
    fn handle_replay_history(&mut self, history: Vec<RolloutItem>) -> Result<(), Error> {
        for item in history {
            match item {
                RolloutItem::EventMsg(event_msg) => {
                    self.replay_event_msg(&event_msg);
                }
                RolloutItem::ResponseItem(response_item) => {
                    self.replay_response_item(&response_item);
                }
                // Skip SessionMeta, TurnContext, Compacted
                _ => {}
            }
        }
        Ok(())
    }

    /// Convert and send an EventMsg as ACP notification(s) during replay.
    /// Handles messages and reasoning - mirrors the live event handling in PromptState.
    fn replay_event_msg(&self, msg: &EventMsg) {
        match msg {
            EventMsg::UserMessage(UserMessageEvent { message, .. }) => {
                self.client.send_user_message(message.clone());
            }
            EventMsg::AgentMessage(AgentMessageEvent {
                message,
                phase,
                memory_citation: _,
            }) => {
                if is_commentary_phase(phase.as_ref()) {
                    return;
                }
                self.client.send_agent_text(message.clone());
            }
            EventMsg::AgentReasoning(AgentReasoningEvent { text }) => {
                self.client.send_agent_thought(text.clone());
            }
            EventMsg::AgentReasoningRawContent(AgentReasoningRawContentEvent { text }) => {
                self.client.send_agent_thought(text.clone());
            }
            EventMsg::ThreadGoalUpdated(event) => {
                self.client
                    .send_agent_text(format_thread_goal_update(event));
            }
            // Skip other event types during replay - they either:
            // - Are transient (deltas, turn lifecycle)
            // - Don't have direct ACP equivalents
            // - Are handled via ResponseItem instead
            _ => {}
        }
    }

    /// Parse apply_patch call input to extract patch content for display.
    /// Returns (title, locations, content) if successful.
    /// For CustomToolCall, the input is the patch string directly.
    fn parse_apply_patch_call(
        &self,
        input: &str,
    ) -> Option<(String, Vec<ToolCallLocation>, Vec<ToolCallContent>)> {
        // Try to parse the patch using codex-apply-patch parser
        let parsed = parse_patch(input).ok()?;

        let mut locations = Vec::new();
        let mut file_names = Vec::new();
        let mut content = Vec::new();

        for hunk in &parsed.hunks {
            match hunk {
                codex_apply_patch::Hunk::AddFile { path, contents } => {
                    let full_path = self.config.cwd.as_path().join(path);
                    file_names.push(path.display().to_string());
                    locations.push(ToolCallLocation::new(full_path.clone()));
                    // New file: no old_text, new_text is the contents
                    content.push(ToolCallContent::Diff(Diff::new(
                        full_path,
                        contents.clone(),
                    )));
                }
                codex_apply_patch::Hunk::DeleteFile { path } => {
                    let full_path = self.config.cwd.as_path().join(path);
                    file_names.push(path.display().to_string());
                    locations.push(ToolCallLocation::new(full_path.clone()));
                    // Delete file: old_text would be original content, new_text is empty
                    content.push(ToolCallContent::Diff(
                        Diff::new(full_path, "").old_text("[file deleted]"),
                    ));
                }
                codex_apply_patch::Hunk::UpdateFile {
                    path,
                    move_path,
                    chunks,
                } => {
                    let full_path = self.config.cwd.as_path().join(path);
                    let dest_path = move_path
                        .as_ref()
                        .map(|p| self.config.cwd.as_path().join(p))
                        .unwrap_or_else(|| full_path.clone());
                    file_names.push(path.display().to_string());
                    locations.push(ToolCallLocation::new(dest_path.clone()));

                    // Build old and new text from chunks
                    let old_lines: Vec<String> = chunks
                        .iter()
                        .flat_map(|c| c.old_lines.iter().cloned())
                        .collect();
                    let new_lines: Vec<String> = chunks
                        .iter()
                        .flat_map(|c| c.new_lines.iter().cloned())
                        .collect();

                    content.push(ToolCallContent::Diff(
                        Diff::new(dest_path, new_lines.join("\n")).old_text(old_lines.join("\n")),
                    ));
                }
            }
        }

        let title = if file_names.is_empty() {
            "Apply patch".to_string()
        } else {
            format!("Edit {}", file_names.join(", "))
        };

        Some((title, locations, content))
    }

    /// Parse shell function call arguments to extract command info for rich display.
    /// Returns (title, kind, locations) if successful.
    ///
    /// Handles both:
    /// - `shell` / `container.exec`: `command` is `Vec<String>`
    /// - `shell_command`: `command` is a `String` (shell script)
    fn parse_shell_function_call(
        &self,
        name: &str,
        arguments: &str,
    ) -> Option<(String, ToolKind, Vec<ToolCallLocation>)> {
        // Extract command and workdir based on tool type
        let (command_vec, workdir): (Vec<String>, Option<String>) = if name == "shell_command" {
            // shell_command: command is a string (shell script)
            #[derive(serde::Deserialize)]
            struct ShellCommandArgs {
                command: String,
                #[serde(default)]
                workdir: Option<String>,
            }
            let args: ShellCommandArgs = serde_json::from_str(arguments).ok()?;
            // Wrap in bash -lc for parsing
            (
                vec!["bash".to_string(), "-lc".to_string(), args.command],
                args.workdir,
            )
        } else {
            // shell / container.exec: command is Vec<String>
            #[derive(serde::Deserialize)]
            struct ShellArgs {
                command: Vec<String>,
                #[serde(default)]
                workdir: Option<String>,
            }
            let args: ShellArgs = serde_json::from_str(arguments).ok()?;
            (args.command, args.workdir)
        };

        let cwd = workdir
            .map(PathBuf::from)
            .unwrap_or_else(|| self.config.cwd.clone().into());

        let parsed_cmd = parse_command(&command_vec);
        let ParseCommandToolCall {
            title,
            file_extension: _,
            terminal_output: _,
            locations,
            kind,
        } = parse_command_tool_call(parsed_cmd, &cwd);

        Some((title, kind, locations))
    }

    /// Convert and send a single ResponseItem as ACP notification(s) during replay.
    /// Only handles tool calls - messages/reasoning are handled via EventMsg.
    fn replay_response_item(&self, item: &ResponseItem) {
        match item {
            // Skip Message and Reasoning - these are handled via EventMsg
            ResponseItem::Message { .. } | ResponseItem::Reasoning { .. } => {}
            ResponseItem::FunctionCall {
                name,
                arguments,
                call_id,
                ..
            } => {
                // Check if this is a shell command - parse it like we do for LocalShellCall
                if matches!(name.as_str(), "shell" | "container.exec" | "shell_command")
                    && let Some((title, kind, locations)) =
                        self.parse_shell_function_call(name, arguments)
                {
                    self.client.send_tool_call(
                        ToolCall::new(call_id.clone(), title)
                            .kind(kind)
                            .status(ToolCallStatus::Completed)
                            .locations(locations)
                            .raw_input(serde_json::from_str::<serde_json::Value>(arguments).ok()),
                    );
                    return;
                }

                // Fall through to generic function call handling
                self.client.send_completed_tool_call(
                    call_id.clone(),
                    name.clone(),
                    ToolKind::Other,
                    serde_json::from_str(arguments).ok(),
                );
            }
            ResponseItem::FunctionCallOutput { call_id, output } => {
                self.client
                    .send_tool_call_completed(call_id.clone(), serde_json::to_value(output).ok());
            }
            ResponseItem::LocalShellCall {
                call_id: Some(call_id),
                action,
                status,
                ..
            } => {
                let codex_protocol::models::LocalShellAction::Exec(exec) = action;
                let cwd = exec
                    .working_directory
                    .as_ref()
                    .map(PathBuf::from)
                    .unwrap_or_else(|| self.config.cwd.clone().into());

                // Parse the command to get rich info like the live event handler does
                let parsed_cmd = parse_command(&exec.command);
                let ParseCommandToolCall {
                    title,
                    file_extension: _,
                    terminal_output: _,
                    locations,
                    kind,
                } = parse_command_tool_call(parsed_cmd, &cwd);

                let tool_status = match status {
                    codex_protocol::models::LocalShellStatus::Completed => {
                        ToolCallStatus::Completed
                    }
                    codex_protocol::models::LocalShellStatus::InProgress
                    | codex_protocol::models::LocalShellStatus::Incomplete => {
                        ToolCallStatus::Failed
                    }
                };
                self.client.send_tool_call(
                    ToolCall::new(call_id.clone(), title)
                        .kind(kind)
                        .status(tool_status)
                        .locations(locations),
                );
            }
            ResponseItem::CustomToolCall {
                name,
                input,
                call_id,
                ..
            } => {
                // Check if this is an apply_patch call - show the patch content
                if name == "apply_patch"
                    && let Some((title, locations, content)) = self.parse_apply_patch_call(input)
                {
                    self.client.send_tool_call(
                        ToolCall::new(call_id.clone(), title)
                            .kind(ToolKind::Edit)
                            .status(ToolCallStatus::Completed)
                            .locations(locations)
                            .content(content)
                            .raw_input(serde_json::from_str::<serde_json::Value>(input).ok()),
                    );
                    return;
                }

                // Fall through to generic custom tool call handling
                self.client.send_completed_tool_call(
                    call_id.clone(),
                    name.clone(),
                    ToolKind::Other,
                    serde_json::from_str(input).ok(),
                );
            }
            ResponseItem::CustomToolCallOutput {
                name: _,
                call_id,
                output,
            } => {
                self.client
                    .send_tool_call_completed(call_id.clone(), Some(serde_json::json!(output)));
            }
            ResponseItem::WebSearchCall { id, action, .. } => {
                let (title, call_id) = if let Some(action) = action {
                    web_search_action_to_title_and_id(id, action)
                } else {
                    ("Web Search".into(), generate_fallback_id("web_search"))
                };
                self.client.send_tool_call(
                    ToolCall::new(call_id, title)
                        .kind(ToolKind::Search)
                        .status(ToolCallStatus::Completed),
                );
            }
            ResponseItem::ImageGenerationCall {
                id,
                status,
                revised_prompt,
                result,
            } => {
                self.client.send_tool_call(
                    ToolCall::new(id.clone(), "Image generation")
                        .kind(ToolKind::Other)
                        .status(image_generation_tool_status(status))
                        .content(image_generation_content(
                            revised_prompt.clone(),
                            result.clone(),
                            None,
                        ))
                        .raw_output(serde_json::json!({
                            "status": status,
                            "revised_prompt": revised_prompt,
                            "result": result,
                        })),
                );
            }
            // Skip GhostSnapshot, Compaction, Other, LocalShellCall without call_id
            _ => {}
        }
    }

    async fn handle_event(&mut self, Event { id, msg }: Event) {
        let handled_globally = self.handle_global_event(&msg);
        if let Some(submission) = self.submissions.get_mut(&id) {
            submission.handle_event(&self.client, msg).await;
        } else if !handled_globally {
            warn!("Received event for unknown submission ID: {id} {msg:?}");
        }
    }

    fn handle_global_event(&mut self, msg: &EventMsg) -> bool {
        if let EventMsg::SessionConfigured(event) = msg {
            if let Some(title) = event
                .thread_name
                .as_deref()
                .and_then(|title| normalize_session_title(title, None))
            {
                publish_session_title(&self.session_title, &self.client, title);
            }
            true
        } else {
            false
        }
    }
}
