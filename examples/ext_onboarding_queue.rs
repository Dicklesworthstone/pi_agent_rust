#![forbid(unsafe_code)]

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use clap::Parser;
use pi::extension_popularity::{CandidateItem, CandidatePool, CandidateSource};
use serde::Serialize;
use url::Url;

#[derive(Debug, Parser)]
#[command(name = "ext_onboarding_queue")]
#[command(about = "Build a de-biased extension onboarding queue from candidate pool")]
struct Args {
    #[arg(long, default_value = "docs/extension-candidate-pool.json")]
    candidate_pool: PathBuf,
    #[arg(long, default_value = "docs/extension-onboarding-queue.json")]
    json_out: PathBuf,
    #[arg(long, default_value = "docs/extension-onboarding-queue.md")]
    md_out: PathBuf,
    #[arg(long)]
    generated_at: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct SelectionPolicy {
    goal: String,
    base_formula: String,
    debias_formula: String,
    relevance_formula: String,
    source_bonus: BTreeMap<String, i64>,
    recency_bands_days: BTreeMap<String, i64>,
    non_relevant_multiplier: f64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
#[allow(clippy::struct_field_names)]
struct QueueSummary {
    top_100: usize,
    top_300: usize,
    top_500: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct QueueEntry {
    id: String,
    name: String,
    source_tier: String,
    source_type: String,
    package: Option<String>,
    repository_url: Option<String>,
    repo_key: String,
    repo_candidate_count: usize,
    repo_rank: usize,
    pi_relevant: bool,
    pi_relevance_score: u32,
    impact_score: f64,
    raw_impact_score: f64,
    debiased_impact_score: f64,
    github_stars: u64,
    github_forks: u64,
    github_watchers: u64,
    npm_downloads_monthly: u64,
    updated_at: Option<String>,
    days_since_update: Option<i64>,
    notes: Option<String>,
    rank: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct QueueDocument {
    schema: String,
    generated_at: String,
    source_pool_path: String,
    source_pool_total: usize,
    eligible_unvendored: usize,
    eligible_pi_relevant: usize,
    selection_policy: SelectionPolicy,
    summary: QueueSummary,
    top_100: Vec<QueueEntry>,
    top_300: Vec<QueueEntry>,
    all: Vec<QueueEntry>,
}

#[derive(Debug, Clone)]
struct QueueDraft {
    entry: QueueEntry,
    score_key: ScoreKey,
}

#[derive(Debug, Clone, Copy)]
struct ScoreKey {
    raw_impact: f64,
    impact: f64,
}

impl ScoreKey {
    fn cmp_desc(self, other: Self) -> Ordering {
        other
            .impact
            .partial_cmp(&self.impact)
            .unwrap_or(Ordering::Equal)
            .then_with(|| {
                other
                    .raw_impact
                    .partial_cmp(&self.raw_impact)
                    .unwrap_or(Ordering::Equal)
            })
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    let pool_bytes = fs::read(&args.candidate_pool)
        .with_context(|| format!("read {}", args.candidate_pool.display()))?;
    let pool: CandidatePool =
        serde_json::from_slice(&pool_bytes).context("parse extension candidate pool")?;

    let now = parse_generated_at(args.generated_at)?;
    let unvendored = pool
        .items
        .iter()
        .filter(|item| item.status == "unvendored")
        .collect::<Vec<_>>();

    let repo_counts = build_repo_counts(&unvendored);
    let mut drafts = unvendored
        .iter()
        .map(|item| draft_from_item(item, &repo_counts, now))
        .collect::<Vec<_>>();

    assign_repo_rank(&mut drafts);
    recompute_debiased_scores(&mut drafts);
    drafts.sort_by(|left, right| {
        left.score_key
            .cmp_desc(right.score_key)
            .then_with(|| right.entry.pi_relevant.cmp(&left.entry.pi_relevant))
            .then_with(|| left.entry.id.cmp(&right.entry.id))
    });

    for (index, draft) in drafts.iter_mut().enumerate() {
        draft.entry.rank = index + 1;
    }

    let all = drafts
        .into_iter()
        .map(|draft| draft.entry)
        .collect::<Vec<_>>();
    let top_100 = all.iter().take(100).cloned().collect::<Vec<_>>();
    let top_300 = all.iter().take(300).cloned().collect::<Vec<_>>();
    let eligible_pi_relevant = all.iter().filter(|entry| entry.pi_relevant).count();

    let selection_policy = build_selection_policy();
    let summary = QueueSummary {
        top_100: top_100.len(),
        top_300: top_300.len(),
        top_500: all.len().min(500),
    };

    let document = QueueDocument {
        schema: "pi.ext.onboarding_queue.v2".to_string(),
        generated_at: now.to_rfc3339(),
        source_pool_path: args.candidate_pool.display().to_string(),
        source_pool_total: pool.items.len(),
        eligible_unvendored: all.len(),
        eligible_pi_relevant,
        selection_policy,
        summary,
        top_100,
        top_300,
        all,
    };

    let json =
        serde_json::to_string_pretty(&document).context("serialize onboarding queue json")?;
    fs::write(&args.json_out, format!("{json}\n"))
        .with_context(|| format!("write {}", args.json_out.display()))?;

    let markdown = render_markdown(&document);
    fs::write(&args.md_out, markdown)
        .with_context(|| format!("write {}", args.md_out.display()))?;

    eprintln!(
        "Wrote queue: {} entries ({} pi-relevant) -> {}, {}",
        document.eligible_unvendored,
        document.eligible_pi_relevant,
        args.json_out.display(),
        args.md_out.display()
    );

    Ok(())
}

fn parse_generated_at(generated_at: Option<String>) -> Result<DateTime<Utc>> {
    let Some(value) = generated_at else {
        return Ok(Utc::now());
    };
    let parsed = DateTime::parse_from_rfc3339(&value)
        .with_context(|| format!("parse --generated-at timestamp: {value}"))?;
    Ok(parsed.with_timezone(&Utc))
}

fn build_selection_policy() -> SelectionPolicy {
    let source_bonus = BTreeMap::from([
        ("npm-registry".to_string(), 8),
        ("third-party-github".to_string(), 6),
        ("community".to_string(), 5),
        ("official-pi-mono".to_string(), 1),
    ]);
    let recency_bands_days = BTreeMap::from([
        ("<=30".to_string(), 20),
        ("<=90".to_string(), 16),
        ("<=180".to_string(), 12),
        ("<=365".to_string(), 8),
        ("<=730".to_string(), 4),
        (">730".to_string(), 0),
    ]);

    SelectionPolicy {
        goal: "Prioritize high-impact, Pi-relevant unvendored candidates for vendor+conformance onboarding while preventing single-repo popularity inflation.".to_string(),
        base_formula:
            "22*log10(1+stars)+8*log10(1+forks)+4*log10(1+watchers)+22*log10(1+npm_monthly)+recency+source_bonus+signal_bonus"
                .to_string(),
        debias_formula:
            "debiased=raw*(1/(1+ln(repo_count)))*(0.85^(repo_rank-1)); final=debiased*non_relevant_multiplier_if_needed + pi_relevance_score"
                .to_string(),
        relevance_formula:
            "Token and notes heuristic over id/name/package/repo/notes; strong boosts for Pi API imports and registration markers."
                .to_string(),
        source_bonus,
        recency_bands_days,
        non_relevant_multiplier: 0.45,
    }
}

fn build_repo_counts(items: &[&CandidateItem]) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    for item in items {
        let key = repo_key(item);
        *counts.entry(key).or_insert(0) += 1;
    }
    counts
}

fn draft_from_item(
    item: &CandidateItem,
    repo_counts: &BTreeMap<String, usize>,
    now: DateTime<Utc>,
) -> QueueDraft {
    let (source_type, package) = source_type_and_package(&item.source);
    let repo_key = repo_key(item);
    let repo_candidate_count = *repo_counts.get(&repo_key).unwrap_or(&1);

    let stars = item.popularity.github_stars.unwrap_or(0);
    let forks = item.popularity.github_forks.unwrap_or(0);
    let watchers = item.popularity.github_watchers.unwrap_or(0);
    let npm_monthly = item.popularity.npm_downloads_monthly.unwrap_or(0);
    let updated_at = item
        .popularity
        .github_last_commit
        .clone()
        .or_else(|| item.popularity.npm_last_publish.clone())
        .or_else(|| item.retrieved.clone());
    let days_since_update = updated_at
        .as_deref()
        .and_then(|timestamp| days_since(timestamp, now));
    let recency = recency_bonus(days_since_update);
    let source_bonus = source_bonus(&item.source_tier);
    let signal_bonus = signal_bonus(item.notes.as_deref());
    let raw_impact = weighted_log(22.0, stars)
        + weighted_log(8.0, forks)
        + weighted_log(4.0, watchers)
        + weighted_log(22.0, npm_monthly)
        + recency
        + source_bonus
        + signal_bonus;

    let pi_relevance_score = pi_relevance_score(item, package.as_deref());
    let pi_relevant = pi_relevance_score >= 18;

    let entry = QueueEntry {
        id: item.id.clone(),
        name: item.name.clone(),
        source_tier: item.source_tier.clone(),
        source_type: source_type.to_string(),
        package,
        repository_url: item.repository_url.clone(),
        repo_key,
        repo_candidate_count,
        repo_rank: 1,
        pi_relevant,
        pi_relevance_score,
        impact_score: 0.0,
        raw_impact_score: round3(raw_impact),
        debiased_impact_score: 0.0,
        github_stars: stars,
        github_forks: forks,
        github_watchers: watchers,
        npm_downloads_monthly: npm_monthly,
        updated_at,
        days_since_update,
        notes: item.notes.clone(),
        rank: 0,
    };

    QueueDraft {
        entry,
        score_key: ScoreKey {
            raw_impact,
            impact: raw_impact,
        },
    }
}

fn assign_repo_rank(drafts: &mut [QueueDraft]) {
    let mut per_repo: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (index, draft) in drafts.iter().enumerate() {
        per_repo
            .entry(draft.entry.repo_key.clone())
            .or_default()
            .push(index);
    }
    for indexes in per_repo.values_mut() {
        indexes.sort_by(|left, right| {
            drafts[*right]
                .score_key
                .raw_impact
                .partial_cmp(&drafts[*left].score_key.raw_impact)
                .unwrap_or(Ordering::Equal)
                .then_with(|| drafts[*left].entry.id.cmp(&drafts[*right].entry.id))
        });
        for (repo_rank, index) in indexes.iter().enumerate() {
            drafts[*index].entry.repo_rank = repo_rank + 1;
        }
    }
}

#[allow(clippy::cast_precision_loss)]
fn recompute_debiased_scores(drafts: &mut [QueueDraft]) {
    for draft in drafts.iter_mut() {
        let repo_count = draft.entry.repo_candidate_count as f64;
        let rank_exponent =
            i32::try_from(draft.entry.repo_rank.saturating_sub(1)).unwrap_or(i32::MAX);
        let rank_penalty = 0.85_f64.powi(rank_exponent);
        let multiplicity_penalty = 1.0 / (1.0 + repo_count.ln());
        let relevance_multiplier = if draft.entry.pi_relevant { 1.0 } else { 0.45 };

        let raw = draft.score_key.raw_impact;
        let debiased = raw * multiplicity_penalty * rank_penalty;
        let impact = debiased * relevance_multiplier + f64::from(draft.entry.pi_relevance_score);

        draft.entry.debiased_impact_score = round3(debiased);
        draft.entry.impact_score = round3(impact);
        draft.score_key.impact = impact;
    }
}

fn source_type_and_package(source: &CandidateSource) -> (&'static str, Option<String>) {
    match source {
        CandidateSource::Git { .. } => ("git", None),
        CandidateSource::Npm { package, .. } => ("npm", Some(package.clone())),
        CandidateSource::Url { .. } => ("url", None),
    }
}

fn repo_key(item: &CandidateItem) -> String {
    if let Some(repo_url) = item.repository_url.as_deref()
        && let Some(key) = canonical_repo_url(repo_url)
    {
        return key;
    }

    match &item.source {
        CandidateSource::Git { repo, .. } => format!("git:{}", repo.trim().to_ascii_lowercase()),
        CandidateSource::Npm { package, .. } => {
            format!("npm:{}", package.trim().to_ascii_lowercase())
        }
        CandidateSource::Url { url } => format!("url:{}", url.trim().to_ascii_lowercase()),
    }
}

fn canonical_repo_url(repo_url: &str) -> Option<String> {
    let raw = repo_url.trim();
    if raw.is_empty() {
        return None;
    }

    if let Some(path) = raw.strip_prefix("git@github.com:") {
        let normalized = path.trim_end_matches(".git").trim_matches('/');
        return Some(format!("github.com/{}", normalized.to_ascii_lowercase()));
    }

    let no_prefix = raw.strip_prefix("git+").unwrap_or(raw);
    let url_str = if no_prefix.contains("://") {
        no_prefix.to_string()
    } else {
        format!("https://{no_prefix}")
    };
    let parsed = Url::parse(&url_str).ok()?;
    let host = parsed.host_str()?.to_ascii_lowercase();
    let segments = parsed
        .path_segments()
        .map(|it| it.filter(|seg| !seg.is_empty()).collect::<Vec<_>>())?;
    if segments.is_empty() {
        return None;
    }

    if host == "github.com" && segments.len() >= 2 {
        let owner = segments[0].to_ascii_lowercase();
        let repo = segments[1].trim_end_matches(".git").to_ascii_lowercase();
        return Some(format!("github.com/{owner}/{repo}"));
    }

    if segments.len() >= 2 {
        return Some(format!(
            "{}/{}/{}",
            host,
            segments[0].to_ascii_lowercase(),
            segments[1].trim_end_matches(".git").to_ascii_lowercase()
        ));
    }

    Some(format!("{}/{}", host, segments[0].to_ascii_lowercase()))
}

#[allow(clippy::cast_precision_loss)]
fn weighted_log(weight: f64, value: u64) -> f64 {
    weight * (1.0 + value as f64).log10()
}

const fn recency_bonus(days_since_update: Option<i64>) -> f64 {
    match days_since_update {
        Some(days) if days <= 30 => 20.0,
        Some(days) if days <= 90 => 16.0,
        Some(days) if days <= 180 => 12.0,
        Some(days) if days <= 365 => 8.0,
        Some(days) if days <= 730 => 4.0,
        Some(_) | None => 0.0,
    }
}

fn source_bonus(source_tier: &str) -> f64 {
    match source_tier {
        "npm-registry" => 8.0,
        "third-party-github" => 6.0,
        "community" => 5.0,
        "official-pi-mono" => 1.0,
        _ => 0.0,
    }
}

fn signal_bonus(notes: Option<&str>) -> f64 {
    let Some(notes) = notes else {
        return 0.0;
    };
    let normalized = notes.to_ascii_lowercase();
    let mut bonus = 0.0;
    if normalized.contains("registration calls detected") {
        bonus += 8.0;
    }
    if normalized.contains("pi api import found") {
        bonus += 6.0;
    }
    if normalized.contains("export default present") {
        bonus += 4.0;
    }
    bonus
}

fn pi_relevance_score(item: &CandidateItem, package: Option<&str>) -> u32 {
    let mut score: u32 = 0;
    let mut text = String::new();
    text.push_str(&item.id.to_ascii_lowercase());
    text.push(' ');
    text.push_str(&item.name.to_ascii_lowercase());
    text.push(' ');
    if let Some(pkg) = package {
        text.push_str(&pkg.to_ascii_lowercase());
        text.push(' ');
    }
    if let Some(repo) = item.repository_url.as_deref() {
        text.push_str(&repo.to_ascii_lowercase());
        text.push(' ');
    }
    if let Some(notes) = item.notes.as_deref() {
        text.push_str(&notes.to_ascii_lowercase());
    }

    if contains_word_token(&text, "pi") || text.contains("pi-") || text.contains("/pi-") {
        score = score.saturating_add(14);
    }
    if text.contains("openclaw") || text.contains("clawdbot") || contains_word_token(&text, "clawd")
    {
        score = score.saturating_add(14);
    }
    if contains_word_token(&text, "extension") || contains_word_token(&text, "extensions") {
        score = score.saturating_add(8);
    }
    if contains_word_token(&text, "skill") || contains_word_token(&text, "skills") {
        score = score.saturating_add(6);
    }
    if contains_word_token(&text, "mcp") {
        score = score.saturating_add(6);
    }
    if contains_word_token(&text, "agent") || contains_word_token(&text, "agents") {
        score = score.saturating_add(4);
    }

    if let Some(notes) = item.notes.as_deref() {
        let notes = notes.to_ascii_lowercase();
        if notes.contains("pi api import found") {
            score = score.saturating_add(30);
        }
        if notes.contains("registration calls detected") {
            score = score.saturating_add(20);
        }
        if notes.contains("export default present") {
            score = score.saturating_add(8);
        }
    }

    if matches!(item.source_tier.as_str(), "official-pi-mono" | "community") {
        score = score.saturating_add(6);
    }

    score.min(100)
}

fn contains_word_token(text: &str, token: &str) -> bool {
    text.split(|character: char| !character.is_ascii_alphanumeric())
        .any(|part| part == token)
}

fn days_since(timestamp: &str, now: DateTime<Utc>) -> Option<i64> {
    let parsed = DateTime::parse_from_rfc3339(timestamp).ok()?;
    let parsed_utc = parsed.with_timezone(&Utc);
    let duration = now.signed_duration_since(parsed_utc);
    Some(duration.num_days().max(0))
}

#[allow(clippy::cast_precision_loss)]
fn round3(value: f64) -> f64 {
    (value * 1000.0).round() / 1000.0
}

fn render_markdown(document: &QueueDocument) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "# Extension Onboarding Queue");
    let _ = writeln!(out);
    let _ = writeln!(out, "Generated: {}", document.generated_at);
    let _ = writeln!(
        out,
        "Source pool: `{}` ({} total)",
        document.source_pool_path, document.source_pool_total
    );
    let _ = writeln!(
        out,
        "Eligible (unvendored): **{}**",
        document.eligible_unvendored
    );
    let _ = writeln!(
        out,
        "Pi-relevant (heuristic): **{}**",
        document.eligible_pi_relevant
    );
    let _ = writeln!(
        out,
        "Schema: `{}` (repo-level de-bias enabled)",
        document.schema
    );
    let _ = writeln!(out);
    let _ = writeln!(out, "## Top 100 (Start Here)");
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "| Rank | ID | Tier | Repo n | Rel | Stars | npm/mo | Impact |"
    );
    let _ = writeln!(out, "|---:|---|---|---:|---:|---:|---:|---:|");
    for entry in &document.top_100 {
        let _ = writeln!(
            out,
            "| {} | `{}` | `{}` | {} | {} | {} | {} | {:.3} |",
            entry.rank,
            entry.id,
            entry.source_tier,
            entry.repo_candidate_count,
            entry.pi_relevance_score,
            format_u64(entry.github_stars),
            format_u64(entry.npm_downloads_monthly),
            entry.impact_score
        );
    }

    let _ = writeln!(out);
    let _ = writeln!(out, "## Next 200 (Parallel Agents)");
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "| Rank | ID | Tier | Repo n | Rel | Stars | npm/mo | Impact |"
    );
    let _ = writeln!(out, "|---:|---|---|---:|---:|---:|---:|---:|");
    for entry in document
        .all
        .iter()
        .filter(|entry| (101..=300).contains(&entry.rank))
    {
        let _ = writeln!(
            out,
            "| {} | `{}` | `{}` | {} | {} | {} | {} | {:.3} |",
            entry.rank,
            entry.id,
            entry.source_tier,
            entry.repo_candidate_count,
            entry.pi_relevance_score,
            format_u64(entry.github_stars),
            format_u64(entry.npm_downloads_monthly),
            entry.impact_score
        );
    }
    out
}

fn format_u64(value: u64) -> String {
    let text = value.to_string();
    let mut out = String::with_capacity(text.len() + text.len() / 3);
    let bytes = text.as_bytes();
    for (index, byte) in bytes.iter().enumerate() {
        out.push(char::from(*byte));
        let remaining = bytes.len().saturating_sub(index + 1);
        if remaining > 0 && remaining.is_multiple_of(3) {
            out.push(',');
        }
    }
    out
}
