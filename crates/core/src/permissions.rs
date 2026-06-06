use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PermissionDecision {
    Allow,
    Deny,
    Ask,
}

/// A user's reply to an interactive permission prompt (the runtime answer carried
/// on the prompt's response channel). Distinct from [`PermissionDecision`], which
/// is the *evaluation* result of rules and can never carry human feedback — a
/// rule-based deny stays feedback-less. A user `Deny` may attach feedback that is
/// surfaced to the model in the tool result so it can adjust course.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionReply {
    Allow,
    Deny { feedback: Option<String> },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PermissionRule {
    pub action: String,
    pub resource: String,
    pub decision: PermissionDecision,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionPrompt {
    pub permission_id: String,
    pub tool_name: String,
    pub action: String,
    pub resources: Vec<String>,
    pub preview: String,
}

pub fn match_action(pattern: &str, action: &str) -> bool {
    pattern == "*" || pattern == action
}

pub fn match_resource(pattern: &str, resource: &str) -> bool {
    if pattern == "*" {
        return true;
    }

    // Recursive wildcard matcher supporting '*' and '?'.
    fn match_recursive(p: &[char], i: &[char]) -> bool {
        if p.is_empty() {
            return i.is_empty();
        }
        if p[0] == '*' {
            if p.len() == 1 {
                return true;
            }
            for idx in 0..=i.len() {
                if match_recursive(&p[1..], &i[idx..]) {
                    return true;
                }
            }
            return false;
        }
        if i.is_empty() {
            return false;
        }
        if p[0] == '?' || p[0] == i[0] {
            return match_recursive(&p[1..], &i[1..]);
        }
        false
    }

    let p_vec: Vec<char> = pattern.chars().collect();
    let i_vec: Vec<char> = resource.chars().collect();
    match_recursive(&p_vec, &i_vec)
}

/// Is `resource` inside the workspace? Relative paths are inside by
/// construction. Absolute paths are resolved (symlinks too); when the target
/// doesn't exist yet (a new file), its nearest existing ancestor is resolved.
/// On any failure we conservatively answer `false` (treat as external → prompt),
/// which closes the lexical-`..` hole the previous `starts_with` had.
pub fn is_under_workspace(resource: &str, workspace: &Path) -> bool {
    let path = Path::new(resource);
    if path.is_relative() {
        return true;
    }
    let canonical_ws = match workspace.canonicalize() {
        Ok(w) => w,
        Err(_) => return false,
    };
    if let Ok(p) = path.canonicalize() {
        return p.starts_with(&canonical_ws);
    }
    // New file: resolve the nearest existing ancestor and re-attach the tail.
    for ancestor in path.ancestors().skip(1) {
        if let Ok(real) = ancestor.canonicalize() {
            return real.starts_with(&canonical_ws);
        }
    }
    false
}

/// Built-in agent permission rulesets. Ordered broad→specific because
/// evaluation is **last-match-wins** (a later, more specific rule overrides an
/// earlier one). Mirrors the reference's agent defaults (see plan.md §6A).
pub fn agent_default_rules(agent_id: &str) -> Vec<PermissionRule> {
    use PermissionDecision::*;
    let r = |a: &str, res: &str, d: PermissionDecision| PermissionRule {
        action: a.to_string(),
        resource: res.to_string(),
        decision: d,
    };
    match agent_id {
        "plan" => vec![
            r("*", "*", Ask),
            r("read_file", "*", Allow),
            r("glob", "*", Allow),
            r("grep", "*", Allow),
            r("write_file", "*", Deny),
            r("edit", "*", Deny),
            r("patch", "*", Deny),
            r("bash", "*", Ask),
            // .env secrets still prompt even for reads.
            r("read_file", "*.env", Ask),
            r("read_file", "*.env.*", Ask),
            r("read_file", "*.env.example", Allow),
        ],
        // Search-only subagent (the reference's "explore").
        "explore" => vec![
            r("*", "*", Deny),
            r("read_file", "*", Allow),
            r("glob", "*", Allow),
            r("grep", "*", Allow),
            r("web_fetch", "*", Allow),
            r("bash", "*", Allow),
            r("read_file", "*.env", Ask),
            r("read_file", "*.env.*", Ask),
            r("read_file", "*.env.example", Allow),
        ],
        // General helper: broad allow, but consequential actions prompt.
        "general" => vec![
            r("*", "*", Allow),
            r("write_file", "*", Ask),
            r("edit", "*", Ask),
            r("patch", "*", Ask),
            r("bash", "*", Ask),
            r("web_fetch", "*", Ask),
            r("read_file", "*.env", Ask),
            r("read_file", "*.env.*", Ask),
            r("read_file", "*.env.example", Allow),
        ],
        // "build" (default): reads allowed; mutating + bash + fetch prompt.
        _ => vec![
            r("read_file", "*", Allow),
            r("glob", "*", Allow),
            r("grep", "*", Allow),
            r("write_file", "*", Ask),
            r("edit", "*", Ask),
            r("patch", "*", Ask),
            r("bash", "*", Ask),
            r("web_fetch", "*", Ask),
            // .env carve-out: reading secrets prompts even though reads are allowed.
            r("read_file", "*.env", Ask),
            r("read_file", "*.env.*", Ask),
            r("read_file", "*.env.example", Allow),
        ],
    }
}

fn last_match(
    rules: &[PermissionRule],
    action: &str,
    resource: &str,
) -> Option<PermissionDecision> {
    rules
        .iter()
        .rev()
        .find(|r| match_action(&r.action, action) && match_resource(&r.resource, resource))
        .map(|r| r.decision)
}

/// Evaluate a permission decision. Algorithm (mirrors the reference):
/// 1. Last-match over the **agent** ruleset alone; a `Deny` short-circuits and
///    is **non-overridable** by saved rules (a security property).
/// 2. Last-match over `[...agent, ...saved]` (saved rules are coerced to `Allow`),
///    with an `ask` fallback when nothing matches.
/// 3. A path action resolving **outside** the workspace prompts (external_directory).
pub fn evaluate(
    action: &str,
    resource: &str,
    agent_id: &str,
    workspace_path: &Path,
    saved_rules: &[PermissionRule],
) -> PermissionDecision {
    let agent_rules = agent_default_rules(agent_id);

    // 1. Agent-deny short-circuit (non-overridable).
    if last_match(&agent_rules, action, resource) == Some(PermissionDecision::Deny) {
        return PermissionDecision::Deny;
    }

    // 2. Combined last-match (saved rules coerced to Allow), ask fallback.
    let mut combined = agent_rules;
    for s in saved_rules {
        combined.push(PermissionRule {
            action: s.action.clone(),
            resource: s.resource.clone(),
            decision: PermissionDecision::Allow,
        });
    }
    let mut decision = last_match(&combined, action, resource).unwrap_or(PermissionDecision::Ask);

    // 3. External-directory gating: a path action outside the workspace prompts.
    if matches!(action, "read_file" | "write_file" | "edit" | "patch")
        && !is_under_workspace(resource, workspace_path)
        && decision == PermissionDecision::Allow
    {
        decision = PermissionDecision::Ask;
    }

    decision
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_wildcard_matching() {
        assert!(match_resource("*", "anything"));
        assert!(match_resource("*.rs", "main.rs"));
        assert!(match_resource("/path/to/*", "/path/to/file.rs"));
        assert!(!match_resource("/path/to/*", "/path/other/file.rs"));
        assert!(match_resource("src/???/lib.rs", "src/foo/lib.rs"));
        assert!(match_resource("*.env", "config/.env"));
        assert!(match_resource("*.env.example", ".env.example"));
        assert!(!match_resource("*.env", ".env.example"));
    }

    #[test]
    fn test_evaluate_defaults() {
        let ws = Path::new("/workspace");
        let rules = vec![];
        assert_eq!(
            evaluate("read_file", "src/main.rs", "plan", ws, &rules),
            PermissionDecision::Allow
        );
        assert_eq!(
            evaluate("write_file", "src/main.rs", "plan", ws, &rules),
            PermissionDecision::Deny
        );
        assert_eq!(
            evaluate("read_file", "src/main.rs", "build", ws, &rules),
            PermissionDecision::Allow
        );
        assert_eq!(
            evaluate("write_file", "src/main.rs", "build", ws, &rules),
            PermissionDecision::Ask
        );
        assert_eq!(
            evaluate("bash", "cargo build", "build", ws, &rules),
            PermissionDecision::Ask
        );
    }

    #[test]
    fn test_env_carveout() {
        let ws = Path::new("/workspace");
        let rules = vec![];
        // Reading a .env secret prompts even though reads are otherwise allowed.
        assert_eq!(
            evaluate("read_file", "config/.env", "build", ws, &rules),
            PermissionDecision::Ask
        );
        assert_eq!(
            evaluate("read_file", ".env.local", "build", ws, &rules),
            PermissionDecision::Ask
        );
        // .env.example is harmless and allowed.
        assert_eq!(
            evaluate("read_file", ".env.example", "build", ws, &rules),
            PermissionDecision::Allow
        );
    }

    #[test]
    fn test_agent_deny_not_overridable_by_saved() {
        let ws = Path::new("/workspace");
        // A saved "always allow" must NOT override a plan-agent write deny.
        let saved = vec![PermissionRule {
            action: "write_file".into(),
            resource: "*".into(),
            decision: PermissionDecision::Allow,
        }];
        assert_eq!(
            evaluate("write_file", "src/main.rs", "plan", ws, &saved),
            PermissionDecision::Deny
        );
    }

    #[test]
    fn test_saved_rule_last_match() {
        let ws = Path::new("/workspace");
        // Saved allow turns an otherwise-Ask bash into Allow (last match wins).
        let saved = vec![PermissionRule {
            action: "bash".into(),
            resource: "cargo *".into(),
            decision: PermissionDecision::Allow,
        }];
        assert_eq!(
            evaluate("bash", "cargo test", "build", ws, &saved),
            PermissionDecision::Allow
        );
        assert_eq!(
            evaluate("bash", "rm -rf /", "build", ws, &saved),
            PermissionDecision::Ask
        );
    }

    #[test]
    fn test_explore_is_search_only() {
        let ws = Path::new("/workspace");
        let rules = vec![];
        assert_eq!(
            evaluate("read_file", "src/main.rs", "explore", ws, &rules),
            PermissionDecision::Allow
        );
        assert_eq!(
            evaluate("grep", "TODO", "explore", ws, &rules),
            PermissionDecision::Allow
        );
        // explore denies writes non-overridably.
        assert_eq!(
            evaluate("write_file", "src/main.rs", "explore", ws, &rules),
            PermissionDecision::Deny
        );
    }
}
