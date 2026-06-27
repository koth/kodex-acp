use super::*;

impl<A: Auth> ThreadActor<A> {
    pub(super) fn builtin_commands() -> Vec<AvailableCommand> {
        vec![
            AvailableCommand::new("review", "Review my current changes and find issues").input(
                AvailableCommandInput::Unstructured(UnstructuredCommandInput::new(
                    "optional custom review instructions",
                )),
            ),
            AvailableCommand::new(
                "review-branch",
                "Review the code changes against a specific branch",
            )
            .input(AvailableCommandInput::Unstructured(
                UnstructuredCommandInput::new("branch name"),
            )),
            AvailableCommand::new(
                "review-commit",
                "Review the code changes introduced by a commit",
            )
            .input(AvailableCommandInput::Unstructured(
                UnstructuredCommandInput::new("commit sha"),
            )),
            AvailableCommand::new(
                "init",
                "create an AGENTS.md file with instructions for Codex",
            ),
            AvailableCommand::new(
                "compact",
                "summarize conversation to prevent hitting the context limit",
            ),
            AvailableCommand::new("logout", "logout of Codex"),
        ]
    }

    pub(super) fn modes(&self) -> Option<SessionModeState> {
        let current_mode_id = current_session_mode_id(&self.config)?;

        Some(SessionModeState::new(
            current_mode_id,
            APPROVAL_PRESETS
                .iter()
                .map(|preset| {
                    SessionMode::new(preset.id, preset.label).description(preset.description)
                })
                .collect(),
        ))
    }

    pub(super) async fn find_current_model(&self) -> Option<ModelId> {
        let provider_entries = Self::kodex_model_provider_entries();
        let mut used_provider_entries = vec![false; provider_entries.len()];
        let active_provider = self.config.model_provider_id.clone();
        let model_presets = self.models_manager.list_models().await;
        let config_model = self.get_current_model().await;
        let (preset, provider) = model_presets
            .iter()
            .filter_map(|preset| {
                let provider = Self::provider_for_preset_from_entries(
                    &provider_entries,
                    &mut used_provider_entries,
                    &preset.model,
                    &preset.display_name,
                )
                .unwrap_or_else(|| active_provider.clone());
                Some((preset, provider))
            })
            .find(|(preset, provider)| preset.model == config_model && provider == &active_provider)
            .or_else(|| {
                model_presets
                    .iter()
                    .find(|preset| preset.model == config_model)
                    .map(|preset| (preset, active_provider.clone()))
            })?;

        let effort = self
            .config
            .model_reasoning_effort
            .and_then(|effort| {
                preset
                    .supported_reasoning_efforts
                    .iter()
                    .find_map(|e| (e.effort == effort).then_some(effort))
            })
            .unwrap_or(preset.default_reasoning_effort);

        Some(Self::model_id_for_provider(&preset.id, effort, &provider))
    }

    pub(super) fn model_id_for_provider(
        id: &str,
        effort: ReasoningEffort,
        provider: &str,
    ) -> ModelId {
        ModelId::new(Self::encode_provider_value(
            &format!("{id}/{effort}"),
            provider,
        ))
    }

    pub(super) fn model_provider_meta(provider: &str) -> Meta {
        let mut meta = Meta::new();
        meta.insert("provider".to_string(), json!(provider));
        meta
    }

    pub(super) fn kodex_model_provider_entries() -> Vec<KodexModelProviderEntry> {
        std::env::var(KODEX_MODEL_PROVIDER_MAP_ENV)
            .ok()
            .and_then(|value| serde_json::from_str(&value).ok())
            .unwrap_or_default()
    }

    pub(super) fn provider_for_preset_from_entries(
        entries: &[KodexModelProviderEntry],
        used_entries: &mut [bool],
        model: &str,
        display_name: &str,
    ) -> Option<String> {
        entries.iter().enumerate().find_map(|(index, entry)| {
            if used_entries.get(index).copied().unwrap_or(true) {
                return None;
            }
            if entry.model == model || entry.display_name == display_name {
                if let Some(used) = used_entries.get_mut(index) {
                    *used = true;
                }
                Some(entry.provider.clone())
            } else {
                None
            }
        })
    }

    pub(super) fn encode_provider_value(value: &str, provider: &str) -> String {
        if value.starts_with(KODEX_PROVIDER_VALUE_PREFIX) || provider.trim().is_empty() {
            value.to_string()
        } else {
            format!("{KODEX_PROVIDER_VALUE_PREFIX}{}:{value}", provider.trim())
        }
    }

    pub(super) fn decode_provider_value(value: &str) -> (Option<String>, String) {
        let Some(rest) = value.strip_prefix(KODEX_PROVIDER_VALUE_PREFIX) else {
            return (None, value.to_string());
        };
        let Some((provider, model)) = rest.split_once(':') else {
            return (None, value.to_string());
        };
        let provider = provider.trim();
        if provider.is_empty() {
            (None, model.to_string())
        } else {
            (Some(provider.to_string()), model.to_string())
        }
    }

    pub(super) fn set_active_model_provider(&mut self, provider: &str) -> Result<(), Error> {
        match provider_activation_for_request(
            provider,
            &self.config.model_provider_id,
            |candidate| self.config.model_providers.contains_key(candidate),
        )? {
            ProviderActivation::KeepCurrent => Ok(()),
            ProviderActivation::Activate(target) => {
                let provider_config = self
                    .config
                    .model_providers
                    .get(&target)
                    .cloned()
                    .expect("target provider existence is validated by provider_activation_for_request");
                self.config.model_provider_id = target;
                self.config.model_provider = provider_config;
                Ok(())
            }
        }
    }

    pub(super) fn parse_model_id(id: &ModelId) -> Option<(String, ReasoningEffort)> {
        let (model, reasoning) = id.0.split_once('/')?;
        let reasoning = serde_json::from_value(reasoning.into()).ok()?;
        Some((model.to_owned(), reasoning))
    }

    pub(super) async fn config_options(&self) -> Result<Vec<SessionConfigOption>, Error> {
        let mut options = Vec::new();

        if let Some(modes) = self.modes() {
            let select_options = modes
                .available_modes
                .into_iter()
                .map(|m| SessionConfigSelectOption::new(m.id.0, m.name).description(m.description))
                .collect::<Vec<_>>();

            options.push(
                SessionConfigOption::select(
                    "mode",
                    "Approval Preset",
                    modes.current_mode_id.0,
                    select_options,
                )
                .category(SessionConfigOptionCategory::Mode)
                .description("Choose an approval and sandboxing preset for your session"),
            );
        }

        let provider_entries = Self::kodex_model_provider_entries();
        let mut used_provider_entries = vec![false; provider_entries.len()];
        let active_provider = self.config.model_provider_id.clone();
        let presets = self.models_manager.list_models().await;

        let current_model = self.get_current_model().await;
        let preset_options = presets
            .into_iter()
            .filter(|model| model.show_in_picker || model.model == current_model)
            .map(|preset| {
                let provider = Self::provider_for_preset_from_entries(
                    &provider_entries,
                    &mut used_provider_entries,
                    &preset.model,
                    &preset.display_name,
                )
                .unwrap_or_else(|| active_provider.clone());
                (preset, provider)
            })
            .collect::<Vec<_>>();
        let current_preset = preset_options
            .iter()
            .find(|(preset, provider)| {
                preset.model == current_model && provider == &active_provider
            })
            .or_else(|| {
                preset_options
                    .iter()
                    .find(|(preset, _provider)| preset.model == current_model)
            });
        let current_value = current_preset
            .map(|(preset, provider)| Self::encode_provider_value(&preset.id, provider))
            .unwrap_or_else(|| Self::encode_provider_value(&current_model, &active_provider));

        let mut model_select_options = Vec::new();

        if current_preset.is_none() {
            // If no preset found, return the current model string as-is
            model_select_options.push(
                SessionConfigSelectOption::new(current_value.clone(), current_model.clone())
                    .meta(Self::model_provider_meta(&active_provider)),
            );
        };

        model_select_options.extend(preset_options.iter().map(|(preset, provider)| {
            SessionConfigSelectOption::new(
                Self::encode_provider_value(&preset.id, provider),
                preset.display_name.clone(),
            )
            .description(preset.description.clone())
            .meta(Self::model_provider_meta(provider))
        }));

        options.push(
            SessionConfigOption::select("model", "Model", current_value, model_select_options)
                .category(SessionConfigOptionCategory::Model)
                .description("Choose which model Codex should use"),
        );

        // Reasoning effort selector (only if the current preset exists and has >1 supported effort)
        if let Some((preset, _provider)) = current_preset
            && preset.supported_reasoning_efforts.len() > 1
        {
            let supported = &preset.supported_reasoning_efforts;

            let current_effort = self
                .config
                .model_reasoning_effort
                .and_then(|effort| {
                    supported
                        .iter()
                        .find_map(|e| (e.effort == effort).then_some(effort))
                })
                .unwrap_or(preset.default_reasoning_effort);

            let effort_select_options = supported
                .iter()
                .map(|e| {
                    SessionConfigSelectOption::new(
                        e.effort.to_string(),
                        e.effort.to_string().to_title_case(),
                    )
                    .description(e.description.clone())
                })
                .collect::<Vec<_>>();

            options.push(
                SessionConfigOption::select(
                    "reasoning_effort",
                    "Reasoning Effort",
                    current_effort.to_string(),
                    effort_select_options,
                )
                .category(SessionConfigOptionCategory::ThoughtLevel)
                .description("Choose how much reasoning effort the model should use"),
            );
        }

        Ok(options)
    }

    pub(super) async fn maybe_emit_config_options_update(&mut self) {
        let config_options = self.config_options().await.unwrap_or_default();

        if self
            .last_sent_config_options
            .as_ref()
            .is_some_and(|prev| prev == &config_options)
        {
            return;
        }

        self.last_sent_config_options = Some(config_options.clone());

        self.client
            .send_notification(SessionUpdate::ConfigOptionUpdate(ConfigOptionUpdate::new(
                config_options,
            )));
    }

    pub(super) async fn handle_set_config_option(
        &mut self,
        config_id: SessionConfigId,
        value: SessionConfigOptionValue,
    ) -> Result<(), Error> {
        let SessionConfigOptionValue::ValueId { value } = value else {
            return Err(Error::invalid_params().data("Unsupported config option value"));
        };
        match config_id.0.as_ref() {
            "mode" => self.handle_set_mode(SessionModeId::new(value.0)).await,
            "model" => self.handle_set_config_model(value).await,
            "reasoning_effort" => self.handle_set_config_reasoning_effort(value).await,
            _ => Err(Error::invalid_params().data("Unsupported config option")),
        }
    }

    pub(super) async fn handle_set_config_model(
        &mut self,
        value: SessionConfigValueId,
    ) -> Result<(), Error> {
        let (selected_provider, model_id) = Self::decode_provider_value(value.0.as_ref());
        if let Some(provider) = selected_provider.as_deref() {
            self.set_active_model_provider(provider)?;
        }

        let presets = self.models_manager.list_models().await;
        let preset = presets.iter().find(|p| p.id.as_str() == model_id.as_str());

        let model_to_use = preset
            .map(|p| p.model.clone())
            .unwrap_or_else(|| model_id.to_string());

        if model_to_use.is_empty() {
            return Err(Error::invalid_params().data("No model selected"));
        }

        let effort_to_use = if let Some(preset) = preset {
            if let Some(effort) = self.config.model_reasoning_effort
                && preset
                    .supported_reasoning_efforts
                    .iter()
                    .any(|e| e.effort == effort)
            {
                Some(effort)
            } else {
                Some(preset.default_reasoning_effort)
            }
        } else {
            // If the user selected a raw model string (not a known preset), don't invent a default.
            // Keep whatever was previously configured (or leave unset) so Codex can decide.
            self.config.model_reasoning_effort
        };

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

    pub(super) async fn handle_set_config_reasoning_effort(
        &mut self,
        value: SessionConfigValueId,
    ) -> Result<(), Error> {
        let effort: ReasoningEffort =
            serde_json::from_value(value.0.as_ref().into()).map_err(|_| Error::invalid_params())?;

        let current_model = self.get_current_model().await;
        let presets = self.models_manager.list_models().await;
        let Some(preset) = presets.iter().find(|p| p.model == current_model) else {
            return Err(Error::invalid_params()
                .data("Reasoning effort can only be set for known model presets"));
        };

        if !preset
            .supported_reasoning_efforts
            .iter()
            .any(|e| e.effort == effort)
        {
            return Err(
                Error::invalid_params().data("Unsupported reasoning effort for selected model")
            );
        }

        self.thread
            .submit(Op::ThreadSettings {
                thread_settings: ThreadSettingsOverrides {
                    effort: Some(Some(effort)),
                    ..Default::default()
                },
            })
            .await
            .map_err(|e| Error::from(anyhow::anyhow!(e)))?;

        self.config.model_reasoning_effort = Some(effort);

        Ok(())
    }

    pub(super) async fn models(&self) -> Result<SessionModelState, Error> {
        let mut available_models = Vec::new();
        let config_model = self.get_current_model().await;
        let provider_entries = Self::kodex_model_provider_entries();
        let mut used_provider_entries = vec![false; provider_entries.len()];
        let active_provider = self.config.model_provider_id.clone();

        let current_model_id = if let Some(model_id) = self.find_current_model().await {
            model_id
        } else {
            // If no preset found, return the current model string as-is
            let current_model = self.get_current_model().await;
            let model_id = ModelId::new(Self::encode_provider_value(
                &current_model,
                &active_provider,
            ));
            available_models.push(
                ModelInfo::new(model_id.clone(), current_model)
                    .meta(Self::model_provider_meta(&active_provider)),
            );
            model_id
        };

        available_models.extend(
            self.models_manager
                .list_models()
                .await
                .into_iter()
                .filter(|model| model.show_in_picker || model.model == config_model)
                .flat_map(|preset| {
                    let provider = Self::provider_for_preset_from_entries(
                        &provider_entries,
                        &mut used_provider_entries,
                        &preset.model,
                        &preset.display_name,
                    )
                    .unwrap_or_else(|| active_provider.clone());
                    preset
                        .supported_reasoning_efforts
                        .iter()
                        .map(|effort| {
                            ModelInfo::new(
                                Self::model_id_for_provider(&preset.id, effort.effort, &provider),
                                format!("{} ({})", preset.display_name, effort.effort),
                            )
                            .description(format!("{} {}", preset.description, effort.description))
                            .meta(Self::model_provider_meta(&provider))
                        })
                        .collect::<Vec<_>>()
                }),
        );

        Ok(SessionModelState::new(current_model_id, available_models))
    }

    pub(super) async fn collaboration_mode_for_session_mode(
        &self,
        mode_id: &str,
    ) -> CollaborationMode {
        let mode = if mode_id == "read-only" {
            ModeKind::Plan
        } else {
            ModeKind::Default
        };
        CollaborationMode {
            mode,
            settings: Settings {
                model: self.get_current_model().await,
                reasoning_effort: self.config.model_reasoning_effort,
                developer_instructions: None,
            },
        }
    }
}

/// Runtime provider id of the local BYOK proxy. All BYOK source providers are served
/// through this single proxy, which routes to the upstream by decoding the
/// `kodex-provider/byok/<source>/<model>` slug encoded in the model id.
const KODEX_BYOK_PROXY_PROVIDER_ID: &str = "byok";

/// The runtime provider a model switch should activate, or whether the current one
/// should be kept.
enum ProviderActivation {
    KeepCurrent,
    Activate(String),
}

/// Resolves the runtime provider to use after a model switch that targets
/// `requested_provider`.
///
/// BYOK source providers (e.g. "custom", "timiai") are routing labels encoded in the
/// model slug and served by the local "byok" proxy rather than standalone codex
/// providers. A model backed by such a source provider must run behind the "byok"
/// runtime provider (which decodes the source provider from the slug) regardless of
/// the provider that was active before, so switching to one re-activates "byok" when
/// it is not already current. A provider that is neither configured nor served by the
/// proxy is genuinely unsupported and yields an error.
fn provider_activation_for_request(
    requested_provider: &str,
    current_provider_id: &str,
    is_provider_configured: impl Fn(&str) -> bool,
) -> Result<ProviderActivation, Error> {
    if requested_provider == current_provider_id {
        return Ok(ProviderActivation::KeepCurrent);
    }
    if is_provider_configured(requested_provider) {
        return Ok(ProviderActivation::Activate(requested_provider.to_string()));
    }
    if is_provider_configured(KODEX_BYOK_PROXY_PROVIDER_ID) {
        return Ok(if current_provider_id == KODEX_BYOK_PROXY_PROVIDER_ID {
            ProviderActivation::KeepCurrent
        } else {
            ProviderActivation::Activate(KODEX_BYOK_PROXY_PROVIDER_ID.to_string())
        });
    }
    Err(Error::invalid_params().data(format!(
        "Unsupported provider for selected model: {requested_provider}"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    // A BYOK deployment always configures the "byok" proxy provider and may also
    // configure concrete upstream providers (e.g. "timiai") that double as source
    // providers in the model catalog.
    fn byok_configured_providers(provider: &str) -> bool {
        matches!(provider, "byok" | "timiai")
    }

    #[test]
    fn keeps_byok_runtime_when_switching_from_byok_to_source_provider() {
        // from=byok, request=custom (a BYOK source provider not in model_providers)
        assert!(matches!(
            provider_activation_for_request("custom", "byok", byok_configured_providers).unwrap(),
            ProviderActivation::KeepCurrent
        ));
    }

    #[test]
    fn routes_to_byok_proxy_when_switching_from_upstream_to_source_provider() {
        // from=timiai (upstream runtime), request=custom (source provider served by proxy)
        assert!(matches!(
            provider_activation_for_request("custom", "timiai", byok_configured_providers).unwrap(),
            ProviderActivation::Activate(ref target)
                if target == KODEX_BYOK_PROXY_PROVIDER_ID
        ));
    }

    #[test]
    fn activates_configured_upstream_provider() {
        assert!(matches!(
            provider_activation_for_request("timiai", "byok", byok_configured_providers).unwrap(),
            ProviderActivation::Activate(ref target) if target == "timiai"
        ));
    }

    #[test]
    fn keeps_current_provider_when_request_matches_current() {
        assert!(matches!(
            provider_activation_for_request("timiai", "timiai", byok_configured_providers).unwrap(),
            ProviderActivation::KeepCurrent
        ));
    }

    #[test]
    fn rejects_unsupported_provider_without_byok_proxy() {
        // No "byok" proxy configured, and the requested provider is unknown.
        let configured = |provider: &str| provider == "openai";
        assert!(provider_activation_for_request("custom", "openai", configured).is_err());
    }
}
