use super::*;

pub(super) fn build_prompt_items(prompt: Vec<ContentBlock>) -> Vec<UserInput> {
    prompt
        .into_iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text_block) => Some(UserInput::Text {
                text: text_block.text,
                text_elements: vec![],
            }),
            ContentBlock::Image(image_block) => Some(UserInput::Image {
                image_url: format!("data:{};base64,{}", image_block.mime_type, image_block.data),
                detail: None,
            }),
            ContentBlock::ResourceLink(ResourceLink { name, uri, .. }) => Some(UserInput::Text {
                text: format_uri_as_link(Some(name), uri),
                text_elements: vec![],
            }),
            ContentBlock::Resource(EmbeddedResource {
                resource:
                    EmbeddedResourceResource::TextResourceContents(TextResourceContents {
                        text,
                        uri,
                        ..
                    }),
                ..
            }) => Some(UserInput::Text {
                text: format!(
                    "{}\n<context ref=\"{uri}\">\n{text}\n</context>",
                    format_uri_as_link(None, uri.clone())
                ),
                text_elements: vec![],
            }),
            // Skip other content types for now
            ContentBlock::Audio(..) | ContentBlock::Resource(..) | _ => None,
        })
        .collect()
}

pub(super) fn format_uri_as_link(name: Option<String>, uri: String) -> String {
    if let Some(name) = name
        && !name.is_empty()
    {
        format!("[@{name}]({uri})")
    } else if let Some(path) = uri.strip_prefix("file://") {
        let name = path.split('/').next_back().unwrap_or(path);
        format!("[@{name}]({uri})")
    } else if uri.starts_with("zed://") {
        let name = uri.split('/').next_back().unwrap_or(&uri);
        format!("[@{name}]({uri})")
    } else {
        uri
    }
}

pub(super) fn extract_tool_call_content_from_changes(
    changes: HashMap<PathBuf, FileChange>,
) -> (
    String,
    Vec<ToolCallLocation>,
    impl Iterator<Item = ToolCallContent>,
) {
    let changes = changes.into_iter().collect_vec();
    let title = if changes.is_empty() {
        "Edit".to_string()
    } else {
        format!(
            "Edit {}",
            changes
                .iter()
                .map(|(path, change)| tool_call_location_for_change(path, change)
                    .display()
                    .to_string())
                .join(", ")
        )
    };
    let locations = changes
        .iter()
        .map(|(path, change)| ToolCallLocation::new(tool_call_location_for_change(path, change)))
        .collect_vec();
    let content = changes
        .into_iter()
        .flat_map(|(path, change)| extract_tool_call_content_from_change(path, change));

    (title, locations, content)
}

fn tool_call_location_for_change(path: &Path, change: &FileChange) -> PathBuf {
    match change {
        FileChange::Update {
            move_path: Some(move_path),
            ..
        } => move_path.clone(),
        _ => path.to_path_buf(),
    }
}

fn extract_tool_call_content_from_change(
    path: PathBuf,
    change: FileChange,
) -> Vec<ToolCallContent> {
    match change {
        FileChange::Add { content } => vec![ToolCallContent::Diff(Diff::new(path, content))],
        FileChange::Delete { content } => {
            vec![ToolCallContent::Diff(
                Diff::new(path, String::new()).old_text(content),
            )]
        }
        FileChange::Update {
            unified_diff,
            move_path,
        } => extract_tool_call_content_from_unified_diff(move_path.unwrap_or(path), unified_diff),
    }
}

fn extract_tool_call_content_from_unified_diff(
    path: PathBuf,
    unified_diff: String,
) -> Vec<ToolCallContent> {
    let Ok(patch) = diffy::Patch::from_str(&unified_diff) else {
        return vec![ToolCallContent::Content(Content::new(ContentBlock::Text(
            TextContent::new(unified_diff),
        )))];
    };

    let diffs = patch
        .hunks()
        .iter()
        .map(|hunk| {
            let mut old_text = String::new();
            let mut new_text = String::new();

            for line in hunk.lines() {
                match line {
                    diffy::Line::Context(text) => {
                        old_text.push_str(text);
                        new_text.push_str(text);
                    }
                    diffy::Line::Delete(text) => old_text.push_str(text),
                    diffy::Line::Insert(text) => new_text.push_str(text),
                }
            }

            ToolCallContent::Diff(Diff::new(path.clone(), new_text).old_text(old_text))
        })
        .collect_vec();

    if diffs.is_empty() {
        vec![ToolCallContent::Content(Content::new(ContentBlock::Text(
            TextContent::new(unified_diff),
        )))]
    } else {
        diffs
    }
}

pub(super) fn guardian_assessment_tool_call_id(id: &str) -> String {
    format!("guardian_assessment:{id}")
}

pub(super) fn guardian_assessment_tool_call_status(
    status: &GuardianAssessmentStatus,
) -> ToolCallStatus {
    match status {
        GuardianAssessmentStatus::InProgress => ToolCallStatus::InProgress,
        GuardianAssessmentStatus::Approved => ToolCallStatus::Completed,
        GuardianAssessmentStatus::Denied
        | GuardianAssessmentStatus::Aborted
        | GuardianAssessmentStatus::TimedOut => ToolCallStatus::Failed,
    }
}

pub(super) fn guardian_assessment_content(event: &GuardianAssessmentEvent) -> Vec<ToolCallContent> {
    let mut lines = vec![format!(
        "Status: {}",
        match event.status {
            GuardianAssessmentStatus::InProgress => "In progress",
            GuardianAssessmentStatus::Approved => "Approved",
            GuardianAssessmentStatus::Denied => "Denied",
            GuardianAssessmentStatus::Aborted => "Aborted",
            GuardianAssessmentStatus::TimedOut => "Timed out",
        }
    )];

    if let Some(summary) = guardian_action_summary(&event.action) {
        lines.push(format!("Action: {summary}"));
    }

    if let Some(level) = event.risk_level {
        lines.push(format!("Risk: {}", format!("{level:?}").to_lowercase()));
    }

    if let Some(rationale) = event.rationale.as_ref()
        && !rationale.trim().is_empty()
    {
        lines.push(format!("Rationale: {rationale}"));
    }

    let content = vec![ToolCallContent::Content(Content::new(ContentBlock::Text(
        TextContent::new(lines.join("\n")),
    )))];

    content
}

pub(super) fn guardian_action_summary(action: &GuardianAssessmentAction) -> Option<String> {
    match action {
        GuardianAssessmentAction::Command {
            source,
            command,
            cwd: _,
        } => {
            let label = guardian_command_source_label(source);
            Some(format!("{label} {command}"))
        }
        GuardianAssessmentAction::Execve {
            source,
            program,
            argv,
            cwd: _,
        } => {
            let label = guardian_command_source_label(source);
            let command: Vec<&str> = if argv.is_empty() {
                vec![program.as_str()]
            } else {
                argv.iter().map(String::as_str).collect()
            };
            let joined = shlex::try_join(command.iter().copied())
                .ok()
                .unwrap_or_else(|| command.join(" "));
            Some(format!("{label} {joined}"))
        }
        GuardianAssessmentAction::ApplyPatch { files, cwd: _ } => Some(if files.len() == 1 {
            format!("apply_patch touching {}", files[0].display())
        } else {
            format!("apply_patch touching {} files", files.len())
        }),
        GuardianAssessmentAction::NetworkAccess { target, host, .. } => {
            let label = if target.is_empty() { host } else { target };
            Some(format!("network access to {label}"))
        }
        GuardianAssessmentAction::McpToolCall {
            server,
            tool_name,
            connector_name,
            ..
        } => {
            let label = connector_name.as_deref().unwrap_or(server.as_str());
            Some(format!("MCP {tool_name} on {label}"))
        }
        GuardianAssessmentAction::RequestPermissions { reason, .. } => Some(
            reason
                .clone()
                .unwrap_or_else(|| "request additional permissions".to_string()),
        ),
    }
}

fn guardian_command_source_label(source: &GuardianCommandSource) -> &'static str {
    match source {
        GuardianCommandSource::Shell => "shell",
        GuardianCommandSource::UnifiedExec => "exec",
    }
}

pub(super) fn format_file_system_entries<'a>(
    entries: impl Iterator<Item = &'a FileSystemSandboxEntry>,
) -> String {
    entries
        .map(format_file_system_entry)
        .collect::<Vec<_>>()
        .join(", ")
}

fn format_file_system_entry(entry: &FileSystemSandboxEntry) -> String {
    match &entry.path {
        FileSystemPath::Path { path } => path.display().to_string(),
        FileSystemPath::GlobPattern { pattern } => format!("glob `{pattern}`"),
        FileSystemPath::Special { value } => format_file_system_special(value),
    }
}

fn format_file_system_special(value: &FileSystemSpecialPath) -> String {
    match value {
        FileSystemSpecialPath::Root => ":root".to_string(),
        FileSystemSpecialPath::Minimal => ":minimal".to_string(),
        FileSystemSpecialPath::ProjectRoots { subpath } => {
            format_file_system_subpath(":project_roots", subpath.as_deref())
        }
        FileSystemSpecialPath::Tmpdir => ":tmpdir".to_string(),
        FileSystemSpecialPath::SlashTmp => "/tmp".to_string(),
        FileSystemSpecialPath::Unknown { path, subpath } => {
            format_file_system_subpath(path, subpath.as_deref())
        }
    }
}

fn format_file_system_subpath(base: &str, subpath: Option<&Path>) -> String {
    match subpath {
        Some(subpath) => format!("{base}/{}", subpath.display()),
        None => base.to_string(),
    }
}

/// Extract title and call_id from a WebSearchAction (used for replay)
pub(super) fn web_search_action_to_title_and_id(
    id: &Option<String>,
    action: &codex_protocol::models::WebSearchAction,
) -> (String, String) {
    match action {
        codex_protocol::models::WebSearchAction::Search { query, queries } => {
            let title = queries
                .as_ref()
                .map(|q| q.join(", "))
                .or_else(|| query.clone())
                .unwrap_or_else(|| "Web search".to_string());
            let call_id = id
                .clone()
                .unwrap_or_else(|| generate_fallback_id("web_search"));
            (title, call_id)
        }
        codex_protocol::models::WebSearchAction::OpenPage { url } => {
            let title = url.clone().unwrap_or_else(|| "Open page".to_string());
            let call_id = id
                .clone()
                .unwrap_or_else(|| generate_fallback_id("web_open"));
            (title, call_id)
        }
        codex_protocol::models::WebSearchAction::FindInPage { pattern, .. } => {
            let title = pattern
                .clone()
                .unwrap_or_else(|| "Find in page".to_string());
            let call_id = id
                .clone()
                .unwrap_or_else(|| generate_fallback_id("web_find"));
            (title, call_id)
        }
        codex_protocol::models::WebSearchAction::Other => {
            ("Unknown".to_string(), generate_fallback_id("web_search"))
        }
    }
}

pub(super) fn image_generation_tool_status(status: &str) -> ToolCallStatus {
    match status {
        "completed" => ToolCallStatus::Completed,
        "generating" | "in_progress" | "incomplete" => ToolCallStatus::InProgress,
        "failed" => ToolCallStatus::Failed,
        _ => ToolCallStatus::Completed,
    }
}

pub(super) fn image_generation_content(
    revised_prompt: Option<String>,
    result: String,
    saved_path: Option<String>,
) -> Vec<ToolCallContent> {
    let mut content = Vec::new();

    if let Some(revised_prompt) = revised_prompt.filter(|prompt| !prompt.trim().is_empty()) {
        content.push(ToolCallContent::Content(Content::new(ContentBlock::Text(
            TextContent::new(format!("Revised prompt: {revised_prompt}")),
        ))));
    }

    if !result.is_empty() {
        let mut image = ImageContent::new(result, "image/png");
        if let Some(saved_path) = saved_path
            .as_ref()
            .filter(|saved_path| !saved_path.trim().is_empty())
        {
            image = image.uri(saved_path.clone());
        }

        content.push(ToolCallContent::Content(Content::new(ContentBlock::Image(
            image,
        ))));
    }

    content
}

/// Generate a fallback ID using UUID (used when id is missing)
pub(super) fn generate_fallback_id(prefix: &str) -> String {
    format!("{}_{}", prefix, Uuid::new_v4())
}

/// Checks if a prompt is slash command
pub(super) fn extract_slash_command(content: &[UserInput]) -> Option<(&str, &str)> {
    let line = content.first().and_then(|block| match block {
        UserInput::Text { text, .. } => Some(text),
        _ => None,
    })?;
    // Parse a first-line slash command of the form `/name <rest>`.
    // Returns `(name, rest_after_name)` if the line begins with `/` and contains
    // a non-empty name; otherwise returns `None`.
    let stripped = line.strip_prefix('/')?;
    let mut name_end = stripped.len();
    for (idx, ch) in stripped.char_indices() {
        if ch.is_whitespace() {
            name_end = idx;
            break;
        }
    }
    let name = &stripped[..name_end];
    if name.is_empty() {
        return None;
    }
    let rest = stripped[name_end..].trim_start();
    Some((name, rest))
}

pub(super) fn is_commentary_phase(phase: Option<&MessagePhase>) -> bool {
    matches!(phase, Some(MessagePhase::Commentary))
}
