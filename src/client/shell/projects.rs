//! The Projects tree: one list holding repos, their worktrees, and the agents
//! running inside each worktree.
//!
//! Herdr's expanded sidebar normally splits into a Spaces panel and an Agents
//! panel. Every agent already knows its workspace, so that split puts related
//! things in two places and spends half the sidebar doing it. This module folds
//! them into a single tree -- repo, worktree, agents -- and the Spaces and
//! Agents row layouts keep describing their own rows.
//!
//! Presentation only: no new runtime or API state, per the runtime/client
//! boundary guardrail in AGENTS.md.

use super::agent_sidebar::{agent_row_with, AgentRow};
use super::render::{put_right_text, put_text};
use super::*;
use ratatui::widgets::{Paragraph, Widget};

/// One entry in the tree. Each renders as one or more terminal rows.
pub(super) enum ProjectRow {
    /// A repo parent, or a worktree indented beneath one.
    Workspace(WorkspaceEntry),
    /// The "N agents" fold belonging to the workspace above it.
    AgentFold {
        workspace_id: String,
        count: usize,
        collapsed: bool,
    },
    /// One agent inside that workspace.
    Agent(Box<AgentRow>),
    /// A pane of that workspace that no agent occupies: an editor, a dev
    /// server, a shell. `process` is what it is running, once that is known.
    Pane {
        pane_id: String,
        process: Option<String>,
        /// Ports it is listening on, when it is serving something.
        ports: Vec<u16>,
        focused: bool,
    },
}

/// Columns an agent row and its fold sit in, relative to the Space row that
/// owns them. A child Space draws a six-column "   |- " prefix, so anything
/// shallower than that reads as a sibling of the Space rather than its child.
pub(super) const AGENT_INDENT: u16 = 8;

/// Spinner frames for a working agent. Braille, because the half-circle glyphs
/// herdr uses elsewhere are missing from common terminal fonts and fall back to
/// a different face -- which visibly changes size as a row is highlighted.
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Marks a pane with no agent in it.
const PANE_GLYPH: &str = "\u{25aa}";

/// A pane that is serving breathes: the mark grows and shrinks again.
///
/// Deliberately not a spinner. A spinner sweeps in one direction and restarts,
/// which reads as waiting on something, and a server that has been up for an
/// hour should not look like it is stuck mid-request. This oscillates instead,
/// so it has no restart to notice -- it just looks alive.
const SERVER_PULSE: [&str; 4] = ["\u{00b7}", "\u{2022}", "\u{25cf}", "\u{2022}"];

/// How long each step of the breath lasts. Slow: one breath is about two
/// seconds, which reads as a heartbeat rather than activity.
pub(crate) const SERVER_PULSE_INTERVAL: std::time::Duration = std::time::Duration::from_millis(450);

/// The mark for a pane: breathing when it is serving, static otherwise.
fn pane_glyph(ports: &[u16], pulse: usize) -> &'static str {
    if ports.is_empty() {
        return PANE_GLYPH;
    }
    SERVER_PULSE[pulse % SERVER_PULSE.len()]
}

/// The agent mark itself, for an agent that is present but not working.
const AGENT_GLYPH: &str = "\u{f06a9}";

/// Commands that stand in front of the one actually being run. The job leader
/// is normally the command you typed, but a wrapper takes that place while the
/// thing you care about runs underneath it.
const PROCESS_WRAPPERS: [&str; 8] = [
    "safe-chain",
    "caffeinate",
    "env",
    "nohup",
    "time",
    "sudo",
    "timeout",
    "script",
];

/// The command a process was started as: `bun run dev` is `bun`.
fn process_command(process: &crate::api::schema::PaneProcessInfoProcess) -> Option<String> {
    let name = process
        .argv0
        .as_deref()
        .filter(|argv0| !argv0.is_empty())
        .unwrap_or(process.name.as_str());
    // argv0 can be a path, and a login shell arrives as "-zsh".
    let name = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let name = name.trim_start_matches('-');
    // Some runtimes rewrite argv0 to a banner: "next-server (v15.5.24)".
    let name = name.split_whitespace().next().unwrap_or(name);
    (!name.is_empty()).then(|| name.to_owned())
}

/// What a pane is running, for display.
///
/// Prefers `argv0` over the kernel's process name, which for some runtimes is a
/// version string rather than the command -- Claude Code reports "2.1.294".
///
/// Normally that is the job leader: in `caffeinate -i | claude` the leader is
/// claude, which is the point of the pane. A wrapper leader is the exception --
/// `safe-chain` fronts `bun run dev` -- so the search falls through to the
/// first process underneath it that is not itself a wrapper.
pub(super) fn foreground_process_name(
    info: &crate::api::schema::PaneProcessInfo,
) -> Option<String> {
    let is_wrapper = |process: &crate::api::schema::PaneProcessInfoProcess| {
        process_command(process).is_some_and(|name| PROCESS_WRAPPERS.contains(&name.as_str()))
    };
    let leader = info
        .foreground_processes
        .iter()
        .find(|process| Some(process.pid) == info.foreground_process_group_id);
    if let Some(leader) = leader.filter(|leader| !is_wrapper(leader)) {
        return process_command(leader);
    }
    let mut rest = info
        .foreground_processes
        .iter()
        .filter(|process| !is_wrapper(process))
        .collect::<Vec<_>>();
    // Oldest first: a wrapper's immediate child is what it was asked to run.
    rest.sort_by_key(|process| process.pid);
    rest.first().copied().or(leader).and_then(process_command)
}

/// How often each pane is asked what it is running. Slow on purpose: it is a
/// process-tree walk per pane, and a command that just started is still news a
/// second later.
pub(crate) const PANE_PROCESS_INTERVAL: std::time::Duration =
    std::time::Duration::from_millis(2000);

/// How often the spinner advances.
pub(crate) const AGENT_ANIMATION_INTERVAL: std::time::Duration =
    std::time::Duration::from_millis(120);

/// Glyph and colour for an agent in the tree: the agent mark normally, a
/// spinner while it works, a check when it finishes.
///
/// Only the states worth reacting to carry colour. An agent that is merely
/// present or busy stays grey -- the spinner's motion already says it is busy,
/// and a column of coloured marks reads as alarm when nothing is wrong.
pub(super) fn agent_icon(
    status: crate::api::schema::AgentStatus,
    frame: usize,
    palette: &Palette,
) -> (&'static str, Style) {
    use crate::api::schema::AgentStatus;
    let quiet = Style::default().fg(palette.overlay0);
    match status {
        AgentStatus::Working => (SPINNER[frame % SPINNER.len()], quiet),
        AgentStatus::Done => (
            "✓",
            Style::default()
                .fg(palette.green)
                .add_modifier(Modifier::BOLD),
        ),
        // Blocked keeps its colour: it is the one state asking for attention.
        AgentStatus::Blocked => ("×", Style::default().fg(palette.red)),
        AgentStatus::Idle | AgentStatus::Unknown => (AGENT_GLYPH, quiet),
    }
}

impl ProjectRow {
    /// Terminal rows this entry occupies.
    pub(super) fn height(&self, snapshot: &ClientShellSnapshot, config: &ClientShellConfig) -> u16 {
        let lines = match self {
            Self::Workspace(entry) => snapshot
                .workspaces
                .get(entry.index)
                .map(|workspace| {
                    super::sidebar::workspace_rows(
                        workspace,
                        workspace.agent_status,
                        entry.indented,
                        &config.spaces,
                    )
                    .len()
                })
                .unwrap_or(1),
            Self::AgentFold { .. } => 1,
            Self::Agent(row) => row.rows.len(),
            Self::Pane { .. } => 1,
        };
        lines.max(1).min(u16::MAX as usize) as u16
    }

    /// A workspace row starting a new top-level group gets the configured gap;
    /// nested rows stay packed against the row they belong to.
    pub(super) fn starts_group(&self) -> bool {
        matches!(self, Self::Workspace(entry) if !entry.indented)
    }

    /// Whether this entry heads a project rather than sitting inside one.
    fn is_project_header(&self) -> bool {
        matches!(self, Self::Workspace(entry) if !entry.indented)
    }
}

/// A repo parent with no worktrees under it heads nothing: it is the repo
/// still being tracked so a worktree can be made from it later, not somewhere
/// work is happening. Keep it out of the tree unless it is focused or running
/// an agent, which would otherwise strand a live pane with no way back to it.
fn is_empty_repo_parent(snapshot: &ClientShellSnapshot, entry: &WorkspaceEntry) -> bool {
    let Some(workspace) = snapshot.workspaces.get(entry.index) else {
        return false;
    };
    let Some(worktree) = workspace.worktree.as_ref() else {
        return false;
    };
    if worktree.is_linked_worktree || workspace.focused {
        return false;
    }
    if snapshot
        .agents
        .iter()
        .any(|agent| agent.workspace_id == workspace.workspace_id)
    {
        return false;
    }
    !snapshot.workspaces.iter().any(|candidate| {
        candidate
            .worktree
            .as_ref()
            .is_some_and(|other| other.key == worktree.key && other.is_linked_worktree)
    })
}

/// Blank rows between two entries in the Projects tree. Separation is what
/// shows the grouping here, in place of the tree connectors the Spaces panel
/// draws, so it has to say what belongs together as well as what does not: a
/// project stands well clear of the project above it, its first Space stays
/// attached to it, later Spaces are parted from their siblings, and an agent
/// hugs the Space it runs in.
pub(super) fn gap_between(previous: &ProjectRow, next: &ProjectRow) -> u16 {
    match next {
        ProjectRow::Workspace(entry) if !entry.indented => 2,
        ProjectRow::Workspace(_) if previous.is_project_header() => 0,
        ProjectRow::Workspace(_) => 1,
        _ => 0,
    }
}

/// The row layout for an agent inside the tree. Its Space is already the line
/// above it, so this is the compact layout rather than the panel's.
fn nested_agents(config: &ClientShellConfig) -> crate::config::AgentsSidebarConfig {
    crate::config::AgentsSidebarConfig {
        rows: config.agents.nested_rows.clone(),
        rows_by_agent: Default::default(),
        row_gap: 0,
        nested_rows: config.agents.nested_rows.clone(),
    }
}

/// Expand the space entries into the full tree. With `include_agents` false
/// this is exactly the Spaces list, so both sidebar modes share one code path.
pub(super) fn project_rows(
    snapshot: &ClientShellSnapshot,
    config: &ClientShellConfig,
    collapsed_groups: &HashSet<String>,
    collapsed_agent_lists: &HashSet<String>,
    pane_processes: &std::collections::HashMap<String, String>,
    pane_ports: &std::collections::HashMap<String, Vec<u16>>,
    include_agents: bool,
) -> Vec<ProjectRow> {
    let entries = super::sidebar::workspace_entries(snapshot, collapsed_groups);
    if !include_agents {
        return entries.into_iter().map(ProjectRow::Workspace).collect();
    }
    let entries = entries
        .into_iter()
        .filter(|entry| !is_empty_repo_parent(snapshot, entry))
        .collect::<Vec<_>>();
    // Built once: this is a render path scaled by workspaces x agents.
    let nested = nested_agents(config);
    let mut rows = Vec::with_capacity(entries.len());
    for entry in entries {
        let workspace_id = snapshot
            .workspaces
            .get(entry.index)
            .map(|workspace| workspace.workspace_id.clone());
        rows.push(ProjectRow::Workspace(entry));
        let Some(workspace_id) = workspace_id else {
            continue;
        };
        // Keep the panel's own ordering so an agent sits where the Agents panel
        // would have put it, just filtered to this workspace.
        let pane_ids =
            super::agent_sidebar::ordered_agent_pane_ids(snapshot, config.agent_panel_sort)
                .into_iter()
                .filter(|pane_id| {
                    snapshot.agents.iter().any(|agent| {
                        &agent.pane_id == pane_id && agent.workspace_id == workspace_id
                    })
                })
                .collect::<Vec<_>>();
        if pane_ids.is_empty() {
            push_plain_panes(
                &mut rows,
                snapshot,
                &workspace_id,
                &pane_ids,
                pane_processes,
                pane_ports,
            );
            continue;
        }
        // One agent needs no fold: the header would be taller than the row it
        // hides. Several get one, so a busy Space can be folded shut.
        let collapsed = collapsed_agent_lists.contains(&workspace_id);
        if pane_ids.len() > 1 {
            rows.push(ProjectRow::AgentFold {
                workspace_id: workspace_id.clone(),
                count: pane_ids.len(),
                collapsed,
            });
            if collapsed {
                continue;
            }
        }
        for pane_id in &pane_ids {
            if let Some(row) = agent_row_with(snapshot, pane_id, None, &nested) {
                rows.push(ProjectRow::Agent(Box::new(row)));
            }
        }
        push_plain_panes(
            &mut rows,
            snapshot,
            &workspace_id,
            &pane_ids,
            pane_processes,
            pane_ports,
        );
    }
    rows
}

/// The space's panes that no agent occupies: the editor, the dev server, the
/// shell left running. An agent's pane is already listed as the agent itself.
fn push_plain_panes(
    rows: &mut Vec<ProjectRow>,
    snapshot: &ClientShellSnapshot,
    workspace_id: &str,
    agent_panes: &[String],
    pane_processes: &std::collections::HashMap<String, String>,
    pane_ports: &std::collections::HashMap<String, Vec<u16>>,
) {
    // A repo parent heads the worktrees under it. Its own shell is implied, and
    // listing it puts a pane row against every project heading for nothing.
    let is_repo_parent = snapshot
        .workspaces
        .iter()
        .find(|workspace| workspace.workspace_id == workspace_id)
        .and_then(|workspace| workspace.worktree.as_ref())
        .is_some_and(|worktree| !worktree.is_linked_worktree);
    if is_repo_parent {
        return;
    }
    for pane in snapshot
        .panes
        .iter()
        .filter(|pane| pane.workspace_id == workspace_id)
        .filter(|pane| !agent_panes.iter().any(|agent| agent == &pane.pane_id))
    {
        rows.push(ProjectRow::Pane {
            pane_id: pane.pane_id.clone(),
            process: pane_processes.get(&pane.pane_id).cloned(),
            ports: pane_ports.get(&pane.pane_id).cloned().unwrap_or_default(),
            focused: pane.focused,
        });
    }
}

/// Draw one agent nested under its Space.
pub(super) fn render_nested_agent(
    buffer: &mut Buffer,
    rect: Rect,
    row: &AgentRow,
    frame: usize,
    config: &ClientShellConfig,
) {
    let palette = &config.palette;
    if row.focused {
        buffer.set_style(rect, Style::default().bg(palette.active_row_bg));
    }
    let icon = agent_icon(row.status, frame, palette);
    let name_style = Style::default().fg(if row.focused {
        palette.text
    } else {
        palette.subtext0
    });
    let secondary = Style::default().fg(palette.overlay0);
    for (index, tokens) in row.rows.iter().take(rect.height as usize).enumerate() {
        let x = rect.x.saturating_add(AGENT_INDENT);
        let width = rect.width.saturating_sub(AGENT_INDENT);
        if width == 0 {
            break;
        }
        let spans = crate::ui::resolved_token_spans(
            tokens,
            icon,
            secondary,
            name_style,
            secondary,
            secondary,
            palette,
            width as usize,
        );
        let line = ratatui::text::Line::from(spans);
        Paragraph::new(line).render(
            Rect::new(x, rect.y.saturating_add(index as u16), width, 1),
            buffer,
        );
    }
}

/// Draw one pane nested under its Space: what it is running, or just the pane
/// when that is not known yet.
pub(super) fn render_pane(
    buffer: &mut Buffer,
    rect: Rect,
    process: Option<&str>,
    ports: &[u16],
    pulse: usize,
    focused: bool,
    palette: &Palette,
) {
    if focused {
        buffer.set_style(rect, Style::default().bg(palette.active_row_bg));
    }
    // A pane that serves something is mostly interesting for where to reach it,
    // so the ports follow the command.
    let mut owned = process.unwrap_or("pane").to_owned();
    for port in ports {
        owned.push_str(&format!("  :{port}"));
    }
    let label = owned.as_str();
    let style = Style::default().fg(if process.is_some() {
        palette.subtext0
    } else {
        palette.overlay0
    });
    put_text(
        buffer,
        rect.x.saturating_add(AGENT_INDENT),
        rect.y,
        rect.width.saturating_sub(AGENT_INDENT),
        &format!("{} {label}", pane_glyph(ports, pulse)),
        style,
    );
}

/// Draw the "N agents" fold.
pub(super) fn render_agent_fold(
    buffer: &mut Buffer,
    rect: Rect,
    count: usize,
    collapsed: bool,
    palette: &Palette,
) {
    let label = if count == 1 {
        "1 agent".to_string()
    } else {
        format!("{count} agents")
    };
    put_text(
        buffer,
        rect.x.saturating_add(AGENT_INDENT),
        rect.y,
        rect.width.saturating_sub(AGENT_INDENT),
        &label,
        Style::default().fg(palette.overlay0),
    );
    put_right_text(
        buffer,
        rect,
        rect.y,
        if collapsed { "▸" } else { "▾" },
        Style::default().fg(palette.accent),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::AgentStatus;
    use crate::protocol::ClientShellAgent;
    use std::collections::HashMap;

    fn agent(pane_id: &str, workspace_id: &str) -> ClientShellAgent {
        ClientShellAgent {
            pane_id: pane_id.into(),
            workspace_id: workspace_id.into(),
            tab_id: "tab_1".into(),
            name: None,
            display_agent: None,
            agent: None,
            title: None,
            terminal_title: None,
            terminal_title_stripped: None,
            agent_status: AgentStatus::Idle,
            state_change_seq: 1,
            state_labels: Vec::new(),
            tokens: Vec::new(),
            focused: false,
        }
    }

    fn config() -> ClientShellConfig {
        ClientShellConfig::from_config(&crate::config::Config::default())
    }

    fn kinds(rows: &[ProjectRow]) -> Vec<&'static str> {
        rows.iter()
            .map(|row| match row {
                ProjectRow::Workspace(_) => "workspace",
                ProjectRow::AgentFold { .. } => "fold",
                ProjectRow::Agent(_) => "agent",
                ProjectRow::Pane { .. } => "pane",
            })
            .collect()
    }

    #[test]
    fn nested_agent_rows_do_not_repeat_their_workspace() {
        use crate::ui::ResolvedTokenKind;
        let mut snapshot = crate::client::shell::tests::snapshot();
        snapshot.agents = vec![agent("pane_1", "ws_1"), agent("pane_2", "ws_1")];
        let rows = project_rows(
            &snapshot,
            &config(),
            &HashSet::new(),
            &HashSet::new(),
            &HashMap::new(),
            &HashMap::new(),
            true,
        );
        let ProjectRow::Agent(row) = rows
            .iter()
            .find(|r| matches!(r, ProjectRow::Agent(_)))
            .unwrap()
        else {
            panic!("expected an agent row");
        };
        // The Space is the line directly above, so repeating it is duplication.
        for line in &row.rows {
            for token in line {
                assert!(
                    !matches!(token.kind, ResolvedTokenKind::Workspace(_)),
                    "nested agent row repeats the workspace: {:?}",
                    token.kind
                );
            }
        }
        assert_eq!(row.rows.len(), 1, "nested agents render on one line");
    }

    #[test]
    fn the_agent_icon_spins_while_working_and_checks_when_done() {
        use crate::api::schema::AgentStatus;
        let palette = Palette::catppuccin();
        let working: Vec<&str> = (0..SPINNER.len())
            .map(|frame| agent_icon(AgentStatus::Working, frame, &palette).0)
            .collect();
        assert_eq!(
            working
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len(),
            SPINNER.len(),
            "every frame should differ, or it does not read as motion"
        );
        // The frame counter wraps rather than panicking on a long-running agent.
        assert_eq!(
            agent_icon(AgentStatus::Working, SPINNER.len(), &palette).0,
            SPINNER[0]
        );
        assert_eq!(agent_icon(AgentStatus::Done, 0, &palette).0, "\u{2713}");
        // Only finishing and blocking earn colour; the rest stay quiet.
        assert_eq!(
            agent_icon(AgentStatus::Idle, 0, &palette).1.fg,
            Some(palette.overlay0)
        );
        assert_eq!(
            agent_icon(AgentStatus::Working, 0, &palette).1.fg,
            Some(palette.overlay0)
        );
        assert_eq!(
            agent_icon(AgentStatus::Done, 0, &palette).1.fg,
            Some(palette.green)
        );
        assert_eq!(
            agent_icon(AgentStatus::Idle, 0, &palette).0,
            AGENT_GLYPH,
            "an idle agent still shows it is an agent"
        );
        // A still agent must not change with the frame, or the sidebar would
        // repaint forever.
        for frame in 0..4 {
            assert_eq!(
                agent_icon(AgentStatus::Idle, frame, &palette).0,
                agent_icon(AgentStatus::Idle, 0, &palette).0
            );
        }
    }

    #[test]
    fn a_project_is_parted_from_the_one_above_it() {
        let mut snapshot = crate::client::shell::tests::snapshot();
        snapshot.agents = vec![agent("pane_1", "ws_1")];
        let rows = project_rows(
            &snapshot,
            &config(),
            &HashSet::new(),
            &HashSet::new(),
            &HashMap::new(),
            &HashMap::new(),
            true,
        );
        let project = ProjectRow::Workspace(WorkspaceEntry {
            index: 0,
            indented: false,
            last_child: false,
        });
        let space = ProjectRow::Workspace(WorkspaceEntry {
            index: 0,
            indented: true,
            last_child: false,
        });
        assert_eq!(
            gap_between(&space, &project),
            2,
            "a project stands clear of the one above it"
        );
        assert_eq!(
            gap_between(&project, &space),
            0,
            "a project's first space stays attached to it"
        );
        assert_eq!(
            gap_between(&space, &space),
            1,
            "sibling spaces are parted from each other"
        );
        let agent = rows
            .iter()
            .find(|r| matches!(r, ProjectRow::Agent(_)))
            .unwrap();
        assert_eq!(
            gap_between(&space, agent),
            0,
            "an agent hugs the space it runs in"
        );
    }

    #[test]
    fn a_repo_with_no_worktrees_left_drops_out_of_the_tree() {
        use crate::protocol::ClientShellWorktree;
        let mut snapshot = crate::client::shell::tests::snapshot();
        snapshot.agents = Vec::new();
        snapshot.workspaces[0].focused = false;
        snapshot.workspaces[0].worktree = Some(ClientShellWorktree {
            key: "/repo".into(),
            label: "repo".into(),
            is_linked_worktree: false,
        });
        let rows = project_rows(
            &snapshot,
            &config(),
            &HashSet::new(),
            &HashSet::new(),
            &HashMap::new(),
            &HashMap::new(),
            true,
        );
        assert!(kinds(&rows).is_empty(), "a repo heading nothing is hidden");

        // Still tracked, so a live pane in it is never stranded.
        snapshot.agents = vec![agent("pane_1", "ws_1")];
        let rows = project_rows(
            &snapshot,
            &config(),
            &HashSet::new(),
            &HashSet::new(),
            &HashMap::new(),
            &HashMap::new(),
            true,
        );
        assert_eq!(kinds(&rows), ["workspace", "agent"]);

        snapshot.agents = Vec::new();
        snapshot.workspaces[0].focused = true;
        let rows = project_rows(
            &snapshot,
            &config(),
            &HashSet::new(),
            &HashSet::new(),
            &HashMap::new(),
            &HashMap::new(),
            true,
        );
        // A repo parent heads its worktrees; its own shell is implied, so it
        // gets no pane row of its own.
        assert_eq!(kinds(&rows), ["workspace"]);
    }

    #[test]
    fn without_agents_the_tree_is_exactly_the_spaces_list() {
        let mut snapshot = crate::client::shell::tests::snapshot();
        snapshot.agents = vec![agent("pane_1", "ws_1")];
        let rows = project_rows(
            &snapshot,
            &config(),
            &HashSet::new(),
            &HashSet::new(),
            &HashMap::new(),
            &HashMap::new(),
            false,
        );
        assert_eq!(kinds(&rows), ["workspace"]);
    }

    #[test]
    fn agents_nest_under_their_own_workspace_behind_a_fold() {
        let mut snapshot = crate::client::shell::tests::snapshot();
        snapshot.agents = vec![agent("pane_1", "ws_1"), agent("pane_2", "ws_1")];
        let rows = project_rows(
            &snapshot,
            &config(),
            &HashSet::new(),
            &HashSet::new(),
            &HashMap::new(),
            &HashMap::new(),
            true,
        );
        assert_eq!(kinds(&rows), ["workspace", "fold", "agent", "agent"]);
        let ProjectRow::AgentFold {
            count, collapsed, ..
        } = &rows[1]
        else {
            panic!("expected a fold");
        };
        assert_eq!(*count, 2);
        assert!(!collapsed);
    }

    #[test]
    fn a_collapsed_fold_keeps_its_count_and_drops_its_agents() {
        let mut snapshot = crate::client::shell::tests::snapshot();
        snapshot.agents = vec![agent("pane_1", "ws_1"), agent("pane_2", "ws_1")];
        let collapsed = HashSet::from(["ws_1".to_string()]);
        let rows = project_rows(
            &snapshot,
            &config(),
            &HashSet::new(),
            &collapsed,
            &HashMap::new(),
            &HashMap::new(),
            true,
        );
        assert_eq!(kinds(&rows), ["workspace", "fold"]);
        let ProjectRow::AgentFold {
            count, collapsed, ..
        } = &rows[1]
        else {
            panic!("expected a fold");
        };
        assert_eq!(*count, 2);
        assert!(collapsed);
    }

    #[test]
    fn a_workspace_with_no_agents_gets_no_fold() {
        let mut snapshot = crate::client::shell::tests::snapshot();
        snapshot.agents = vec![agent("pane_9", "ws_other")];
        let rows = project_rows(
            &snapshot,
            &config(),
            &HashSet::new(),
            &HashSet::new(),
            &HashMap::new(),
            &HashMap::new(),
            true,
        );
        // No agent, but the space still has a pane of its own to show.
        assert_eq!(kinds(&rows), ["workspace", "pane"]);
    }

    #[test]
    fn a_serving_pane_breathes_and_an_idle_one_does_not() {
        // Only a serving pane animates; everything else holds still.
        assert_eq!(pane_glyph(&[], 0), PANE_GLYPH);
        assert_eq!(pane_glyph(&[], 7), PANE_GLYPH);

        let breath: Vec<&str> = (0..SERVER_PULSE.len())
            .map(|frame| pane_glyph(&[3000], frame))
            .collect();

        // It oscillates rather than sweeping: the second half retraces the
        // first. That is what keeps it from reading as a spinner, which always
        // moves one way and restarts, and so looks like it is waiting.
        assert_eq!(breath[1], breath[3]);
        assert_ne!(breath[0], breath[2]);

        // And it returns to where it began, so there is no restart to see.
        assert_eq!(pane_glyph(&[3000], SERVER_PULSE.len()), breath[0]);
    }

    #[test]
    fn a_serving_pane_carries_its_ports() {
        let mut snapshot = crate::client::shell::tests::snapshot();
        snapshot.agents = Vec::new();
        let processes = HashMap::from([("pane_1".to_string(), "bun".to_string())]);
        let ports = HashMap::from([("pane_1".to_string(), vec![3000u16, 3500])]);
        let rows = project_rows(
            &snapshot,
            &config(),
            &HashSet::new(),
            &HashSet::new(),
            &processes,
            &ports,
            true,
        );
        let ProjectRow::Pane { ports, .. } = &rows[1] else {
            panic!("expected a pane row");
        };
        assert_eq!(ports, &[3000, 3500]);

        // A pane serving nothing carries none rather than an empty placeholder.
        let rows = project_rows(
            &snapshot,
            &config(),
            &HashSet::new(),
            &HashSet::new(),
            &processes,
            &HashMap::new(),
            true,
        );
        let ProjectRow::Pane { ports, .. } = &rows[1] else {
            panic!("expected a pane row");
        };
        assert!(ports.is_empty());
    }

    #[test]
    fn a_pane_is_listed_once_and_shows_what_it_runs() {
        let mut snapshot = crate::client::shell::tests::snapshot();
        snapshot.agents = Vec::new();
        let processes = HashMap::from([("pane_1".to_string(), "nvim".to_string())]);
        let rows = project_rows(
            &snapshot,
            &config(),
            &HashSet::new(),
            &HashSet::new(),
            &processes,
            &HashMap::new(),
            true,
        );
        assert_eq!(kinds(&rows), ["workspace", "pane"]);
        let ProjectRow::Pane {
            process, pane_id, ..
        } = &rows[1]
        else {
            panic!("expected a pane row");
        };
        assert_eq!(process.as_deref(), Some("nvim"));
        assert_eq!(pane_id, "pane_1");

        // The same pane running an agent is the agent row, not a second row.
        snapshot.agents = vec![agent("pane_1", "ws_1")];
        let rows = project_rows(
            &snapshot,
            &config(),
            &HashSet::new(),
            &HashSet::new(),
            &processes,
            &HashMap::new(),
            true,
        );
        assert_eq!(kinds(&rows), ["workspace", "agent"]);
    }

    #[test]
    fn a_lone_agent_shows_without_a_fold() {
        let mut snapshot = crate::client::shell::tests::snapshot();
        snapshot.agents = vec![agent("pane_1", "ws_1")];
        let rows = project_rows(
            &snapshot,
            &config(),
            &HashSet::new(),
            &HashSet::new(),
            &HashMap::new(),
            &HashMap::new(),
            true,
        );
        assert_eq!(kinds(&rows), ["workspace", "agent"]);
    }
}
