use std::{
    collections::{HashMap, HashSet},
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{Arc, LazyLock, Mutex},
    time::Duration,
};

use agent_client_protocol::{
    Client, ConnectionTo, Error,
    schema::{
        AvailableCommand, AvailableCommandInput, AvailableCommandsUpdate, ClientCapabilities,
        ConfigOptionUpdate, Content, ContentBlock, ContentChunk, Diff, EmbeddedResource,
        EmbeddedResourceResource, ImageContent, LoadSessionResponse, Meta, ModelId, ModelInfo,
        PermissionOption, PermissionOptionKind, Plan, PlanEntry, PlanEntryPriority,
        PlanEntryStatus, PromptRequest, RequestPermissionOutcome, RequestPermissionRequest,
        RequestPermissionResponse, ResourceLink, SelectedPermissionOutcome, SessionConfigId,
        SessionConfigOption, SessionConfigOptionCategory, SessionConfigOptionValue,
        SessionConfigSelectOption, SessionConfigValueId, SessionId, SessionInfoUpdate, SessionMode,
        SessionModeId, SessionModeState, SessionModelState, SessionNotification, SessionUpdate,
        StopReason, Terminal, TextContent, TextResourceContents, ToolCall, ToolCallContent,
        ToolCallId, ToolCallLocation, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
        ToolKind, UnstructuredCommandInput, UsageUpdate,
    },
};
use codex_apply_patch::parse_patch;
use codex_core::{
    CodexThread, ModelClient, Prompt, ResponseEvent,
    config::{Config, set_project_trust_level},
    resolve_installation_id,
    review_format::format_review_findings_block,
    review_prompts::user_facing_hint,
};
use codex_login::auth::AuthManager;
use codex_models_manager::manager::{ModelsManager, RefreshStrategy};
use codex_otel::SessionTelemetry;
use codex_protocol::{
    SessionId as CodexSessionId, ThreadId as CodexThreadId,
    approvals::{
        ElicitationRequest, ElicitationRequestEvent, GuardianAssessmentAction,
        GuardianCommandSource,
    },
    config_types::{CollaborationMode, ModeKind, ReasoningSummary, Settings, TrustLevel},
    dynamic_tools::{DynamicToolCallOutputContentItem, DynamicToolCallRequest},
    error::CodexErr,
    items::TurnItem,
    mcp::CallToolResult,
    models::{
        ActivePermissionProfile, AdditionalPermissionProfile, ContentItem, MessagePhase,
        PermissionProfile, ResponseItem, WebSearchAction,
    },
    openai_models::{ModelInfo as CodexModelInfo, ModelPreset, ReasoningEffort},
    parse_command::ParsedCommand,
    permissions::{
        FileSystemAccessMode, FileSystemPath, FileSystemSandboxEntry, FileSystemSpecialPath,
        NetworkSandboxPolicy,
    },
    plan_tool::{PlanItemArg, StepStatus, UpdatePlanArgs},
    protocol::{
        AgentMessageContentDeltaEvent, AgentMessageEvent, AgentReasoningEvent,
        AgentReasoningRawContentEvent, AgentReasoningSectionBreakEvent,
        ApplyPatchApprovalRequestEvent, DynamicToolCallResponseEvent, ElicitationAction,
        ErrorEvent, Event, EventMsg, ExecApprovalRequestEvent, ExecCommandBeginEvent,
        ExecCommandEndEvent, ExecCommandOutputDeltaEvent, ExecCommandStatus, ExitedReviewModeEvent,
        FileChange, GuardianAssessmentEvent, GuardianAssessmentStatus, ImageGenerationBeginEvent,
        ImageGenerationEndEvent, ItemCompletedEvent, ItemStartedEvent, McpInvocation,
        McpServerRefreshConfig, McpStartupCompleteEvent, McpStartupUpdateEvent,
        McpToolCallBeginEvent, McpToolCallEndEvent, ModelRerouteEvent, NetworkApprovalContext,
        NetworkPolicyRuleAction, Op, PatchApplyBeginEvent, PatchApplyEndEvent, PatchApplyStatus,
        PatchApplyUpdatedEvent, ReasoningContentDeltaEvent, ReasoningRawContentDeltaEvent,
        ReviewDecision, ReviewOutputEvent, ReviewRequest, ReviewTarget, RolloutItem, SessionSource,
        StreamErrorEvent, TerminalInteractionEvent, ThreadGoalStatus, ThreadGoalUpdatedEvent,
        ThreadSettingsOverrides, TokenCountEvent, TokenUsage, TurnAbortedEvent,
        TurnCompleteEvent, TurnStartedEvent, UserMessageEvent, ViewImageToolCallEvent,
        WarningEvent, WebSearchBeginEvent, WebSearchEndEvent,
    },
    request_permissions::{
        PermissionGrantScope, RequestPermissionProfile, RequestPermissionsEvent,
        RequestPermissionsResponse,
    },
    request_user_input::{
        RequestUserInputAnswer, RequestUserInputEvent, RequestUserInputQuestion,
        RequestUserInputResponse,
    },
    user_input::UserInput,
};
use codex_rollout_trace::InferenceTraceContext;
use codex_shell_command::parse_command::parse_command;
use codex_thread_store::ThreadMetadataPatch;
use codex_utils_approval_presets::{ApprovalPreset, builtin_approval_presets};
use futures::StreamExt;
use heck::ToTitleCase;
use itertools::Itertools;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};
use tracing::{error, info, warn};
use uuid::Uuid;

mod actor;
mod event_mapping;
mod permissions;
mod prompt_state;
mod session_config;
mod title;

use actor::ThreadActor;
use event_mapping::{
    build_prompt_items, extract_slash_command, extract_tool_call_content_from_changes,
    format_file_system_entries, generate_fallback_id, guardian_assessment_content,
    guardian_assessment_tool_call_id, guardian_assessment_tool_call_status,
    image_generation_content, image_generation_tool_status, is_commentary_phase,
    web_search_action_to_title_and_id,
};
use permissions::{
    ParseCommandToolCall, PendingPermissionInteraction, PendingPermissionRequest,
    ResolvedMcpElicitation, build_exec_permission_options,
    build_supported_mcp_elicitation_permission_request, build_user_input_permission_request,
    empty_user_input_response, exec_request_key, format_thread_goal_update,
    parse_command_tool_call, patch_request_key, permission_guidance_followup,
    permission_guidance_from_response, permissions_request_key, user_input_permission_meta,
    user_input_request_key, user_input_response_from_answer,
    user_input_response_from_permission_response,
};
use prompt_state::{ActiveCommand, PromptState, SubmissionState};
use session_config::{
    APPROVAL_PRESETS, INIT_COMMAND_PROMPT, KODEX_CONTEXT_COMPACTED_META_KEY,
    KODEX_CONTEXT_COMPACTION_META_KEY, KODEX_MODEL_PROVIDER_MAP_ENV,
    KODEX_PERMISSION_GUIDANCE_META_KEY, KODEX_PERMISSION_INPUT_META_KEY,
    KODEX_PROVIDER_VALUE_PREFIX, KODEX_USER_INPUT_ANSWERS_META_KEY, KodexModelProviderEntry,
    SESSION_TITLE_GENERATION_TIMEOUT, SESSION_TITLE_INSTRUCTIONS, SESSION_TITLE_MAX_CHARS,
    SESSION_TITLE_PROMPT_MAX_CHARS, SESSION_TITLE_ROLLBACK_TIMEOUT,
    active_profile_id_for_session_mode, current_session_mode_id, mode_trusts_project,
};
pub use title::generate_session_title_with_model;
use title::{
    CodexThreadImpl, ModelSessionTitleGenerator, ModelsManagerImpl, SessionTitleGenerator,
    build_session_title_prompt, non_empty_str, normalize_session_title, prompt_text_from_items,
    publish_session_title, truncate_chars,
};

/// Abstraction over the ACP connection for sending notifications and requests
/// back to the client. This replaces the old `Client` trait usage.
trait ClientSender: Send + Sync + 'static {
    fn send_session_notification(&self, notif: SessionNotification) -> Result<(), Error>;
    fn request_permission(
        &self,
        req: RequestPermissionRequest,
    ) -> Pin<Box<dyn Future<Output = Result<RequestPermissionResponse, Error>> + Send + '_>>;
}

/// Production implementation that wraps a `ConnectionTo<Client>`.
struct AcpConnection(ConnectionTo<Client>);

impl ClientSender for AcpConnection {
    fn send_session_notification(&self, notif: SessionNotification) -> Result<(), Error> {
        self.0.send_notification(notif)
    }

    fn request_permission(
        &self,
        req: RequestPermissionRequest,
    ) -> Pin<Box<dyn Future<Output = Result<RequestPermissionResponse, Error>> + Send + '_>> {
        Box::pin(async move { self.0.send_request(req).block_task().await })
    }
}

pub trait Auth {
    fn logout(&self) -> impl Future<Output = Result<bool, Error>> + Send;
}

impl Auth for Arc<AuthManager> {
    async fn logout(&self) -> Result<bool, Error> {
        self.as_ref()
            .logout()
            .await
            .map_err(|e| Error::internal_error().data(e.to_string()))
    }
}

enum ThreadMessage {
    Load {
        response_tx: oneshot::Sender<Result<LoadSessionResponse, Error>>,
    },
    GetConfigOptions {
        response_tx: oneshot::Sender<Result<Vec<SessionConfigOption>, Error>>,
    },
    Prompt {
        request: PromptRequest,
        response_tx: oneshot::Sender<Result<oneshot::Receiver<Result<StopReason, Error>>, Error>>,
    },
    SetMode {
        mode: SessionModeId,
        response_tx: oneshot::Sender<Result<(), Error>>,
    },
    SetModel {
        model: ModelId,
        response_tx: oneshot::Sender<Result<(), Error>>,
    },
    SetConfigOption {
        config_id: SessionConfigId,
        value: SessionConfigOptionValue,
        response_tx: oneshot::Sender<Result<(), Error>>,
    },
    Cancel {
        response_tx: oneshot::Sender<Result<(), Error>>,
    },
    StopTool {
        tool_call_id: String,
        response_tx: oneshot::Sender<Result<bool, Error>>,
    },
    Shutdown {
        response_tx: oneshot::Sender<Result<(), Error>>,
    },
    ReplayHistory {
        history: Vec<RolloutItem>,
        response_tx: oneshot::Sender<Result<(), Error>>,
    },
    PermissionRequestResolved {
        submission_id: String,
        interaction_id: u64,
        request_key: String,
        response: Result<RequestPermissionResponse, Error>,
    },
}

pub struct Thread {
    /// Direct handle to the underlying Codex thread for out-of-band shutdown.
    thread: Arc<dyn CodexThreadImpl>,
    /// A sender for interacting with the thread.
    message_tx: mpsc::UnboundedSender<ThreadMessage>,
    /// Keep the actor task alive for the lifetime of the thread wrapper.
    _handle: tokio::task::JoinHandle<()>,
}

impl Thread {
    pub fn new(
        session_id: SessionId,
        thread: Arc<dyn CodexThreadImpl>,
        auth: Arc<AuthManager>,
        models_manager: Arc<dyn ModelsManagerImpl>,
        client_capabilities: Arc<Mutex<ClientCapabilities>>,
        config: Config,
        cx: ConnectionTo<Client>,
    ) -> Self {
        let (message_tx, message_rx) = mpsc::unbounded_channel();
        let (resolution_tx, resolution_rx) = mpsc::unbounded_channel();
        let title_generator = Arc::new(ModelSessionTitleGenerator::new(
            auth.clone(),
            models_manager.clone(),
            config.clone(),
        ));

        let actor = ThreadActor::new(
            auth,
            SessionClient::new(session_id, cx, client_capabilities),
            thread.clone(),
            models_manager,
            config,
            Some(title_generator),
            message_rx,
            resolution_tx,
            resolution_rx,
        );
        let handle = tokio::spawn(actor.spawn());

        Self {
            thread,
            message_tx,
            _handle: handle,
        }
    }

    pub async fn load(&self) -> Result<LoadSessionResponse, Error> {
        let (response_tx, response_rx) = oneshot::channel();

        let message = ThreadMessage::Load { response_tx };
        drop(self.message_tx.send(message));

        response_rx
            .await
            .map_err(|e| Error::internal_error().data(e.to_string()))?
    }

    pub async fn config_options(&self) -> Result<Vec<SessionConfigOption>, Error> {
        let (response_tx, response_rx) = oneshot::channel();

        let message = ThreadMessage::GetConfigOptions { response_tx };
        drop(self.message_tx.send(message));

        response_rx
            .await
            .map_err(|e| Error::internal_error().data(e.to_string()))?
    }

    pub async fn prompt(&self, request: PromptRequest) -> Result<StopReason, Error> {
        let (response_tx, response_rx) = oneshot::channel();

        let message = ThreadMessage::Prompt {
            request,
            response_tx,
        };
        drop(self.message_tx.send(message));

        response_rx
            .await
            .map_err(|e| Error::internal_error().data(e.to_string()))??
            .await
            .map_err(|e| Error::internal_error().data(e.to_string()))?
    }

    pub async fn set_mode(&self, mode: SessionModeId) -> Result<(), Error> {
        let (response_tx, response_rx) = oneshot::channel();

        let message = ThreadMessage::SetMode { mode, response_tx };
        drop(self.message_tx.send(message));

        response_rx
            .await
            .map_err(|e| Error::internal_error().data(e.to_string()))?
    }

    pub async fn set_model(&self, model: ModelId) -> Result<(), Error> {
        let (response_tx, response_rx) = oneshot::channel();

        let message = ThreadMessage::SetModel { model, response_tx };
        drop(self.message_tx.send(message));

        response_rx
            .await
            .map_err(|e| Error::internal_error().data(e.to_string()))?
    }

    pub async fn set_config_option(
        &self,
        config_id: SessionConfigId,
        value: SessionConfigOptionValue,
    ) -> Result<(), Error> {
        let (response_tx, response_rx) = oneshot::channel();

        let message = ThreadMessage::SetConfigOption {
            config_id,
            value,
            response_tx,
        };
        drop(self.message_tx.send(message));

        response_rx
            .await
            .map_err(|e| Error::internal_error().data(e.to_string()))?
    }

    pub async fn cancel(&self) -> Result<(), Error> {
        let (response_tx, response_rx) = oneshot::channel();

        let message = ThreadMessage::Cancel { response_tx };
        drop(self.message_tx.send(message));

        response_rx
            .await
            .map_err(|e| Error::internal_error().data(e.to_string()))?
    }

    pub async fn stop_tool(&self, tool_call_id: String) -> Result<bool, Error> {
        let (response_tx, response_rx) = oneshot::channel();

        let message = ThreadMessage::StopTool {
            tool_call_id,
            response_tx,
        };
        drop(self.message_tx.send(message));

        response_rx
            .await
            .map_err(|e| Error::internal_error().data(e.to_string()))?
    }

    pub async fn replay_history(&self, history: Vec<RolloutItem>) -> Result<(), Error> {
        let (response_tx, response_rx) = oneshot::channel();

        let message = ThreadMessage::ReplayHistory {
            history,
            response_tx,
        };
        drop(self.message_tx.send(message));

        response_rx
            .await
            .map_err(|e| Error::internal_error().data(e.to_string()))?
    }

    pub async fn refresh_mcp_servers(&self, config: McpServerRefreshConfig) -> Result<(), Error> {
        self.thread
            .submit(Op::RefreshMcpServers { config })
            .await
            .map(|_| ())
            .map_err(|e| Error::internal_error().data(e.to_string()))
    }

    pub async fn shutdown(&self) -> Result<(), Error> {
        let (response_tx, response_rx) = oneshot::channel();
        let message = ThreadMessage::Shutdown { response_tx };

        if self.message_tx.send(message).is_err() {
            self.thread
                .submit(Op::Shutdown)
                .await
                .map_err(|e| Error::from(anyhow::anyhow!(e)))?;
        } else {
            response_rx
                .await
                .map_err(|e| Error::internal_error().data(e.to_string()))??;
        }
        // Let the actor drain the resulting turn-aborted/shutdown events so any in-flight
        // prompt callers observe a clean cancellation instead of a dropped response channel.
        Ok(())
    }
}

#[derive(Clone)]
struct SessionClient {
    session_id: SessionId,
    client: Arc<dyn ClientSender>,
    client_capabilities: Arc<Mutex<ClientCapabilities>>,
}

impl SessionClient {
    fn new(
        session_id: SessionId,
        cx: ConnectionTo<Client>,
        client_capabilities: Arc<Mutex<ClientCapabilities>>,
    ) -> Self {
        Self {
            session_id,
            client: Arc::new(AcpConnection(cx)),
            client_capabilities,
        }
    }

    #[cfg(test)]
    fn with_client(
        session_id: SessionId,
        client: Arc<dyn ClientSender>,
        client_capabilities: Arc<Mutex<ClientCapabilities>>,
    ) -> Self {
        Self {
            session_id,
            client,
            client_capabilities,
        }
    }

    fn supports_terminal_output(&self, active_command: &ActiveCommand) -> bool {
        active_command.terminal_output
            && self
                .client_capabilities
                .lock()
                .unwrap()
                .meta
                .as_ref()
                .is_some_and(|v| {
                    v.get("terminal_output")
                        .is_some_and(|v| v.as_bool().unwrap_or_default())
                })
    }

    fn send_notification(&self, update: SessionUpdate) {
        if let Err(e) = self
            .client
            .send_session_notification(SessionNotification::new(self.session_id.clone(), update))
        {
            error!("Failed to send session notification: {:?}", e);
        }
    }

    fn send_user_message(&self, text: impl Into<String>) {
        self.send_notification(SessionUpdate::UserMessageChunk(ContentChunk::new(
            text.into().into(),
        )));
    }

    fn send_agent_text(&self, text: impl Into<String>) {
        self.send_notification(SessionUpdate::AgentMessageChunk(ContentChunk::new(
            text.into().into(),
        )));
    }

    fn send_agent_thought(&self, text: impl Into<String>) {
        self.send_notification(SessionUpdate::AgentThoughtChunk(ContentChunk::new(
            text.into().into(),
        )));
    }

    fn send_session_title(&self, title: impl Into<String>) {
        self.send_notification(SessionUpdate::SessionInfoUpdate(
            SessionInfoUpdate::new().title(title.into()),
        ));
    }

    fn send_context_compacted(&self) {
        let notification = SessionNotification::new(
            self.session_id.clone(),
            SessionUpdate::SessionInfoUpdate(SessionInfoUpdate::new()),
        )
        .meta(Meta::from_iter([
            (
                KODEX_CONTEXT_COMPACTION_META_KEY.to_owned(),
                json!({
                    "phase": "completed",
                    "message": "上下文已自动压缩",
                }),
            ),
            (KODEX_CONTEXT_COMPACTED_META_KEY.to_owned(), json!({})),
        ]));

        if let Err(e) = self.client.send_session_notification(notification) {
            error!("Failed to send context compaction notification: {:?}", e);
        }
    }

    fn send_context_compaction_started(&self) {
        let notification = SessionNotification::new(
            self.session_id.clone(),
            SessionUpdate::SessionInfoUpdate(SessionInfoUpdate::new()),
        )
        .meta(Meta::from_iter([(
            KODEX_CONTEXT_COMPACTION_META_KEY.to_owned(),
            json!({
                "phase": "started",
                "message": "正在压缩上下文",
            }),
        )]));

        if let Err(e) = self.client.send_session_notification(notification) {
            error!(
                "Failed to send context compaction start notification: {:?}",
                e
            );
        }
    }

    fn send_tool_call(&self, tool_call: ToolCall) {
        self.send_notification(SessionUpdate::ToolCall(tool_call));
    }

    fn send_tool_call_update(&self, update: ToolCallUpdate) {
        self.send_notification(SessionUpdate::ToolCallUpdate(update));
    }

    /// Send a completed tool call (used for replay and simple cases)
    fn send_completed_tool_call(
        &self,
        call_id: impl Into<ToolCallId>,
        title: impl Into<String>,
        kind: ToolKind,
        raw_input: Option<serde_json::Value>,
    ) {
        let mut tool_call = ToolCall::new(call_id, title)
            .kind(kind)
            .status(ToolCallStatus::Completed);
        if let Some(input) = raw_input {
            tool_call = tool_call.raw_input(input);
        }
        self.send_tool_call(tool_call);
    }

    /// Send a tool call completion update (used for replay)
    fn send_tool_call_completed(
        &self,
        call_id: impl Into<ToolCallId>,
        raw_output: Option<serde_json::Value>,
    ) {
        let mut fields = ToolCallUpdateFields::new().status(ToolCallStatus::Completed);
        if let Some(output) = raw_output {
            fields = fields.raw_output(output);
        }
        self.send_tool_call_update(ToolCallUpdate::new(call_id, fields));
    }

    fn update_plan(&self, plan: Vec<PlanItemArg>) {
        self.send_notification(SessionUpdate::Plan(Plan::new(
            plan.into_iter()
                .map(|entry| {
                    PlanEntry::new(
                        entry.step,
                        PlanEntryPriority::Medium,
                        match entry.status {
                            StepStatus::Pending => PlanEntryStatus::Pending,
                            StepStatus::InProgress => PlanEntryStatus::InProgress,
                            StepStatus::Completed => PlanEntryStatus::Completed,
                        },
                    )
                })
                .collect(),
        )));
    }

    async fn request_permission_with_meta(
        &self,
        tool_call: ToolCallUpdate,
        options: Vec<PermissionOption>,
        meta: Option<Meta>,
    ) -> Result<RequestPermissionResponse, Error> {
        let mut request =
            RequestPermissionRequest::new(self.session_id.clone(), tool_call, options);
        if let Some(meta) = meta {
            request = request.meta(meta);
        }
        self.client.request_permission(request).await
    }
}

const KODEX_TOOL_STOP_META_KEY: &str = "kodex.ai/toolStop";

fn agent_owned_tool_stop_meta(tool_call_id: &str) -> Meta {
    Meta::from_iter([(
        KODEX_TOOL_STOP_META_KEY.to_owned(),
        json!({
            "toolCallId": tool_call_id,
            "stopKind": "agent_owned",
        }),
    )])
}

/// Build the `kodex.ai/usage` metadata attached to ACP `UsageUpdate` notifications.
///
/// `last` is the most recent request's `TokenUsage` (per-turn delta). `total` is the
/// cumulative session `TokenUsage`. The top-level fields describe `total` (under
/// `scope: "session_total"`); the nested `turn_delta` object describes `last` so the
/// Kodex reducer can update both the session total and the per-turn current usage
/// from a single notification.
///
/// Field mapping from Codex → Kodex:
///   - `input_tokens`            → `input_tokens`
///   - `cached_input_tokens`     → `cache_read_tokens`
///   - `output_tokens`           → `output_tokens`
///   - `reasoning_output_tokens` → `reasoning_tokens`
///   - `total_tokens`            → `total_tokens`
///
/// Codex does not expose a separate cache-creation count, so `cache_write_tokens`
/// is intentionally absent (set to `null`) on the Kodex side.
fn kodex_usage_meta(last: &TokenUsage, total: &TokenUsage, _context_window: i64) -> Meta {
    fn value(usage: &TokenUsage) -> Value {
        json!({
            "input_tokens": usage.input_tokens,
            "cache_read_tokens": usage.cached_input_tokens,
            "output_tokens": usage.output_tokens,
            "reasoning_tokens": usage.reasoning_output_tokens,
            "total_tokens": usage.total_tokens,
            "cache_write_tokens": Value::Null,
        })
    }

    Meta::from_iter([(
        "kodex.ai/usage".to_string(),
        json!({
            "scope": "session_total",
            "agent_cli": "codex-acp",
            "provider": "openai",
            "model": Value::Null,
            "input_tokens": total.input_tokens,
            "cache_read_tokens": total.cached_input_tokens,
            "output_tokens": total.output_tokens,
            "reasoning_tokens": total.reasoning_output_tokens,
            "total_tokens": total.total_tokens,
            "cache_write_tokens": Value::Null,
            "turn_delta": value(last),
        }),
    )])
}

fn merge_meta(mut base: Meta, extra: Meta) -> Meta {
    base.extend(extra);
    base
}

#[cfg(test)]
mod tests;
