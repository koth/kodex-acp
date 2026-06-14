use super::*;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ThreadTitleState {
    pub(super) name: Option<String>,
    pub(super) first_user_message: Option<String>,
}

/// Trait for abstracting over the `CodexThread` to make testing easier.
pub trait CodexThreadImpl: Send + Sync {
    fn submit(&self, op: Op)
    -> Pin<Box<dyn Future<Output = Result<String, CodexErr>> + Send + '_>>;
    fn next_event(&self) -> Pin<Box<dyn Future<Output = Result<Event, CodexErr>> + Send + '_>>;
    fn read_thread_title_state(
        &self,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<ThreadTitleState>> + Send + '_>>;
    fn set_thread_name(
        &self,
        name: String,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + '_>>;
}

impl CodexThreadImpl for CodexThread {
    fn submit(
        &self,
        op: Op,
    ) -> Pin<Box<dyn Future<Output = Result<String, CodexErr>> + Send + '_>> {
        Box::pin(self.submit(op))
    }

    fn next_event(&self) -> Pin<Box<dyn Future<Output = Result<Event, CodexErr>> + Send + '_>> {
        Box::pin(self.next_event())
    }

    fn read_thread_title_state(
        &self,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<ThreadTitleState>> + Send + '_>> {
        Box::pin(async {
            let thread = self
                .read_thread(
                    /*include_archived*/ true, /*include_history*/ false,
                )
                .await?;
            Ok(ThreadTitleState {
                name: thread.name,
                first_user_message: thread.first_user_message,
            })
        })
    }

    fn set_thread_name(
        &self,
        name: String,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + '_>> {
        Box::pin(async move {
            self.update_thread_metadata(
                ThreadMetadataPatch {
                    name: Some(Some(name)),
                    ..Default::default()
                },
                /*include_archived*/ false,
            )
            .await?;
            Ok(())
        })
    }
}

pub trait ModelsManagerImpl: Send + Sync {
    fn get_model(
        &self,
        model_id: &Option<String>,
    ) -> Pin<Box<dyn Future<Output = String> + Send + '_>>;
    fn get_model_info(
        &self,
        model: &str,
        config: &Config,
    ) -> Pin<Box<dyn Future<Output = CodexModelInfo> + Send + '_>>;
    fn list_models(&self) -> Pin<Box<dyn Future<Output = Vec<ModelPreset>> + Send + '_>>;
}

impl ModelsManagerImpl for Arc<dyn ModelsManager> {
    fn get_model(
        &self,
        model_id: &Option<String>,
    ) -> Pin<Box<dyn Future<Output = String> + Send + '_>> {
        let model_id = model_id.clone();
        Box::pin(async move {
            self.get_default_model(&model_id, RefreshStrategy::OnlineIfUncached)
                .await
        })
    }

    fn get_model_info(
        &self,
        model: &str,
        config: &Config,
    ) -> Pin<Box<dyn Future<Output = CodexModelInfo> + Send + '_>> {
        let model = model.to_string();
        let manager_config = config.to_models_manager_config();
        Box::pin(async move {
            ModelsManager::get_model_info(self.as_ref(), &model, &manager_config).await
        })
    }

    fn list_models(&self) -> Pin<Box<dyn Future<Output = Vec<ModelPreset>> + Send + '_>> {
        Box::pin(async move {
            ModelsManager::list_models(self.as_ref(), RefreshStrategy::OnlineIfUncached).await
        })
    }
}

pub(super) trait SessionTitleGenerator: Send + Sync {
    fn generate_title(
        &self,
        session_id: &SessionId,
        prompt_text: &str,
        response_text: Option<&str>,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<Option<String>>> + Send + '_>>;
}

pub(super) struct ModelSessionTitleGenerator {
    auth: Arc<AuthManager>,
    models_manager: Arc<dyn ModelsManagerImpl>,
    config: Config,
}

impl ModelSessionTitleGenerator {
    pub(super) fn new(
        auth: Arc<AuthManager>,
        models_manager: Arc<dyn ModelsManagerImpl>,
        config: Config,
    ) -> Self {
        Self {
            auth,
            models_manager,
            config,
        }
    }
}

impl SessionTitleGenerator for ModelSessionTitleGenerator {
    fn generate_title(
        &self,
        session_id: &SessionId,
        prompt_text: &str,
        response_text: Option<&str>,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<Option<String>>> + Send + '_>> {
        let session_id = session_id.0.to_string();
        let prompt_text = prompt_text.to_string();
        let response_text = response_text.map(ToOwned::to_owned);

        Box::pin(async move {
            let thread_id = CodexThreadId::from_string(&session_id)?;
            let codex_session_id = CodexSessionId::from(thread_id);
            let model = self.models_manager.get_model(&self.config.model).await;
            let model_info = self
                .models_manager
                .get_model_info(&model, &self.config)
                .await;
            let installation_id = resolve_installation_id(&self.config.codex_home).await?;
            let model_client = ModelClient::new(
                Some(self.auth.clone()),
                codex_session_id,
                thread_id,
                installation_id,
                self.config.model_provider.clone(),
                SessionSource::Custom("codex-acp-title".to_string()),
                self.config.model_verbosity,
                /*enable_request_compression*/ false,
                /*include_timing_metrics*/ false,
                /*beta_features_header*/ None,
                /*attestation_provider*/ None,
            );

            let telemetry = SessionTelemetry::new(
                thread_id,
                model.as_str(),
                model_info.slug.as_str(),
                /*account_id*/ None,
                /*account_email*/ None,
                /*auth_mode*/ None,
                "codex-acp".to_string(),
                /*log_user_prompts*/ false,
                "acp".to_string(),
                SessionSource::Custom("codex-acp-title".to_string()),
            );

            let mut prompt = Prompt::default();
            prompt.input = vec![ResponseItem::Message {
                id: None,
                role: "user".to_string(),
                content: vec![ContentItem::InputText {
                    text: build_session_title_prompt(&prompt_text, response_text.as_deref()),
                }],
                phase: None,
            }];

            let mut session = model_client.new_session();
            let mut stream = session
                .stream(
                    &prompt,
                    &model_info,
                    &telemetry,
                    /*effort*/ None,
                    ReasoningSummary::None,
                    self.config.service_tier.clone(),
                    /*turn_metadata_header*/ None,
                    &InferenceTraceContext::disabled(),
                )
                .await?;

            let mut title = String::new();
            while let Some(event) = stream.next().await {
                match event? {
                    ResponseEvent::OutputTextDelta(delta) => title.push_str(&delta),
                    ResponseEvent::OutputItemDone(ResponseItem::Message { content, .. }) => {
                        if title.trim().is_empty() {
                            for item in content {
                                if let ContentItem::OutputText { text } = item {
                                    title.push_str(&text);
                                }
                            }
                        }
                    }
                    ResponseEvent::Completed { .. } => break,
                    _ => {}
                }
            }

            Ok(normalize_session_title(&title, Some(&prompt_text)))
        })
    }
}

pub async fn generate_session_title_with_model(
    auth: Arc<AuthManager>,
    models_manager: Arc<dyn ModelsManagerImpl>,
    config: Config,
    session_id: &SessionId,
    prompt_text: &str,
    response_text: Option<&str>,
) -> anyhow::Result<Option<String>> {
    let generator = ModelSessionTitleGenerator::new(auth, models_manager, config);
    generator
        .generate_title(session_id, prompt_text, response_text)
        .await
}


pub(super) fn publish_session_title(
    session_title: &Arc<Mutex<Option<String>>>,
    client: &SessionClient,
    title: String,
) {
    let mut current_title = session_title.lock().unwrap();
    if current_title.as_deref() == Some(title.as_str()) {
        return;
    }
    *current_title = Some(title.clone());
    drop(current_title);
    client.send_session_title(title);
}

pub(super) fn non_empty_str(text: &str) -> Option<&str> {
    (!text.trim().is_empty()).then_some(text)
}

pub(super) fn build_session_title_prompt(prompt_text: &str, response_text: Option<&str>) -> String {
    let prompt_text = truncate_for_title_prompt(prompt_text);
    let response_text = response_text
        .map(truncate_for_title_prompt)
        .unwrap_or_default();
    format!(
        "{SESSION_TITLE_INSTRUCTIONS}\n\n<user_request>\n{prompt_text}\n</user_request>\n\n<assistant_response>\n{response_text}\n</assistant_response>"
    )
}

fn truncate_for_title_prompt(text: &str) -> String {
    truncate_chars(text, SESSION_TITLE_PROMPT_MAX_CHARS)
}

pub(super) fn prompt_text_from_items(items: &[UserInput]) -> Option<String> {
    let text = items
        .iter()
        .filter_map(|item| match item {
            UserInput::Text { text, .. } => Some(text.trim()),
            _ => None,
        })
        .filter(|text| !text.is_empty())
        .join("\n");
    (!text.is_empty()).then_some(text)
}

pub(super) fn normalize_session_title(title: &str, prompt_text: Option<&str>) -> Option<String> {
    let title = title
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
        .trim_matches(|ch: char| matches!(ch, '"' | '\'' | '`' | '*'))
        .trim_end_matches(|ch: char| matches!(ch, '.' | '。'))
        .trim();
    if title.is_empty() {
        return None;
    }
    if prompt_text.is_some_and(|prompt| equivalent_title_text(title, prompt)) {
        return None;
    }
    Some(truncate_chars(title, SESSION_TITLE_MAX_CHARS))
}

fn equivalent_title_text(left: &str, right: &str) -> bool {
    canonical_title_text(left) == canonical_title_text(right)
}

fn canonical_title_text(text: &str) -> String {
    text.split_whitespace().join(" ")
}

pub(super) fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let keep = max_chars.saturating_sub(3);
    let truncated = text.chars().take(keep).collect::<String>();
    format!("{truncated}...")
}
