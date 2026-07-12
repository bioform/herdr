//! Compose the outer terminal window/tab title from live agent state.
//!
//! Opt-in via `[terminal] set_window_title`. The composed title is a run of
//! per-agent status icons (one per agent pane, in sidebar order, animated while
//! working) followed by a body:
//!
//! - the live OSC title of the agent in the active tab — the one the user is
//!   looking at — with its leading spinner glyph stripped (the icon row already
//!   shows a spinner for it). With several agents the body follows the focused
//!   tab; with a single agent it is simply that agent.
//! - when the active tab holds no agent (a plain shell tab, or no agents at
//!   all) -> the idle fallback: the active workspace label, or the
//!   `[terminal] window_title_format` template when set.
//!
//! The tricky formatting lives in the pure `format_window_title` (unit-tested
//! without PTYs). `App::compute_window_title` is the thin gatherer that reads
//! pure `AppState` for the fleet plus the active agent's live OSC title from the
//! runtime registry (which is intentionally kept out of `AppState`).

use super::{App, AppState};
use crate::detect::AgentState;

/// Fallback title when nothing else applies. Matches the client's OSC-0 default
/// in `window_title_osc`, so clearing and idling converge on the same text.
const DEFAULT_TITLE: &str = "herdr";

impl App {
    /// Compose the outer window title for the current agent fleet and report
    /// whether the title is animating — any agent working (spinner) or blocked
    /// (breathing pulse) — so the caller can arm the animation tick without
    /// re-walking the fleet. `frame` advances both animations (once per ~100ms
    /// driver tick).
    ///
    /// The fleet and its states come from pure `AppState`; the active tab
    /// agent's live title comes from the runtime registry, which is
    /// deliberately not part of `AppState`.
    pub(crate) fn compute_window_title(&self, frame: u32) -> (String, bool) {
        let spinner = crate::ui::spinner_glyph_at(frame);
        let blocked = crate::ui::blocked_pulse_glyph_at(frame);
        let mut icons: Vec<&'static str> = Vec::new();
        let mut any_working = false;
        let mut any_blocked = false;
        // Sidebar order (workspaces -> tabs -> pane layout order) and the same
        // agent-pane filter as the sidebar's `navigator_pane_rows_for_tab`: a
        // detected or hook-reported agent label, or a user-assigned name — not a
        // bare launched command (which `is_agent_terminal` would also accept).
        for ws in &self.state.workspaces {
            for tab in &ws.tabs {
                for pane_id in tab.layout.pane_ids() {
                    let Some(pane) = ws.pane_state(pane_id) else {
                        continue;
                    };
                    let Some(terminal) = self.state.terminals.get(&pane.attached_terminal_id)
                    else {
                        continue;
                    };
                    if terminal.agent_name.is_none() && terminal.effective_agent_label().is_none() {
                        continue;
                    }
                    match terminal.state {
                        AgentState::Working => any_working = true,
                        AgentState::Blocked => any_blocked = true,
                        _ => {}
                    }
                    icons.push(crate::ui::agent_state_glyph(
                        terminal.state,
                        pane.seen,
                        spinner,
                        blocked,
                    ));
                }
            }
        }
        // The body mirrors the live OSC title of the agent in the active tab —
        // the one the user is currently looking at — so with several agents the
        // title follows the focused tab instead of a static label. A freshly
        // (re)started agent idling at its prompt may not have emitted an OSC
        // title yet; then fall back to that agent's own sidebar label (e.g.
        // `claude`) so the body stays agent-specific instead of dropping to the
        // workspace label — which, in a repo literally named `herdr`, reads as
        // the feature being broken. `None` only when the active tab holds no
        // agent at all (then `format_window_title` uses the workspace label).
        let active_title = active_tab_agent_terminal_id(&self.state).and_then(|id| {
            let osc = self
                .terminal_runtimes
                .get(&id)
                .map(|rt| rt.agent_osc_title())
                .filter(|title| !title.trim().is_empty());
            osc.or_else(|| self.state.terminals.get(&id).and_then(agent_fallback_title))
        });
        let title = format_window_title(
            &icons,
            active_title.as_deref(),
            &self.window_title_idle_fallback(),
        );
        (title, any_working || any_blocked)
    }

    /// The body shown when no agent is working: the active workspace label, or
    /// the `window_title_format` template (tokens `{workspace}`, `{tab}`).
    fn window_title_idle_fallback(&self) -> String {
        let ws = self
            .state
            .active
            .and_then(|idx| self.state.workspaces.get(idx));
        let workspace = ws
            .map(|ws| ws.display_name())
            .unwrap_or_else(|| DEFAULT_TITLE.to_string());
        let format = self.state.window_title_format.trim();
        if format.is_empty() {
            return workspace;
        }
        let tab = ws
            .and_then(|ws| ws.active_tab_display_name())
            .unwrap_or_default();
        substitute_window_title_tokens(format, &workspace, &tab)
    }
}

/// Terminal id of the agent whose live title the window body mirrors: the agent
/// pane in the active workspace's active tab. When that tab is split and the
/// focused pane is itself an agent, that focused agent wins; otherwise the first
/// agent pane in the tab is used. `None` when the active tab holds no agent pane
/// (agents in other tabs still get an icon; the body then falls back to the
/// workspace label). Uses the same agent-pane filter as the icon row, so the
/// body agent is always one of the rendered icons.
fn active_tab_agent_terminal_id(state: &AppState) -> Option<crate::terminal::TerminalId> {
    let ws = state.active.and_then(|idx| state.workspaces.get(idx))?;
    let tab = ws.active_tab()?;
    let focused = tab.layout.focused();
    let mut first_agent: Option<crate::terminal::TerminalId> = None;
    for pane_id in tab.layout.pane_ids() {
        let is_focused = pane_id == focused;
        let Some(pane) = ws.pane_state(pane_id) else {
            continue;
        };
        let Some(terminal) = state.terminals.get(&pane.attached_terminal_id) else {
            continue;
        };
        if terminal.agent_name.is_none() && terminal.effective_agent_label().is_none() {
            continue;
        }
        if is_focused {
            return Some(terminal.id.clone());
        }
        if first_agent.is_none() {
            first_agent = Some(terminal.id.clone());
        }
    }
    first_agent
}

/// A per-agent label for the window body when the active tab's agent has no live
/// OSC title yet (a freshly (re)started agent idling before it emits one).
/// Mirrors the sidebar's label chain (`navigator_pane_rows_for_tab`) so the title
/// stays agent-specific rather than dropping to the workspace label. Returns
/// `Some` for any real agent pane (the caller only reaches this for one whose
/// `agent_name`/`effective_agent_label` is set), so the empty-OSC case never
/// falls through to the workspace label.
fn agent_fallback_title(term: &crate::terminal::TerminalState) -> Option<String> {
    term.effective_title()
        .or_else(|| term.manual_label.as_deref().map(str::to_string))
        .or_else(|| term.agent_name.as_deref().map(str::to_string))
        .or_else(|| term.effective_agent_label().map(str::to_string))
}

/// Pure title composition from the already-resolved per-agent icons and the
/// active tab's agent live title. Kept free of `AppState`/PTYs so it is
/// exhaustively unit-testable. The body is that active agent's own title (its
/// leading spinner stripped, since the icon row already shows one); when there
/// is no active-agent title — no agents at all, or the active tab holds none —
/// it falls back to the idle label (the workspace label). The icon count never
/// changes the body, so it doesn't flip between a title and a static word as
/// agents come and go.
fn format_window_title(
    icons: &[&str],
    active_agent_title: Option<&str>,
    idle_fallback: &str,
) -> String {
    let body = active_agent_title
        .map(strip_leading_spinner)
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| idle_fallback.trim().to_string());
    let body = if body.is_empty() {
        DEFAULT_TITLE.to_string()
    } else {
        body
    };
    // One space between icons (they otherwise look sticky) and before the body.
    let icons = icons.join(" ");
    if icons.is_empty() {
        body
    } else {
        format!("{icons} {body}")
    }
}

/// Single-pass `{workspace}`/`{tab}` substitution. Unlike sequential
/// `str::replace`, a token-like substring inside a substituted value is left
/// intact (e.g. a workspace literally named `{tab}` is not rewritten), and
/// unknown tokens are kept verbatim.
fn substitute_window_title_tokens(format: &str, workspace: &str, tab: &str) -> String {
    let mut out = String::with_capacity(format.len());
    let mut rest = format;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let after = &rest[start..];
        match after.find('}') {
            Some(end) => {
                match &after[..=end] {
                    "{workspace}" => out.push_str(workspace),
                    "{tab}" => out.push_str(tab),
                    other => out.push_str(other),
                }
                rest = &after[end + 1..];
            }
            None => {
                out.push_str(after);
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Strip a single leading spinner-style glyph (and following spaces) from an
/// agent's own title. The composed icon row already renders a spinner for the
/// working agent, so this prevents a double spinner like `⠹ ⠐ Refactoring…`.
fn strip_leading_spinner(title: &str) -> &str {
    let trimmed = title.trim_start();
    let mut chars = trimmed.chars();
    match chars.next() {
        Some(first) if is_spinner_glyph(first) => chars.as_str().trim_start(),
        _ => trimmed,
    }
}

/// Glyphs coding agents commonly use as an animated leading "working" indicator
/// in their own tab title: the full Braille Patterns block (Claude and herdr
/// both animate with braille) plus a few star/sparkle pulses. Kept conservative
/// so we never strip the first character of ordinary title text.
fn is_spinner_glyph(c: char) -> bool {
    matches!(c, '\u{2800}'..='\u{28FF}')
        || matches!(c, '✶' | '✳' | '✻' | '✽' | '✢' | '✥' | '✦' | '✧' | '❋' | '❂')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_agents_uses_idle_fallback() {
        assert_eq!(format_window_title(&[], None, "my-project"), "my-project");
    }

    #[test]
    fn empty_fallback_defaults_to_herdr() {
        assert_eq!(format_window_title(&[], None, ""), "herdr");
    }

    #[test]
    fn single_agent_mirrors_live_title_and_strips_leading_spinner() {
        // A working agent's live OSC title already leads with a braille spinner.
        assert_eq!(
            format_window_title(
                &["⠹"],
                Some("⠐ Refactoring… (12s · esc to interrupt)"),
                "my-project",
            ),
            "⠹ Refactoring… (12s · esc to interrupt)"
        );
    }

    #[test]
    fn single_idle_agent_still_shows_its_title_not_the_fallback() {
        // Regression: a lone IDLE agent must show its own title, not the idle
        // fallback — which is the workspace label, often literally "herdr" and
        // therefore indistinguishable from the multi-agent body.
        assert_eq!(
            format_window_title(&["✓"], Some("Ready — /help for commands"), "herdr"),
            "✓ Ready — /help for commands"
        );
    }

    #[test]
    fn single_agent_without_title_falls_back_to_workspace() {
        assert_eq!(
            format_window_title(&["⠹"], None, "my-project"),
            "⠹ my-project"
        );
    }

    #[test]
    fn multiple_agents_show_active_tab_agent_title_with_one_space() {
        // The body mirrors the active tab's agent title (not the literal
        // `herdr`); icons stay one-space separated and the active agent's own
        // leading spinner is stripped.
        assert_eq!(
            format_window_title(
                &["⠹", "⠹", "⠹"],
                Some("⠐ Debugging tests (3s · esc to interrupt)"),
                "my-project",
            ),
            "⠹ ⠹ ⠹ Debugging tests (3s · esc to interrupt)"
        );
    }

    #[test]
    fn fleet_mixes_working_and_done_icons() {
        // Three agents (two working + one done); the body is the active tab's
        // agent title, regardless of how many are working.
        assert_eq!(
            format_window_title(&["⠹", "⠹", "●"], Some("Reviewing diff"), "my-project"),
            "⠹ ⠹ ● Reviewing diff"
        );
    }

    #[test]
    fn multiple_agents_without_active_agent_fall_back_to_workspace() {
        // No active-tab agent title (e.g. the active tab is a plain shell) ->
        // the body is the workspace label, not the literal `herdr`.
        assert_eq!(
            format_window_title(&["●", "●"], None, "my-project"),
            "● ● my-project"
        );
    }

    #[test]
    fn strip_leading_spinner_handles_braille_star_and_plain() {
        assert_eq!(strip_leading_spinner("⠐ Add feature"), "Add feature");
        assert_eq!(strip_leading_spinner("✻ Working"), "Working");
        assert_eq!(strip_leading_spinner("  ⠋   spaced"), "spaced");
        // No leading spinner: unchanged (trimmed).
        assert_eq!(strip_leading_spinner("Plain title"), "Plain title");
        // Does not eat ordinary leading punctuation/text.
        assert_eq!(strip_leading_spinner("* not a spinner"), "* not a spinner");
    }

    #[test]
    fn token_substitution_is_single_pass() {
        assert_eq!(
            substitute_window_title_tokens("{workspace}", "proj", "main"),
            "proj"
        );
        assert_eq!(
            substitute_window_title_tokens("{workspace} — {tab}", "proj", "main"),
            "proj — main"
        );
        // A workspace value that itself contains a token is not re-substituted.
        assert_eq!(
            substitute_window_title_tokens("{workspace}", "{tab}", "main"),
            "{tab}"
        );
        // Unknown tokens are left literal.
        assert_eq!(
            substitute_window_title_tokens("{unknown}", "proj", "main"),
            "{unknown}"
        );
    }

    /// Build a plain (non-agent) terminal for `pane` and register it in `state`.
    fn register_terminal(
        state: &mut AppState,
        ws: &crate::workspace::Workspace,
        pane: crate::layout::PaneId,
    ) -> crate::terminal::TerminalId {
        let tid = ws.tabs[0].panes[&pane].attached_terminal_id.clone();
        state.terminals.insert(
            tid.clone(),
            crate::terminal::TerminalState::new(tid.clone(), std::path::PathBuf::from("/tmp")),
        );
        tid
    }

    #[test]
    fn active_tab_agent_returns_the_sole_agent() {
        let mut state = AppState::test_new();
        let ws = crate::workspace::Workspace::test_new("proj");
        let pane = ws.tabs[0].root_pane;
        let tid = register_terminal(&mut state, &ws, pane);
        state
            .terminals
            .get_mut(&tid)
            .expect("terminal")
            .set_agent_name("claude".to_string());
        state.workspaces = vec![ws];
        state.active = Some(0);
        assert_eq!(active_tab_agent_terminal_id(&state), Some(tid));
    }

    #[test]
    fn active_tab_agent_none_when_active_tab_has_no_agent() {
        let mut state = AppState::test_new();
        let ws = crate::workspace::Workspace::test_new("proj");
        let pane = ws.tabs[0].root_pane;
        // A plain terminal (no agent name / label) is not an agent.
        register_terminal(&mut state, &ws, pane);
        state.workspaces = vec![ws];
        state.active = Some(0);
        assert_eq!(active_tab_agent_terminal_id(&state), None);
    }

    #[test]
    fn active_tab_agent_prefers_the_focused_agent() {
        let mut state = AppState::test_new();
        let mut ws = crate::workspace::Workspace::test_new("proj");
        let first = ws.tabs[0].root_pane;
        let second = ws.test_split(ratatui::layout::Direction::Horizontal);
        let first_tid = register_terminal(&mut state, &ws, first);
        let second_tid = register_terminal(&mut state, &ws, second);
        for tid in [&first_tid, &second_tid] {
            state
                .terminals
                .get_mut(tid)
                .expect("terminal")
                .set_agent_name("claude".to_string());
        }
        ws.tabs[0].layout.focus_pane(second);
        state.workspaces = vec![ws];
        state.active = Some(0);
        // Both panes are agents; the focused one wins over the first in order.
        assert_eq!(active_tab_agent_terminal_id(&state), Some(second_tid));
    }

    #[test]
    fn active_tab_agent_uses_tab_agent_when_focused_pane_is_not_an_agent() {
        let mut state = AppState::test_new();
        let mut ws = crate::workspace::Workspace::test_new("proj");
        let agent_pane = ws.tabs[0].root_pane;
        let shell_pane = ws.test_split(ratatui::layout::Direction::Horizontal);
        let agent_tid = register_terminal(&mut state, &ws, agent_pane);
        register_terminal(&mut state, &ws, shell_pane); // plain shell, not an agent
        state
            .terminals
            .get_mut(&agent_tid)
            .expect("terminal")
            .set_agent_name("claude".to_string());
        ws.tabs[0].layout.focus_pane(shell_pane);
        state.workspaces = vec![ws];
        state.active = Some(0);
        // The focused pane isn't an agent, but the tab has one -> show it.
        assert_eq!(active_tab_agent_terminal_id(&state), Some(agent_tid));
    }

    #[test]
    fn agent_fallback_title_uses_agent_name_when_no_osc_title() {
        // A restored agent idling before it emits an OSC title still yields an
        // agent-specific body (its name), never `None` — so `compute_window_title`
        // shows e.g. `claude`, not the workspace label.
        let id = crate::terminal::TerminalId::alloc();
        let mut term = crate::terminal::TerminalState::new(id, std::path::PathBuf::from("/tmp"));
        assert_eq!(
            agent_fallback_title(&term),
            None,
            "plain terminal has no label"
        );
        term.set_agent_name("claude".to_string());
        assert_eq!(agent_fallback_title(&term), Some("claude".to_string()));
    }
}
