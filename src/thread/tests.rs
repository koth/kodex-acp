use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use agent_client_protocol::schema::{RequestPermissionResponse, TextContent};
use codex_core::{config::ConfigOverrides, test_support::all_model_presets};
use codex_protocol::config_types::ModeKind;
use codex_protocol::items::AgentMessageItem;
use codex_protocol::{ThreadId, protocol::ThreadGoal};
use tokio::sync::{Mutex, Notify, mpsc::UnboundedSender};

use super::event_mapping::guardian_action_summary;
use super::permissions::{
    MCP_TOOL_APPROVAL_ALLOW_ALWAYS_OPTION_ID, MCP_TOOL_APPROVAL_ALLOW_OPTION_ID,
    MCP_TOOL_APPROVAL_ALLOW_SESSION_OPTION_ID, MCP_TOOL_APPROVAL_CANCEL_OPTION_ID,
    MCP_TOOL_APPROVAL_PERSIST_SESSION, MCP_TOOL_APPROVAL_REQUEST_ID_PREFIX,
};
use super::title::ThreadTitleState;
use super::*;

mod basic;
mod permissions;
mod title_flow;

fn image_generation_test_saved_path() -> PathBuf {
    std::env::temp_dir().join("ig-1.png")
}

async fn setup() -> anyhow::Result<(
    SessionId,
    Arc<StubClient>,
    Arc<StubCodexThread>,
    UnboundedSender<ThreadMessage>,
    tokio::task::JoinHandle<()>,
)> {
    setup_with_title_generator(None).await
}

async fn setup_with_title_generator(
    title_generator: Option<Arc<dyn SessionTitleGenerator>>,
) -> anyhow::Result<(
    SessionId,
    Arc<StubClient>,
    Arc<StubCodexThread>,
    UnboundedSender<ThreadMessage>,
    tokio::task::JoinHandle<()>,
)> {
    let session_id = SessionId::new(ThreadId::default().to_string());
    let client = Arc::new(StubClient::new());
    let session_client =
        SessionClient::with_client(session_id.clone(), client.clone(), Arc::default());
    let conversation = Arc::new(StubCodexThread::new());
    let models_manager = Arc::new(StubModelsManager);
    let config =
        Config::load_with_cli_overrides_and_harness_overrides(vec![], ConfigOverrides::default())
            .await?;
    let (message_tx, message_rx) = tokio::sync::mpsc::unbounded_channel();
    let (resolution_tx, resolution_rx) = tokio::sync::mpsc::unbounded_channel();

    let actor = ThreadActor::new(
        StubAuth,
        session_client,
        conversation.clone(),
        models_manager,
        config,
        title_generator,
        message_rx,
        resolution_tx,
        resolution_rx,
    );

    let handle = tokio::spawn(actor.spawn());
    Ok((session_id, client, conversation, message_tx, handle))
}

struct StubAuth;

impl Auth for StubAuth {
    async fn logout(&self) -> Result<bool, Error> {
        Ok(true)
    }
}

struct StubModelsManager;

impl ModelsManagerImpl for StubModelsManager {
    fn get_model(
        &self,
        _model_id: &Option<String>,
    ) -> Pin<Box<dyn Future<Output = String> + Send + '_>> {
        Box::pin(async { all_model_presets()[0].to_owned().id })
    }

    fn get_model_info(
        &self,
        model: &str,
        _config: &Config,
    ) -> Pin<Box<dyn Future<Output = CodexModelInfo> + Send + '_>> {
        let model = model.to_string();
        Box::pin(async move { codex_models_manager::model_info::model_info_from_slug(&model) })
    }

    fn list_models(&self) -> Pin<Box<dyn Future<Output = Vec<ModelPreset>> + Send + '_>> {
        Box::pin(async { all_model_presets().to_owned() })
    }
}

struct StubSessionTitleGenerator {
    title: Option<String>,
    calls: AtomicUsize,
}

impl StubSessionTitleGenerator {
    fn new(title: Option<&str>) -> Self {
        Self {
            title: title.map(ToOwned::to_owned),
            calls: AtomicUsize::new(0),
        }
    }
}

impl SessionTitleGenerator for StubSessionTitleGenerator {
    fn generate_title(
        &self,
        _session_id: &SessionId,
        _prompt_text: &str,
        _response_text: Option<&str>,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<Option<String>>> + Send + '_>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(self.title.clone()) })
    }
}

struct FailingSessionTitleGenerator {
    calls: AtomicUsize,
}

impl FailingSessionTitleGenerator {
    fn new() -> Self {
        Self {
            calls: AtomicUsize::new(0),
        }
    }
}

impl SessionTitleGenerator for FailingSessionTitleGenerator {
    fn generate_title(
        &self,
        _session_id: &SessionId,
        _prompt_text: &str,
        _response_text: Option<&str>,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<Option<String>>> + Send + '_>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Err(anyhow::anyhow!("403 Forbidden")) })
    }
}

struct StubCodexThread {
    current_id: AtomicUsize,
    active_prompt_id: std::sync::Mutex<Option<String>>,
    thread_name: std::sync::Mutex<Option<String>>,
    first_user_message: std::sync::Mutex<Option<String>>,
    ops: std::sync::Mutex<Vec<Op>>,
    op_tx: mpsc::UnboundedSender<Event>,
    op_rx: Mutex<mpsc::UnboundedReceiver<Event>>,
}

impl StubCodexThread {
    fn new() -> Self {
        let (op_tx, op_rx) = mpsc::unbounded_channel();
        StubCodexThread {
            current_id: AtomicUsize::new(0),
            active_prompt_id: std::sync::Mutex::default(),
            thread_name: std::sync::Mutex::default(),
            first_user_message: std::sync::Mutex::default(),
            ops: std::sync::Mutex::default(),
            op_tx,
            op_rx: Mutex::new(op_rx),
        }
    }
}

impl CodexThreadImpl for StubCodexThread {
    fn submit(
        &self,
        op: Op,
    ) -> Pin<Box<dyn Future<Output = Result<String, CodexErr>> + Send + '_>> {
        Box::pin(async move {
            let id = self
                .current_id
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);

            self.ops.lock().unwrap().push(op.clone());

            match op {
                Op::UserInput { items, .. } => {
                    *self.active_prompt_id.lock().unwrap() = Some(id.to_string());
                    let prompt = items
                        .into_iter()
                        .map(|i| match i {
                            UserInput::Text { text, .. } => text,
                            _ => unimplemented!(),
                        })
                        .join("\n");
                    let mut first_user_message = self.first_user_message.lock().unwrap();
                    if first_user_message.is_none() {
                        *first_user_message = Some(prompt.clone());
                    }
                    drop(first_user_message);

                    if prompt == "steer-reject" {
                        return Err(CodexErr::UnsupportedOperation(
                            "active turn is not steerable".to_string(),
                        ));
                    } else if prompt.starts_with(SESSION_TITLE_INSTRUCTIONS) {
                        let turn_id = id.to_string();
                        let title = "WOA Title Fix".to_string();
                        let send = |msg| {
                            self.op_tx
                                .send(Event {
                                    id: id.to_string(),
                                    msg,
                                })
                                .unwrap();
                        };
                        send(EventMsg::AgentMessageContentDelta(
                            AgentMessageContentDeltaEvent {
                                thread_id: id.to_string(),
                                turn_id: turn_id.clone(),
                                item_id: id.to_string(),
                                delta: title.clone(),
                            },
                        ));
                        send(EventMsg::TurnComplete(TurnCompleteEvent {
                            last_agent_message: Some(title),
                            turn_id,
                            completed_at: None,
                            duration_ms: None,
                            time_to_first_token_ms: None,
                        }));
                    } else if prompt == "parallel-exec" {
                        // Emit interleaved exec events: Begin A, Begin B, End A, End B
                        let turn_id = id.to_string();
                        let cwd = std::env::current_dir().unwrap();
                        let send = |msg| {
                            self.op_tx
                                .send(Event {
                                    id: id.to_string(),
                                    msg,
                                })
                                .unwrap();
                        };
                        send(EventMsg::ExecCommandBegin(ExecCommandBeginEvent {
                            call_id: "call-a".into(),
                            process_id: None,
                            turn_id: turn_id.clone(),
                            command: vec!["echo".into(), "a".into()],
                            cwd: cwd.clone().try_into()?,
                            parsed_cmd: vec![ParsedCommand::Unknown {
                                cmd: "echo a".into(),
                            }],
                            source: Default::default(),
                            interaction_input: None,
                            started_at_ms: 0,
                        }));
                        send(EventMsg::ExecCommandBegin(ExecCommandBeginEvent {
                            call_id: "call-b".into(),
                            process_id: None,
                            turn_id: turn_id.clone(),
                            command: vec!["echo".into(), "b".into()],
                            cwd: cwd.clone().try_into()?,
                            parsed_cmd: vec![ParsedCommand::Unknown {
                                cmd: "echo b".into(),
                            }],
                            source: Default::default(),
                            interaction_input: None,
                            started_at_ms: 0,
                        }));
                        send(EventMsg::ExecCommandEnd(ExecCommandEndEvent {
                            call_id: "call-a".into(),
                            process_id: None,
                            turn_id: turn_id.clone(),
                            command: vec!["echo".into(), "a".into()],
                            cwd: cwd.clone().try_into()?,
                            parsed_cmd: vec![],
                            source: Default::default(),
                            interaction_input: None,
                            stdout: "a\n".into(),
                            stderr: String::new(),
                            aggregated_output: "a\n".into(),
                            exit_code: 0,
                            duration: std::time::Duration::from_millis(10),
                            formatted_output: "a\n".into(),
                            status: ExecCommandStatus::Completed,
                            completed_at_ms: 0,
                        }));
                        send(EventMsg::ExecCommandEnd(ExecCommandEndEvent {
                            call_id: "call-b".into(),
                            process_id: None,
                            turn_id: turn_id.clone(),
                            command: vec!["echo".into(), "b".into()],
                            cwd: cwd.clone().try_into()?,
                            parsed_cmd: vec![],
                            source: Default::default(),
                            interaction_input: None,
                            stdout: "b\n".into(),
                            stderr: String::new(),
                            aggregated_output: "b\n".into(),
                            exit_code: 0,
                            duration: std::time::Duration::from_millis(10),
                            formatted_output: "b\n".into(),
                            status: ExecCommandStatus::Completed,
                            completed_at_ms: 0,
                        }));
                        send(EventMsg::TurnComplete(TurnCompleteEvent {
                            last_agent_message: None,
                            turn_id,
                            completed_at: None,
                            duration_ms: None,
                            time_to_first_token_ms: None,
                        }));
                    } else if prompt == "long-exec" {
                        let turn_id = id.to_string();
                        let cwd = std::env::current_dir().unwrap();
                        self.op_tx
                            .send(Event {
                                id: id.to_string(),
                                msg: EventMsg::ExecCommandBegin(ExecCommandBeginEvent {
                                    call_id: "call-long".into(),
                                    process_id: None,
                                    turn_id,
                                    command: vec!["sleep".into(), "60".into()],
                                    cwd: cwd.try_into()?,
                                    parsed_cmd: vec![ParsedCommand::Unknown {
                                        cmd: "sleep 60".into(),
                                    }],
                                    source: Default::default(),
                                    interaction_input: None,
                                    started_at_ms: 0,
                                }),
                            })
                            .unwrap();
                    } else if prompt == "title-sync" {
                        let turn_id = id.to_string();
                        let send = |msg| {
                            self.op_tx
                                .send(Event {
                                    id: id.to_string(),
                                    msg,
                                })
                                .unwrap();
                        };
                        send(EventMsg::TurnComplete(TurnCompleteEvent {
                            last_agent_message: Some("Session title sync is fixed.".into()),
                            turn_id,
                            completed_at: None,
                            duration_ms: None,
                            time_to_first_token_ms: None,
                        }));
                    } else if prompt == "image-generation" {
                        let turn_id = id.to_string();
                        let saved_path = image_generation_test_saved_path();
                        let send = |msg| {
                            self.op_tx
                                .send(Event {
                                    id: id.to_string(),
                                    msg,
                                })
                                .unwrap();
                        };
                        send(EventMsg::ImageGenerationBegin(ImageGenerationBeginEvent {
                            call_id: "ig-1".into(),
                        }));
                        send(EventMsg::ImageGenerationEnd(ImageGenerationEndEvent {
                            call_id: "ig-1".into(),
                            status: "completed".into(),
                            revised_prompt: Some("A tiny blue square".into()),
                            result: "Zm9v".into(),
                            saved_path: Some(saved_path.try_into()?),
                        }));
                        send(EventMsg::TurnComplete(TurnCompleteEvent {
                            last_agent_message: None,
                            turn_id,
                            completed_at: None,
                            duration_ms: None,
                            time_to_first_token_ms: None,
                        }));
                    } else if prompt == "thread-goal-update" {
                        let turn_id = id.to_string();
                        let thread_id = ThreadId::default();
                        self.op_tx
                            .send(Event {
                                id: id.to_string(),
                                msg: EventMsg::ThreadGoalUpdated(ThreadGoalUpdatedEvent {
                                    thread_id,
                                    turn_id: Some(turn_id.clone()),
                                    goal: ThreadGoal {
                                        thread_id,
                                        objective: "Ship the goal update".to_string(),
                                        status: ThreadGoalStatus::Active,
                                        token_budget: Some(100),
                                        tokens_used: 10,
                                        time_used_seconds: 2,
                                        created_at: 1,
                                        updated_at: 2,
                                    },
                                }),
                            })
                            .unwrap();
                        self.op_tx
                            .send(Event {
                                id: id.to_string(),
                                msg: EventMsg::TurnComplete(TurnCompleteEvent {
                                    last_agent_message: None,
                                    turn_id,
                                    completed_at: None,
                                    duration_ms: None,
                                    time_to_first_token_ms: None,
                                }),
                            })
                            .unwrap();
                    } else if prompt == "commentary-only" {
                        let turn_id = id.to_string();
                        self.op_tx
                            .send(Event {
                                id: id.to_string(),
                                msg: EventMsg::AgentMessage(AgentMessageEvent {
                                    message: "Need patch.".to_string(),
                                    phase: Some(MessagePhase::Commentary),
                                    memory_citation: None,
                                }),
                            })
                            .unwrap();
                        self.op_tx
                            .send(Event {
                                id: id.to_string(),
                                msg: EventMsg::TurnComplete(TurnCompleteEvent {
                                    last_agent_message: None,
                                    turn_id,
                                    completed_at: None,
                                    duration_ms: None,
                                    time_to_first_token_ms: None,
                                }),
                            })
                            .unwrap();
                    } else if prompt == "commentary-delta-then-final" {
                        let turn_id = id.to_string();
                        let commentary_item_id = "commentary-item".to_string();
                        self.op_tx
                            .send(Event {
                                id: id.to_string(),
                                msg: EventMsg::ItemStarted(ItemStartedEvent {
                                    thread_id: ThreadId::default(),
                                    turn_id: turn_id.clone(),
                                    item: TurnItem::AgentMessage(AgentMessageItem {
                                        id: commentary_item_id.clone(),
                                        content: vec![],
                                        phase: Some(MessagePhase::Commentary),
                                        memory_citation: None,
                                    }),
                                    started_at_ms: 0,
                                }),
                            })
                            .unwrap();
                        self.op_tx
                            .send(Event {
                                id: id.to_string(),
                                msg: EventMsg::AgentMessageContentDelta(
                                    AgentMessageContentDeltaEvent {
                                        thread_id: id.to_string(),
                                        turn_id: turn_id.clone(),
                                        item_id: commentary_item_id,
                                        delta: "Need internal note.".to_string(),
                                    },
                                ),
                            })
                            .unwrap();
                        self.op_tx
                            .send(Event {
                                id: id.to_string(),
                                msg: EventMsg::AgentMessage(AgentMessageEvent {
                                    message: "Final answer.".to_string(),
                                    phase: Some(MessagePhase::FinalAnswer),
                                    memory_citation: None,
                                }),
                            })
                            .unwrap();
                        self.op_tx
                            .send(Event {
                                id: id.to_string(),
                                msg: EventMsg::TurnComplete(TurnCompleteEvent {
                                    last_agent_message: None,
                                    turn_id,
                                    completed_at: None,
                                    duration_ms: None,
                                    time_to_first_token_ms: None,
                                }),
                            })
                            .unwrap();
                    } else if prompt == "approval-block" {
                        self.op_tx
                            .send(Event {
                                id: id.to_string(),
                                msg: EventMsg::ExecApprovalRequest(ExecApprovalRequestEvent {
                                    call_id: "call-id".to_string(),
                                    approval_id: Some("approval-id".to_string()),
                                    turn_id: id.to_string(),
                                    started_at_ms: 0,
                                    command: vec!["echo".to_string(), "hi".to_string()],
                                    cwd: std::env::current_dir().unwrap().try_into().unwrap(),
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
                            })
                            .unwrap();
                    } else {
                        self.op_tx
                            .send(Event {
                                id: id.to_string(),
                                msg: EventMsg::AgentMessageContentDelta(
                                    AgentMessageContentDeltaEvent {
                                        thread_id: id.to_string(),
                                        turn_id: id.to_string(),
                                        item_id: id.to_string(),
                                        delta: prompt.clone(),
                                    },
                                ),
                            })
                            .unwrap();
                        // Send non-delta event (should be deduplicated, but handled by deduplication)
                        self.op_tx
                            .send(Event {
                                id: id.to_string(),
                                msg: EventMsg::AgentMessage(AgentMessageEvent {
                                    message: prompt,
                                    phase: None,
                                    memory_citation: None,
                                }),
                            })
                            .unwrap();
                        self.op_tx
                            .send(Event {
                                id: id.to_string(),
                                msg: EventMsg::TurnComplete(TurnCompleteEvent {
                                    last_agent_message: None,
                                    turn_id: id.to_string(),
                                    completed_at: None,
                                    duration_ms: None,
                                    time_to_first_token_ms: None,
                                }),
                            })
                            .unwrap();
                    }
                }
                Op::Compact => {
                    self.op_tx
                        .send(Event {
                            id: id.to_string(),
                            msg: EventMsg::TurnStarted(TurnStartedEvent {
                                model_context_window: None,
                                collaboration_mode_kind: ModeKind::default(),
                                turn_id: id.to_string(),
                                trace_id: None,
                                started_at: None,
                            }),
                        })
                        .unwrap();
                    self.op_tx
                        .send(Event {
                            id: id.to_string(),
                            msg: EventMsg::ContextCompacted(
                                codex_protocol::protocol::ContextCompactedEvent {},
                            ),
                        })
                        .unwrap();
                    self.op_tx
                        .send(Event {
                            id: id.to_string(),
                            msg: EventMsg::AgentMessage(AgentMessageEvent {
                                message: "Compact task completed".to_string(),
                                phase: None,
                                memory_citation: None,
                            }),
                        })
                        .unwrap();
                    self.op_tx
                        .send(Event {
                            id: id.to_string(),
                            msg: EventMsg::TurnComplete(TurnCompleteEvent {
                                last_agent_message: None,
                                turn_id: id.to_string(),
                                completed_at: None,
                                duration_ms: None,
                                time_to_first_token_ms: None,
                            }),
                        })
                        .unwrap();
                }
                Op::Review { review_request } => {
                    self.op_tx
                        .send(Event {
                            id: id.to_string(),
                            msg: EventMsg::EnteredReviewMode(review_request.clone()),
                        })
                        .unwrap();
                    self.op_tx
                        .send(Event {
                            id: id.to_string(),
                            msg: EventMsg::ExitedReviewMode(ExitedReviewModeEvent {
                                review_output: Some(ReviewOutputEvent {
                                    findings: vec![],
                                    overall_correctness: String::new(),
                                    overall_explanation: review_request
                                        .user_facing_hint
                                        .clone()
                                        .unwrap_or_default(),
                                    overall_confidence_score: 1.,
                                }),
                            }),
                        })
                        .unwrap();
                    self.op_tx
                        .send(Event {
                            id: id.to_string(),
                            msg: EventMsg::TurnComplete(TurnCompleteEvent {
                                last_agent_message: None,
                                turn_id: id.to_string(),
                                completed_at: None,
                                duration_ms: None,
                                time_to_first_token_ms: None,
                            }),
                        })
                        .unwrap();
                }
                Op::ThreadRollback { num_turns } => {
                    self.op_tx
                        .send(Event {
                            id: id.to_string(),
                            msg: EventMsg::ThreadRolledBack(
                                codex_protocol::protocol::ThreadRolledBackEvent { num_turns },
                            ),
                        })
                        .unwrap();
                }
                Op::ExecApproval { .. }
                | Op::ResolveElicitation { .. }
                | Op::RequestPermissionsResponse { .. }
                | Op::UserInputAnswer { .. }
                | Op::PatchApproval { .. }
                | Op::ThreadSettings { .. }
                | Op::RefreshMcpServers { .. }
                | Op::Interrupt => {}
                Op::Shutdown => {
                    if let Some(active_prompt_id) = self.active_prompt_id.lock().unwrap().take() {
                        self.op_tx
                            .send(Event {
                                id: active_prompt_id.clone(),
                                msg: EventMsg::TurnAborted(TurnAbortedEvent {
                                    turn_id: Some(active_prompt_id),
                                    reason: codex_protocol::protocol::TurnAbortReason::Interrupted,
                                    completed_at: None,
                                    duration_ms: None,
                                }),
                            })
                            .unwrap();
                    }
                }
                _ => {
                    unimplemented!()
                }
            }
            Ok(id.to_string())
        })
    }

    fn next_event(&self) -> Pin<Box<dyn Future<Output = Result<Event, CodexErr>> + Send + '_>> {
        Box::pin(async {
            let Some(event) = self.op_rx.lock().await.recv().await else {
                return Err(CodexErr::InternalAgentDied);
            };
            Ok(event)
        })
    }

    fn read_thread_title_state(
        &self,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<ThreadTitleState>> + Send + '_>> {
        Box::pin(async {
            Ok(ThreadTitleState {
                name: self.thread_name.lock().unwrap().clone(),
                first_user_message: self.first_user_message.lock().unwrap().clone(),
            })
        })
    }

    fn set_thread_name(
        &self,
        name: String,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + '_>> {
        Box::pin(async move {
            *self.thread_name.lock().unwrap() = Some(name);
            Ok(())
        })
    }
}

struct StubClient {
    notifications: std::sync::Mutex<Vec<SessionNotification>>,
    permission_requests: std::sync::Mutex<Vec<RequestPermissionRequest>>,
    permission_responses: std::sync::Mutex<VecDeque<RequestPermissionResponse>>,
    block_permission_requests: Option<Arc<Notify>>,
}

impl StubClient {
    fn new() -> Self {
        StubClient {
            notifications: std::sync::Mutex::default(),
            permission_requests: std::sync::Mutex::default(),
            permission_responses: std::sync::Mutex::default(),
            block_permission_requests: None,
        }
    }

    fn with_permission_responses(responses: Vec<RequestPermissionResponse>) -> Self {
        StubClient {
            notifications: std::sync::Mutex::default(),
            permission_requests: std::sync::Mutex::default(),
            permission_responses: std::sync::Mutex::new(responses.into()),
            block_permission_requests: None,
        }
    }

    fn with_blocked_permission_requests(
        responses: Vec<RequestPermissionResponse>,
        notify: Arc<Notify>,
    ) -> Self {
        StubClient {
            notifications: std::sync::Mutex::default(),
            permission_requests: std::sync::Mutex::default(),
            permission_responses: std::sync::Mutex::new(responses.into()),
            block_permission_requests: Some(notify),
        }
    }
}

impl ClientSender for StubClient {
    fn send_session_notification(&self, args: SessionNotification) -> Result<(), Error> {
        self.notifications.lock().unwrap().push(args);
        Ok(())
    }

    fn request_permission(
        &self,
        args: RequestPermissionRequest,
    ) -> Pin<Box<dyn Future<Output = Result<RequestPermissionResponse, Error>> + Send + '_>> {
        Box::pin(async move {
            self.permission_requests.lock().unwrap().push(args);
            if let Some(notify) = &self.block_permission_requests {
                notify.notified().await;
            }
            Ok(self
                .permission_responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| {
                    RequestPermissionResponse::new(RequestPermissionOutcome::Cancelled)
                }))
        })
    }
}

#[tokio::test]
async fn test_parallel_exec_commands() -> anyhow::Result<()> {
    let (session_id, client, _, message_tx, _handle) = setup().await?;
    let (prompt_response_tx, prompt_response_rx) = tokio::sync::oneshot::channel();

    message_tx.send(ThreadMessage::Prompt {
        request: PromptRequest::new(session_id.clone(), vec!["parallel-exec".into()]),
        response_tx: prompt_response_tx,
    })?;

    let stop_reason = prompt_response_rx.await??.await??;
    assert_eq!(stop_reason, StopReason::EndTurn);
    drop(message_tx);

    let notifications = client.notifications.lock().unwrap();

    // Collect all ToolCall (begin) notifications keyed by their tool_call_id prefix.
    let tool_calls: Vec<_> = notifications
        .iter()
        .filter_map(|n| match &n.update {
            SessionUpdate::ToolCall(tc) => Some(tc.clone()),
            _ => None,
        })
        .collect();

    // Collect all ToolCallUpdate notifications that carry a terminal status.
    let completed_updates: Vec<_> = notifications
        .iter()
        .filter_map(|n| match &n.update {
            SessionUpdate::ToolCallUpdate(update) => {
                if update.fields.status == Some(ToolCallStatus::Completed) {
                    Some(update.clone())
                } else {
                    None
                }
            }
            _ => None,
        })
        .collect();

    // Both commands A and B should have produced a ToolCall (begin).
    assert_eq!(
        tool_calls.len(),
        2,
        "expected 2 ToolCall begin notifications, got {tool_calls:?}"
    );

    // Both commands A and B should have produced a completed ToolCallUpdate.
    assert_eq!(
        completed_updates.len(),
        2,
        "expected 2 completed ToolCallUpdate notifications, got {completed_updates:?}"
    );

    // The completed updates should reference the same tool_call_ids as the begins.
    let begin_ids: std::collections::HashSet<_> = tool_calls
        .iter()
        .map(|tc| tc.tool_call_id.clone())
        .collect();
    let end_ids: std::collections::HashSet<_> = completed_updates
        .iter()
        .map(|u| u.tool_call_id.clone())
        .collect();
    assert_eq!(
        begin_ids, end_ids,
        "completed update tool_call_ids should match begin tool_call_ids"
    );

    Ok(())
}

#[tokio::test]
async fn stop_tool_interrupts_only_registered_in_flight_tool() -> anyhow::Result<()> {
    let (session_id, client, conversation, message_tx, _handle) = setup().await?;
    let (prompt_response_tx, prompt_response_rx) = tokio::sync::oneshot::channel();

    message_tx.send(ThreadMessage::Prompt {
        request: PromptRequest::new(session_id.clone(), vec!["long-exec".into()]),
        response_tx: prompt_response_tx,
    })?;

    let _prompt_stop_rx = prompt_response_rx.await??;

    let tool_call = wait_for_tool_call(&client, "call-long").await?;
    let tool_stop = tool_call
        .meta
        .as_ref()
        .and_then(|meta| meta.get(KODEX_TOOL_STOP_META_KEY))
        .expect("tool call should include stop metadata");
    assert_eq!(
        tool_stop
            .get("toolCallId")
            .and_then(serde_json::Value::as_str),
        Some("call-long")
    );
    assert_eq!(
        tool_stop
            .get("stopKind")
            .and_then(serde_json::Value::as_str),
        Some("agent_owned")
    );

    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
    message_tx.send(ThreadMessage::StopTool {
        tool_call_id: "call-long".into(),
        response_tx: stop_tx,
    })?;
    assert!(stop_rx.await??);

    let ops = conversation.ops.lock().unwrap();
    assert!(matches!(ops.last(), Some(Op::Interrupt)));
    drop(ops);

    let notifications = client.notifications.lock().unwrap();
    assert!(notifications.iter().any(|notification| {
        matches!(
            &notification.update,
            SessionUpdate::ToolCallUpdate(update)
                if update.tool_call_id.0.as_ref() == "call-long"
                    && update.fields.status == Some(ToolCallStatus::Failed)
        )
    }));

    Ok(())
}

#[tokio::test]
async fn stop_tool_ignores_unknown_tool_without_interrupting_turn() -> anyhow::Result<()> {
    let (_session_id, _client, conversation, message_tx, _handle) = setup().await?;
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();

    message_tx.send(ThreadMessage::StopTool {
        tool_call_id: "missing-tool".into(),
        response_tx: stop_tx,
    })?;

    assert!(!stop_rx.await??);
    let ops = conversation.ops.lock().unwrap();
    assert!(
        !ops.iter().any(|op| matches!(op, Op::Interrupt)),
        "unknown tool stop must not interrupt the turn"
    );

    Ok(())
}

async fn wait_for_tool_call(client: &StubClient, tool_call_id: &str) -> anyhow::Result<ToolCall> {
    for _ in 0..50 {
        if let Some(tool_call) =
            client
                .notifications
                .lock()
                .unwrap()
                .iter()
                .find_map(|notification| match &notification.update {
                    SessionUpdate::ToolCall(tool_call)
                        if tool_call.tool_call_id.0.as_ref() == tool_call_id =>
                    {
                        Some(tool_call.clone())
                    }
                    _ => None,
                })
        {
            return Ok(tool_call);
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    anyhow::bail!("timed out waiting for tool call {tool_call_id}");
}
