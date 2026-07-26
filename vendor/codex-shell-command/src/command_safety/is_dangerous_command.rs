use crate::bash::parse_shell_lc_plain_commands;
use std::path::Path;
#[cfg(windows)]
#[path = "windows_dangerous_commands.rs"]
mod windows_dangerous_commands;

pub fn command_might_be_dangerous(command: &[String]) -> bool {
    #[cfg(windows)]
    {
        if windows_dangerous_commands::is_dangerous_command_windows(command) {
            return true;
        }
    }

    if is_dangerous_to_call_with_exec(command) {
        return true;
    }

    // Support `bash -lc "<script>"` where the any part of the script might contain a dangerous command.
    if let Some(all_commands) = parse_shell_lc_plain_commands(command)
        && all_commands
            .iter()
        .any(|cmd| is_dangerous_to_call_with_exec(cmd))
    {
        return true;
    }

    // Route shell commands that directly mutate files — and scripted
    // interpreters reading a stdin heredoc, whose body is not present in
    // the parsed argv — to the ACP host's permission broker. Otherwise the
    // sandbox auto-approves in-workspace shell writes, bypassing the host's
    // apply_patch guidance for reviewable patches. The broker re-evaluates
    // the full command text and either redirects to apply_patch, prompts, or
    // auto-allows non-mutating commands.
    if command_routes_shell_file_write_to_broker(command) {
        return true;
    }

    false
}

/// Returns whether already-tokenized PowerShell words should be treated as
/// dangerous by the Windows unmatched-command heuristics.
pub fn is_dangerous_powershell_words(command: &[String]) -> bool {
    #[cfg(windows)]
    {
        windows_dangerous_commands::is_dangerous_powershell_words(command)
    }

    #[cfg(not(windows))]
    {
        let _ = command;
        false
    }
}

fn is_git_global_option_with_value(arg: &str) -> bool {
    matches!(
        arg,
        "-C" | "-c"
            | "--config-env"
            | "--exec-path"
            | "--git-dir"
            | "--namespace"
            | "--super-prefix"
            | "--work-tree"
    )
}

fn is_git_global_option_with_inline_value(arg: &str) -> bool {
    matches!(
        arg,
        s if s.starts_with("--config-env=")
            || s.starts_with("--exec-path=")
            || s.starts_with("--git-dir=")
            || s.starts_with("--namespace=")
            || s.starts_with("--super-prefix=")
            || s.starts_with("--work-tree=")
    ) || ((arg.starts_with("-C") || arg.starts_with("-c")) && arg.len() > 2)
}

pub(crate) fn executable_name_lookup_key(raw: &str) -> Option<String> {
    #[cfg(windows)]
    {
        Path::new(raw)
            .file_name()
            .and_then(|name| name.to_str())
            .map(|name| {
                let name = name.to_ascii_lowercase();
                for suffix in [".exe", ".cmd", ".bat", ".com"] {
                    if let Some(stripped) = name.strip_suffix(suffix) {
                        return stripped.to_string();
                    }
                }
                name
            })
    }

    #[cfg(not(windows))]
    {
        Path::new(raw)
            .file_name()
            .and_then(|name| name.to_str())
            .map(std::borrow::ToOwned::to_owned)
    }
}

/// Find the first matching git subcommand, skipping known global options that
/// may appear before it (e.g., `-C`, `-c`, `--git-dir`).
///
/// Shared with `is_safe_command` to avoid git-global-option bypasses.
pub(crate) fn find_git_subcommand<'a>(
    command: &'a [String],
    subcommands: &[&str],
) -> Option<(usize, &'a str)> {
    let cmd0 = command.first().map(String::as_str)?;
    if executable_name_lookup_key(cmd0).as_deref() != Some("git") {
        return None;
    }

    let mut skip_next = false;
    for (idx, arg) in command.iter().enumerate().skip(1) {
        if skip_next {
            skip_next = false;
            continue;
        }

        let arg = arg.as_str();

        if is_git_global_option_with_inline_value(arg) {
            continue;
        }

        if is_git_global_option_with_value(arg) {
            skip_next = true;
            continue;
        }

        if arg == "--" || arg.starts_with('-') {
            continue;
        }

        if subcommands.contains(&arg) {
            return Some((idx, arg));
        }

        // In git, the first non-option token is the subcommand. If it isn't
        // one of the subcommands we're looking for, we must stop scanning to
        // avoid misclassifying later positional args (e.g., branch names).
        return None;
    }

    None
}

fn is_dangerous_to_call_with_exec(command: &[String]) -> bool {
    let cmd0 = command.first().map(String::as_str);

    match cmd0 {
        Some("rm") => matches!(command.get(1).map(String::as_str), Some("-f" | "-rf")),

        // for sudo <cmd> simply do the check for <cmd>
        Some("sudo") => is_dangerous_to_call_with_exec(&command[1..]),

        // ── anything else ─────────────────────────────────────────────────
        _ => false,
    }
}

/// Whether a shell command should be routed to the host permission broker
/// because it appears to mutate files via shell primitives instead of the
/// apply_patch tool. This mirrors Maju's `shell_command_directly_mutates_files`
/// detection at a coarse granularity; the broker makes the final, precise
/// call (apply_patch redirect vs prompt vs auto-allow) using the full command
/// text, including heredoc bodies that are invisible to this parsed argv.
fn command_routes_shell_file_write_to_broker(command: &[String]) -> bool {
    let joined = command.join(" ");
    let lower = joined.to_ascii_lowercase();
    // apply_patch is the desired patch tool; never route it as a file write.
    if contains_command_token(&lower, "apply_patch") {
        return false;
    }
    if shell_redirection_writes_file(&joined, &lower) {
        return true;
    }
    if command_is_scripted_interpreter_writer(command) {
        return true;
    }
    if command_uses_file_mutation_primitive(command) {
        return true;
    }
    false
}

/// Detect shell `>`/`>>` redirection to a file path (not fd redirects like
/// `2>`, not `>&`, not `/dev/null`). Operates on the joined command text so
/// heredoc-script bodies delivered as a single `-lc` argv element are scanned.
fn shell_redirection_writes_file(joined: &str, lower: &str) -> bool {
    let bytes = lower.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'>' {
            index += 1;
            continue;
        }
        if index > 0 && bytes[index - 1].is_ascii_digit() {
            index += 1;
            continue;
        }
        let mut target_start = index + 1;
        if target_start < bytes.len() && bytes[target_start] == b'>' {
            target_start += 1;
        }
        let target = shell_redirection_target(joined, target_start);
        if let Some(target) = target
            && !is_null_redirection_target(&target)
            && !target.starts_with('&')
        {
            return true;
        }
        index = target_start;
    }
    false
}

fn shell_redirection_target(command: &str, start: usize) -> Option<String> {
    let bytes = command.as_bytes();
    let mut index = start;
    while index < bytes.len() && bytes[index].is_ascii_whitespace() {
        index += 1;
    }
    if index >= bytes.len() {
        return None;
    }
    let quote = bytes[index];
    if quote == b'\'' || quote == b'"' {
        let mut end = index + 1;
        while end < bytes.len() && bytes[end] != quote {
            end += 1;
        }
        return Some(command[index + 1..end].trim().to_string());
    }
    let mut end = index;
    while end < bytes.len()
        && !bytes[end].is_ascii_whitespace()
        && !matches!(bytes[end], b';' | b'|')
    {
        end += 1;
    }
    Some(command[index..end].trim().to_string()).filter(|target| !target.is_empty())
}

fn is_null_redirection_target(target: &str) -> bool {
    matches!(target.trim().to_ascii_lowercase().as_str(), "/dev/null" | "$null" | "nul" | "null")
}

/// Scripted interpreters (`python3 -`, `python -c`, `node -e`, `ruby -e`,
/// `perl -e`) are the primary vector for bypassing reviewable patches: the
/// agent can write files via `Path(...).write_text(...)` inside an inline
/// script or a stdin heredoc. The heredoc body is stripped from the parsed
/// argv, so we cannot inspect it here — route the whole command to the
/// broker, which sees the full text and decides.
fn command_is_scripted_interpreter_writer(command: &[String]) -> bool {
    let Some(cmd0) = command.first().map(String::as_str) else {
        return false;
    };
    let basename = cmd0.rsplit(['/', '\\']).next().unwrap_or(cmd0);
    if !matches!(
        basename,
        "python" | "python3" | "python2" | "py" | "node" | "node.exe" | "ruby" | "perl" | "php"
    ) {
        return false;
    }
    command
        .iter()
        .skip(1)
        .any(|arg| matches!(arg.as_str(), "-" | "-c" | "-e" | "--eval"))
}

/// File-mutation primitives invoked as the command (after a possible `sudo`).
fn command_uses_file_mutation_primitive(command: &[String]) -> bool {
    let words: Vec<&str> = command.iter().map(String::as_str).collect();
    let mut start = 0;
    if words.first() == Some(&"sudo") && words.len() > 1 {
        start = 1;
    }
    let Some(cmd0) = words.get(start).map(|w| w.rsplit(['/', '\\']).next().unwrap_or(w)) else {
        return false;
    };
    match cmd0 {
        "tee" | "truncate" | "touch" | "rm" | "mv" | "cp" | "dd" | "install" => true,
        "sed" => words.iter().any(|w| w.starts_with("-i")),
        "perl" => words
            .iter()
            .any(|w| w.starts_with("-pi") || w.starts_with("-pie")),
        _ => false,
    }
}

/// Word-boundary token check so `apply_patch` does not match inside a longer
/// identifier such as `my_apply_patch_helper`.
fn contains_command_token(text: &str, token: &str) -> bool {
    let mut offset = 0;
    while let Some(relative) = text[offset..].find(token) {
        let index = offset + relative;
        let before = text[..index].chars().next_back();
        let after = text[index + token.len()..].chars().next();
        if !before.is_some_and(is_command_word_char) && !after.is_some_and(is_command_word_char) {
            return true;
        }
        offset = index + token.len();
    }
    false
}

fn is_command_word_char(value: char) -> bool {
    value.is_ascii_alphanumeric() || matches!(value, '_' | '-')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vec_str(items: &[&str]) -> Vec<String> {
        items.iter().map(std::string::ToString::to_string).collect()
    }

    #[test]
    fn rm_rf_is_dangerous() {
        assert!(command_might_be_dangerous(&vec_str(&["rm", "-rf", "/"])));
    }

    #[test]
    fn rm_f_is_dangerous() {
        assert!(command_might_be_dangerous(&vec_str(&["rm", "-f", "/"])));
    }

    #[test]
    fn direct_powershell_words_reuse_windows_dangerous_detection() {
        let command = vec_str(&["Remove-Item", "test", "-Force"]);

        if cfg!(windows) {
            assert!(is_dangerous_powershell_words(&command));
        } else {
            assert!(!is_dangerous_powershell_words(&command));
        }
    }
}
