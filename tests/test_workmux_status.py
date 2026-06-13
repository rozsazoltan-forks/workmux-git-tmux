"""
Tests for `workmux status` command.

Tests output format, JSON mode, filtering, and behavior with real agent state.
"""

import json
from pathlib import Path

from .conftest import (
    MuxEnvironment,
    run_workmux_add,
    run_workmux_command,
    write_workmux_config,
)
from .support.agent_state import start_active_agent


def test_status_no_agents(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """Status shows 'No active agents' when no agents are running."""
    result = run_workmux_command(mux_server, workmux_exe_path, mux_repo_path, "status")
    assert "No active agents" in result.stdout


def test_status_json_no_agents(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """Status --json returns empty array when no agents are running."""
    result = run_workmux_command(
        mux_server, workmux_exe_path, mux_repo_path, "status --json"
    )
    parsed = json.loads(result.stdout)
    assert parsed == []


def test_status_with_active_agent(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """Status shows agent info when an agent is active."""
    env = mux_server
    start_active_agent(
        env,
        workmux_exe_path,
        mux_repo_path,
        "feature-status-active",
        status="working",
    )

    result = run_workmux_command(env, workmux_exe_path, mux_repo_path, "status")
    assert "working" in result.stdout
    assert "WORKTREE" in result.stdout
    assert "STATUS" in result.stdout


def test_status_json_with_active_agent(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """Status --json returns agent data when an agent is active."""
    env = mux_server
    start_active_agent(
        env,
        workmux_exe_path,
        mux_repo_path,
        "feature-status-json",
        status="done",
    )

    result = run_workmux_command(env, workmux_exe_path, mux_repo_path, "status --json")
    parsed = json.loads(result.stdout)
    assert isinstance(parsed, list)
    assert len(parsed) >= 1

    entry = parsed[0]
    assert "worktree" in entry
    assert "branch" in entry
    assert "status" in entry
    assert "pane_id" in entry
    assert entry["status"] == "done"


def test_status_filter_by_worktree(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """Status filters to show only the specified worktree."""
    env = mux_server
    branch_name = "feature-status-filt"
    start_active_agent(
        env,
        workmux_exe_path,
        mux_repo_path,
        branch_name,
        status="working",
    )

    result = run_workmux_command(
        env,
        workmux_exe_path,
        mux_repo_path,
        f"status --json {branch_name}",
    )
    parsed = json.loads(result.stdout)
    assert len(parsed) >= 1
    for entry in parsed:
        assert entry["branch"] == branch_name


def test_status_filter_no_match(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """Status with a filter that matches no agents shows 'No active agents'."""
    env = mux_server
    start_active_agent(
        env,
        workmux_exe_path,
        mux_repo_path,
        "feature-status-exists",
        status="working",
    )

    result = run_workmux_command(
        env,
        workmux_exe_path,
        mux_repo_path,
        "status nonexistent-worktree",
    )
    assert "No active agents" in result.stdout
