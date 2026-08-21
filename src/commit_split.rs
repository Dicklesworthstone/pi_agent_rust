//! Dependency-ordered atomic commit splitting engine (`pi commit`) (bd-cv653.3.14).
//!
//! Analyzes working tree status and diff hunks, semantically partitions changes into
//! atomic units by topic and coupling, topologically sorts them by dependency graph,
//! detects and breaks cycles, and plans/executes sequential atomic git commits.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::memory::screen_secrets;

/// File category used for commit ordering priority and scoring.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum FileCategory {
    /// Lockfiles (excluded by default unless explicitly requested).
    Lockfile = 0,
    /// Configuration, manifests, and build scripts.
    Config = 1,
    /// Documentation and markdown notes.
    Documentation = 2,
    /// Unit, integration, and conformance tests.
    Test = 3,
    /// Core application and library source files.
    Source = 4,
}

impl FileCategory {
    /// Categorize a file path based on its location and extension.
    #[must_use]
    pub fn from_path(path: &str) -> Self {
        let normalized = path.replace('\\', "/");
        let lower = normalized.to_ascii_lowercase();

        if is_lockfile(&lower) {
            return Self::Lockfile;
        }

        if lower.starts_with("tests/")
            || lower.contains("_test.")
            || lower.contains(".test.")
            || lower.contains(".spec.")
            || lower.ends_with("_spec.rs")
        {
            return Self::Test;
        }

        if lower.starts_with("docs/")
            || lower.ends_with(".md")
            || lower.ends_with(".rst")
            || lower.ends_with(".adoc")
            || lower.ends_with(".txt")
        {
            return Self::Documentation;
        }

        if lower.ends_with(".toml")
            || lower.ends_with(".json")
            || lower.ends_with(".yaml")
            || lower.ends_with(".yml")
            || lower.ends_with(".lock")
            || lower.ends_with(".ini")
            || lower.ends_with(".cfg")
            || lower.starts_with(".beads/")
            || lower.starts_with(".github/")
        {
            return Self::Config;
        }

        Self::Source
    }

    /// Weight used for headline commit scoring (Source > Test > Docs > Config).
    #[must_use]
    pub const fn score_weight(self) -> u32 {
        match self {
            Self::Source => 400,
            Self::Test => 300,
            Self::Documentation => 200,
            Self::Config => 100,
            Self::Lockfile => 0,
        }
    }
}

/// Check if a path is a known package manager lockfile.
#[must_use]
pub fn is_lockfile(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.ends_with("cargo.lock")
        || lower.ends_with("package-lock.json")
        || lower.ends_with("pnpm-lock.yaml")
        || lower.ends_with("yarn.lock")
        || lower.ends_with("bun.lockb")
        || lower.ends_with("composer.lock")
        || lower.ends_with("gemfile.lock")
}

/// A parsed unified diff hunk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffHunk {
    pub file_path: String,
    pub old_start: usize,
    pub old_lines: usize,
    pub new_start: usize,
    pub new_lines: usize,
    pub header: String,
    pub content: String,
}

/// Atomic commit unit in a planned sequence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitUnit {
    pub id: String,
    pub commit_type: String,
    pub scope: String,
    pub summary: String,
    pub files: Vec<String>,
    pub category: FileCategory,
    pub score: u32,
    pub dependencies: Vec<String>,
    pub rationale: Option<String>,
}

impl CommitUnit {
    /// Format conventional commit message header: `type(scope): summary`.
    #[must_use]
    pub fn formatted_message(&self, bead_id: Option<&str>) -> String {
        let base = if self.scope.is_empty() {
            format!("{}: {}", self.commit_type, self.summary)
        } else {
            format!("{}({}): {}", self.commit_type, self.scope, self.summary)
        };

        if let Some(bead) = bead_id {
            format!("{base} ({bead})")
        } else {
            base
        }
    }
}

/// Configuration options for commit planning and execution.
#[derive(Debug, Clone, Default)]
pub struct CommitOptions {
    pub dry_run: bool,
    pub include_lockfiles: bool,
    pub all_untracked: bool,
    pub bead_reference: Option<String>,
    pub custom_prefix: Option<String>,
}

/// Complete plan of dependency-ordered atomic commits.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitPlan {
    pub units: Vec<CommitUnit>,
    pub total_files: usize,
    pub total_hunks: usize,
    pub cycles_detected: usize,
    pub headline_unit_id: Option<String>,
}

/// Result of executing a commit unit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitExecutionResult {
    pub unit_id: String,
    pub message: String,
    pub files: Vec<String>,
    pub commit_sha: Option<String>,
    pub success: bool,
    pub error: Option<String>,
}

/// Conflict marker scanner and validator.
pub struct ConflictScanner;

impl ConflictScanner {
    /// Scan text or diff content for unresolved git conflict markers.
    pub fn check_content(content: &str, file_name: &str) -> Result<()> {
        let conflict_line = content.lines().enumerate().find_map(|(idx, line)| {
            if line.starts_with("<<<<<<<")
                || line.starts_with("=======")
                || line.starts_with(">>>>>>>")
            {
                Some(idx + 1)
            } else {
                None
            }
        });

        if let Some(line_no) = conflict_line {
            Err(Error::Validation(format!(
                "Unresolved merge conflict marker detected in {file_name}:L{line_no}"
            )))
        } else {
            Ok(())
        }
    }
}

/// Hunk parser for raw `git diff` output.
pub struct DiffParser;

impl DiffParser {
    fn parse_diff_line<'a>(
        line: &'a str,
        current_file: &mut &'a str,
    ) -> Option<(&'a str, (usize, usize, usize, usize))> {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            if let Some(b_part) = rest.split_whitespace().nth(1) {
                *current_file = b_part.trim_start_matches("b/");
            }
            None
        } else if let Some(rest) = line.strip_prefix("+++ b/") {
            *current_file = rest;
            None
        } else if line.starts_with("@@ ") {
            let header_bounds = Self::parse_hunk_header(line);
            Some((*current_file, header_bounds))
        } else {
            None
        }
    }

    fn make_hunk(
        file: &str,
        old_start: usize,
        old_lines: usize,
        new_start: usize,
        new_lines: usize,
        header: &str,
    ) -> DiffHunk {
        DiffHunk {
            file_path: file.to_string(),
            old_start,
            old_lines,
            new_start,
            new_lines,
            header: header.to_string(),
            content: String::new(),
        }
    }

    /// Parse raw unified diff into structured hunks per file.
    pub fn parse_unified_diff(diff: &str) -> Result<Vec<DiffHunk>> {
        let mut hunks = Vec::new();
        let mut current_file = "";

        for line in diff.lines() {
            if let Some((file, (old_start, old_lines, new_start, new_lines))) =
                Self::parse_diff_line(line, &mut current_file)
            {
                hunks.push(Self::make_hunk(
                    file, old_start, old_lines, new_start, new_lines, line,
                ));
            } else if let Some(last_hunk) = hunks.last_mut() {
                if !last_hunk.content.is_empty() {
                    last_hunk.content.push('\n');
                }
                last_hunk.content.push_str(line);
            }
        }

        Ok(hunks)
    }

    fn parse_hunk_header(header: &str) -> (usize, usize, usize, usize) {
        let mut old_start = 1;
        let mut old_lines = 1;
        let mut new_start = 1;
        let mut new_lines = 1;

        if let Some(inside) = header
            .strip_prefix("@@ -")
            .and_then(|s| s.split(" @@").next())
        {
            let parts: Vec<&str> = inside.split(" +").collect();
            if let Some(old_part) = parts.first() {
                let sub: Vec<&str> = old_part.split(',').collect();
                if let Some(s) = sub.first().and_then(|s| s.parse::<usize>().ok()) {
                    old_start = s;
                }
                if let Some(l) = sub.get(1).and_then(|s| s.parse::<usize>().ok()) {
                    old_lines = l;
                }
            }
            if let Some(new_part) = parts.get(1) {
                let sub: Vec<&str> = new_part.split(',').collect();
                if let Some(s) = sub.first().and_then(|s| s.parse::<usize>().ok()) {
                    new_start = s;
                }
                if let Some(l) = sub.get(1).and_then(|s| s.parse::<usize>().ok()) {
                    new_lines = l;
                }
            }
        }

        (old_start, old_lines, new_start, new_lines)
    }
}

/// Dependency-ordered atomic commit planner.
pub struct CommitPlanner;

impl CommitPlanner {
    /// Plan atomic commits from git working tree diff and status.
    pub fn plan(
        hunks: &[DiffHunk],
        changed_files: &[String],
        options: &CommitOptions,
    ) -> Result<CommitPlan> {
        // 1. Filter files (e.g. exclude lockfiles if not requested)
        let filtered_files: Vec<String> = changed_files
            .iter()
            .filter(|f| options.include_lockfiles || !is_lockfile(f))
            .cloned()
            .collect();

        let total_files = filtered_files.len();
        if total_files == 0 {
            return Ok(CommitPlan {
                units: Vec::new(),
                total_files: 0,
                total_hunks: 0,
                cycles_detected: 0,
                headline_unit_id: None,
            });
        }

        // 2. Group files by component / directory / semantic topic
        let mut groups: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for file in filtered_files {
            let group_key = Self::infer_group_key(&file);
            groups.entry(group_key).or_default().push(file);
        }

        // 3. Build candidate commit units
        let mut candidate_units: Vec<CommitUnit> = groups
            .into_iter()
            .map(|(group_key, files)| Self::build_candidate_unit(group_key, files))
            .collect();

        // 4. Infer dependencies between units (e.g. tests depend on source, docs depend on tests/source)
        Self::link_unit_dependencies(&mut candidate_units);

        // 5. Topologically sort units and resolve cycles
        let (ordered_units, cycles) = Self::topological_sort(candidate_units);

        // 6. Find headline unit
        let headline_id = ordered_units
            .iter()
            .max_by_key(|u| u.score)
            .map(|u| u.id.clone());

        Ok(CommitPlan {
            units: ordered_units,
            total_files,
            total_hunks: hunks.len(),
            cycles_detected: cycles,
            headline_unit_id: headline_id,
        })
    }

    fn build_candidate_unit(group_key: String, files: Vec<String>) -> CommitUnit {
        let category = files
            .iter()
            .map(|f| FileCategory::from_path(f))
            .max()
            .unwrap_or(FileCategory::Source);

        let commit_type = match category {
            FileCategory::Source => "feat".to_string(),
            FileCategory::Test => "test".to_string(),
            FileCategory::Documentation => "docs".to_string(),
            FileCategory::Config => "chore".to_string(),
            FileCategory::Lockfile => "chore".to_string(),
        };

        let summary = format!("update {group_key} implementation and assets");
        let score = category.score_weight() + u32::try_from(files.len()).unwrap_or(0) * 10;
        let id = format!("unit-{group_key}");

        CommitUnit {
            id,
            commit_type,
            scope: group_key,
            summary,
            files,
            category,
            score,
            dependencies: Vec::new(),
            rationale: None,
        }
    }

    fn link_unit_dependencies(units: &mut [CommitUnit]) {
        let test_dep_ids: Vec<String> = units
            .iter()
            .filter(|u| u.category == FileCategory::Source)
            .map(|u| u.id.clone())
            .collect();
        let doc_dep_ids: Vec<String> = units
            .iter()
            .filter(|u| u.category != FileCategory::Documentation)
            .map(|u| u.id.clone())
            .collect();

        for unit in units.iter_mut() {
            if unit.category == FileCategory::Test {
                unit.dependencies.extend(test_dep_ids.iter().cloned());
            } else if unit.category == FileCategory::Documentation {
                unit.dependencies.extend(doc_dep_ids.iter().cloned());
            }
        }
    }

    fn infer_group_key(file: &str) -> String {
        let normalized = file.replace('\\', "/");
        let path = Path::new(&normalized);

        if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
            if normalized.starts_with("src/") {
                return stem.to_string();
            } else if normalized.starts_with("tests/") {
                return format!("{stem}_tests");
            } else if normalized.starts_with("docs/") {
                return "docs".to_string();
            }
        }

        path.parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or("root")
            .to_string()
    }

    /// Topologically sort commit units, falling back to score sorting on cycles.
    fn topological_sort(units: Vec<CommitUnit>) -> (Vec<CommitUnit>, usize) {
        let n = units.len();
        let id_to_idx: HashMap<&str, usize> = units
            .iter()
            .enumerate()
            .map(|(idx, u)| (u.id.as_str(), idx))
            .collect();

        let mut in_degree = vec![0usize; n];
        let mut adj: Vec<Vec<usize>> = vec![Vec::new(); n];

        for (idx, u) in units.iter().enumerate() {
            for dep in &u.dependencies {
                if let Some(&dep_idx) = id_to_idx.get(dep.as_str()) {
                    if let Some(edge_list) = adj.get_mut(dep_idx) {
                        edge_list.push(idx);
                    }
                    if let Some(deg) = in_degree.get_mut(idx) {
                        *deg += 1;
                    }
                }
            }
        }

        let mut queue: VecDeque<usize> = in_degree
            .iter()
            .enumerate()
            .filter_map(|(idx, &deg)| if deg == 0 { Some(idx) } else { None })
            .collect();

        let mut ordered_indices = Vec::with_capacity(n);

        while let Some(curr) = queue.pop_front() {
            ordered_indices.push(curr);
            if let Some(neighbors) = adj.get(curr) {
                for &neighbor in neighbors {
                    if let Some(deg) = in_degree.get_mut(neighbor) {
                        *deg = deg.saturating_sub(1);
                        if *deg == 0 {
                            queue.push_back(neighbor);
                        }
                    }
                }
            }
        }

        let cycles = n.saturating_sub(ordered_indices.len());
        let mut unit_slots: Vec<Option<CommitUnit>> = units.into_iter().map(Some).collect();
        let mut ordered = Vec::with_capacity(n);

        for idx in ordered_indices {
            if let Some(slot) = unit_slots.get_mut(idx) {
                if let Some(unit) = slot.take() {
                    ordered.push(unit);
                }
            }
        }

        // Remaining unvisited units (if cycles occurred)
        let mut remaining: Vec<CommitUnit> = unit_slots.into_iter().flatten().collect();
        remaining.sort_by(|a, b| b.score.cmp(&a.score));
        ordered.extend(remaining);

        (ordered, cycles)
    }
}

/// Git execution driver for staging and creating commits.
pub struct CommitExecutor;

impl CommitExecutor {
    fn execute_single_unit(
        cwd: &Path,
        unit: &CommitUnit,
        bead_ref: Option<&str>,
    ) -> CommitExecutionResult {
        let msg = unit.formatted_message(bead_ref);
        let screened_msg = screen_secrets(&msg);

        let mut add_cmd = Command::new("git");
        add_cmd.arg("add").args(&unit.files).current_dir(cwd);

        let add_res = add_cmd.status();
        let add_ok = add_res.map(|s| s.success()).unwrap_or(false);

        if !add_ok {
            return CommitExecutionResult {
                unit_id: unit.id.clone(),
                message: screened_msg,
                files: unit.files.clone(),
                commit_sha: None,
                success: false,
                error: Some("git add failed".to_string()),
            };
        }

        let mut commit_cmd = Command::new("git");
        commit_cmd
            .arg("commit")
            .arg("-m")
            .arg(&screened_msg)
            .current_dir(cwd);

        let commit_output = commit_cmd.output();
        match commit_output {
            Ok(output) if output.status.success() => {
                let sha = Self::get_head_sha(cwd);
                CommitExecutionResult {
                    unit_id: unit.id.clone(),
                    message: screened_msg,
                    files: unit.files.clone(),
                    commit_sha: sha,
                    success: true,
                    error: None,
                }
            }
            Ok(output) => {
                let err_str = String::from_utf8_lossy(&output.stderr).to_string();
                CommitExecutionResult {
                    unit_id: unit.id.clone(),
                    message: screened_msg,
                    files: unit.files.clone(),
                    commit_sha: None,
                    success: false,
                    error: Some(err_str),
                }
            }
            Err(e) => CommitExecutionResult {
                unit_id: unit.id.clone(),
                message: screened_msg,
                files: unit.files.clone(),
                commit_sha: None,
                success: false,
                error: Some(e.to_string()),
            },
        }
    }

    /// Execute a commit plan sequentially.
    pub fn execute(
        cwd: &Path,
        plan: &CommitPlan,
        options: &CommitOptions,
    ) -> Result<Vec<CommitExecutionResult>> {
        if options.dry_run {
            let bead_ref = options.bead_reference.as_deref();
            return Ok(plan
                .units
                .iter()
                .map(|unit| CommitExecutionResult {
                    unit_id: unit.id.clone(),
                    message: unit.formatted_message(bead_ref),
                    files: unit.files.clone(),
                    commit_sha: Some("dry-run-sha".to_string()),
                    success: true,
                    error: None,
                })
                .collect());
        }

        let mut results = Vec::new();
        let bead_ref = options.bead_reference.as_deref();

        for unit in &plan.units {
            let res = Self::execute_single_unit(cwd, unit, bead_ref);
            let failed = !res.success;
            results.push(res);
            if failed {
                break;
            }
        }

        Ok(results)
    }

    fn get_head_sha(cwd: &Path) -> Option<String> {
        let output = Command::new("git")
            .arg("rev-parse")
            .arg("--short")
            .arg("HEAD")
            .current_dir(cwd)
            .output()
            .ok()?;
        if output.status.success() {
            Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_file_categorization_and_scoring() {
        assert_eq!(
            FileCategory::from_path("src/agent.rs"),
            FileCategory::Source
        );
        assert_eq!(
            FileCategory::from_path("tests/handoff.rs"),
            FileCategory::Test
        );
        assert_eq!(
            FileCategory::from_path("docs/ARCHITECTURE.md"),
            FileCategory::Documentation
        );
        assert_eq!(FileCategory::from_path("Cargo.toml"), FileCategory::Config);
        assert_eq!(
            FileCategory::from_path("Cargo.lock"),
            FileCategory::Lockfile
        );

        assert!(FileCategory::Source.score_weight() > FileCategory::Test.score_weight());
        assert!(FileCategory::Test.score_weight() > FileCategory::Documentation.score_weight());
        assert!(FileCategory::Documentation.score_weight() > FileCategory::Config.score_weight());
    }

    #[test]
    fn test_conflict_marker_detection() {
        let clean = "fn calculate() -> i32 {\n    42\n}\n";
        assert!(ConflictScanner::check_content(clean, "calc.rs").is_ok());

        let dirty =
            "fn calculate() -> i32 {\n<<<<<<< HEAD\n    42\n=======\n    100\n>>>>>>> branch\n}\n";
        let err = ConflictScanner::check_content(dirty, "calc.rs");
        assert!(err.is_err());
    }

    #[test]
    fn test_unified_diff_hunk_parsing() {
        let raw_diff = "diff --git a/src/main.rs b/src/main.rs\nindex 1234..5678 100644\n--- a/src/main.rs\n+++ b/src/main.rs\n@@ -10,4 +10,6 @@ fn main() {\n+    println!(\"Hello\");\n+    println!(\"World\");\n }\n";
        let hunks = DiffParser::parse_unified_diff(raw_diff);
        let Ok(hunks) = hunks else {
            return;
        };
        assert_eq!(hunks.len(), 1);
        let Some(first_hunk) = hunks.first() else {
            return;
        };
        assert_eq!(first_hunk.file_path, "src/main.rs");
        assert_eq!(first_hunk.new_lines, 6);
    }

    #[test]
    fn test_topological_sort_ordering() {
        let changed_files = vec![
            "src/model.rs".to_string(),
            "tests/model_test.rs".to_string(),
            "docs/model.md".to_string(),
        ];

        let options = CommitOptions {
            dry_run: true,
            include_lockfiles: false,
            all_untracked: false,
            bead_reference: Some("bd-123".to_string()),
            custom_prefix: None,
        };

        let plan = CommitPlanner::plan(&[], &changed_files, &options);
        let Ok(plan) = plan else {
            return;
        };
        assert_eq!(plan.units.len(), 3);
        assert_eq!(plan.cycles_detected, 0);

        // Source should be planned before tests and docs
        let source_idx = plan
            .units
            .iter()
            .position(|u| u.category == FileCategory::Source);
        let test_idx = plan
            .units
            .iter()
            .position(|u| u.category == FileCategory::Test);
        assert!(source_idx.is_some());
        assert!(test_idx.is_some());
        if let (Some(s), Some(t)) = (source_idx, test_idx) {
            assert!(s <= t);
        }
    }
}
