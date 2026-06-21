use super::*;

mod title_flow;
mod tool_events;

pub(super) enum SubmissionState {
    /// User prompts, including slash commands like /init, /review, /compact.
    Prompt(PromptState),
}

impl SubmissionState {
    pub(super) fn is_active(&self) -> bool {
        match self {
            Self::Prompt(state) => state.is_active(),
        }
    }

    pub(super) async fn handle_event(&mut self, client: &SessionClient, event: EventMsg) {
        match self {
            Self::Prompt(state) => state.handle_event(client, event).await,
        }
    }

    pub(super) async fn handle_permission_request_resolved(
        &mut self,
        client: &SessionClient,
        interaction_id: u64,
        request_key: String,
        response: Result<RequestPermissionResponse, Error>,
    ) -> Result<Option<String>, Error> {
        match self {
            Self::Prompt(state) => {
                state
                    .handle_permission_request_resolved(
                        client,
                        interaction_id,
                        request_key,
                        response,
                    )
                    .await
            }
        }
    }

    pub(super) fn detach_pending_interactions(&mut self) {
        match self {
            Self::Prompt(state) => {
                state.detach_pending_interactions();
            }
        }
    }

    pub(super) fn stop_tool(&mut self, client: &SessionClient, tool_call_id: &str) -> bool {
        match self {
            Self::Prompt(state) => state.stop_tool(client, tool_call_id),
        }
    }

    pub(super) fn fail(&mut self, err: Error) {
        if let Self::Prompt(state) = self
            && let Some(response_tx) = state.response_tx.take()
        {
            drop(response_tx.send(Err(err)));
        }
    }
}

pub(super) struct ActiveCommand {
    pub(super) tool_call_id: ToolCallId,
    pub(super) terminal_output: bool,
    pub(super) output: String,
    pub(super) file_extension: Option<String>,
}

pub(super) struct PromptState {
    submission_id: String,
    session_id: SessionId,
    active_commands: HashMap<String, ActiveCommand>,
    active_agent_owned_tools: HashSet<String>,
    stopped_agent_owned_tools: HashSet<String>,
    active_web_search: Option<String>,
    active_image_generations: HashSet<String>,
    active_guardian_assessments: HashSet<String>,
    thread: Arc<dyn CodexThreadImpl>,
    session_title: Arc<Mutex<Option<String>>>,
    title_generator: Option<Arc<dyn SessionTitleGenerator>>,
    prompt_text: Option<String>,
    resolution_tx: mpsc::UnboundedSender<ThreadMessage>,
    pending_permission_interactions: HashMap<String, PendingPermissionInteraction>,
    next_permission_interaction_id: u64,
    event_count: usize,
    response_tx: Option<oneshot::Sender<Result<StopReason, Error>>>,
    seen_final_message_deltas: bool,
    seen_commentary_message_deltas: bool,
    seen_reasoning_deltas: bool,
    agent_message_text: String,
    commentary_message_item_ids: HashSet<String>,
}

impl PromptState {
    pub(super) fn new(
        submission_id: String,
        session_id: SessionId,
        thread: Arc<dyn CodexThreadImpl>,
        session_title: Arc<Mutex<Option<String>>>,
        title_generator: Option<Arc<dyn SessionTitleGenerator>>,
        prompt_text: Option<String>,
        resolution_tx: mpsc::UnboundedSender<ThreadMessage>,
        response_tx: oneshot::Sender<Result<StopReason, Error>>,
    ) -> Self {
        Self {
            submission_id,
            session_id,
            active_commands: HashMap::new(),
            active_agent_owned_tools: HashSet::new(),
            stopped_agent_owned_tools: HashSet::new(),
            active_web_search: None,
            active_image_generations: HashSet::new(),
            active_guardian_assessments: HashSet::new(),
            thread,
            session_title,
            title_generator,
            prompt_text,
            resolution_tx,
            pending_permission_interactions: HashMap::new(),
            next_permission_interaction_id: 0,
            event_count: 0,
            response_tx: Some(response_tx),
            seen_final_message_deltas: false,
            seen_commentary_message_deltas: false,
            seen_reasoning_deltas: false,
            agent_message_text: String::new(),
            commentary_message_item_ids: HashSet::new(),
        }
    }

    fn is_active(&self) -> bool {
        let Some(response_tx) = &self.response_tx else {
            return false;
        };
        !response_tx.is_closed()
    }

    pub(in crate::thread) fn detach_pending_interactions(&mut self) {
        // Keep detached permission request tasks running so ACP can route the
        // client's required `Cancelled` response after session cancellation.
        self.pending_permission_interactions.clear();
    }

    pub(in crate::thread) fn stop_tool(
        &mut self,
        client: &SessionClient,
        tool_call_id: &str,
    ) -> bool {
        let mut stopped = false;

        if self
            .active_commands
            .remove(tool_call_id)
            .or_else(|| self.remove_active_command_by_tool_call_id(tool_call_id))
            .is_some()
        {
            stopped = true;
        }

        if self.active_web_search.as_deref() == Some(tool_call_id) {
            self.active_web_search = None;
            stopped = true;
        }

        if self.active_image_generations.remove(tool_call_id) {
            stopped = true;
        }

        if self.active_agent_owned_tools.remove(tool_call_id) {
            stopped = true;
        }

        if let Some(assessment_id) = self
            .active_guardian_assessments
            .iter()
            .find(|id| guardian_assessment_tool_call_id(id) == tool_call_id)
            .cloned()
        {
            self.active_guardian_assessments.remove(&assessment_id);
            stopped = true;
        }

        if stopped {
            self.stopped_agent_owned_tools
                .insert(tool_call_id.to_string());
            client.send_tool_call_update(ToolCallUpdate::new(
                ToolCallId::new(tool_call_id.to_string()),
                ToolCallUpdateFields::new()
                    .status(ToolCallStatus::Failed)
                    .content(vec![ToolCallContent::Content(Content::new(
                        "Tool stopped by user",
                    ))])
                    .raw_output(json!({
                        "interrupted": true,
                        "reason": "tool stopped by user"
                    })),
            ));
        }

        stopped
    }

    fn remove_active_command_by_tool_call_id(
        &mut self,
        tool_call_id: &str,
    ) -> Option<ActiveCommand> {
        let command_id = self
            .active_commands
            .iter()
            .find_map(|(command_id, command)| {
                (command.tool_call_id.0.as_ref() == tool_call_id).then(|| command_id.clone())
            })?;
        self.active_commands.remove(&command_id)
    }

    fn spawn_permission_request(
        &mut self,
        client: &SessionClient,
        request_key: String,
        pending_request: PendingPermissionRequest,
        tool_call: ToolCallUpdate,
        options: Vec<PermissionOption>,
        request_meta: Option<Meta>,
    ) {
        let interaction_id = self.next_permission_interaction_id;
        self.next_permission_interaction_id = self.next_permission_interaction_id.wrapping_add(1);
        let client = client.clone();
        let resolution_tx = self.resolution_tx.clone();
        let submission_id = self.submission_id.clone();
        let resolved_request_key = request_key.clone();
        drop(tokio::spawn(async move {
            let response = client
                .request_permission_with_meta(tool_call, options, request_meta)
                .await;
            drop(
                resolution_tx.send(ThreadMessage::PermissionRequestResolved {
                    submission_id,
                    interaction_id,
                    request_key: resolved_request_key,
                    response,
                }),
            );
        }));

        self.pending_permission_interactions.insert(
            request_key,
            PendingPermissionInteraction {
                id: interaction_id,
                request: pending_request,
            },
        );
    }

    pub(in crate::thread) async fn handle_permission_request_resolved(
        &mut self,
        _client: &SessionClient,
        interaction_id: u64,
        request_key: String,
        response: Result<RequestPermissionResponse, Error>,
    ) -> Result<Option<String>, Error> {
        let Some(pending_interaction_id) = self
            .pending_permission_interactions
            .get(&request_key)
            .map(|interaction| interaction.id)
        else {
            warn!("Ignoring permission response for unknown request key: {request_key}");
            return Ok(None);
        };

        if pending_interaction_id != interaction_id {
            warn!("Ignoring stale permission response for request key: {request_key}");
            return Ok(None);
        }

        let Some(interaction) = self.pending_permission_interactions.remove(&request_key) else {
            warn!("Ignoring permission response for unknown request key: {request_key}");
            return Ok(None);
        };
        let pending_request = interaction.request;
        let response = response?;
        let permission_guidance = permission_guidance_from_response(&response);
        let user_input_response = user_input_response_from_permission_response(&response);

        match pending_request {
            PendingPermissionRequest::Exec {
                approval_id,
                turn_id,
                option_map,
            } => {
                let decision = match response.outcome {
                    RequestPermissionOutcome::Selected(SelectedPermissionOutcome {
                        option_id,
                        ..
                    }) => option_map
                        .get(option_id.0.as_ref())
                        .cloned()
                        .unwrap_or(ReviewDecision::Abort),
                    RequestPermissionOutcome::Cancelled | _ => ReviewDecision::Abort,
                };

                self.thread
                    .submit(Op::ExecApproval {
                        id: approval_id,
                        turn_id: Some(turn_id),
                        decision: decision.clone(),
                    })
                    .await
                    .map_err(|e| Error::from(anyhow::anyhow!(e)))?;
                Ok(permission_guidance_followup(&decision, permission_guidance))
            }
            PendingPermissionRequest::Patch {
                call_id,
                option_map,
            } => {
                let decision = match response.outcome {
                    RequestPermissionOutcome::Selected(SelectedPermissionOutcome {
                        option_id,
                        ..
                    }) => option_map
                        .get(option_id.0.as_ref())
                        .cloned()
                        .unwrap_or(ReviewDecision::Abort),
                    RequestPermissionOutcome::Cancelled | _ => ReviewDecision::Abort,
                };

                self.thread
                    .submit(Op::PatchApproval {
                        id: call_id,
                        decision: decision.clone(),
                    })
                    .await
                    .map_err(|e| Error::from(anyhow::anyhow!(e)))?;
                Ok(permission_guidance_followup(&decision, permission_guidance))
            }
            PendingPermissionRequest::RequestPermissions {
                call_id,
                permissions,
            } => {
                let response = match response.outcome {
                    RequestPermissionOutcome::Selected(SelectedPermissionOutcome {
                        option_id,
                        ..
                    }) => match option_id.0.as_ref() {
                        "approved-for-session" => RequestPermissionsResponse {
                            permissions: permissions.clone(),
                            scope: PermissionGrantScope::Session,
                            strict_auto_review: false,
                        },
                        "approved" => RequestPermissionsResponse {
                            permissions: permissions.clone(),
                            scope: PermissionGrantScope::Turn,
                            strict_auto_review: false,
                        },
                        _ => RequestPermissionsResponse {
                            permissions: RequestPermissionProfile::default(),
                            scope: PermissionGrantScope::Turn,
                            strict_auto_review: true,
                        },
                    },
                    RequestPermissionOutcome::Cancelled | _ => RequestPermissionsResponse {
                        permissions: RequestPermissionProfile::default(),
                        scope: PermissionGrantScope::Turn,
                        strict_auto_review: true,
                    },
                };

                self.thread
                    .submit(Op::RequestPermissionsResponse {
                        id: call_id,
                        response,
                    })
                    .await
                    .map_err(|e| Error::from(anyhow::anyhow!(e)))?;
                Ok(None)
            }
            PendingPermissionRequest::McpElicitation {
                server_name,
                request_id,
                option_map,
            } => {
                let response = match response.outcome {
                    RequestPermissionOutcome::Selected(SelectedPermissionOutcome {
                        option_id,
                        ..
                    }) => option_map
                        .get(option_id.0.as_ref())
                        .cloned()
                        .unwrap_or_else(ResolvedMcpElicitation::cancel),
                    RequestPermissionOutcome::Cancelled | _ => ResolvedMcpElicitation::cancel(),
                };

                self.thread
                    .submit(Op::ResolveElicitation {
                        server_name,
                        request_id,
                        decision: response.action,
                        content: response.content,
                        meta: response.meta,
                    })
                    .await
                    .map_err(|e| Error::from(anyhow::anyhow!(e)))?;
                Ok(None)
            }
            PendingPermissionRequest::UserInput { id, option_map } => {
                let response = user_input_response.unwrap_or_else(|| match response.outcome {
                    RequestPermissionOutcome::Selected(SelectedPermissionOutcome {
                        option_id,
                        ..
                    }) => option_map
                        .get(option_id.0.as_ref())
                        .map(|answer| user_input_response_from_answer(answer, permission_guidance))
                        .unwrap_or_else(empty_user_input_response),
                    RequestPermissionOutcome::Cancelled | _ => empty_user_input_response(),
                });

                self.thread
                    .submit(Op::UserInputAnswer { id, response })
                    .await
                    .map_err(|e| Error::from(anyhow::anyhow!(e)))?;
                Ok(None)
            }
        }
    }

    #[expect(clippy::too_many_lines)]
    pub(in crate::thread) async fn handle_event(
        &mut self,
        client: &SessionClient,
        event: EventMsg,
    ) {
        self.event_count += 1;

        // Complete any previous web search before starting a new one
        match &event {
            EventMsg::Error(..)
            | EventMsg::StreamError(..)
            | EventMsg::WebSearchBegin(..)
            | EventMsg::UserMessage(..)
            | EventMsg::ExecApprovalRequest(..)
            | EventMsg::ImageGenerationBegin(..)
            | EventMsg::ImageGenerationEnd(..)
            | EventMsg::ExecCommandBegin(..)
            | EventMsg::ExecCommandOutputDelta(..)
            | EventMsg::ExecCommandEnd(..)
            | EventMsg::McpToolCallBegin(..)
            | EventMsg::McpToolCallEnd(..)
            | EventMsg::ApplyPatchApprovalRequest(..)
            | EventMsg::PatchApplyBegin(..)
            | EventMsg::PatchApplyEnd(..)
            | EventMsg::TurnStarted(..)
            | EventMsg::TurnComplete(..)
            | EventMsg::TurnDiff(..)
            | EventMsg::TurnAborted(..)
            | EventMsg::EnteredReviewMode(..)
            | EventMsg::ExitedReviewMode(..)
            | EventMsg::ShutdownComplete => {
                self.complete_web_search(client);
            }
            _ => {}
        }

        match event {
            EventMsg::TurnStarted(TurnStartedEvent {
                model_context_window,
                collaboration_mode_kind,
                turn_id,
                started_at: _,
                ..
            }) => {
                info!("Task started with context window of {turn_id} {model_context_window:?} {collaboration_mode_kind:?}");
            }
            EventMsg::TokenCount(TokenCountEvent { info, .. }) => {
                if let Some(info) = info
                    && let Some(size) = info.model_context_window {
                        let used = info.last_token_usage.tokens_in_context_window().max(0) as u64;
                        client.send_notification(SessionUpdate::UsageUpdate(UsageUpdate::new(
                            used,
                            size as u64,
                        )));
                    }
            }
            EventMsg::ItemStarted(ItemStartedEvent {
                thread_id,
                turn_id,
                item,
                started_at_ms: _,
                ..
            }) => {
                info!("Item started with thread_id: {thread_id}, turn_id: {turn_id}, item: {item:?}");
                if let TurnItem::AgentMessage(message) = &item
                    && is_commentary_phase(message.phase.as_ref())
                {
                    self.commentary_message_item_ids.insert(message.id.clone());
                }
            }
            EventMsg::UserMessage(UserMessageEvent {
                message,
                images: _,
                text_elements: _,
                local_images: _,
                ..
            }) => {
                info!("User message: {message:?}");
            }
            EventMsg::AgentMessageContentDelta(AgentMessageContentDeltaEvent {
                thread_id,
                turn_id,
                item_id,
                delta,
                ..
            }) => {
                info!("Agent message content delta received: thread_id: {thread_id}, turn_id: {turn_id}, item_id: {item_id}, delta: {delta:?}");
                if self.commentary_message_item_ids.contains(&item_id) {
                    self.seen_commentary_message_deltas = true;
                } else {
                    self.seen_final_message_deltas = true;
                    self.agent_message_text.push_str(&delta);
                }
                client.send_agent_text(delta);
            }
            EventMsg::ReasoningContentDelta(ReasoningContentDeltaEvent {
                thread_id,
                turn_id,
                item_id,
                delta,
                summary_index: index,
                ..
            })
            | EventMsg::ReasoningRawContentDelta(ReasoningRawContentDeltaEvent {
                thread_id,
                turn_id,
                item_id,
                delta,
                content_index: index,
                ..
            }) => {
                info!("Agent reasoning content delta received: thread_id: {thread_id}, turn_id: {turn_id}, item_id: {item_id}, index: {index}, delta: {delta:?}");
                self.seen_reasoning_deltas = true;
                client.send_agent_thought(delta);
            }
            EventMsg::AgentReasoningSectionBreak(AgentReasoningSectionBreakEvent {
                item_id,
                summary_index,
                ..
            }) => {
                info!("Agent reasoning section break received:  item_id: {item_id}, index: {summary_index}");
                // Make sure the section heading actually get spacing
                self.seen_reasoning_deltas = true;
                client.send_agent_thought("\n\n");
            }
            EventMsg::AgentMessage(AgentMessageEvent {
                message,
                phase,
                memory_citation: _,
                ..
            }) => {
                info!("Agent message (non-delta) received: {message:?}");
                let is_commentary = is_commentary_phase(phase.as_ref());
                let saw_delta = if is_commentary {
                    &mut self.seen_commentary_message_deltas
                } else {
                    &mut self.seen_final_message_deltas
                };
                // We didn't receive this message via streaming
                if !std::mem::take(saw_delta) {
                    if !is_commentary && self.agent_message_text.is_empty() {
                        self.agent_message_text.push_str(&message);
                    }
                    client.send_agent_text(message);
                }
            }
            EventMsg::AgentReasoning(AgentReasoningEvent { text, .. }) => {
                info!("Agent reasoning (non-delta) received: {text:?}");
                // We didn't receive this message via streaming
                if !std::mem::take(&mut self.seen_reasoning_deltas) {
                    client.send_agent_thought(text);
                }
            }
            EventMsg::ThreadGoalUpdated(event) => {
                info!("Thread goal updated: {:?}", event.goal.objective);
                client.send_agent_text(format_thread_goal_update(&event));
            }
            EventMsg::PlanUpdate(UpdatePlanArgs { explanation, plan }) => {
                // Send this to the client via session/update notification
                info!("Agent plan updated. Explanation: {:?}", explanation);
                client.update_plan(plan);
            }
            EventMsg::WebSearchBegin(WebSearchBeginEvent { call_id, .. }) => {
                info!("Web search started: call_id={}", call_id);
                // Create a ToolCall notification for the search beginning
                self.start_web_search(client, call_id);
            }
            EventMsg::WebSearchEnd(WebSearchEndEvent {
                call_id,
                query,
                action,
                ..
            }) => {
                info!("Web search query received: call_id={call_id}, query={query}");
                // Send update that the search is in progress with the query
                // (WebSearchEnd just means we have the query, not that results are ready)
                self.update_web_search_query(client, call_id, query, action);
                // The actual search results will come through AgentMessage events
                // We mark as completed when a new tool call begins
            }
            EventMsg::ImageGenerationBegin(event) => {
                info!("Image generation started: call_id={}", event.call_id);
                self.start_image_generation(client, event);
            }
            EventMsg::ImageGenerationEnd(event) => {
                info!(
                    "Image generation ended: call_id={}, status={}",
                    event.call_id, event.status
                );
                self.end_image_generation(client, event);
            }
            EventMsg::ExecApprovalRequest(event) => {
                info!(
                    "Command execution started: call_id={}, command={:?}",
                    event.call_id, event.command
                );
                if let Err(err) = self.exec_approval(client, event)
                    && let Some(response_tx) = self.response_tx.take()
                {
                    drop(response_tx.send(Err(err)));
                }
            }
            EventMsg::ExecCommandBegin(event) => {
                info!(
                    "Command execution started: call_id={}, command={:?}",
                    event.call_id, event.command
                );
                self.exec_command_begin(client, event);
            }
            EventMsg::ExecCommandOutputDelta(delta_event) => {
                self.exec_command_output_delta(client, delta_event);
            }
            EventMsg::ExecCommandEnd(end_event) => {
                info!(
                    "Command execution ended: call_id={}, exit_code={}",
                    end_event.call_id, end_event.exit_code
                );
                self.exec_command_end(client, end_event);
            }
            EventMsg::TerminalInteraction(event) => {
                info!(
                    "Terminal interaction: call_id={}, process_id={}, stdin={}",
                    event.call_id, event.process_id, event.stdin
                );
                self.terminal_interaction(client, event);
            }
            EventMsg::DynamicToolCallRequest(DynamicToolCallRequest {
                call_id,
                turn_id,
                namespace,
                tool,
                arguments,
                started_at_ms: _,
                ..
            }) => {
                info!("Dynamic tool call request: call_id={call_id}, turn_id={turn_id}, namespace={namespace:?}, tool={tool}");
                self.start_dynamic_tool_call(client, call_id, tool, arguments);
            }
            EventMsg::DynamicToolCallResponse(event) => {
                info!(
                    "Dynamic tool call response: call_id={}, turn_id={}, tool={}",
                    event.call_id, event.turn_id, event.tool
                );
                self.end_dynamic_tool_call(client, event);
            }
            EventMsg::McpToolCallBegin(McpToolCallBeginEvent {
                call_id,
                invocation,
                mcp_app_resource_uri: _,
                ..
            }) => {
                info!(
                    "MCP tool call begin: call_id={call_id}, invocation={} {}",
                    invocation.server, invocation.tool
                );
                self.start_mcp_tool_call(client, call_id, invocation);
            }
            EventMsg::McpToolCallEnd(McpToolCallEndEvent {
                call_id,
                invocation,
                duration,
                result,
                mcp_app_resource_uri: _,
                ..
            }) => {
                info!(
                    "MCP tool call ended: call_id={call_id}, invocation={} {}, duration={duration:?}",
                    invocation.server, invocation.tool
                );
                self.end_mcp_tool_call(client, call_id, result);
            }
            EventMsg::ApplyPatchApprovalRequest(event) => {
                info!(
                    "Apply patch approval request: call_id={}, reason={:?}",
                    event.call_id, event.reason
                );
                if let Err(err) = self.patch_approval(client, event)
                    && let Some(response_tx) = self.response_tx.take()
                {
                    drop(response_tx.send(Err(err)));
                }
            }
            EventMsg::PatchApplyBegin(event) => {
                info!(
                    "Patch apply begin: call_id={}, auto_approved={}",
                    event.call_id, event.auto_approved
                );
                self.start_patch_apply(client, event);
            }
            EventMsg::PatchApplyUpdated(event) => {
                info!(
                    "Patch apply updated: call_id={}, change_count={}",
                    event.call_id,
                    event.changes.len()
                );
                self.update_patch_apply(client, event);
            }
            EventMsg::PatchApplyEnd(event) => {
                info!(
                    "Patch apply end: call_id={}, success={}",
                    event.call_id, event.success
                );
                self.end_patch_apply(client, event);
            }
            EventMsg::ItemCompleted(ItemCompletedEvent {
                thread_id,
                turn_id,
                item,
                completed_at_ms: _,
                ..
            }) => {
                info!("Item completed: thread_id={}, turn_id={}, item={:?}", thread_id, turn_id, item);
            }
            EventMsg::TurnComplete(TurnCompleteEvent {
                last_agent_message,
                turn_id,
                completed_at: _,
                duration_ms: _,
                time_to_first_token_ms: _,
                ..
            }) => {
                self.maybe_publish_session_title(
                    client,
                    last_agent_message
                        .as_deref()
                        .or_else(|| non_empty_str(&self.agent_message_text)),
                )
                .await;
                info!(
                    "Task {turn_id} completed successfully after {} events. Last agent message: {last_agent_message:?}",
                    self.event_count
                );
                self.detach_pending_interactions();
                if let Some(response_tx) = self.response_tx.take() {
                    response_tx.send(Ok(StopReason::EndTurn)).ok();
                }
            }
            EventMsg::StreamError(StreamErrorEvent {
                message,
                codex_error_info,
                additional_details,
                ..
            }) => {
                error!(
                    "Handled error during turn: {message} {codex_error_info:?} {additional_details:?}"
                );
            }
            EventMsg::Error(ErrorEvent {
                message,
                codex_error_info,
                ..
            }) => {
                error!("Unhandled error during turn: {message} {codex_error_info:?}");
                self.detach_pending_interactions();
                if let Some(response_tx) = self.response_tx.take() {
                    response_tx
                        .send(Err(Error::internal_error().data(
                            json!({ "message": message, "codex_error_info": codex_error_info }),
                        )))
                        .ok();
                }
            }
            EventMsg::TurnAborted(TurnAbortedEvent {
                reason,
                turn_id,
                completed_at: _,
                duration_ms: _,
                ..
            }) => {
                info!("Turn {turn_id:?} aborted: {reason:?}");
                self.detach_pending_interactions();
                if let Some(response_tx) = self.response_tx.take() {
                    response_tx.send(Ok(StopReason::Cancelled)).ok();
                }
            }
            EventMsg::ShutdownComplete => {
                info!("Agent shutting down");
                self.detach_pending_interactions();
                if let Some(response_tx) = self.response_tx.take() {
                    response_tx.send(Ok(StopReason::Cancelled)).ok();
                }
            }
            EventMsg::ViewImageToolCall(ViewImageToolCallEvent { call_id, path, .. }) => {
                info!("ViewImageToolCallEvent received");
                let display_path = path.display().to_string();
                client.send_notification(
                    SessionUpdate::ToolCall(
                        ToolCall::new(call_id, format!("View Image {display_path}"))
                            .kind(ToolKind::Read).status(ToolCallStatus::Completed)
                            .content(vec![ToolCallContent::Content(Content::new(ContentBlock::ResourceLink(ResourceLink::new(display_path.clone(), display_path.clone())
                        )
                    )
                )]).locations(vec![ToolCallLocation::new(path)])));
            }
            EventMsg::EnteredReviewMode(review_request) => {
                info!("Review begin: request={review_request:?}");
            }
            EventMsg::ExitedReviewMode(event) => {
                info!("Review end: output={event:?}");
                if let Err(err) = self.review_mode_exit(client, event)
                    && let Some(response_tx) = self.response_tx.take()
                {
                    drop(response_tx.send(Err(err)));
                }
            }
            EventMsg::Warning(WarningEvent { message, .. })
            | EventMsg::GuardianWarning(WarningEvent { message, .. }) => {
                warn!("Warning: {message}");
                // Forward warnings to the client as agent messages so users see
                // informational notices (e.g., the post-compact advisory message).
                client.send_agent_text(message);
            }
            EventMsg::McpStartupUpdate(McpStartupUpdateEvent { server, status, .. }) => {
                info!("MCP startup update: server={server}, status={status:?}");
            }
            EventMsg::McpStartupComplete(McpStartupCompleteEvent {
                ready,
                failed,
                cancelled,
                ..
            }) => {
                info!(
                    "MCP startup complete: ready={ready:?}, failed={failed:?}, cancelled={cancelled:?}"
                );
            }
            EventMsg::ElicitationRequest(event) => {
                info!("Elicitation request: server={}, id={:?}", event.server_name, event.id);
                if let Err(err) = self.mcp_elicitation(client, event).await
                    && let Some(response_tx) = self.response_tx.take()
                {
                    drop(response_tx.send(Err(err)));
                }
            }
            EventMsg::ModelReroute(ModelRerouteEvent {
                from_model,
                to_model,
                reason,
                ..
            }) => {
                info!("Model reroute: from={from_model}, to={to_model}, reason={reason:?}");
            }
            EventMsg::ModelVerification(event) => {
                info!("Model verification requested: {event:?}");
            }

            EventMsg::ContextCompacted(..) => {
                info!("Context compacted");
                client.send_context_compacted();
            }
            EventMsg::RequestPermissions(event) => {
                info!("Request permissions: {} {}", event.call_id, event.turn_id);
                if let Err(err) = self.request_permissions(client, event)
                    && let Some(response_tx) = self.response_tx.take()
                {
                    drop(response_tx.send(Err(err)));
                }
            }
            EventMsg::RequestUserInput(event) => {
                info!(
                    "Request user input: {} {}",
                    event.call_id, event.turn_id
                );
                if let Err(err) = self.request_user_input(client, event).await
                    && let Some(response_tx) = self.response_tx.take()
                {
                    drop(response_tx.send(Err(err)));
                }
            }
            EventMsg::GuardianAssessment(event) => {
                info!(
                    "Guardian assessment: id={}, status={:?}, turn_id={}",
                    event.id, event.status, event.turn_id
                );
                self.guardian_assessment(client, event);
            }

            // Ignore these events
            EventMsg::AgentReasoningRawContent(..)
            | EventMsg::ThreadRolledBack(..)
            | EventMsg::HookStarted(..)
            | EventMsg::HookCompleted(..)
            // we already have a way to diff the turn, so ignore
            | EventMsg::TurnDiff(..)
            | EventMsg::ThreadSettingsApplied(..)
            // Old events
            | EventMsg::RawResponseItem(..)
            | EventMsg::SessionConfigured(..)
            // TODO: Subagent UI?
            | EventMsg::CollabAgentSpawnBegin(..)
            | EventMsg::CollabAgentSpawnEnd(..)
            | EventMsg::CollabAgentInteractionBegin(..)
            | EventMsg::CollabAgentInteractionEnd(..)
            | EventMsg::RealtimeConversationStarted(..)
            | EventMsg::RealtimeConversationRealtime(..)
            | EventMsg::RealtimeConversationClosed(..)
            | EventMsg::RealtimeConversationSdp(..)
            | EventMsg::CollabWaitingBegin(..)
            | EventMsg::CollabWaitingEnd(..)
            | EventMsg::CollabResumeBegin(..)
            | EventMsg::CollabResumeEnd(..)
            | EventMsg::CollabCloseBegin(..)
            | EventMsg::CollabCloseEnd(..)
            | EventMsg::PlanDelta(..)=> {}
            e @ (EventMsg::RealtimeConversationListVoicesResponse(..)
            | EventMsg::DeprecationNotice(..)) => {
                warn!("Unexpected event: {:?}", e);
            }
        }
    }
}
