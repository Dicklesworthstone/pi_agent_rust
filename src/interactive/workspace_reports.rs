//! Workspace slash commands shared by the interactive stacks: `/rules`,
//! `/omfg`, `/commit`, `/review`, `/handoff`, `/approval`, `/advisor`, and
//! (default stack) `/memory`, `/hub`, `/security`, `/plugins`. Each takes
//! what it reads (working directory, session, approval state, package
//! manager) and returns what to show; the stacks differ only in where they
//! put it.

use std::fmt::Write as _;
use std::path::Path;

/// A command's result: an optional transcript card plus a one-line status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub card: Option<String>,
    pub status: String,
}

impl Report {
    fn status(status: impl Into<String>) -> Self {
        Self {
            card: None,
            status: status.into(),
        }
    }

    fn card(card: String, status: impl Into<String>) -> Self {
        Self {
            card: Some(card),
            status: status.into(),
        }
    }
}

/// `/rules [list|remove <id>|toggle <id>]`: the project's TTSR stream rules.
pub fn rules(cwd: &Path, args: &str) -> Report {
    let args = args.trim();
    let mut store = crate::stream_rules::StreamRuleStore::load_for_project(cwd);
    if args.is_empty() || args == "list" {
        let rules = store.list_all_rules();
        let mut text = format!("### 🛡️ Active Stream Rules ({})\n\n", rules.len());
        if rules.is_empty() {
            text.push_str("No stream rules configured. Use `/rules add <id> <pattern> <body>` or `/omfg <complaint>` to create one.\n");
        } else {
            for r in &rules {
                let status = if r.enabled {
                    "✅ enabled"
                } else {
                    "⏸️ disabled"
                };
                let _ = writeln!(
                    text,
                    "- **{}** [{status}]: `/{}/`\n  {}",
                    r.name, r.pattern, r.body
                );
            }
        }
        let count = rules.len();
        return Report::card(text, format!("{count} stream rule(s)"));
    }
    if let Some(rest) = args.strip_prefix("remove ") {
        let id = rest.trim();
        return Report::status(match store.remove_rule(id) {
            Ok(true) => format!("Removed stream rule '{id}'"),
            Ok(false) => format!("Stream rule '{id}' not found"),
            Err(e) => format!("Error removing rule: {e}"),
        });
    }
    if let Some(rest) = args.strip_prefix("toggle ") {
        let id = rest.trim();
        let current = store
            .list_all_rules()
            .into_iter()
            .find(|r| r.id == id)
            .is_none_or(|r| r.enabled);
        return Report::status(match store.toggle_rule(id, !current) {
            Ok(true) => {
                let st = if current { "disabled" } else { "enabled" };
                format!("Stream rule '{id}' is now {st}")
            }
            _ => format!("Stream rule '{id}' not found"),
        });
    }
    Report::status("Usage: /rules [list|remove <id>|toggle <id>]")
}

/// `/omfg <complaint>`: log the grievance and forge an active stream rule.
pub fn omfg(cwd: &Path, args: &str) -> Report {
    let args = args.trim();
    if args.is_empty() {
        return Report::status("Usage: /omfg <complaint about model behavior>");
    }
    match crate::stream_rules::GrievancesLedger::record_complaint(cwd, args, None) {
        Ok(g) => {
            let candidate = crate::stream_rules::GrievancesLedger::forge_candidate_rule(&g);
            let mut store = crate::stream_rules::StreamRuleStore::load_for_project(cwd);
            let _ = store.add_rule(candidate.clone(), false);
            let card = format!(
                "### 📝 Grievance Logged & Stream Rule Forged\n\n\
                 - **Grievance ID:** `{gid}`\n\
                 - **Complaint:** {complaint}\n\n\
                 **Generated TTSR Stream Rule (`{rid}`):**\n\
                 - **Name:** {name}\n\
                 - **Pattern:** `/{pattern}/`\n\
                 - **Directive:** {body}\n\n\
                 *Rule is now active for this project and will abort & retry if this pattern occurs mid-stream.*",
                gid = g.id,
                complaint = g.complaint,
                rid = candidate.id,
                name = candidate.name,
                pattern = candidate.pattern,
                body = candidate.body,
            );
            Report::card(
                card,
                format!("Forged and activated stream rule '{}'", candidate.id),
            )
        }
        Err(e) => Report::status(format!("Failed to record grievance: {e}")),
    }
}

/// Paths from `git status --porcelain` output. Each line is `XY <path>` (or
/// `XY <old> -> <new>` for a rename); X is a space for an unstaged change,
/// so the line must not be trimmed before the path is sliced off.
fn porcelain_paths(status: &str) -> Vec<String> {
    status
        .lines()
        .filter_map(|line| line.get(3..))
        .map(|path| {
            path.split_once(" -> ")
                .map_or(path, |(_, new)| new)
                .trim()
                .to_string()
        })
        .filter(|path| !path.is_empty())
        .collect()
}

/// `/commit [--dry-run|-n|plan] [--include-lockfiles]`: split the working
/// tree's changes into atomic commits, or only show the plan.
pub fn commit(cwd: &Path, args: &str) -> Report {
    let args = args.trim();
    let dry_run = args == "dry-run"
        || args == "plan"
        || args
            .split_whitespace()
            .any(|arg| arg == "--dry-run" || arg == "-n");
    let include_lockfiles = args
        .split_whitespace()
        .any(|arg| arg == "--include-lockfiles");

    let status_out = match std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(cwd)
        .output()
    {
        Ok(o) => o,
        Err(e) => return Report::status(format!("Failed to run git status: {e}")),
    };
    let changed_files = porcelain_paths(&String::from_utf8_lossy(&status_out.stdout));
    if changed_files.is_empty() {
        return Report::status("Working tree clean; nothing to commit.");
    }

    let hunks = std::process::Command::new("git")
        .args(["diff", "HEAD"])
        .current_dir(cwd)
        .output()
        .ok()
        .and_then(|out| {
            crate::commit_split::DiffParser::parse_unified_diff(&String::from_utf8_lossy(
                &out.stdout,
            ))
            .ok()
        })
        .unwrap_or_default();

    let options = crate::commit_split::CommitOptions {
        dry_run,
        include_lockfiles,
        all_untracked: false,
        bead_reference: None,
        custom_prefix: None,
    };
    let plan = match crate::commit_split::CommitPlanner::plan(&hunks, &changed_files, &options) {
        Ok(plan) => plan,
        Err(e) => return Report::status(format!("Failed to plan commits: {e}")),
    };
    if plan.units.is_empty() {
        return Report::status("No eligible files to commit.");
    }

    let mut card = format!("### 📦 Planned Atomic Commits ({})\n\n", plan.units.len());
    for (idx, unit) in plan.units.iter().enumerate() {
        let msg = unit.formatted_message(None);
        let _ = writeln!(card, "{}. **{}** (`{}`)", idx + 1, msg, unit.scope);
        for f in &unit.files {
            let _ = writeln!(card, "   - `{f}`");
        }
    }
    if dry_run {
        card.push_str("\n*Dry run: no commits were created.*");
    } else {
        match crate::commit_split::CommitExecutor::execute(cwd, &plan, &options) {
            Ok(results) => {
                let successful = results.iter().filter(|r| r.success).count();
                let _ = writeln!(
                    card,
                    "\n\n**Committed {successful}/{} units successfully.**",
                    plan.units.len()
                );
                for res in results {
                    if let Some(ref sha) = res.commit_sha {
                        let _ = writeln!(card, "- `[{sha}]` {}", res.message);
                    }
                }
            }
            Err(e) => {
                let _ = write!(card, "\n\n**Error executing commits:** {e}");
            }
        }
    }
    let units = plan.units.len();
    Report::card(card, format!("Generated commit plan with {units} units"))
}

/// `/review [target]`: heuristic code review of the working tree (or target).
pub fn review(cwd: &Path, args: &str) -> Report {
    let args = args.trim();
    let options = crate::review::ReviewOptions {
        target: (!args.is_empty()).then(|| args.to_string()),
        fail_on: None,
        confidence_threshold: 0.70,
        format: "markdown".to_string(),
        max_findings: 15,
        out_file: None,
    };
    match crate::review::CodeReviewer::review(cwd, &options) {
        Ok(report) => Report::card(
            report.format_markdown(),
            format!("{}: {}", report.verdict.badge(), report.summary),
        ),
        Err(e) => Report::status(format!("Review failed: {e}")),
    }
}

/// `/handoff [human|agent|<target>] [path]`: a handoff brief of the session.
pub fn handoff(session: &crate::session::Session, args: &str) -> Report {
    let args = args.trim();
    let (to_target, out_path) = if args.is_empty() {
        (crate::handoff::HandoffTarget::Human, None)
    } else {
        let mut parts = args.split_whitespace();
        let target_str = parts.next().unwrap_or("human");
        let path_str = parts.next().map(std::path::PathBuf::from);
        (crate::handoff::HandoffTarget::parse(target_str), path_str)
    };
    let doc = crate::handoff::HandoffGenerator::generate_from_session(session);
    match crate::handoff::HandoffGenerator::deliver(&doc, &to_target, out_path.as_deref()) {
        Ok(report) => Report::card(
            format!(
                "### 📋 Handoff Brief Generated\n\n{}\n\n*{}*",
                doc.to_markdown(),
                report.status
            ),
            "Handoff brief generated successfully",
        ),
        Err(e) => Report::status(format!("Failed to generate handoff: {e}")),
    }
}

const MEMORY_USAGE: &str = "Usage: /memory [view|list|search <query>|forget <id>]";

/// `/memory [view|list|search <query>|forget <id>]`: this project's memory
/// bank (bd-cv653.4.1). `view` (the default) is the mental model the agent
/// is given at session start.
pub fn memory(cwd: &Path, args: &str) -> Report {
    match crate::memory::MemoryStore::open(cwd) {
        Ok(store) => memory_in(&store, args),
        Err(e) => Report::status(format!("Memory bank unavailable: {e}")),
    }
}

fn memory_in(store: &crate::memory::MemoryStore, args: &str) -> Report {
    let args = args.trim();
    let (verb, rest) = args.split_once(char::is_whitespace).unwrap_or((args, ""));
    let rest = rest.trim();
    let listing = |title: &str, memories: Vec<crate::memory::Memory>| {
        if memories.is_empty() {
            return Report::status(format!("{title}: none"));
        }
        let mut card = format!("### 🧠 {title} ({})\n\n", memories.len());
        for m in &memories {
            let _ = write!(card, "- `#{}` [{}] {}", m.id, m.kind, m.content);
            if !m.tags.is_empty() {
                let _ = write!(card, " _({})_", m.tags.join(", "));
            }
            card.push('\n');
        }
        Report::card(card, format!("{title}: {}", memories.len()))
    };
    match verb.to_ascii_lowercase().as_str() {
        "" | "view" => match store.mental_model() {
            Ok(model) if model.is_empty() => {
                Report::status("Memory bank is empty for this project.")
            }
            Ok(model) => Report::card(
                format!("### 🧠 Project memory\n\n{model}"),
                "Project memory",
            ),
            Err(e) => Report::status(format!("Memory view failed: {e}")),
        },
        "list" => match store.list(20) {
            Ok(memories) => listing("Recent memories", memories),
            Err(e) => Report::status(format!("Memory list failed: {e}")),
        },
        "search" if !rest.is_empty() => match store.recall(rest, Some(10)) {
            Ok(memories) => listing(&format!("Memories matching \"{rest}\""), memories),
            Err(e) => Report::status(format!("Memory search failed: {e}")),
        },
        "forget" => rest.trim_start_matches('#').parse::<i64>().map_or_else(
            |_| Report::status(MEMORY_USAGE),
            |id| match store.edit(id, crate::memory::MemoryEditOp::Forget, None) {
                Ok(()) => Report::status(format!("Forgot memory #{id}")),
                Err(e) => Report::status(format!("Could not forget #{id}: {e}")),
            },
        ),
        _ => Report::status(MEMORY_USAGE),
    }
}

/// `/security [paths...]`: the native source scan (bd-cv653.2.6) over the
/// workspace or the given paths, with recorded dispositions applied, as the
/// `security_scan` tool reports it.
pub fn security(cwd: &Path, args: &str) -> Report {
    const SHOWN: usize = 30;
    let paths: Vec<String> = args.split_whitespace().map(str::to_string).collect();
    let findings = match crate::security_scan::run_scan(cwd, &paths) {
        Ok(findings) => findings,
        Err(e) => return Report::status(format!("Security scan failed: {e}")),
    };
    let dispositions = match crate::security_scan::load_dispositions(cwd) {
        Ok(dispositions) => dispositions,
        Err(e) => return Report::status(format!("Security dispositions unreadable: {e}")),
    };
    let (active, suppressed) =
        crate::security_scan::partition_by_disposition(findings, &dispositions);
    if active.is_empty() {
        return Report::status(format!(
            "Security scan: no findings ({} suppressed by dispositions).",
            suppressed.len()
        ));
    }
    let mut card = format!("### 🔒 Security scan: {} finding(s)", active.len());
    if !suppressed.is_empty() {
        let _ = write!(card, ", {} suppressed", suppressed.len());
    }
    card.push_str("\n\n");
    for finding in active.iter().take(SHOWN) {
        let _ = writeln!(
            card,
            "- [{}] `{}:{}` {} (`{}`)",
            finding.severity, finding.path, finding.line, finding.message, finding.rule_id
        );
    }
    if active.len() > SHOWN {
        let _ = writeln!(card, "- … {} more", active.len() - SHOWN);
    }
    Report::card(card, format!("{} security finding(s)", active.len()))
}

/// `/plugins`: the packages (extensions, skills, prompts, themes) installed
/// at user and project scope.
pub fn plugins(manager: &crate::package_manager::PackageManager) -> Report {
    use crate::package_manager::PackageScope;
    let packages = match manager.list_packages_blocking() {
        Ok(packages) => packages,
        Err(e) => return Report::status(format!("Could not list packages: {e}")),
    };
    if packages.is_empty() {
        return Report::status("No packages installed. `pi install <source>` adds one.");
    }
    let mut card = format!("### 📦 Installed packages ({})\n\n", packages.len());
    for package in &packages {
        let scope = match package.scope {
            PackageScope::User => "user",
            PackageScope::Project => "project",
            PackageScope::Temporary => "temporary",
        };
        let _ = writeln!(card, "- `{}` ({scope})", package.source);
    }
    card.push_str(
        "\n`pi install <source>` / `pi remove <source>` manage them; `/reload` applies changes.",
    );
    Report::card(card, format!("{} package(s)", packages.len()))
}

/// `/hub [id]`: this session's subagent children (bd-cv653.5.3), or one
/// child's transcript tail.
pub fn hub(args: &str) -> Report {
    let registry = crate::agent_hub::registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let id = args.trim();
    if id.is_empty() {
        return format_roster(&registry.roster());
    }
    match registry.transcript_page(id) {
        Ok(page) if page.trim().is_empty() => Report::status(format!("{id}: no transcript yet")),
        Ok(page) => Report::card(format!("### {id} transcript (tail)\n\n{page}"), id),
        Err(e) => Report::status(e.to_string()),
    }
}

fn format_roster(roster: &[crate::agent_hub::ChildEntry]) -> Report {
    if roster.is_empty() {
        return Report::status(
            "No subagents in this session (the subagent tool and /tan spawn them).",
        );
    }
    let mut card = format!("### Agent hub ({})\n\n", roster.len());
    for child in roster {
        let _ = writeln!(
            card,
            "- `{}` [{}] {} · {}",
            child.id,
            child.kind.as_str(),
            child.status.as_str(),
            child.task
        );
    }
    card.push_str("\n`/hub <id>` shows a child's transcript.");
    let running = roster.iter().filter(|c| !c.status.settled()).count();
    Report::card(
        card,
        format!("{} children, {running} running", roster.len()),
    )
}

/// OMP `/advisor [toggle|on|off|status]`: bare toggles. `configured` is the
/// advisor role's model spec, when one is assigned. `pause`/`resume` stay
/// accepted as aliases of `off`/`on`.
pub fn advisor(configured: Option<&str>, args: &str) -> Report {
    advisor_with(&crate::advisor::ADVISOR_PAUSED, configured, args)
}

fn advisor_with(
    paused: &std::sync::atomic::AtomicBool,
    configured: Option<&str>,
    args: &str,
) -> Report {
    use std::sync::atomic::Ordering;
    let enable = match args.trim().to_ascii_lowercase().as_str() {
        "" | "toggle" => paused.load(Ordering::SeqCst),
        "on" | "resume" => true,
        "off" | "pause" => false,
        "status" => {
            return Report::status(match (configured, paused.load(Ordering::SeqCst)) {
                (Some(spec), false) => format!("Advisor: on ({spec})"),
                (Some(spec), true) => format!("Advisor: off ({spec} assigned)"),
                (None, _) => String::from("Advisor: no model is assigned to the 'advisor' role"),
            });
        }
        other => {
            return Report::status(format!(
                "Unknown /advisor subcommand {other:?}: use /advisor [on|off|status]"
            ));
        }
    };
    paused.store(!enable, Ordering::SeqCst);
    Report::status(match (enable, configured) {
        (false, _) => String::from("Advisor disabled."),
        (true, Some(_)) => String::from("Advisor enabled."),
        (true, None) => {
            String::from("Advisor enabled, but no model is assigned to the 'advisor' role.")
        }
    })
}

/// `/approval [status|always-ask|write|yolo]`. Returns the new mode when it
/// changed, so the caller can record the transition in the session.
pub fn approval(
    state: &crate::approval::ApprovalState,
    args: &str,
) -> (Report, Option<crate::approval::ApprovalMode>) {
    use crate::approval::ApprovalMode;
    let (mode, message) = match args.trim().to_ascii_lowercase().as_str() {
        "" | "status" => {
            let classes = state.dual_confirm_classes();
            let dual = if classes.is_empty() {
                String::from("none")
            } else {
                classes
                    .iter()
                    .map(|c| c.label())
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            return (
                Report::status(format!(
                    "Approval mode: {} | Dual-confirm classes: {dual}",
                    state.mode().as_str()
                )),
                None,
            );
        }
        "always-ask" | "always_ask" | "always" | "ask" => {
            (ApprovalMode::AlwaysAsk, "Approval mode set to always-ask")
        }
        "write" | "files" => (
            ApprovalMode::Write,
            "Approval mode set to write (file mutations auto-approved)",
        ),
        "yolo" | "auto-approve" | "auto" | "all" => (
            ApprovalMode::Yolo,
            "Approval mode set to yolo (all auto-approved except hard policy gates)",
        ),
        other => {
            return (
                Report::status(format!(
                    "Unknown /approval mode {other:?}: use /approval [always-ask|write|yolo|status]"
                )),
                None,
            );
        }
    };
    state.set_mode(mode);
    (Report::status(message), Some(mode))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `/memory` over a real (per-test) project bank: retained facts are
    /// listed and searchable, and `forget` removes one.
    #[test]
    fn memory_lists_searches_and_forgets() {
        let root = std::env::temp_dir().join(format!(
            "pi-memory-report-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&root).expect("root");
        let store = crate::memory::MemoryStore::open(&root).expect("open");
        assert_eq!(
            memory_in(&store, "").status,
            "Memory bank is empty for this project."
        );
        let kept = store
            .retain(
                crate::memory::MemoryKind::Fact,
                "the parser lives in src/parser.rs",
                &["layout".to_string()],
                None,
            )
            .expect("retain");
        let listed = memory_in(&store, "list");
        assert!(
            listed
                .card
                .as_deref()
                .is_some_and(|card| card.contains("src/parser.rs")),
            "{listed:?}"
        );
        assert!(memory_in(&store, "search parser").card.is_some());
        assert_eq!(memory_in(&store, "search").status, MEMORY_USAGE);
        assert_eq!(
            memory_in(&store, &format!("forget #{}", kept.id)).status,
            format!("Forgot memory #{}", kept.id)
        );
        assert_eq!(memory_in(&store, "list").status, "Recent memories: none");
    }

    /// A planted secret-shaped line is reported with its location; a clean
    /// tree reports no findings.
    #[test]
    fn security_reports_findings_with_their_location() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("clean.rs"), "fn main() {}\n").expect("write");
        let clean = security(dir.path(), "");
        assert!(
            clean.status.starts_with("Security scan: no findings"),
            "{clean:?}"
        );

        std::fs::write(
            dir.path().join("leak.py"),
            // ubs:ignore planted fake key for the scanner under test
            "import os\napi_key = \"abcdefghijklmnopqrstuvwxyz0123\"\n",
        )
        .expect("write");
        let found = security(dir.path(), "");
        assert!(
            found.status.ends_with("security finding(s)"),
            "{}",
            found.status
        );
        let card = found.card.expect("a findings card");
        assert!(card.contains("`leak.py:2`"), "{card}");
        assert!(card.contains("secret.generic-api-key"), "{card}");
    }

    #[test]
    fn plugins_lists_nothing_for_an_empty_project() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manager = crate::package_manager::PackageManager::new(dir.path().to_path_buf());
        let report = plugins(&manager);
        // The user's own global packages may be listed; the call must not
        // fail, and an empty result says how to add one.
        assert!(
            report.card.is_some() || report.status.starts_with("No packages installed"),
            "{report:?}"
        );
    }

    #[test]
    fn hub_reports_an_empty_roster_and_unknown_children() {
        assert!(format_roster(&[]).status.starts_with("No subagents"));
        assert!(hub("no-such-child-9").status.contains("unknown child"));
    }

    /// OMP semantics: bare `/advisor` toggles; on/off are idempotent; the
    /// reply says when no advisor model is assigned.
    #[test]
    fn advisor_toggles_and_reports_a_missing_model() {
        let paused = std::sync::atomic::AtomicBool::new(false);
        let spec = Some("anthropic/claude-haiku-4-5");
        let paused_now = || paused.load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(advisor_with(&paused, spec, "").status, "Advisor disabled.");
        assert!(paused_now());
        assert_eq!(advisor_with(&paused, spec, "").status, "Advisor enabled.");
        assert!(!paused_now());
        assert_eq!(advisor_with(&paused, spec, "on").status, "Advisor enabled.");
        assert!(!paused_now(), "on is idempotent");
        assert_eq!(
            advisor_with(&paused, spec, "off").status,
            "Advisor disabled."
        );
        assert_eq!(
            advisor_with(&paused, spec, "status").status,
            "Advisor: off (anthropic/claude-haiku-4-5 assigned)"
        );
        assert_eq!(
            advisor_with(&paused, None, "on").status,
            "Advisor enabled, but no model is assigned to the 'advisor' role."
        );
        assert!(
            advisor_with(&paused, None, "sideways")
                .status
                .starts_with("Unknown /advisor subcommand")
        );
    }

    #[test]
    fn approval_sets_modes_and_reports_status() {
        use crate::approval::{ApprovalMode, ApprovalState};
        let state = ApprovalState::new(ApprovalMode::AlwaysAsk, false, Vec::new());
        let (report, changed) = approval(&state, "");
        assert_eq!(
            report.status,
            "Approval mode: always-ask | Dual-confirm classes: none"
        );
        assert_eq!(changed, None);
        let (_, changed) = approval(&state, "yolo");
        assert_eq!(changed, Some(ApprovalMode::Yolo));
        assert_eq!(state.mode(), ApprovalMode::Yolo);
        let (report, changed) = approval(&state, "maybe");
        assert_eq!(changed, None);
        assert!(report.status.starts_with("Unknown /approval mode"));
        assert_eq!(
            state.mode(),
            ApprovalMode::Yolo,
            "an unknown mode changes nothing"
        );
    }

    #[test]
    fn porcelain_paths_keep_the_first_character_of_unstaged_paths() {
        let status = " M src/a.rs\nM  src/b.rs\n?? new.txt\nR  old.rs -> src/renamed.rs\n";
        assert_eq!(
            porcelain_paths(status),
            vec!["src/a.rs", "src/b.rs", "new.txt", "src/renamed.rs"]
        );
    }

    /// `/omfg` forges a rule that `/rules` can then toggle and remove, in the
    /// same project. (Counts are not asserted: the store also merges the
    /// user's global rules file.)
    #[test]
    fn omfg_forges_a_rule_that_rules_manages() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(
            omfg(dir.path(), "  ").status,
            "Usage: /omfg <complaint about model behavior>"
        );
        assert_eq!(
            rules(dir.path(), "bogus").status,
            "Usage: /rules [list|remove <id>|toggle <id>]"
        );
        assert_eq!(
            rules(dir.path(), "toggle nope").status,
            "Stream rule 'nope' not found"
        );

        let forged = omfg(dir.path(), "stop apologizing before every answer");
        assert!(forged.card.is_some(), "{forged:?}");
        let id = forged
            .status
            .strip_prefix("Forged and activated stream rule '")
            .and_then(|rest| rest.strip_suffix('\''))
            .expect("status names the rule")
            .to_string();
        assert!(rules(dir.path(), "list").card.is_some());
        assert_eq!(
            rules(dir.path(), &format!("toggle {id}")).status,
            format!("Stream rule '{id}' is now disabled")
        );
        assert_eq!(
            rules(dir.path(), &format!("remove {id}")).status,
            format!("Removed stream rule '{id}'")
        );
    }

    #[test]
    fn commit_in_a_clean_repo_has_nothing_to_do() {
        let dir = tempfile::tempdir().expect("tempdir");
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .output()
                .expect("git")
        };
        git(&["init", "-q"]);
        assert_eq!(
            commit(dir.path(), "").status,
            "Working tree clean; nothing to commit."
        );
        std::fs::write(dir.path().join("notes.md"), "hello\n").expect("write");
        git(&["add", "notes.md"]);
        git(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.com",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-qm",
            "init",
        ]);
        // An UNSTAGED edit: porcelain prints ` M notes.md`, the line shape
        // whose path the old parser cut to `otes.md`.
        std::fs::write(dir.path().join("notes.md"), "hello again\n").expect("write");
        let planned = commit(dir.path(), "--dry-run");
        let card = planned.card.expect("a plan card");
        assert!(card.contains("`notes.md`"), "{card}");
        assert!(card.contains("Dry run"), "{card}");
    }
}
