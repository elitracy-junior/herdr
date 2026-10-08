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
}

/// Columns an agent row and its fold sit in, relative to the Space row that
/// owns them. Deeper than the Space's own indent so the nesting reads one way.
pub(super) const AGENT_INDENT: u16 = 4;

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
        };
        lines.max(1).min(u16::MAX as usize) as u16
    }

    /// A workspace row starting a new top-level group gets the configured gap;
    /// nested rows stay packed against the row they belong to.
    pub(super) fn starts_group(&self) -> bool {
        matches!(self, Self::Workspace(entry) if !entry.indented)
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
    include_agents: bool,
) -> Vec<ProjectRow> {
    let entries = super::sidebar::workspace_entries(snapshot, collapsed_groups);
    if !include_agents {
        return entries.into_iter().map(ProjectRow::Workspace).collect();
    }
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
        // A Space's own state icon is derived from its agents, so a Space with
        // a single agent would say the same thing twice -- which is most Spaces.
        // List agents only where there are several and the Space icon can no
        // longer speak for all of them.
        if pane_ids.len() < 2 {
            continue;
        }
        let collapsed = collapsed_agent_lists.contains(&workspace_id);
        rows.push(ProjectRow::AgentFold {
            workspace_id: workspace_id.clone(),
            count: pane_ids.len(),
            collapsed,
        });
        if collapsed {
            continue;
        }
        for pane_id in pane_ids {
            if let Some(row) = agent_row_with(snapshot, &pane_id, None, &nested) {
                rows.push(ProjectRow::Agent(Box::new(row)));
            }
        }
    }
    rows
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
            })
            .collect()
    }

    #[test]
    fn nested_agent_rows_do_not_repeat_their_workspace() {
        use crate::ui::ResolvedTokenKind;
        let mut snapshot = crate::client::shell::tests::snapshot();
        snapshot.agents = vec![agent("pane_1", "ws_1"), agent("pane_2", "ws_1")];
        let rows = project_rows(&snapshot, &config(), &HashSet::new(), &HashSet::new(), true);
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
    fn without_agents_the_tree_is_exactly_the_spaces_list() {
        let mut snapshot = crate::client::shell::tests::snapshot();
        snapshot.agents = vec![agent("pane_1", "ws_1")];
        let rows = project_rows(
            &snapshot,
            &config(),
            &HashSet::new(),
            &HashSet::new(),
            false,
        );
        assert_eq!(kinds(&rows), ["workspace"]);
    }

    #[test]
    fn agents_nest_under_their_own_workspace_behind_a_fold() {
        let mut snapshot = crate::client::shell::tests::snapshot();
        snapshot.agents = vec![agent("pane_1", "ws_1"), agent("pane_2", "ws_1")];
        let rows = project_rows(&snapshot, &config(), &HashSet::new(), &HashSet::new(), true);
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
        let rows = project_rows(&snapshot, &config(), &HashSet::new(), &collapsed, true);
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
        let rows = project_rows(&snapshot, &config(), &HashSet::new(), &HashSet::new(), true);
        assert_eq!(kinds(&rows), ["workspace"]);
    }

    #[test]
    fn a_lone_agent_is_left_to_its_space_icon() {
        let mut snapshot = crate::client::shell::tests::snapshot();
        snapshot.agents = vec![agent("pane_1", "ws_1")];
        let rows = project_rows(&snapshot, &config(), &HashSet::new(), &HashSet::new(), true);
        assert_eq!(kinds(&rows), ["workspace"]);
    }
}
