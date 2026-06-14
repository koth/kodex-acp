use super::*;

impl PromptState {
    pub(in crate::thread) async fn mcp_elicitation(
        &mut self,
        client: &SessionClient,
        event: ElicitationRequestEvent,
    ) -> Result<(), Error> {
        let raw_input = serde_json::json!(&event);
        let ElicitationRequestEvent {
            server_name,
            id,
            request,
            turn_id: _,
            ..
        } = event;
        if let Some(supported_request) = build_supported_mcp_elicitation_permission_request(
            &server_name,
            &id,
            &request,
            raw_input,
        ) {
            info!(
                "Routing MCP tool approval elicitation through ACP permission request: server={}, id={:?}",
                server_name, id
            );
            self.spawn_permission_request(
                client,
                supported_request.request_key,
                PendingPermissionRequest::McpElicitation {
                    server_name,
                    request_id: id,
                    option_map: supported_request.option_map,
                },
                supported_request.tool_call,
                supported_request.options,
            );
            return Ok(());
        }

        let request_kind = match &request {
            ElicitationRequest::Form { .. } => "form",
            ElicitationRequest::Url { .. } => "url",
        };

        info!(
            "Auto-declining unsupported MCP elicitation: server={}, id={:?}, kind={request_kind}",
            server_name, id
        );

        self.thread
            .submit(Op::ResolveElicitation {
                server_name,
                request_id: id,
                decision: ElicitationAction::Decline,
                content: None,
                meta: None,
            })
            .await
            .map_err(|e| Error::from(anyhow::anyhow!(e)))?;

        Ok(())
    }

    pub(super) fn review_mode_exit(
        &self,
        client: &SessionClient,
        event: ExitedReviewModeEvent,
    ) -> Result<(), Error> {
        let ExitedReviewModeEvent { review_output } = event;
        let Some(ReviewOutputEvent {
            findings,
            overall_correctness: _,
            overall_explanation,
            overall_confidence_score: _,
        }) = review_output
        else {
            return Ok(());
        };

        let text = if findings.is_empty() {
            let explanation = overall_explanation.trim();
            if explanation.is_empty() {
                "Reviewer failed to output a response"
            } else {
                explanation
            }
            .to_string()
        } else {
            format_review_findings_block(&findings, None)
        };

        client.send_agent_text(&text);
        Ok(())
    }

    pub(super) fn patch_approval(
        &mut self,
        client: &SessionClient,
        event: ApplyPatchApprovalRequestEvent,
    ) -> Result<(), Error> {
        let raw_input = serde_json::json!(&event);
        let ApplyPatchApprovalRequestEvent {
            call_id,
            changes,
            reason,
            // grant_root doesn't seem to be set anywhere on the codex side
            grant_root: _,
            turn_id: _,
            ..
        } = event;
        let (title, locations, content) = extract_tool_call_content_from_changes(changes);
        let request_key = patch_request_key(&call_id);
        let options = vec![
            PermissionOption::new("approved", "Yes", PermissionOptionKind::AllowOnce),
            PermissionOption::new(
                "abort",
                "No, provide feedback",
                PermissionOptionKind::RejectOnce,
            ),
        ];
        self.spawn_permission_request(
            client,
            request_key,
            PendingPermissionRequest::Patch {
                call_id: call_id.clone(),
                option_map: HashMap::from([
                    ("approved".to_string(), ReviewDecision::Approved),
                    ("abort".to_string(), ReviewDecision::Abort),
                ]),
            },
            ToolCallUpdate::new(
                call_id,
                ToolCallUpdateFields::new()
                    .kind(ToolKind::Edit)
                    .status(ToolCallStatus::Pending)
                    .title(title)
                    .locations(locations)
                    .content(content.chain(reason.map(|r| r.into())).collect::<Vec<_>>())
                    .raw_input(raw_input),
            ),
            options,
        );
        Ok(())
    }

    pub(super) fn start_patch_apply(&self, client: &SessionClient, event: PatchApplyBeginEvent) {
        let raw_input = serde_json::json!(&event);
        let PatchApplyBeginEvent {
            call_id,
            auto_approved: _,
            changes,
            turn_id: _,
            ..
        } = event;

        let (title, locations, content) = extract_tool_call_content_from_changes(changes);

        client.send_tool_call(
            ToolCall::new(call_id, title)
                .kind(ToolKind::Edit)
                .status(ToolCallStatus::InProgress)
                .locations(locations)
                .content(content.collect())
                .raw_input(raw_input),
        );
    }

    pub(super) fn update_patch_apply(&self, client: &SessionClient, event: PatchApplyUpdatedEvent) {
        let raw_input = serde_json::json!(&event);
        let PatchApplyUpdatedEvent {
            call_id, changes, ..
        } = event;

        if changes.is_empty() {
            return;
        }

        let (title, locations, content) = extract_tool_call_content_from_changes(changes);

        client.send_tool_call_update(ToolCallUpdate::new(
            call_id,
            ToolCallUpdateFields::new()
                .kind(ToolKind::Edit)
                .status(ToolCallStatus::InProgress)
                .title(title)
                .locations(locations)
                .content(content.collect::<Vec<_>>())
                .raw_input(raw_input),
        ));
    }

    pub(super) fn end_patch_apply(&self, client: &SessionClient, event: PatchApplyEndEvent) {
        let raw_output = serde_json::json!(&event);
        let PatchApplyEndEvent {
            call_id,
            stdout: _,
            stderr: _,
            success,
            changes,
            turn_id: _,
            status,
            ..
        } = event;

        let (title, locations, content) = if !changes.is_empty() {
            let (title, locations, content) = extract_tool_call_content_from_changes(changes);
            (Some(title), Some(locations), Some(content.collect()))
        } else {
            (None, None, None)
        };

        let status = match status {
            PatchApplyStatus::Completed => ToolCallStatus::Completed,
            _ if success => ToolCallStatus::Completed,
            PatchApplyStatus::Failed | PatchApplyStatus::Declined => ToolCallStatus::Failed,
        };

        client.send_tool_call_update(ToolCallUpdate::new(
            call_id,
            ToolCallUpdateFields::new()
                .status(status)
                .raw_output(raw_output)
                .title(title)
                .locations(locations)
                .content(content),
        ));
    }

    pub(super) fn start_dynamic_tool_call(
        &self,
        client: &SessionClient,
        call_id: String,
        tool: String,
        arguments: serde_json::Value,
    ) {
        client.send_tool_call(
            ToolCall::new(call_id, format!("Tool: {tool}"))
                .status(ToolCallStatus::InProgress)
                .raw_input(serde_json::json!(&arguments)),
        );
    }

    pub(super) fn start_mcp_tool_call(
        &self,
        client: &SessionClient,
        call_id: String,
        invocation: McpInvocation,
    ) {
        let title = format!("Tool: {}/{}", invocation.server, invocation.tool);
        client.send_tool_call(
            ToolCall::new(call_id, title)
                .status(ToolCallStatus::InProgress)
                .raw_input(serde_json::json!(&invocation)),
        );
    }

    pub(super) fn end_dynamic_tool_call(
        &self,
        client: &SessionClient,
        event: DynamicToolCallResponseEvent,
    ) {
        let raw_output = serde_json::json!(event);
        let DynamicToolCallResponseEvent {
            call_id,
            turn_id: _,
            tool: _,
            arguments: _,
            completed_at_ms: _,
            namespace: _,
            content_items,
            success,
            error,
            duration: _,
            ..
        } = event;

        client.send_tool_call_update(ToolCallUpdate::new(
            call_id,
            ToolCallUpdateFields::new()
                .status(if success {
                    ToolCallStatus::Completed
                } else {
                    ToolCallStatus::Failed
                })
                .raw_output(raw_output)
                .content(
                    content_items
                        .into_iter()
                        .map(|item| match item {
                            DynamicToolCallOutputContentItem::InputText { text } => {
                                ToolCallContent::Content(Content::new(text))
                            }
                            DynamicToolCallOutputContentItem::InputImage { image_url } => {
                                ToolCallContent::Content(Content::new(ContentBlock::ResourceLink(
                                    ResourceLink::new(image_url.clone(), image_url),
                                )))
                            }
                        })
                        .chain(error.map(|e| ToolCallContent::Content(Content::new(e))))
                        .collect::<Vec<_>>(),
                ),
        ));
    }

    pub(super) fn end_mcp_tool_call(
        &self,
        client: &SessionClient,
        call_id: String,
        result: Result<CallToolResult, String>,
    ) {
        let is_error = match result.as_ref() {
            Ok(result) => result.is_error.unwrap_or_default(),
            Err(_) => true,
        };
        let raw_output = match result.as_ref() {
            Ok(result) => serde_json::json!(result),
            Err(err) => serde_json::json!(err),
        };

        client.send_tool_call_update(ToolCallUpdate::new(
            call_id,
            ToolCallUpdateFields::new()
                .status(if is_error {
                    ToolCallStatus::Failed
                } else {
                    ToolCallStatus::Completed
                })
                .raw_output(raw_output)
                .content(
                    result
                        .ok()
                        .filter(|result| !result.content.is_empty())
                        .map(|result| {
                            result
                                .content
                                .into_iter()
                                .filter_map(|content| {
                                    serde_json::from_value::<ContentBlock>(content).ok()
                                })
                                .map(|content| ToolCallContent::Content(Content::new(content)))
                                .collect()
                        }),
                ),
        ));
    }

    pub(in crate::thread) fn exec_approval(
        &mut self,
        client: &SessionClient,
        event: ExecApprovalRequestEvent,
    ) -> Result<(), Error> {
        let available_decisions = event.effective_available_decisions();
        let raw_input = serde_json::json!(&event);
        let ExecApprovalRequestEvent {
            call_id,
            command: _,
            turn_id,
            cwd,
            reason,
            parsed_cmd,
            proposed_execpolicy_amendment,
            approval_id,
            network_approval_context,
            additional_permissions,
            available_decisions: _,
            proposed_network_policy_amendments,
            ..
        } = event;

        // Create a new tool call for the command execution
        let tool_call_id = ToolCallId::new(call_id.clone());
        let ParseCommandToolCall {
            title,
            terminal_output,
            file_extension,
            locations,
            kind,
        } = parse_command_tool_call(parsed_cmd, &cwd);
        self.active_commands.insert(
            call_id.clone(),
            ActiveCommand {
                terminal_output,
                tool_call_id: tool_call_id.clone(),
                output: String::new(),
                file_extension,
            },
        );

        let mut content = vec![];

        if let Some(reason) = reason {
            content.push(reason);
        }
        if let Some(amendment) = proposed_execpolicy_amendment.as_ref() {
            content.push(format!(
                "Proposed Amendment: {}",
                amendment.command().join("\n")
            ));
        }
        if let Some(policy) = network_approval_context.as_ref() {
            let NetworkApprovalContext { host, protocol } = policy;
            content.push(format!("Network Approval Context: {:?} {}", protocol, host));
        }
        if let Some(permissions) = additional_permissions.as_ref() {
            content.push(format!(
                "Additional Permissions: {}",
                serde_json::to_string_pretty(&permissions)?
            ));
        }
        content.push(format!(
            "Available Decisions: {}",
            available_decisions.iter().map(|d| d.to_string()).join("\n")
        ));
        if let Some(amendments) = proposed_network_policy_amendments.as_ref() {
            content.push(format!(
                "Proposed Network Policy Amendments: {}",
                amendments
                    .iter()
                    .map(|amendment| format!("{:?} {:?}", amendment.action, amendment.host))
                    .join("\n")
            ));
        }

        let content = if content.is_empty() {
            None
        } else {
            Some(vec![content.join("\n").into()])
        };
        let permission_options = build_exec_permission_options(
            &available_decisions,
            network_approval_context.as_ref(),
            additional_permissions.as_ref(),
        );

        self.spawn_permission_request(
            client,
            exec_request_key(&call_id),
            PendingPermissionRequest::Exec {
                approval_id: approval_id.unwrap_or(call_id.clone()),
                turn_id,
                option_map: permission_options
                    .iter()
                    .map(|option| (option.option_id.to_string(), option.decision.clone()))
                    .collect(),
            },
            ToolCallUpdate::new(
                tool_call_id,
                ToolCallUpdateFields::new()
                    .kind(kind)
                    .status(ToolCallStatus::Pending)
                    .title(title)
                    .raw_input(raw_input)
                    .content(content)
                    .locations(if locations.is_empty() {
                        None
                    } else {
                        Some(locations)
                    }),
            ),
            permission_options
                .into_iter()
                .map(|option| option.permission_option)
                .collect(),
        );

        Ok(())
    }

    pub(super) fn exec_command_begin(
        &mut self,
        client: &SessionClient,
        event: ExecCommandBeginEvent,
    ) {
        let raw_input = serde_json::json!(&event);
        let ExecCommandBeginEvent {
            turn_id: _,
            source: _,
            interaction_input: _,
            call_id,
            command: _,
            started_at_ms: _,
            cwd,
            parsed_cmd,
            process_id: _,
            ..
        } = event;
        // Create a new tool call for the command execution
        let tool_call_id = ToolCallId::new(call_id.clone());
        let ParseCommandToolCall {
            title,
            file_extension,
            locations,
            terminal_output,
            kind,
        } = parse_command_tool_call(parsed_cmd, &cwd);

        let active_command = ActiveCommand {
            tool_call_id: tool_call_id.clone(),
            output: String::new(),
            file_extension,
            terminal_output,
        };
        let (content, meta) = if client.supports_terminal_output(&active_command) {
            let content = vec![ToolCallContent::Terminal(Terminal::new(call_id.clone()))];
            let meta = Some(Meta::from_iter([(
                "terminal_info".to_owned(),
                serde_json::json!({
                    "terminal_id": call_id,
                    "cwd": cwd
                }),
            )]));
            (content, meta)
        } else {
            (vec![], None)
        };

        self.active_commands.insert(call_id.clone(), active_command);

        client.send_tool_call(
            ToolCall::new(tool_call_id, title)
                .kind(kind)
                .status(ToolCallStatus::InProgress)
                .locations(locations)
                .raw_input(raw_input)
                .content(content)
                .meta(meta),
        );
    }

    pub(super) fn exec_command_output_delta(
        &mut self,
        client: &SessionClient,
        event: ExecCommandOutputDeltaEvent,
    ) {
        let ExecCommandOutputDeltaEvent {
            call_id,
            chunk,
            stream: _,
            ..
        } = event;
        // Stream output bytes to the display-only terminal via ToolCallUpdate meta.
        if let Some(active_command) = self.active_commands.get_mut(&call_id) {
            let data_str = String::from_utf8_lossy(&chunk).to_string();

            if client.supports_terminal_output(active_command) {
                let update = ToolCallUpdate::new(
                    active_command.tool_call_id.clone(),
                    ToolCallUpdateFields::new(),
                )
                .meta(Meta::from_iter([(
                    "terminal_output".to_owned(),
                    serde_json::json!({
                        "terminal_id": call_id,
                        "data": data_str
                    }),
                )]));
                client.send_tool_call_update(update);
            } else {
                // Fallback path (no terminal_output capability): accumulate locally
                // and emit a single ToolCallUpdate at exec_command_end. Resending the
                // entire accumulated buffer per chunk is O(N²) memory and crashes the
                // process on large outputs (issue #225).
                active_command.output.push_str(&data_str);
            }
        }
    }

    pub(super) fn exec_command_end(&mut self, client: &SessionClient, event: ExecCommandEndEvent) {
        let raw_output = serde_json::json!(&event);
        let ExecCommandEndEvent {
            turn_id: _,
            command: _,
            cwd: _,
            parsed_cmd: _,
            source: _,
            interaction_input: _,
            call_id,
            exit_code,
            stdout: _,
            stderr: _,
            aggregated_output: _,
            duration: _,
            formatted_output: _,
            process_id: _,
            completed_at_ms: _,
            status,
            ..
        } = event;
        if let Some(active_command) = self.active_commands.remove(&call_id) {
            let is_success = exit_code == 0;

            let status = match status {
                ExecCommandStatus::Completed => ToolCallStatus::Completed,
                _ if is_success => ToolCallStatus::Completed,
                ExecCommandStatus::Failed | ExecCommandStatus::Declined => ToolCallStatus::Failed,
            };

            let supports_terminal = client.supports_terminal_output(&active_command);

            let mut fields = ToolCallUpdateFields::new()
                .status(status)
                .raw_output(raw_output);

            // For the non-terminal fallback path the per-chunk delta handler now
            // accumulates silently (see exec_command_output_delta). Emit the full
            // buffer here, exactly once, as a single content block. Skip the emission
            // entirely when the command produced no output, so we don't surface an
            // empty fenced code block to the client.
            if !supports_terminal && !active_command.output.is_empty() {
                let content = match active_command.file_extension.as_deref() {
                    Some("md") => active_command.output.clone(),
                    Some(ext) => format!(
                        "```{ext}\n{}\n```\n",
                        active_command.output.trim_end_matches('\n')
                    ),
                    None => format!(
                        "```sh\n{}\n```\n",
                        active_command.output.trim_end_matches('\n')
                    ),
                };
                fields = fields.content(vec![content.into()]);
            }

            client.send_tool_call_update(
                ToolCallUpdate::new(active_command.tool_call_id.clone(), fields).meta(
                    supports_terminal.then(|| {
                        Meta::from_iter([(
                            "terminal_exit".into(),
                            serde_json::json!({
                                "terminal_id": call_id,
                                "exit_code": exit_code,
                                "signal": null
                            }),
                        )])
                    }),
                ),
            );
        }
    }

    pub(super) fn terminal_interaction(
        &mut self,
        client: &SessionClient,
        event: TerminalInteractionEvent,
    ) {
        let TerminalInteractionEvent {
            call_id,
            process_id: _,
            stdin,
            ..
        } = event;

        let stdin = format!("\n{stdin}\n");
        // Stream output bytes to the display-only terminal via ToolCallUpdate meta.
        if let Some(active_command) = self.active_commands.get_mut(&call_id) {
            if client.supports_terminal_output(active_command) {
                let update = ToolCallUpdate::new(
                    active_command.tool_call_id.clone(),
                    ToolCallUpdateFields::new(),
                )
                .meta(Meta::from_iter([(
                    "terminal_output".to_owned(),
                    serde_json::json!({
                        "terminal_id": call_id,
                        "data": stdin
                    }),
                )]));
                client.send_tool_call_update(update);
            } else {
                // Fallback path: accumulate stdin into the active command buffer and
                // defer emission to exec_command_end. Emitting per stdin event would
                // re-send the entire output+stdin buffer each time and reintroduce the
                // O(N²) growth fixed in the delta path.
                active_command.output.push_str(&stdin);
            }
        }
    }

    pub(super) fn start_web_search(&mut self, client: &SessionClient, call_id: String) {
        self.active_web_search = Some(call_id.clone());
        client.send_tool_call(ToolCall::new(call_id, "Searching the Web").kind(ToolKind::Fetch));
    }

    pub(super) fn start_image_generation(
        &mut self,
        client: &SessionClient,
        event: ImageGenerationBeginEvent,
    ) {
        let raw_input = serde_json::json!(&event);
        let ImageGenerationBeginEvent { call_id, .. } = event;
        self.active_image_generations.insert(call_id.clone());
        client.send_tool_call(
            ToolCall::new(call_id, "Image generation")
                .kind(ToolKind::Other)
                .status(ToolCallStatus::InProgress)
                .raw_input(raw_input),
        );
    }

    pub(super) fn end_image_generation(
        &mut self,
        client: &SessionClient,
        event: ImageGenerationEndEvent,
    ) {
        let raw_output = serde_json::json!(&event);
        let ImageGenerationEndEvent {
            call_id,
            status,
            revised_prompt,
            result,
            saved_path,
            ..
        } = event;
        let tool_status = image_generation_tool_status(&status);
        let saved_path = saved_path.map(|path| path.to_string_lossy().into_owned());
        let content = image_generation_content(revised_prompt, result, saved_path);

        if self.active_image_generations.remove(&call_id) {
            client.send_tool_call_update(ToolCallUpdate::new(
                call_id,
                ToolCallUpdateFields::new()
                    .status(tool_status)
                    .content(content)
                    .raw_output(raw_output),
            ));
        } else {
            client.send_tool_call(
                ToolCall::new(call_id, "Image generation")
                    .kind(ToolKind::Other)
                    .status(tool_status)
                    .content(content)
                    .raw_output(raw_output),
            );
        }
    }

    pub(super) fn update_web_search_query(
        &self,
        client: &SessionClient,
        call_id: String,
        query: String,
        action: WebSearchAction,
    ) {
        let title = match &action {
            WebSearchAction::Search { query, queries } => queries
                .as_ref()
                .map(|q| format!("Searching for: {}", q.join(", ")))
                .or_else(|| query.as_ref().map(|q| format!("Searching for: {q}")))
                .unwrap_or_else(|| "Web search".to_string()),
            WebSearchAction::OpenPage { url } => url
                .as_ref()
                .map(|u| format!("Opening: {u}"))
                .unwrap_or_else(|| "Open page".to_string()),
            WebSearchAction::FindInPage { pattern, url } => match (pattern, url) {
                (Some(p), Some(u)) => format!("Finding: {p} in {u}"),
                (Some(p), None) => format!("Finding: {p}"),
                (None, Some(u)) => format!("Find in page: {u}"),
                (None, None) => "Find in page".to_string(),
            },
            WebSearchAction::Other => "Web search".to_string(),
        };

        client.send_tool_call_update(ToolCallUpdate::new(
            call_id,
            ToolCallUpdateFields::new()
                .status(ToolCallStatus::InProgress)
                .title(title)
                .raw_input(serde_json::json!({
                    "query": query,
                    "action": action
                })),
        ));
    }

    pub(super) fn complete_web_search(&mut self, client: &SessionClient) {
        if let Some(call_id) = self.active_web_search.take() {
            client.send_tool_call_update(ToolCallUpdate::new(
                call_id,
                ToolCallUpdateFields::new().status(ToolCallStatus::Completed),
            ));
        }
    }

    pub(super) fn request_permissions(
        &mut self,
        client: &SessionClient,
        event: RequestPermissionsEvent,
    ) -> Result<(), Error> {
        let raw_input = serde_json::json!(&event);
        let RequestPermissionsEvent {
            call_id,
            turn_id: _,
            reason,
            permissions,
            cwd: _,
            ..
        } = event;

        // Create a new tool call for the command execution
        let tool_call_id = ToolCallId::new(call_id.clone());

        let mut content = vec![];

        if let Some(reason) = reason.as_ref() {
            content.push(reason.clone());
        }
        if let Some(file_system) = permissions.file_system.as_ref() {
            let reads = format_file_system_entries(
                file_system
                    .entries
                    .iter()
                    .filter(|entry| entry.access == FileSystemAccessMode::Read),
            );
            if !reads.is_empty() {
                content.push(format!("File System Read Access: {reads}"));
            }
            let writes = format_file_system_entries(
                file_system
                    .entries
                    .iter()
                    .filter(|entry| entry.access == FileSystemAccessMode::Write),
            );
            if !writes.is_empty() {
                content.push(format!("File System Write Access: {writes}"));
            }
            let denies = format_file_system_entries(
                file_system
                    .entries
                    .iter()
                    .filter(|entry| entry.access == FileSystemAccessMode::Deny),
            );
            if !denies.is_empty() {
                content.push(format!("File System Denied Access: {denies}"));
            }
        }
        if let Some(network) = permissions.network.as_ref()
            && let Some(enabled) = network.enabled
        {
            content.push(format!("Network Access: {enabled}"));
        }

        let content = if content.is_empty() {
            None
        } else {
            Some(vec![content.join("\n").into()])
        };

        self.spawn_permission_request(
            client,
            permissions_request_key(&call_id),
            PendingPermissionRequest::RequestPermissions {
                call_id,
                permissions,
            },
            ToolCallUpdate::new(
                tool_call_id,
                ToolCallUpdateFields::new()
                    .status(ToolCallStatus::Pending)
                    .title(reason.unwrap_or_else(|| "Permissions Request".to_string()))
                    .raw_input(raw_input)
                    .content(content),
            ),
            vec![
                PermissionOption::new(
                    "approved-for-session",
                    "Yes, for session",
                    PermissionOptionKind::AllowAlways,
                ),
                PermissionOption::new("approved", "Yes", PermissionOptionKind::AllowOnce),
                PermissionOption::new("abort", "No", PermissionOptionKind::RejectOnce),
            ],
        );

        Ok(())
    }

    pub(super) async fn request_user_input(
        &mut self,
        client: &SessionClient,
        event: RequestUserInputEvent,
    ) -> Result<(), Error> {
        let raw_input = serde_json::json!(&event);
        let RequestUserInputEvent {
            call_id,
            turn_id,
            questions,
        } = event;
        let answer_id = if turn_id.trim().is_empty() {
            self.submission_id.clone()
        } else {
            turn_id
        };
        let (title, content, options, option_map) = build_user_input_permission_request(&questions);
        let content = if content.is_empty() {
            None
        } else {
            Some(vec![content.into()])
        };

        if option_map.is_empty() {
            self.thread
                .submit(Op::UserInputAnswer {
                    id: answer_id,
                    response: empty_user_input_response(),
                })
                .await
                .map_err(|e| Error::from(anyhow::anyhow!(e)))?;
            return Ok(());
        }

        self.spawn_permission_request(
            client,
            user_input_request_key(&answer_id, &call_id),
            PendingPermissionRequest::UserInput {
                id: answer_id,
                option_map,
            },
            ToolCallUpdate::new(
                call_id,
                ToolCallUpdateFields::new()
                    .kind(ToolKind::Think)
                    .status(ToolCallStatus::Pending)
                    .title(title)
                    .raw_input(raw_input)
                    .content(content),
            ),
            options,
        );

        Ok(())
    }

    pub(super) fn guardian_assessment(
        &mut self,
        client: &SessionClient,
        event: GuardianAssessmentEvent,
    ) {
        let call_id = guardian_assessment_tool_call_id(&event.id);
        let status = guardian_assessment_tool_call_status(&event.status);
        let content = guardian_assessment_content(&event);
        let raw_event = serde_json::json!(&event);

        match event.status {
            GuardianAssessmentStatus::InProgress => {
                if self.active_guardian_assessments.insert(event.id.clone()) {
                    client.send_tool_call(
                        ToolCall::new(call_id, "Guardian Review")
                            .kind(ToolKind::Think)
                            .status(status)
                            .content(content)
                            .raw_input(raw_event),
                    );
                } else {
                    client.send_tool_call_update(ToolCallUpdate::new(
                        call_id,
                        ToolCallUpdateFields::new()
                            .status(status)
                            .content(content)
                            .raw_output(raw_event),
                    ));
                }
            }
            GuardianAssessmentStatus::TimedOut
            | GuardianAssessmentStatus::Approved
            | GuardianAssessmentStatus::Denied
            | GuardianAssessmentStatus::Aborted => {
                if self.active_guardian_assessments.remove(&event.id) {
                    client.send_tool_call_update(ToolCallUpdate::new(
                        call_id,
                        ToolCallUpdateFields::new()
                            .status(status)
                            .content(content)
                            .raw_output(raw_event),
                    ));
                } else {
                    client.send_tool_call(
                        ToolCall::new(call_id, "Guardian Review")
                            .kind(ToolKind::Think)
                            .status(status)
                            .content(content)
                            .raw_input(raw_event),
                    );
                }
            }
        }
    }
}
