//! Identify which agent is running in a pane from its process names.
//!
//! This is presence detection, not state detection: it answers "is an agent
//! running here, and which one" so the sidebar can show it the moment it
//! launches — without depending on that agent's own lifecycle hooks firing.
//! The agent's *state* (working/blocked/idle) still comes from its hooks.
//!
//! Matching is deliberately conservative. A name only counts when it is a
//! recognizable agent binary, so a plain shell never produces a phantom row.

/// Canonical agent labels keyed by the process names that indicate them. The
/// label is what the sidebar shows and what integrations pass to
/// `report-state --agent`, so detected and reported rows agree.
///
/// Generic names (a bare `pi`, `omp`, …) are deliberately excluded: they are
/// likelier to collide with an unrelated binary than to identify an agent, and
/// a false row is worse than a missing one — the agent's own hook still adds it.
const AGENT_NAMES: &[(&str, &str)] = &[
    ("claude", "claude"),
    ("codex", "codex"),
    ("copilot", "copilot"),
    ("gh-copilot", "copilot"),
    ("github-copilot", "copilot"),
    ("gemini", "gemini"),
    ("cursor", "cursor"),
    ("cursor-agent", "cursor"),
    ("opencode", "opencode"),
    ("aider", "aider"),
    ("amp", "amp"),
    ("droid", "droid"),
    ("cline", "cline"),
    ("kimi", "kimi"),
    ("kiro", "kiro"),
    ("grok", "grok"),
    ("devin", "devin"),
    ("hermes", "hermes"),
    ("kilo", "kilo"),
    ("qodercli", "qodercli"),
    ("mastracode", "mastracode"),
    ("antigravity", "agy"),
];

/// Reduce a raw process name to its comparable form: basename, no `.exe`
/// suffix, lowercase. Windows reports `codex.exe`; macOS `ps` reports a path.
fn normalize(process_name: &str) -> String {
    let base = process_name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(process_name);
    let base = base.strip_suffix(".exe").unwrap_or(base);
    base.trim().to_ascii_lowercase()
}

/// The canonical agent label for a single process name, if it names an agent.
pub fn identify_agent(process_name: &str) -> Option<&'static str> {
    let name = normalize(process_name);
    AGENT_NAMES
        .iter()
        .find(|(binary, _)| *binary == name)
        .map(|(_, label)| *label)
}

/// The agent running among a pane's process names, if any.
///
/// Names are expected in breadth-first order from the pane's shell, so the
/// shallowest agent wins when a session nests (an agent shelling out to
/// another). Non-agent processes — shells, `node`, build tools — are ignored.
pub fn identify_agent_among<I, S>(process_names: I) -> Option<&'static str>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    process_names
        .into_iter()
        .find_map(|name| identify_agent(name.as_ref()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_known_agents() {
        assert_eq!(identify_agent("codex"), Some("codex"));
        assert_eq!(identify_agent("claude"), Some("claude"));
        assert_eq!(identify_agent("gemini"), Some("gemini"));
    }

    #[test]
    fn normalizes_paths_case_and_exe_suffix() {
        assert_eq!(identify_agent("/usr/bin/codex"), Some("codex"));
        assert_eq!(identify_agent("C:\\tools\\Codex.exe"), Some("codex"));
        assert_eq!(identify_agent("CLAUDE"), Some("claude"));
        assert_eq!(identify_agent("  codex  "), Some("codex"));
    }

    #[test]
    fn copilot_aliases_share_one_label() {
        for name in ["copilot", "gh-copilot", "github-copilot", "copilot.exe"] {
            assert_eq!(identify_agent(name), Some("copilot"), "for {name}");
        }
    }

    #[test]
    fn shells_and_runtimes_are_not_agents() {
        for name in [
            "sh", "bash", "zsh", "fish", "node", "python3", "cmd.exe", "pwsh", "powershell.exe",
            "vim", "cargo", "git",
        ] {
            assert_eq!(identify_agent(name), None, "{name} must not match");
        }
    }

    #[test]
    fn generic_short_names_are_excluded_to_avoid_false_rows() {
        // Deliberately unmatched — see AGENT_NAMES' doc comment.
        assert_eq!(identify_agent("pi"), None);
        assert_eq!(identify_agent("omp"), None);
    }

    #[test]
    fn picks_the_first_agent_in_a_descendant_list() {
        // BFS order from the pane's shell: the runtime wrapper comes first but
        // isn't an agent; the real agent below it wins.
        let names = ["node", "codex", "rg"];
        assert_eq!(identify_agent_among(names), Some("codex"));
    }

    #[test]
    fn no_agent_in_a_plain_shell_pane() {
        let names = ["vim", "less", "git"];
        assert_eq!(identify_agent_among(names), None);
        assert_eq!(identify_agent_among(Vec::<String>::new()), None);
    }
}
