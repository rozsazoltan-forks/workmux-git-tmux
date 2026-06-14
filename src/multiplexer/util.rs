//! Backend-agnostic utility functions for multiplexer operations.
//!
//! These helpers are shared between tmux, WezTerm, and any future backends.

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Result;

use super::agent::SelectedAgent;
use crate::config::Config;

use crate::cmd::Cmd;

use super::PaneHandshake;
use super::handshake::UnixPipeHandshake;
use super::types::LivePaneInfo;

/// Helper function to add prefix to window name.
///
/// Used by all backends to construct full window names from prefix and base name.
pub fn prefixed(prefix: &str, window_name: &str) -> String {
    format!("{}{}", prefix, window_name)
}

/// Check if a shell is POSIX-compatible (supports `$(...)` syntax).
///
/// Used to determine whether agent commands need to be wrapped in `sh -c '...'`
/// for shells like nushell or fish that don't support POSIX command substitution.
pub fn is_posix_shell(shell: &str) -> bool {
    let shell_name = Path::new(shell)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("sh");
    matches!(shell_name, "bash" | "zsh" | "sh" | "dash" | "ksh" | "ash")
}

/// Return the last `lines` lines from terminal output.
pub fn tail_lines(output: &str, lines: u16) -> String {
    let all_lines: Vec<&str> = output.lines().collect();
    let start = all_lines.len().saturating_sub(lines as usize);
    all_lines[start..].join("\n")
}

/// Build a `LivePaneInfo` from shared pane fields.
pub fn build_live_pane_info(
    pid: Option<u32>,
    current_command: Option<String>,
    working_dir: PathBuf,
    title: &str,
    session: String,
    window: String,
) -> LivePaneInfo {
    LivePaneInfo {
        pid,
        current_command,
        working_dir,
        title: if title.is_empty() {
            None
        } else {
            Some(title.to_string())
        },
        session: Some(session),
        window: Some(window),
    }
}

/// Snapshot of live pane fields before conversion to `LivePaneInfo`.
pub struct LivePaneSnapshot {
    pub pane_id: String,
    pub pid: Option<u32>,
    pub current_command: Option<String>,
    pub working_dir: PathBuf,
    pub title: String,
    pub session: String,
    pub window: String,
}

impl LivePaneSnapshot {
    pub fn into_pair(self) -> (String, LivePaneInfo) {
        let pane_id = self.pane_id;
        let info = build_live_pane_info(
            self.pid,
            self.current_command,
            self.working_dir,
            &self.title,
            self.session,
            self.window,
        );
        (pane_id, info)
    }
}

/// Build a pane ID map from live pane snapshots.
pub fn live_pane_map<I>(snapshots: I) -> HashMap<String, LivePaneInfo>
where
    I: IntoIterator<Item = LivePaneSnapshot>,
{
    snapshots
        .into_iter()
        .map(LivePaneSnapshot::into_pair)
        .collect()
}

/// Resolve the default shell from `$SHELL`, falling back when unset.
pub fn default_shell(fallback: &str) -> Result<String> {
    std::env::var("SHELL").or_else(|_| Ok(fallback.to_string()))
}

/// Create a Unix pipe handshake for shell startup synchronization.
pub fn unix_pipe_handshake() -> Result<Box<dyn PaneHandshake>> {
    Ok(Box::new(UnixPipeHandshake::new()?))
}

/// Run a shell script detached via `nohup sh -c`.
pub fn run_detached_sh_c(script: &str) -> Result<()> {
    let bg_script = format!("nohup sh -c '{}' >/dev/null 2>&1 &", script);
    Cmd::new("sh").args(&["-c", &bg_script]).run().map(|_| ())
}

/// Resolve a pane's command: handle `<agent>` placeholder, auto-detect known
/// agents, and adjust for prompt injection.
///
/// Returns the final command to send to the pane, or None if no command should be sent.
/// This consolidates the duplicated command resolution logic from both backends' setup_panes.
/// Result of resolving a pane command.
pub struct ResolvedCommand {
    /// The command string to send to the pane.
    pub command: String,
    /// Whether the command was rewritten to inject a prompt (needs auto-status).
    pub prompt_injected: bool,
    /// Selected agent metadata for agent panes.
    pub selected_agent: Option<SelectedAgent>,
}

pub fn resolve_pane_command_with_config(
    pane_command: Option<&str>,
    run_commands: bool,
    prompt_file_path: Option<&Path>,
    working_dir: &Path,
    config: &Config,
    task_agent: Option<&str>,
    shell: &str,
) -> Option<ResolvedCommand> {
    let raw_command = pane_command?;
    if !run_commands {
        return None;
    }

    let default_agent = super::agent::resolve_selected_agent(config, task_agent);
    let mut selected_agent = None;
    let command = if raw_command == "<agent>" {
        let agent = default_agent?;
        let command = agent.shell_command();
        selected_agent = Some(agent);
        command
    } else if let Some(name) = raw_command
        .strip_prefix("<agent:")
        .and_then(|rest| rest.strip_suffix('>'))
    {
        let agent = super::agent::resolve_selected_agent(config, Some(name))?;
        let command = agent.shell_command();
        selected_agent = Some(agent);
        command
    } else if super::agent::is_known_agent(raw_command) {
        selected_agent = super::agent::SelectedAgent::from_raw(raw_command);
        raw_command.to_string()
    } else if default_agent.as_ref().is_some_and(|agent| {
        crate::config::is_agent_command(raw_command, &agent.shell_command())
            || crate::config::is_agent_command(raw_command, agent.kind())
    }) {
        selected_agent = default_agent;
        raw_command.to_string()
    } else {
        raw_command.to_string()
    };

    let selected_ref = selected_agent.as_ref();
    let result =
        adjust_selected_command(&command, prompt_file_path, working_dir, selected_ref, shell);
    let prompt_injected = matches!(result, Cow::Owned(_));
    Some(ResolvedCommand {
        command: result.into_owned(),
        prompt_injected,
        selected_agent,
    })
}

fn adjust_selected_command<'a>(
    command: &'a str,
    prompt_file_path: Option<&Path>,
    working_dir: &Path,
    selected_agent: Option<&SelectedAgent>,
    shell: &str,
) -> Cow<'a, str> {
    if let Some(agent) = selected_agent {
        if let Some(prompt_path) = prompt_file_path {
            let relative = prompt_path.strip_prefix(working_dir).unwrap_or(prompt_path);
            let prompt_path = relative.to_string_lossy();
            let mut inner_cmd = command.to_string();
            if let Some(subcmd) = agent.profile.default_subcommand()
                && needs_default_subcommand(&agent.command.args.join(" "), subcmd)
            {
                let command_text = agent.command.shell_string();
                inner_cmd = inject_flag_after_agent_executable(&command_text, subcmd);
            }
            inner_cmd.push(' ');
            inner_cmd.push_str(&agent.profile.prompt_argument(&prompt_path));
            return if is_posix_shell(shell) {
                Cow::Owned(format!(" {}", inner_cmd))
            } else {
                Cow::Owned(format!(" {}", wrap_for_non_posix_shell(&inner_cmd)))
            };
        }
        if let Some(subcmd) = agent.profile.default_subcommand()
            && needs_default_subcommand(&agent.command.args.join(" "), subcmd)
        {
            return Cow::Owned(inject_flag_after_agent_executable(command, subcmd));
        }
    }
    Cow::Borrowed(command)
}

/// Check whether a default subcommand needs to be inserted.
///
/// Returns `true` when the user's args don't already start with the
/// subcommand (e.g., "chat"). Flags like `--verbose` are not subcommands,
/// so the default is still inserted before them.
fn needs_default_subcommand(rest: &str, subcmd: &str) -> bool {
    match rest.split_whitespace().next() {
        None => true,                                  // no args at all
        Some(first) if first == subcmd => false,       // already has it
        Some(first) if first.starts_with('-') => true, // flag, not a subcommand
        Some(_) => false,                              // some other subcommand
    }
}

/// Escape a string for embedding inside a double-quoted shell context.
///
/// Escapes: backslash, double quote, dollar sign, backtick.
/// Does NOT add surrounding quotes - caller controls the quoting.
///
/// Example: `$HOME` -> `\$HOME`
pub fn escape_for_double_quotes(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('$', "\\$")
        .replace('`', "\\`")
}

/// Escape a command to be safely embedded inside `sh -c "..."`.
///
/// This handles the two-step nesting complexity:
/// 1. Inner single-quoted context (for paths/args inside the command)
/// 2. Outer double-quoted context (for the sh -c wrapper)
///
/// Use when you need to pass a value that will be single-quoted inside
/// a double-quoted sh -c command.
///
/// Example: `/bin/user's shell` inside `sh -c "exec '/bin/user's shell'"`:
/// - Step 1: `'\''` escaping -> `/bin/user'\''s shell`
/// - Step 2: double-quote escaping -> `/bin/user'\''s shell` (no change here)
pub fn escape_for_sh_c_inner_single_quote(s: &str) -> String {
    let single_escaped = s.replace('\'', "'\\''");
    escape_for_double_quotes(&single_escaped)
}

/// Wrap a command in `sh -c '...'` for execution in non-POSIX shells.
///
/// Used when the default shell (nushell, fish, etc.) doesn't support
/// POSIX command substitution like `$(...)`.
pub fn wrap_for_non_posix_shell(command: &str) -> String {
    let escaped = command.replace('\'', "'\\''");
    format!("sh -c '{}'", escaped)
}

/// Inject a permissions flag into an agent command string.
///
/// Inserts the flag after the real agent executable, looking past `env`
/// wrappers and `VAR=value` assignments.
/// For commands like ` claude -- "$(cat PROMPT.md)"`, produces
/// ` claude --dangerously-skip-permissions -- "$(cat PROMPT.md)"`.
///
/// For non-POSIX wrapped commands like ` sh -c 'claude -- ...'`, the flag
/// is inserted inside the inner command.
pub fn inject_skip_permissions_flag(command: &str, flag: &str) -> String {
    // Handle the leading space (history prevention prefix)
    let trimmed = command.trim_start();
    let leading_spaces = &command[..command.len() - trimmed.len()];

    // Handle sh -c wrapper (non-POSIX shells)
    if trimmed.starts_with("sh -c '") && trimmed.ends_with('\'') {
        let inner = &trimmed[7..trimmed.len() - 1];
        let inner_unescaped = inner.replace("'\\''", "'");
        let injected = inject_flag_after_agent_executable(&inner_unescaped, flag);
        let re_escaped = injected.replace('\'', "'\\''");
        return format!("{}sh -c '{}'", leading_spaces, re_escaped);
    }

    format!(
        "{}{}",
        leading_spaces,
        inject_flag_after_agent_executable(trimmed, flag)
    )
}

/// Insert a flag after the real agent executable in a command,
/// handling `env` wrappers and `VAR=value` assignments.
fn inject_flag_after_agent_executable(command: &str, flag: &str) -> String {
    let exe_token = super::agent::find_executable_token(command);
    if exe_token.is_empty() {
        return format!("{} {}", command, flag);
    }

    // Use pointer arithmetic to find the token's position in the original string
    let exe_start = exe_token.as_ptr() as usize - command.as_ptr() as usize;
    let exe_end = exe_start + exe_token.len();

    let before = &command[..exe_end];
    let after = &command[exe_end..];

    if after.is_empty() {
        format!("{} {}", before, flag)
    } else {
        format!("{} {}{}", before, flag, after)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    // --- prefixed tests ---

    #[test]
    fn test_tail_lines() {
        assert_eq!(tail_lines("one\ntwo\nthree", 2), "two\nthree");
        assert_eq!(tail_lines("one\ntwo\nthree", 10), "one\ntwo\nthree");
        assert_eq!(tail_lines("", 5), "");
    }

    #[test]
    fn test_prefixed() {
        assert_eq!(prefixed("wm-", "feature"), "wm-feature");
        assert_eq!(prefixed("", "feature"), "feature");
        assert_eq!(prefixed("prefix-", ""), "prefix-");
    }

    // --- is_posix_shell tests ---

    #[test]
    fn test_is_posix_shell_bash() {
        assert!(is_posix_shell("/bin/bash"));
        assert!(is_posix_shell("/usr/bin/bash"));
    }

    #[test]
    fn test_is_posix_shell_zsh() {
        assert!(is_posix_shell("/bin/zsh"));
        assert!(is_posix_shell("/usr/local/bin/zsh"));
    }

    #[test]
    fn test_is_posix_shell_sh() {
        assert!(is_posix_shell("/bin/sh"));
    }

    #[test]
    fn test_is_posix_shell_nushell() {
        assert!(!is_posix_shell("/opt/homebrew/bin/nu"));
        assert!(!is_posix_shell("/usr/bin/nu"));
    }

    #[test]
    fn test_is_posix_shell_fish() {
        assert!(!is_posix_shell("/usr/bin/fish"));
        assert!(!is_posix_shell("/opt/homebrew/bin/fish"));
    }

    // --- escape_for_double_quotes tests ---

    #[test]
    fn test_escape_for_double_quotes_simple() {
        assert_eq!(escape_for_double_quotes("hello"), "hello");
        assert_eq!(escape_for_double_quotes("foo bar"), "foo bar");
    }

    #[test]
    fn test_escape_for_double_quotes_special_chars() {
        assert_eq!(escape_for_double_quotes("$HOME"), "\\$HOME");
        assert_eq!(escape_for_double_quotes("a\"b"), "a\\\"b");
        assert_eq!(escape_for_double_quotes("$(cmd)"), "\\$(cmd)");
        assert_eq!(escape_for_double_quotes("`cmd`"), "\\`cmd\\`");
    }

    #[test]
    fn test_escape_for_double_quotes_backslash() {
        assert_eq!(escape_for_double_quotes("a\\b"), "a\\\\b");
        assert_eq!(escape_for_double_quotes("\\$HOME"), "\\\\\\$HOME");
    }

    #[test]
    fn test_escape_for_double_quotes_combined() {
        // Test multiple special chars together
        assert_eq!(
            escape_for_double_quotes("echo \"$HOME\" `pwd`"),
            "echo \\\"\\$HOME\\\" \\`pwd\\`"
        );
    }

    // --- escape_for_sh_c_inner_single_quote tests ---

    #[test]
    fn test_escape_for_sh_c_inner_single_quote_simple() {
        assert_eq!(escape_for_sh_c_inner_single_quote("/bin/bash"), "/bin/bash");
    }

    #[test]
    fn test_escape_for_sh_c_inner_single_quote_with_single_quote() {
        // Shell path with single quote
        // Step 1: ' -> '\'' (single quote escaping)
        // Step 2: backslash in '\'' gets doubled for double-quote context -> '\\''
        assert_eq!(
            escape_for_sh_c_inner_single_quote("/bin/user's shell"),
            "/bin/user'\\\\''s shell"
        );
    }

    #[test]
    fn test_escape_for_sh_c_inner_single_quote_with_dollar() {
        // Dollar sign needs double-quote escaping
        assert_eq!(
            escape_for_sh_c_inner_single_quote("/path/$dir/shell"),
            "/path/\\$dir/shell"
        );
    }

    #[test]
    fn test_escape_for_sh_c_inner_single_quote_combined() {
        // Both single quote and dollar sign
        // Single quote becomes '\'' then backslash is doubled -> '\\''
        // Dollar sign becomes \$ (escaped for double quotes)
        assert_eq!(
            escape_for_sh_c_inner_single_quote("it's $HOME"),
            "it'\\\\''s \\$HOME"
        );
    }

    // --- wrap_for_non_posix_shell tests ---

    #[test]
    fn test_wrap_for_non_posix_shell_simple() {
        assert_eq!(wrap_for_non_posix_shell("echo hello"), "sh -c 'echo hello'");
    }

    #[test]
    fn test_wrap_for_non_posix_shell_with_single_quote() {
        assert_eq!(
            wrap_for_non_posix_shell("echo 'quoted'"),
            "sh -c 'echo '\\''quoted'\\'''"
        );
    }

    #[test]
    fn test_wrap_for_non_posix_shell_with_dollar() {
        // Dollar sign doesn't need escaping in single quotes
        assert_eq!(wrap_for_non_posix_shell("echo $HOME"), "sh -c 'echo $HOME'");
    }

    #[test]
    fn test_wrap_for_non_posix_shell_complex() {
        assert_eq!(
            wrap_for_non_posix_shell("claude -- \"$(cat PROMPT.md)\""),
            "sh -c 'claude -- \"$(cat PROMPT.md)\"'"
        );
    }

    // --- inject_skip_permissions_flag tests ---

    #[test]
    fn test_inject_skip_permissions_with_prompt() {
        let result = inject_skip_permissions_flag(
            " claude -- \"$(cat PROMPT.md)\"",
            "--dangerously-skip-permissions",
        );
        assert_eq!(
            result,
            " claude --dangerously-skip-permissions -- \"$(cat PROMPT.md)\""
        );
    }

    #[test]
    fn test_inject_skip_permissions_with_existing_args() {
        let result = inject_skip_permissions_flag(
            " claude --verbose -- \"$(cat PROMPT.md)\"",
            "--dangerously-skip-permissions",
        );
        assert_eq!(
            result,
            " claude --dangerously-skip-permissions --verbose -- \"$(cat PROMPT.md)\""
        );
    }

    #[test]
    fn test_inject_skip_permissions_bare_command() {
        let result = inject_skip_permissions_flag("claude", "--dangerously-skip-permissions");
        assert_eq!(result, "claude --dangerously-skip-permissions");
    }

    #[test]
    fn test_inject_skip_permissions_non_posix_shell() {
        let result = inject_skip_permissions_flag(
            " sh -c 'claude -- \"$(cat PROMPT.md)\"'",
            "--dangerously-skip-permissions",
        );
        assert_eq!(
            result,
            " sh -c 'claude --dangerously-skip-permissions -- \"$(cat PROMPT.md)\"'"
        );
    }

    #[test]
    fn test_inject_skip_permissions_env_wrapped() {
        let result = inject_skip_permissions_flag(
            " env -u FOO claude -- \"$(cat PROMPT.md)\"",
            "--dangerously-skip-permissions",
        );
        assert_eq!(
            result,
            " env -u FOO claude --dangerously-skip-permissions -- \"$(cat PROMPT.md)\""
        );
    }

    #[test]
    fn test_inject_skip_permissions_env_with_assignments() {
        let result =
            inject_skip_permissions_flag("env FOO=bar claude", "--dangerously-skip-permissions");
        assert_eq!(result, "env FOO=bar claude --dangerously-skip-permissions");
    }

    // --- structured agent resolver tests ---

    fn config_with_agent(agent: &str) -> Config {
        Config {
            agent: Some(agent.to_string()),
            selected_agent: Some(agent.to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn resolve_structured_pane_command_expands_agent_placeholder() {
        let config = config_with_agent("claude");
        let resolved = resolve_pane_command_with_config(
            Some("<agent>"),
            true,
            None,
            Path::new("/tmp"),
            &config,
            None,
            "/bin/zsh",
        )
        .unwrap();
        assert_eq!(resolved.command, "claude");
        assert_eq!(resolved.selected_agent.unwrap().kind(), "claude");
    }

    #[test]
    fn resolve_structured_pane_command_uses_named_profile() {
        let mut config = config_with_agent("cc-sonnet");
        config.agents.insert(
            "cc-sonnet".to_string(),
            crate::config::AgentEntry {
                command: Some("claude".to_string()),
                agent_type: Some("claude".to_string()),
                args: vec!["--model".to_string(), "sonnet".to_string()],
                env: std::collections::BTreeMap::new(),
            },
        );
        let resolved = resolve_pane_command_with_config(
            Some("<agent>"),
            true,
            None,
            Path::new("/tmp"),
            &config,
            None,
            "/bin/zsh",
        )
        .unwrap();
        assert_eq!(resolved.command, "claude --model sonnet");
        assert_eq!(resolved.selected_agent.unwrap().kind(), "claude");
    }

    #[test]
    fn resolve_structured_pane_command_supports_named_placeholder() {
        let mut config = config_with_agent("claude");
        config.agents.insert(
            "codex-mini".to_string(),
            crate::config::AgentEntry {
                command: Some("codex".to_string()),
                agent_type: Some("codex".to_string()),
                args: vec!["exec".to_string(), "-m".to_string(), "mini".to_string()],
                env: std::collections::BTreeMap::new(),
            },
        );
        let resolved = resolve_pane_command_with_config(
            Some("<agent:codex-mini>"),
            true,
            None,
            Path::new("/tmp"),
            &config,
            None,
            "/bin/zsh",
        )
        .unwrap();
        assert_eq!(resolved.command, "codex exec -m mini");
        assert_eq!(resolved.selected_agent.unwrap().kind(), "codex");
    }

    #[test]
    fn resolve_structured_pane_command_applies_env_and_prompt() {
        let prompt = PathBuf::from("/tmp/worktree/PROMPT.md");
        let working_dir = PathBuf::from("/tmp/worktree");
        let mut config = config_with_agent("cc-proxy");
        config.agents.insert(
            "cc-proxy".to_string(),
            crate::config::AgentEntry {
                command: Some("/bin/claude wrapper".to_string()),
                agent_type: Some("claude".to_string()),
                args: vec!["--verbose".to_string()],
                env: std::collections::BTreeMap::from([(
                    "ANTHROPIC_BASE_URL".to_string(),
                    crate::config::AgentEnvValue::Literal("http://localhost:18765".to_string()),
                )]),
            },
        );
        let resolved = resolve_pane_command_with_config(
            Some("<agent>"),
            true,
            Some(&prompt),
            &working_dir,
            &config,
            None,
            "/bin/zsh",
        )
        .unwrap();
        assert_eq!(
            resolved.command,
            " env ANTHROPIC_BASE_URL=http://localhost:18765 /bin/claude wrapper --verbose -- \"$(cat PROMPT.md)\""
        );
        assert!(resolved.prompt_injected);
    }

    #[test]
    fn resolve_structured_pane_command_wraps_non_posix_shell() {
        let prompt = PathBuf::from("/tmp/worktree/PROMPT.md");
        let working_dir = PathBuf::from("/tmp/worktree");
        let config = config_with_agent("claude");
        let resolved = resolve_pane_command_with_config(
            Some("<agent>"),
            true,
            Some(&prompt),
            &working_dir,
            &config,
            None,
            "/opt/homebrew/bin/nu",
        )
        .unwrap();
        assert_eq!(resolved.command, " sh -c 'claude -- \"$(cat PROMPT.md)\"'");
    }

    #[test]
    fn resolve_structured_pane_command_preserves_regular_command() {
        let config = config_with_agent("claude");
        let resolved = resolve_pane_command_with_config(
            Some("vim"),
            true,
            None,
            Path::new("/tmp"),
            &config,
            None,
            "/bin/zsh",
        )
        .unwrap();
        assert_eq!(resolved.command, "vim");
        assert!(resolved.selected_agent.is_none());
    }

    // --- needs_default_subcommand tests ---

    #[test]
    fn test_needs_default_subcommand_empty() {
        assert!(needs_default_subcommand("", "chat"));
    }

    #[test]
    fn test_needs_default_subcommand_already_present() {
        assert!(!needs_default_subcommand("chat", "chat"));
        assert!(!needs_default_subcommand("chat --model foo", "chat"));
    }

    #[test]
    fn test_needs_default_subcommand_flag() {
        assert!(needs_default_subcommand("--verbose", "chat"));
        assert!(needs_default_subcommand("-v", "chat"));
    }

    #[test]
    fn test_needs_default_subcommand_other_subcommand() {
        assert!(!needs_default_subcommand("login", "chat"));
        assert!(!needs_default_subcommand("agent list", "chat"));
    }
}
