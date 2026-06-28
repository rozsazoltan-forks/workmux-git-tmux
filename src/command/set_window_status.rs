use anyhow::Result;
use clap::ValueEnum;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tracing::warn;

use crate::config::Config;
use crate::multiplexer::{AgentStatus, LivePaneInfo, Multiplexer, create_backend, detect_backend};

#[derive(ValueEnum, Debug, Clone)]
pub enum SetWindowStatusCommand {
    /// Set status to "working" (agent is processing)
    Working,
    /// Set status to "waiting" (agent needs user input) - auto-clears on window focus
    Waiting,
    /// Set status to "done" (agent finished) - auto-clears on window focus
    Done,
    /// Clear the status
    Clear,
}

pub fn run(cmd: SetWindowStatusCommand) -> Result<()> {
    if std::env::var_os("WORKMUX_DISABLE_SET_WINDOW_STATUS").is_some() {
        return Ok(());
    }

    // Codex compatibility: hook stdin may identify a specific parent/subagent
    // turn. Use it to avoid a child Stop hook marking the pane done early.
    let codex_context = crate::state::codex_status::detect_context_from_stdin();

    // Inside a sandbox guest, route through RPC to the host supervisor
    if crate::sandbox::guest::is_sandbox_guest() {
        // Codex nested hook workaround is host-path only in v1.
        return run_via_rpc(cmd);
    }

    let config = Config::load(None)?;
    let mux = create_backend(detect_backend());

    // Fail silently if not in a multiplexer session. Some agents, including
    // Codex, strip TMUX/TMUX_PANE from hook command environments; in that case
    // fall back to matching the hook cwd to a live pane cwd.
    let Some(pane_id) = resolve_status_pane_id(&*mux) else {
        return Ok(());
    };

    let pane_key = crate::state::PaneKey {
        backend: mux.name().to_string(),
        instance: mux.instance_id(),
        pane_id: pane_id.to_string(),
    };

    match cmd {
        SetWindowStatusCommand::Clear => {
            if let Err(error) = crate::state::codex_status::clear_pane(&pane_key) {
                warn!(%pane_id, error = %error, "failed to clear Codex status workaround state");
            }
            mux.clear_status(&pane_id)?;
        }
        SetWindowStatusCommand::Working
        | SetWindowStatusCommand::Waiting
        | SetWindowStatusCommand::Done => {
            let requested_status = match cmd {
                SetWindowStatusCommand::Working => AgentStatus::Working,
                SetWindowStatusCommand::Waiting => AgentStatus::Waiting,
                SetWindowStatusCommand::Done => AgentStatus::Done,
                SetWindowStatusCommand::Clear => unreachable!(),
            };

            let status = if let Some(context) = codex_context {
                match crate::state::codex_status::apply_status(
                    &pane_key,
                    &context,
                    requested_status,
                ) {
                    Ok(status) => status,
                    Err(error) => {
                        warn!(%pane_id, ?requested_status, error = %error, "failed to update Codex status workaround state");
                        requested_status
                    }
                }
            } else {
                requested_status
            };

            let (icon, auto_clear) = match status {
                AgentStatus::Working => (config.status_icons.working(), false),
                AgentStatus::Waiting => (config.status_icons.waiting(), true),
                AgentStatus::Done => (config.status_icons.done(), true),
            };

            // Ensure the status format is applied so the icon actually shows up
            if config.status_format.unwrap_or(true) {
                let _ = mux.ensure_status_format(&pane_id);
            }

            // Update backend UI (status bar icon)
            mux.set_status(&pane_id, icon, auto_clear)?;

            // Persist to state store so the dashboard sees this agent
            crate::state::persist_agent_update(&*mux, &pane_id, Some(status), None);
        }
    }

    Ok(())
}

fn resolve_status_pane_id(mux: &dyn Multiplexer) -> Option<String> {
    mux.current_pane_id()
        .or_else(|| resolve_status_pane_id_from_cwd(mux).ok().flatten())
}

fn resolve_status_pane_id_from_cwd(mux: &dyn Multiplexer) -> Result<Option<String>> {
    let cwd = std::env::current_dir()?;
    let live_panes = mux.get_all_live_pane_info()?;
    Ok(select_pane_for_cwd(&live_panes, &cwd))
}

fn select_pane_for_cwd(live_panes: &HashMap<String, LivePaneInfo>, cwd: &Path) -> Option<String> {
    let cwd = normalized_path(cwd);
    let mut best: Option<(&str, usize)> = None;
    let mut tied = false;

    for (pane_id, pane) in live_panes {
        let pane_cwd = normalized_path(&pane.working_dir);
        if !cwd.starts_with(&pane_cwd) {
            continue;
        }

        // Prefer the closest ancestor. Exact matches naturally win because
        // they have the most path components.
        let score = pane_cwd.components().count();
        match best {
            None => {
                best = Some((pane_id.as_str(), score));
                tied = false;
            }
            Some((_, best_score)) if score > best_score => {
                best = Some((pane_id.as_str(), score));
                tied = false;
            }
            Some((_, best_score)) if score == best_score => {
                tied = true;
            }
            Some(_) => {}
        }
    }

    if tied {
        None
    } else {
        best.map(|(pane_id, _)| pane_id.to_string())
    }
}

fn normalized_path(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// Send a status update via RPC when running inside a sandbox guest.
fn run_via_rpc(cmd: SetWindowStatusCommand) -> Result<()> {
    use crate::sandbox::rpc::{RpcClient, RpcRequest, RpcResponse};

    let status = match cmd {
        SetWindowStatusCommand::Working => "working",
        SetWindowStatusCommand::Waiting => "waiting",
        SetWindowStatusCommand::Done => "done",
        SetWindowStatusCommand::Clear => "clear",
    };

    let mut client = RpcClient::from_env()?;
    let response = client.call(&RpcRequest::SetStatus {
        status: status.to_string(),
    })?;

    match response {
        RpcResponse::Ok => Ok(()),
        RpcResponse::Error { message } => {
            warn!(error = %message, "RPC SetStatus failed");
            Ok(()) // Fail silently like the host path does
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn live_pane(path: &str) -> LivePaneInfo {
        LivePaneInfo {
            pid: Some(1),
            current_command: Some("codex".to_string()),
            working_dir: PathBuf::from(path),
            title: None,
            session: Some("test".to_string()),
            window: Some("wm-test".to_string()),
            session_id: Some("$1".to_string()),
            window_id: Some("@1".to_string()),
        }
    }

    #[test]
    fn select_pane_for_cwd_prefers_exact_match() {
        let mut panes = HashMap::new();
        panes.insert("%1".to_string(), live_pane("/repo"));
        panes.insert("%2".to_string(), live_pane("/repo/subdir"));

        assert_eq!(
            select_pane_for_cwd(&panes, Path::new("/repo/subdir")),
            Some("%2".to_string())
        );
    }

    #[test]
    fn select_pane_for_cwd_accepts_closest_ancestor() {
        let mut panes = HashMap::new();
        panes.insert("%1".to_string(), live_pane("/repo"));
        panes.insert("%2".to_string(), live_pane("/other"));

        assert_eq!(
            select_pane_for_cwd(&panes, Path::new("/repo/nested/package")),
            Some("%1".to_string())
        );
    }

    #[test]
    fn select_pane_for_cwd_rejects_ambiguous_matches() {
        let mut panes = HashMap::new();
        panes.insert("%1".to_string(), live_pane("/repo"));
        panes.insert("%2".to_string(), live_pane("/repo"));

        assert_eq!(select_pane_for_cwd(&panes, Path::new("/repo")), None);
    }
}
