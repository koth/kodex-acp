use super::*;

pub(super) static APPROVAL_PRESETS: LazyLock<Vec<ApprovalPreset>> =
    LazyLock::new(kodex_approval_presets);
pub(super) const INIT_COMMAND_PROMPT: &str = include_str!("../prompt_for_init_command.md");
const CODEX_READ_ONLY_PROFILE_ID: &str = ":read-only";
const CODEX_WORKSPACE_PROFILE_ID: &str = ":workspace";
const CODEX_DANGER_NO_SANDBOX_PROFILE_ID: &str = ":danger-no-sandbox";
pub(super) const SESSION_TITLE_MAX_CHARS: usize = 60;
pub(super) const SESSION_TITLE_GENERATION_TIMEOUT: Duration = Duration::from_secs(20);
pub(super) const SESSION_TITLE_ROLLBACK_TIMEOUT: Duration = Duration::from_secs(5);
pub(super) const SESSION_TITLE_PROMPT_MAX_CHARS: usize = 4_000;
pub(super) const KODEX_CONTEXT_COMPACTION_META_KEY: &str = "kodex.ai/contextCompaction";
pub(super) const KODEX_CONTEXT_COMPACTED_META_KEY: &str = "kodex.ai/contextCompacted";
pub(super) const KODEX_PERMISSION_GUIDANCE_META_KEY: &str = "kodex.ai/permissionGuidance";
pub(super) const KODEX_PERMISSION_INPUT_META_KEY: &str = "kodex.ai/permissionInput";
pub(super) const KODEX_USER_INPUT_ANSWERS_META_KEY: &str = "kodex.ai/userInputAnswers";
pub(super) const KODEX_MODEL_PROVIDER_MAP_ENV: &str = "KODEX_MODEL_PROVIDER_MAP";
pub(super) const KODEX_PROVIDER_VALUE_PREFIX: &str = "kodex-provider:";
pub(super) const SESSION_TITLE_INSTRUCTIONS: &str = r#"Generate a concise title for this coding session.

Rules:
- Return only the title text.
- Return the title in Simplified Chinese, even if the user wrote in another language.
- Use a short Chinese phrase, roughly 6 to 12 Chinese characters when possible.
- Do not quote the title.
- Do not copy the user's request verbatim.
- Keep technical identifiers like API, ACP, session/list, and file names unchanged when clearer."#;

fn kodex_approval_presets() -> Vec<ApprovalPreset> {
    let mut presets = builtin_approval_presets();
    for preset in &mut presets {
        if preset.id == "auto" {
            preset.description = "Kodex can read and edit files in the current workspace, access the internet, and run commands. Approval is required to edit files outside the workspace.";
            preset.permission_profile = PermissionProfile::workspace_write_with(
                &[],
                NetworkSandboxPolicy::Enabled,
                /*exclude_tmpdir_env_var*/ false,
                /*exclude_slash_tmp*/ false,
            );
        }
    }
    presets
}

#[derive(Debug, Clone, Deserialize)]
pub(super) struct KodexModelProviderEntry {
    pub(super) model: String,
    #[serde(default)]
    pub(super) display_name: String,
    pub(super) provider: String,
}

pub(super) fn session_mode_id_for_active_profile(profile_id: &str) -> Option<&'static str> {
    match profile_id {
        CODEX_READ_ONLY_PROFILE_ID => Some("read-only"),
        CODEX_WORKSPACE_PROFILE_ID => Some("auto"),
        CODEX_DANGER_NO_SANDBOX_PROFILE_ID => Some("full-access"),
        _ => None,
    }
}

pub(super) fn active_profile_id_for_session_mode(mode_id: &str) -> Option<&'static str> {
    match mode_id {
        "read-only" => Some(CODEX_READ_ONLY_PROFILE_ID),
        "auto" => Some(CODEX_WORKSPACE_PROFILE_ID),
        "full-access" => Some(CODEX_DANGER_NO_SANDBOX_PROFILE_ID),
        _ => None,
    }
}

fn approval_matches_current_config(preset: &ApprovalPreset, config: &Config) -> bool {
    std::mem::discriminant(&preset.approval)
        == std::mem::discriminant(config.permissions.approval_policy.get())
}

fn mode_id_if_approval_matches(mode_id: &'static str, config: &Config) -> Option<SessionModeId> {
    APPROVAL_PRESETS
        .iter()
        .find(|preset| preset.id == mode_id && approval_matches_current_config(preset, config))
        .map(|preset| SessionModeId::new(preset.id))
}

fn untrusted_read_only_mode_id(config: &Config) -> Option<SessionModeId> {
    // When the project is untrusted, the approval policy won't match since
    // AskForApproval::UnlessTrusted is not part of the default presets.
    // However, we still want to show the mode selector, which allows the user
    // to choose a different mode and trust the project.
    config
        .active_project
        .is_untrusted()
        .then(|| SessionModeId::new("read-only"))
}

fn semantic_session_mode_id_for_permission_profile(config: &Config) -> Option<&'static str> {
    let permission_profile = config.permissions.permission_profile();

    match permission_profile {
        PermissionProfile::Managed { .. } => {
            let workspace_preset = APPROVAL_PRESETS.iter().find(|preset| preset.id == "auto")?;
            let network_policy = permission_profile.network_sandbox_policy();
            let legacy_workspace_network_policy =
                PermissionProfile::workspace_write().network_sandbox_policy();
            let is_workspace_network_policy = network_policy
                == workspace_preset.permission_profile.network_sandbox_policy()
                || network_policy == legacy_workspace_network_policy;
            if !is_workspace_network_policy {
                return None;
            }

            let file_system = permission_profile.file_system_sandbox_policy();
            let cwd = config.cwd.as_path();
            if file_system.has_full_disk_read_access()
                && !file_system.has_full_disk_write_access()
                && file_system.can_write_path_with_cwd(cwd, cwd)
            {
                Some("auto")
            } else {
                None
            }
        }
        PermissionProfile::Disabled => Some("full-access"),
        PermissionProfile::External { .. } => None,
    }
}

pub(super) fn current_session_mode_id(config: &Config) -> Option<SessionModeId> {
    if let Some(active_profile) = config.permissions.active_permission_profile().as_ref() {
        return session_mode_id_for_active_profile(&active_profile.id)
            .and_then(|mode_id| mode_id_if_approval_matches(mode_id, config))
            .or_else(|| untrusted_read_only_mode_id(config));
    }

    if let Some(preset) = APPROVAL_PRESETS.iter().find(|preset| {
        approval_matches_current_config(preset, config)
            && preset.permission_profile == *config.permissions.permission_profile()
    }) {
        return Some(SessionModeId::new(preset.id));
    }

    semantic_session_mode_id_for_permission_profile(config)
        .and_then(|mode_id| mode_id_if_approval_matches(mode_id, config))
        .or_else(|| untrusted_read_only_mode_id(config))
}

pub(super) fn mode_trusts_project(mode_id: &str) -> bool {
    matches!(mode_id, "auto" | "full-access")
}
