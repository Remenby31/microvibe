use std::cmp::Ordering;
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use glob::Pattern;

const PREFIX_MULTIPLIER: f64 = 2.0;
const WORD_BOUNDARY_MULTIPLIER: f64 = 1.8;
const CONSECUTIVE_MULTIPLIER: f64 = 1.3;
const DEFAULT_TARGET_MATCHES: usize = 100;
const DEFAULT_MAX_ENTRIES_TO_PROCESS: usize = 32_000;
const DEFAULT_IGNORE_PATTERNS: &[(&str, bool)] = &[
    (".git/", true),
    ("__pycache__/", true),
    ("node_modules/", true),
    (".DS_Store", true),
    ("*.pyc", true),
    ("*.log", true),
    (".vscode/", true),
    (".idea/", true),
    ("/build/", true),
    ("dist/", true),
    ("target/", true),
    (".next/", true),
    (".nuxt/", true),
    ("coverage/", true),
    (".nyc_output/", true),
    ("*.egg-info", true),
    (".pytest_cache/", true),
    (".tox/", true),
    ("vendor/", true),
    ("third_party/", true),
    ("deps/", true),
    ("*.min.js", true),
    ("*.min.css", true),
    ("*.bundle.js", true),
    ("*.chunk.js", true),
    (".cache/", true),
    ("tmp/", true),
    ("temp/", true),
    ("logs/", true),
    (".uv-cache/", true),
    (".ruff_cache/", true),
    (".venv/", true),
    ("venv/", true),
    (".mypy_cache/", true),
    ("htmlcov/", true),
    (".coverage", true),
];

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct MatchResult {
    pub matched: bool,
    pub score: f64,
    pub matched_indices: Vec<usize>,
}

pub(crate) fn fuzzy_match(pattern: &str, text: &str) -> MatchResult {
    if pattern.is_empty() {
        return MatchResult {
            matched: true,
            score: 0.0,
            matched_indices: Vec::new(),
        };
    }

    let pattern_lower = pattern.to_lowercase();
    let text_lower = text.to_lowercase();
    find_best_match(pattern, &pattern_lower, &text_lower, text)
}

fn find_best_match(
    pattern_original: &str,
    pattern_lower: &str,
    text_lower: &str,
    text_original: &str,
) -> MatchResult {
    if pattern_lower.chars().count() > text_lower.chars().count() {
        return no_match();
    }

    if text_lower.starts_with(pattern_lower) {
        let indices = (0..pattern_lower.chars().count()).collect::<Vec<_>>();
        let score = calculate_score(
            pattern_original,
            pattern_lower,
            text_lower,
            &indices,
            text_original,
        );
        return MatchResult {
            matched: true,
            score: score * PREFIX_MULTIPLIER,
            matched_indices: indices,
        };
    }

    let mut best = no_match();
    for candidate in [
        try_word_boundary_match(pattern_original, pattern_lower, text_lower, text_original),
        try_consecutive_match(pattern_original, pattern_lower, text_lower, text_original),
        try_subsequence_match(pattern_original, pattern_lower, text_lower, text_original),
    ] {
        if candidate.matched && (!best.matched || candidate.score > best.score) {
            best = candidate;
        }
    }
    best
}

fn no_match() -> MatchResult {
    MatchResult {
        matched: false,
        score: 0.0,
        matched_indices: Vec::new(),
    }
}

fn try_word_boundary_match(
    pattern_original: &str,
    pattern: &str,
    text_lower: &str,
    text_original: &str,
) -> MatchResult {
    let pattern_chars = pattern.chars().collect::<Vec<_>>();
    let text_chars = text_lower.chars().collect::<Vec<_>>();
    let original_chars = text_original.chars().collect::<Vec<_>>();
    let mut indices = Vec::new();
    let mut pattern_index = 0usize;

    for (index, ch) in text_chars.iter().enumerate() {
        if pattern_index >= pattern_chars.len() {
            break;
        }
        let is_boundary = index == 0
            || matches!(text_chars[index - 1], '/' | '-' | '_' | '.')
            || (original_chars[index].is_uppercase() && !original_chars[index - 1].is_uppercase());
        if *ch == pattern_chars[pattern_index]
            && (is_boundary
                || indices.last().is_some_and(|last| index == *last + 1)
                || indices.is_empty())
        {
            indices.push(index);
            pattern_index += 1;
        }
    }

    if pattern_index == pattern_chars.len() {
        let score = calculate_score(
            pattern_original,
            pattern,
            text_lower,
            &indices,
            text_original,
        );
        MatchResult {
            matched: true,
            score: score * WORD_BOUNDARY_MULTIPLIER,
            matched_indices: indices,
        }
    } else {
        no_match()
    }
}

fn try_consecutive_match(
    pattern_original: &str,
    pattern: &str,
    text_lower: &str,
    text_original: &str,
) -> MatchResult {
    let pattern_chars = pattern.chars().collect::<Vec<_>>();
    let mut indices = Vec::new();
    let mut pattern_index = 0usize;

    for (index, ch) in text_lower.chars().enumerate() {
        if pattern_index >= pattern_chars.len() {
            break;
        }
        if ch == pattern_chars[pattern_index] {
            indices.push(index);
            pattern_index += 1;
        } else if !indices.is_empty() {
            indices.clear();
            pattern_index = 0;
        }
    }

    if pattern_index == pattern_chars.len() {
        let score = calculate_score(
            pattern_original,
            pattern,
            text_lower,
            &indices,
            text_original,
        );
        MatchResult {
            matched: true,
            score: score * CONSECUTIVE_MULTIPLIER,
            matched_indices: indices,
        }
    } else {
        no_match()
    }
}

fn try_subsequence_match(
    pattern_original: &str,
    pattern: &str,
    text_lower: &str,
    text_original: &str,
) -> MatchResult {
    let pattern_chars = pattern.chars().collect::<Vec<_>>();
    let mut indices = Vec::new();
    let mut pattern_index = 0usize;

    for (index, ch) in text_lower.chars().enumerate() {
        if pattern_index >= pattern_chars.len() {
            break;
        }
        if ch == pattern_chars[pattern_index] {
            indices.push(index);
            pattern_index += 1;
        }
    }

    if pattern_index == pattern_chars.len() {
        let score = calculate_score(
            pattern_original,
            pattern,
            text_lower,
            &indices,
            text_original,
        );
        MatchResult {
            matched: true,
            score,
            matched_indices: indices,
        }
    } else {
        no_match()
    }
}

fn calculate_score(
    pattern_original: &str,
    pattern: &str,
    text_lower: &str,
    indices: &[usize],
    text_original: &str,
) -> f64 {
    if indices.is_empty() {
        return 0.0;
    }

    let text_lower_chars = text_lower.chars().collect::<Vec<_>>();
    let text_original_chars = text_original.chars().collect::<Vec<_>>();
    let pattern_original_chars = pattern_original.chars().collect::<Vec<_>>();
    let mut base_score = 100.0;
    if indices[0] == 0 {
        base_score += 50.0;
    } else {
        base_score -= indices[0] as f64 * 2.0;
    }

    let consecutive_bonus = indices
        .windows(2)
        .filter(|pair| pair[1] == pair[0] + 1)
        .count() as f64
        * 10.0;

    let mut boundary_bonus = 0.0;
    for idx in indices {
        if *idx == 0 || matches!(text_lower_chars[*idx - 1], '/' | '-' | '_' | '.') {
            boundary_bonus += 5.0;
        } else if text_original_chars[*idx].is_uppercase()
            && (*idx == 0 || !text_original_chars[*idx - 1].is_uppercase())
        {
            boundary_bonus += 3.0;
        }
    }

    let case_bonus = indices
        .iter()
        .enumerate()
        .filter(|(pattern_index, text_index)| {
            *pattern_index < pattern.chars().count()
                && **text_index < text_original_chars.len()
                && pattern_original_chars[*pattern_index] == text_original_chars[**text_index]
        })
        .count() as f64
        * 2.0;

    let gap_penalty = indices
        .windows(2)
        .map(|pair| (pair[1] - pair[0] - 1) as f64 * 1.5)
        .sum::<f64>();

    (base_score + consecutive_bonus + boundary_bonus + case_bonus - gap_penalty).max(0.0)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CompletionItem {
    pub label: String,
    pub description: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CompletionSet {
    pub items: Vec<CompletionItem>,
    pub selected: usize,
    pub replacement: (usize, usize),
}

#[derive(Clone, Debug)]
pub(crate) struct CommandEntry {
    pub alias: String,
    pub description: String,
}

pub(crate) fn command_completions(
    text: &str,
    cursor_pos: usize,
    entries: &[CommandEntry],
) -> Option<CompletionSet> {
    if !text.starts_with('/') {
        return None;
    }
    let cursor_pos = normalize_cursor(text, cursor_pos)?;
    let head = text.split(' ').next().unwrap_or_default();
    let head_end = cursor_pos.min(head.len());
    let query = head
        .get(1..head_end)
        .unwrap_or_default()
        .to_ascii_lowercase();
    let mut scored = Vec::<(CompletionItem, f64)>::new();
    for entry in entries.iter().filter(|entry| entry.alias.starts_with('/')) {
        let boost = match entry.alias.as_str() {
            "/help" => 2.0,
            "/config" => 1.0,
            _ => 0.0,
        };
        if query.is_empty() {
            scored.push((
                CompletionItem {
                    label: entry.alias.clone(),
                    description: entry.description.clone(),
                },
                boost,
            ));
            continue;
        }
        let result = fuzzy_match(&query, entry.alias.trim_start_matches('/'));
        if result.matched {
            scored.push((
                CompletionItem {
                    label: entry.alias.clone(),
                    description: entry.description.clone(),
                },
                result.score + boost,
            ));
        }
    }
    scored.sort_by(|left, right| right.1.partial_cmp(&left.1).unwrap_or(Ordering::Equal));
    let items = scored.into_iter().map(|(item, _)| item).collect::<Vec<_>>();
    (!items.is_empty()).then_some(CompletionSet {
        items,
        selected: 0,
        replacement: (0, head_end),
    })
}

#[derive(Clone, Debug)]
struct IndexEntry {
    rel: String,
    rel_lower: String,
    name: String,
    #[allow(dead_code)]
    path: PathBuf,
    is_dir: bool,
    ascii_mask: Option<u128>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct FileIndexStats {
    pub rebuilds: usize,
    pub incremental_updates: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FileChange {
    #[allow(dead_code)]
    Added,
    #[allow(dead_code)]
    Modified,
    #[allow(dead_code)]
    Deleted,
}

#[derive(Clone, Debug)]
struct FileIndexStore {
    ignore_rules: Option<IgnoreRules>,
    stats: FileIndexStats,
    entries_by_rel: BTreeMap<String, IndexEntry>,
    ordered_entries: Option<Vec<IndexEntry>>,
    root: Option<PathBuf>,
    mass_change_threshold: usize,
}

impl FileIndexStore {
    fn new(mass_change_threshold: usize) -> Self {
        Self {
            ignore_rules: None,
            stats: FileIndexStats::default(),
            entries_by_rel: BTreeMap::new(),
            ordered_entries: None,
            root: None,
            mass_change_threshold,
        }
    }

    fn clear(&mut self) {
        self.ignore_rules = None;
        self.entries_by_rel.clear();
        self.ordered_entries = None;
        self.root = None;
    }

    fn rebuild(&mut self, root: &Path) -> std::io::Result<()> {
        let root = root.canonicalize()?;
        self.ignore_rules = Some(IgnoreRules::for_root(&root));
        let mut entries = Vec::new();
        let mut seen = HashSet::new();
        let ignore_rules = self.ignore_rules.as_ref().expect("ignore rules set");
        collect_index_inner(&root, &root, ignore_rules, &mut entries, &mut seen)?;
        self.entries_by_rel = entries
            .iter()
            .cloned()
            .map(|entry| (entry.rel.clone(), entry))
            .collect();
        self.ordered_entries = Some(entries);
        self.root = Some(root);
        self.stats.rebuilds += 1;
        Ok(())
    }

    fn snapshot(&mut self) -> Vec<IndexEntry> {
        if self.entries_by_rel.is_empty() {
            return Vec::new();
        }
        if self.ordered_entries.is_none() {
            self.ordered_entries = Some(self.entries_by_rel.values().cloned().collect());
        }
        self.ordered_entries.clone().unwrap_or_default()
    }

    fn apply_changes(&mut self, changes: &[(FileChange, PathBuf)]) -> std::io::Result<()> {
        if self.root.is_none() {
            return Ok(());
        }
        if changes.len() > self.mass_change_threshold {
            let root = self.root.clone().expect("root checked");
            return self.rebuild(&root);
        }

        let mut modified = false;
        let root = self.root.clone().expect("root checked");
        let ignore_rules = self
            .ignore_rules
            .get_or_insert_with(|| IgnoreRules::for_root(&root))
            .clone();
        for (change, path) in changes {
            let absolute = if path.is_absolute() {
                path.clone()
            } else {
                root.join(path)
            };
            let absolute = absolute.canonicalize().unwrap_or(absolute);
            let Ok(rel_path) = absolute.strip_prefix(&root) else {
                continue;
            };
            let rel = rel_path.to_string_lossy().replace('\\', "/");
            if rel.is_empty() {
                continue;
            }

            if *change == FileChange::Deleted {
                modified |= self.remove_entry(&rel);
                continue;
            }

            if !absolute.exists() {
                continue;
            }
            if absolute.is_dir() {
                if let Some(entry) = create_index_entry(&root, &absolute, &ignore_rules) {
                    self.entries_by_rel.insert(entry.rel.clone(), entry);
                    modified = true;
                    for entry in collect_index_subtree(&root, &absolute, &ignore_rules)? {
                        self.entries_by_rel.insert(entry.rel.clone(), entry);
                        modified = true;
                    }
                }
            } else if let Some(entry) = create_index_entry(&root, &absolute, &ignore_rules) {
                self.entries_by_rel.insert(entry.rel.clone(), entry);
                modified = true;
            }
        }

        if modified {
            self.ordered_entries = None;
            self.stats.incremental_updates += 1;
        }
        Ok(())
    }

    fn remove_entry(&mut self, rel: &str) -> bool {
        let Some(entry) = self.entries_by_rel.remove(rel) else {
            return false;
        };
        if entry.is_dir {
            let prefix = format!("{rel}/");
            let to_remove = self
                .entries_by_rel
                .keys()
                .filter(|key| key.starts_with(&prefix))
                .cloned()
                .collect::<Vec<_>>();
            for key in to_remove {
                self.entries_by_rel.remove(&key);
            }
        }
        true
    }
}

pub(crate) struct FileIndexer {
    store: FileIndexStore,
    should_enable_watcher: Box<dyn Fn() -> bool>,
    watcher_active: bool,
    watcher_seen: BTreeMap<String, (PathBuf, bool)>,
    shutdown: bool,
}

impl FileIndexer {
    pub(crate) fn new() -> Self {
        Self::with_mass_change_threshold(200)
    }

    fn with_mass_change_threshold(mass_change_threshold: usize) -> Self {
        Self {
            store: FileIndexStore::new(mass_change_threshold),
            should_enable_watcher: Box::new(|| false),
            watcher_active: false,
            watcher_seen: BTreeMap::new(),
            shutdown: false,
        }
    }

    #[allow(dead_code)]
    fn with_watcher_enabled_getter(
        mass_change_threshold: usize,
        should_enable_watcher: impl Fn() -> bool + 'static,
    ) -> Self {
        Self {
            store: FileIndexStore::new(mass_change_threshold),
            should_enable_watcher: Box::new(should_enable_watcher),
            watcher_active: false,
            watcher_seen: BTreeMap::new(),
            shutdown: false,
        }
    }

    #[allow(dead_code)]
    fn stats(&self) -> &FileIndexStats {
        &self.store.stats
    }

    fn get_index(&mut self, root: &Path) -> std::io::Result<Vec<IndexEntry>> {
        if self.shutdown {
            return Ok(Vec::new());
        }
        let resolved_root = root.canonicalize()?;
        if self.store.root.as_deref() != Some(resolved_root.as_path()) {
            self.watcher_active = false;
            self.watcher_seen.clear();
            self.store.rebuild(&resolved_root)?;
        }
        self.sync_watcher(&resolved_root)?;
        Ok(self.store.snapshot())
    }

    #[allow(dead_code)]
    fn refresh(&mut self) {
        self.watcher_active = false;
        self.watcher_seen.clear();
        self.store.clear();
    }

    #[allow(dead_code)]
    fn shutdown(&mut self) {
        self.refresh();
        self.shutdown = true;
    }

    #[allow(dead_code)]
    fn apply_changes(&mut self, changes: &[(FileChange, PathBuf)]) -> std::io::Result<()> {
        self.store.apply_changes(changes)
    }

    fn sync_watcher(&mut self, root: &Path) -> std::io::Result<()> {
        if !(self.should_enable_watcher)() {
            self.watcher_active = false;
            self.watcher_seen.clear();
            return Ok(());
        }
        if !self.watcher_active {
            self.watcher_active = true;
            self.watcher_seen = collect_index(root)?
                .into_iter()
                .map(|entry| (entry.rel.clone(), (entry.path, entry.is_dir)))
                .collect();
            return Ok(());
        }
        let current_entries = collect_index(root)?;
        let current = current_entries
            .iter()
            .map(|entry| (entry.rel.clone(), (entry.path.clone(), entry.is_dir)))
            .collect::<BTreeMap<_, _>>();
        let mut changes = Vec::new();

        for (rel, (path, _)) in &self.watcher_seen {
            if !current.contains_key(rel) {
                changes.push((FileChange::Deleted, path.clone()));
            }
        }
        for (rel, (path, _)) in &current {
            if !self.watcher_seen.contains_key(rel) {
                changes.push((FileChange::Added, path.clone()));
            }
        }

        if !changes.is_empty() {
            self.store.apply_changes(&changes)?;
        }
        self.watcher_seen = current;
        Ok(())
    }
}

#[allow(dead_code)]
pub(crate) fn path_completions(
    base_dir: &Path,
    text: &str,
    cursor_pos: usize,
) -> Option<CompletionSet> {
    let mut indexer = FileIndexer::new();
    path_completions_with_indexer(&mut indexer, base_dir, text, cursor_pos)
}

pub(crate) fn path_completions_with_indexer(
    indexer: &mut FileIndexer,
    base_dir: &Path,
    text: &str,
    cursor_pos: usize,
) -> Option<CompletionSet> {
    let cursor_pos = normalize_cursor(text, cursor_pos)?;
    let before_cursor = &text[..cursor_pos];
    let at_index = before_cursor.rfind('@')?;
    let partial = &before_cursor[at_index + 1..];
    if partial.contains(char::is_whitespace) {
        return None;
    }
    let context = SearchContext::new(partial);
    let entries = indexer.get_index(base_dir).ok()?;
    let mut scored = score_path_matches(entries, &context);
    scored.truncate(DEFAULT_TARGET_MATCHES);
    let items = scored
        .into_iter()
        .map(|(label, _)| CompletionItem {
            label,
            description: String::new(),
        })
        .collect::<Vec<_>>();
    (!items.is_empty()).then_some(CompletionSet {
        items,
        selected: 0,
        replacement: (at_index, cursor_pos),
    })
}

fn normalize_cursor(text: &str, cursor_pos: usize) -> Option<usize> {
    if cursor_pos >= text.len() {
        return Some(text.len());
    }
    if text.is_char_boundary(cursor_pos) {
        return Some(cursor_pos);
    }
    if cursor_pos <= text.chars().count() {
        return text
            .char_indices()
            .nth(cursor_pos)
            .map(|(index, _)| index)
            .or(Some(text.len()));
    }
    None
}

#[derive(Clone, Debug)]
struct SearchContext {
    suffix: String,
    search_pattern: String,
    path_prefix: String,
    immediate_only: bool,
    ascii_mask: Option<u128>,
}

impl SearchContext {
    fn new(partial_path: &str) -> Self {
        let suffix = partial_path
            .rsplit('/')
            .next()
            .unwrap_or_default()
            .to_string();
        let ascii_mask = build_ascii_mask(&partial_path.to_ascii_lowercase());
        if partial_path.is_empty() {
            return Self {
                suffix,
                search_pattern: String::new(),
                path_prefix: String::new(),
                immediate_only: true,
                ascii_mask,
            };
        }
        if partial_path.ends_with('/') {
            return Self {
                suffix,
                search_pattern: String::new(),
                path_prefix: partial_path.to_string(),
                immediate_only: true,
                ascii_mask,
            };
        }
        Self {
            suffix,
            search_pattern: partial_path.to_string(),
            path_prefix: String::new(),
            immediate_only: false,
            ascii_mask,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PathRank {
    exact_directory: i32,
    immediate_child_of_exact_path: i32,
    exact_filename: i32,
    preferred_stem_match: i32,
    exact_stem: i32,
    stem_prefix: i32,
    name_prefix: i32,
    extension_match: i32,
    fuzzy_score_millis: i64,
    shallow_path: i32,
}

impl Ord for PathRank {
    fn cmp(&self, other: &Self) -> Ordering {
        (
            self.exact_directory,
            self.immediate_child_of_exact_path,
            self.exact_filename,
            self.preferred_stem_match,
            self.exact_stem,
            self.stem_prefix,
            self.name_prefix,
            self.extension_match,
            self.fuzzy_score_millis,
            self.shallow_path,
        )
            .cmp(&(
                other.exact_directory,
                other.immediate_child_of_exact_path,
                other.exact_filename,
                other.preferred_stem_match,
                other.exact_stem,
                other.stem_prefix,
                other.name_prefix,
                other.extension_match,
                other.fuzzy_score_millis,
                other.shallow_path,
            ))
    }
}

impl PartialOrd for PathRank {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

fn score_path_matches(
    entries: Vec<IndexEntry>,
    context: &SearchContext,
) -> Vec<(String, PathRank)> {
    let mut scored = Vec::new();
    for entry in entries.into_iter().take(DEFAULT_MAX_ENTRIES_TO_PROCESS) {
        if !matches_prefix(&entry, context) || !is_visible(&entry, context) {
            continue;
        }
        let label = format_path_label(&entry);
        if context.search_pattern.is_empty() {
            scored.push((label, build_path_rank(&entry, context, 0.0)));
            if scored.len() >= DEFAULT_TARGET_MATCHES {
                break;
            }
            continue;
        }
        if let (Some(entry_mask), Some(query_mask)) = (entry.ascii_mask, context.ascii_mask)
            && entry_mask & query_mask != query_mask
        {
            continue;
        }
        let result = fuzzy_match(&context.search_pattern, &entry.rel);
        if result.matched {
            scored.push((label, build_path_rank(&entry, context, result.score)));
        }
    }
    scored.sort_by(|left, right| left.0.cmp(&right.0));
    scored.sort_by(|left, right| right.1.cmp(&left.1));
    scored
}

#[allow(dead_code)]
fn collect_index(base_dir: &Path) -> std::io::Result<Vec<IndexEntry>> {
    let mut indexer = FileIndexer::new();
    indexer.get_index(base_dir)
}

fn collect_index_inner(
    base_dir: &Path,
    dir: &Path,
    ignore_rules: &IgnoreRules,
    entries: &mut Vec<IndexEntry>,
    seen: &mut HashSet<PathBuf>,
) -> std::io::Result<()> {
    let mut children = fs::read_dir(dir)?
        .filter_map(Result::ok)
        .collect::<Vec<_>>();
    children.sort_by_key(|entry| entry.path());
    for child in children {
        let path = child.path();
        let Ok(metadata) = child.metadata() else {
            continue;
        };
        let Ok(rel_path) = path.strip_prefix(base_dir) else {
            continue;
        };
        let rel = rel_path.to_string_lossy().replace('\\', "/");
        if rel.is_empty() || !seen.insert(path.clone()) {
            continue;
        }
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default();
        if ignore_rules.should_ignore(&rel, &name, metadata.is_dir()) {
            continue;
        }
        let rel_lower = rel.to_ascii_lowercase();
        entries.push(IndexEntry {
            rel: rel.clone(),
            rel_lower,
            name,
            path: path.clone(),
            is_dir: metadata.is_dir(),
            ascii_mask: build_ascii_mask(&rel.to_ascii_lowercase()),
        });
        if metadata.is_dir() {
            let _ = collect_index_inner(base_dir, &path, ignore_rules, entries, seen);
        }
    }
    Ok(())
}

fn collect_index_subtree(
    base_dir: &Path,
    dir: &Path,
    ignore_rules: &IgnoreRules,
) -> std::io::Result<Vec<IndexEntry>> {
    let mut entries = Vec::new();
    let mut seen = HashSet::new();
    collect_index_inner(base_dir, dir, ignore_rules, &mut entries, &mut seen)?;
    entries.sort_by(|left, right| left.rel.cmp(&right.rel));
    Ok(entries)
}

fn create_index_entry(
    base_dir: &Path,
    path: &Path,
    ignore_rules: &IgnoreRules,
) -> Option<IndexEntry> {
    let metadata = path.metadata().ok()?;
    let rel_path = path.strip_prefix(base_dir).ok()?;
    let rel = rel_path.to_string_lossy().replace('\\', "/");
    if rel.is_empty() {
        return None;
    }
    let name = path.file_name()?.to_string_lossy().to_string();
    if ignore_rules.should_ignore(&rel, &name, metadata.is_dir()) {
        return None;
    }
    let rel_lower = rel.to_ascii_lowercase();
    Some(IndexEntry {
        rel: rel.clone(),
        rel_lower,
        name,
        path: path.to_path_buf(),
        is_dir: metadata.is_dir(),
        ascii_mask: build_ascii_mask(&rel.to_ascii_lowercase()),
    })
}

#[derive(Clone, Debug)]
struct IgnorePattern {
    matcher: Pattern,
    is_exclude: bool,
    dir_only: bool,
    name_only: bool,
    anchor_root: bool,
}

#[derive(Clone, Debug)]
struct IgnoreRules {
    patterns: Vec<IgnorePattern>,
}

impl IgnoreRules {
    fn for_root(root: &Path) -> Self {
        let mut patterns = DEFAULT_IGNORE_PATTERNS
            .iter()
            .filter_map(|(raw, is_exclude)| Self::compile(raw, *is_exclude))
            .collect::<Vec<_>>();

        let gitignore_path = root.join(".gitignore");
        if let Ok(text) = fs::read_to_string(gitignore_path) {
            for line in text.lines() {
                if let Some((raw, is_exclude)) = parse_gitignore_line(line)
                    && let Some(pattern) = Self::compile(&raw, is_exclude)
                {
                    patterns.push(pattern);
                }
            }
        }

        Self { patterns }
    }

    fn compile(raw: &str, is_exclude: bool) -> Option<IgnorePattern> {
        let anchor_root = raw.starts_with('/');
        let raw = raw.strip_prefix('/').unwrap_or(raw);
        let stripped = raw.trim_end_matches('/').to_string();
        if stripped.is_empty() {
            return None;
        }
        let matcher = Pattern::new(&stripped).ok()?;
        Some(IgnorePattern {
            matcher,
            is_exclude,
            dir_only: raw.ends_with('/'),
            name_only: !stripped.contains('/'),
            anchor_root,
        })
    }

    fn should_ignore(&self, rel: &str, name: &str, is_dir: bool) -> bool {
        let mut ignored = false;
        for pattern in &self.patterns {
            if pattern.matches(rel, name, is_dir) {
                ignored = pattern.is_exclude;
            }
        }
        ignored
    }
}

impl IgnorePattern {
    fn matches(&self, rel: &str, name: &str, is_dir: bool) -> bool {
        if self.name_only && self.anchor_root && rel.contains('/') {
            return false;
        }
        let target = if self.name_only { name } else { rel };
        self.matcher.matches(target) && (!self.dir_only || is_dir)
    }
}

fn parse_gitignore_line(line: &str) -> Option<(String, bool)> {
    let mut raw = line.trim().to_string();
    if raw.is_empty() || raw.starts_with('#') {
        return None;
    }
    if let Some(index) = raw.find('#') {
        raw.truncate(index);
        raw = raw.trim_end().to_string();
        if raw.is_empty() {
            return None;
        }
    }
    let is_exclude = !raw.starts_with('!');
    if !is_exclude {
        raw = raw[1..].trim_start().to_string();
        if raw.is_empty() {
            return None;
        }
    }
    Some((raw, is_exclude))
}

fn build_ascii_mask(value: &str) -> Option<u128> {
    let mut mask = 0u128;
    for ch in value.chars() {
        let codepoint = ch as u32;
        if codepoint >= 128 {
            return None;
        }
        mask |= 1u128 << codepoint;
    }
    Some(mask)
}

fn matches_prefix(entry: &IndexEntry, context: &SearchContext) -> bool {
    let path = entry.rel.as_str();
    if !context.path_prefix.is_empty() {
        let prefix_without_slash = context.path_prefix.trim_end_matches('/');
        if path == prefix_without_slash && entry.is_dir {
            return false;
        }
        return is_immediate_child_of_prefix(path, &context.path_prefix);
    }
    !(context.immediate_only && path.contains('/'))
}

fn is_immediate_child_of_prefix(path: &str, prefix: &str) -> bool {
    let prefix_without_slash = prefix.trim_end_matches('/');
    let prefix_with_slash = format!("{prefix_without_slash}/");
    let after_prefix = if let Some(after) = path.strip_prefix(&prefix_with_slash) {
        after
    } else if let Some(index) = path.find(&prefix_with_slash) {
        if index > 0 && path.as_bytes().get(index - 1) != Some(&b'/') {
            return false;
        }
        &path[index + prefix_with_slash.len()..]
    } else {
        return false;
    };
    !after_prefix.is_empty() && !after_prefix.contains('/')
}

fn is_visible(entry: &IndexEntry, context: &SearchContext) -> bool {
    !entry.name.starts_with('.') || context.suffix.starts_with('.')
}

fn format_path_label(entry: &IndexEntry) -> String {
    let suffix = if entry.is_dir { "/" } else { "" };
    format!("@{}{suffix}", entry.rel)
}

fn build_path_rank(entry: &IndexEntry, context: &SearchContext, fuzzy_score: f64) -> PathRank {
    let query = context.suffix.to_ascii_lowercase();
    if query.is_empty() {
        return PathRank {
            exact_directory: 0,
            immediate_child_of_exact_path: 0,
            exact_filename: 0,
            preferred_stem_match: 0,
            exact_stem: 0,
            stem_prefix: 0,
            name_prefix: 0,
            extension_match: 0,
            fuzzy_score_millis: (fuzzy_score * 1000.0) as i64,
            shallow_path: -(entry.rel.matches('/').count() as i32),
        };
    }
    let name = entry.name.to_ascii_lowercase();
    let rel = entry.rel_lower.as_str();
    let stem = Path::new(&entry.name)
        .file_stem()
        .map(|stem| stem.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    let extension = Path::new(&entry.name)
        .extension()
        .map(|extension| format!(".{}", extension.to_string_lossy().to_ascii_lowercase()))
        .unwrap_or_default();
    let query_path = Path::new(&query);
    let query_extension = query_path
        .extension()
        .map(|extension| format!(".{}", extension.to_string_lossy().to_ascii_lowercase()))
        .unwrap_or_default();
    let query_stem = query_path
        .file_stem()
        .map(|stem| stem.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_else(|| query.clone());
    let query_looks_like_filename = query.contains('.');
    let query_looks_like_path = context.search_pattern.contains('/');
    PathRank {
        exact_directory: i32::from(
            entry.is_dir && rel == context.search_pattern.to_ascii_lowercase(),
        ),
        immediate_child_of_exact_path: i32::from(
            query_looks_like_path
                && is_immediate_child_of_prefix(rel, &context.search_pattern.to_ascii_lowercase()),
        ),
        exact_filename: i32::from(query_looks_like_filename && name == query),
        preferred_stem_match: i32::from(stem == query && extension != ".lock"),
        exact_stem: i32::from(stem == query || (query_looks_like_filename && stem == query_stem)),
        stem_prefix: i32::from(stem.starts_with(if query_looks_like_filename {
            query_stem.as_str()
        } else {
            query.as_str()
        })),
        name_prefix: i32::from(name.starts_with(&query)),
        extension_match: i32::from(!query_extension.is_empty() && extension == query_extension),
        fuzzy_score_millis: (fuzzy_score * 1000.0) as i64,
        shallow_path: -(entry.rel.matches('/').count() as i32),
    }
}

pub(crate) fn replace_completion(
    text: &mut String,
    cursor: &mut usize,
    completion: &CompletionSet,
) {
    let Some(item) = completion.items.get(completion.selected) else {
        return;
    };
    let (start, end) = completion.replacement;
    if start > end
        || end > text.len()
        || !text.is_char_boundary(start)
        || !text.is_char_boundary(end)
    {
        return;
    }
    text.replace_range(start..end, &item.label);
    *cursor = start + item.label.len();
    if item.label.starts_with('@')
        && !item.label.ends_with('/')
        && !text[*cursor..].starts_with(char::is_whitespace)
    {
        text.insert(*cursor, ' ');
        *cursor += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::rc::Rc;

    fn labels(base_dir: &Path, text: &str, cursor_pos: usize) -> Vec<String> {
        path_completions(base_dir, text, cursor_pos)
            .map(|set| set.items.into_iter().map(|item| item.label).collect())
            .unwrap_or_default()
    }

    fn index_rels(indexer: &mut FileIndexer, root: &Path) -> Vec<String> {
        indexer
            .get_index(root)
            .unwrap()
            .into_iter()
            .map(|entry| entry.rel)
            .collect()
    }

    fn build_fuzzy_tree(base_dir: &Path) {
        fs::create_dir_all(base_dir.join("src/utils")).unwrap();
        fs::create_dir_all(base_dir.join("src/core")).unwrap();
        fs::create_dir_all(base_dir.join("config")).unwrap();
        fs::write(base_dir.join("src/main.py"), "").unwrap();
        fs::write(base_dir.join("src/models.py"), "").unwrap();
        fs::write(base_dir.join("src/core/logger.py"), "").unwrap();
        fs::write(base_dir.join("src/core/models.py"), "").unwrap();
        fs::write(base_dir.join("src/core/ports.py"), "").unwrap();
        fs::write(base_dir.join("src/core/sanitize.py"), "").unwrap();
        fs::write(base_dir.join("src/core/use_cases.py"), "").unwrap();
        fs::write(base_dir.join("src/core/validate.py"), "").unwrap();
        fs::write(base_dir.join("README.md"), "").unwrap();
        fs::write(base_dir.join(".env"), "").unwrap();
        fs::write(base_dir.join("config/settings.py"), "").unwrap();
        fs::write(base_dir.join("config/database.py"), "").unwrap();
    }

    fn build_recursive_tree(base_dir: &Path) {
        fs::create_dir_all(base_dir.join("vibe/acp")).unwrap();
        fs::create_dir_all(base_dir.join("vibe/cli/autocompletion")).unwrap();
        fs::create_dir_all(base_dir.join("tests/autocompletion")).unwrap();
        fs::write(base_dir.join("vibe/acp/entrypoint.py"), "").unwrap();
        fs::write(base_dir.join("vibe/acp/agent.py"), "").unwrap();
        fs::write(base_dir.join("vibe/cli/autocompletion/fuzzy.py"), "").unwrap();
        fs::write(base_dir.join("vibe/cli/autocompletion/completers.py"), "").unwrap();
        fs::write(base_dir.join("tests/autocompletion/test_fuzzy.py"), "").unwrap();
        fs::write(base_dir.join("README.md"), "").unwrap();
    }

    #[test]
    fn fuzzy_matches_upstream_examples() {
        assert_eq!(
            fuzzy_match("", "any_text").matched_indices,
            Vec::<usize>::new()
        );
        assert_eq!(
            fuzzy_match("src/", "src/main.py").matched_indices,
            vec![0, 1, 2, 3]
        );
        assert!(!fuzzy_match("ms", "src/main.py").matched);
        assert_eq!(
            fuzzy_match("main", "src/main.py").matched_indices,
            vec![4, 5, 6, 7]
        );
        assert_eq!(
            fuzzy_match("SRC", "src/main.py").matched_indices,
            vec![0, 1, 2]
        );
        assert_eq!(fuzzy_match("sm", "src/main.py").matched_indices, vec![0, 4]);
        assert_eq!(fuzzy_match("m", "src/main.py").matched_indices, vec![4]);
        assert_eq!(
            fuzzy_match("MP", "src/MainPy.py").matched_indices,
            vec![4, 8]
        );
        assert_eq!(fuzzy_match("a", "banana").matched_indices, vec![1]);
    }

    #[test]
    fn fuzzy_scores_match_upstream_ordering() {
        assert!(
            fuzzy_match("ma", "src/main.py").score > fuzzy_match("ma", "src/important.py").score
        );
        assert!(fuzzy_match("src", "src/main.py").score > fuzzy_match("main", "src/main.py").score);
        assert!(fuzzy_match("main", "src/main.py").score > fuzzy_match("mn", "src/main.py").score);
        assert!(
            fuzzy_match("Main", "src/Main.py").score > fuzzy_match("main", "src/Main.py").score
        );
        assert!(!fuzzy_match("very_long_pattern", "short").matched);
    }

    #[test]
    fn command_completion_fuzzy_filters_slash_commands() {
        let entries = vec![
            CommandEntry {
                alias: "/help".to_string(),
                description: "Show help".to_string(),
            },
            CommandEntry {
                alias: "/config".to_string(),
                description: "Config".to_string(),
            },
            CommandEntry {
                alias: "/compact".to_string(),
                description: "Compact".to_string(),
            },
        ];
        let empty = command_completions("/", 1, &entries).unwrap();
        assert_eq!(empty.items[0].label, "/help");
        let fuzzy = command_completions("/cp", 3, &entries).unwrap();
        assert_eq!(fuzzy.items[0].label, "/compact");
        assert_eq!(fuzzy.replacement, (0, 3));
        assert!(command_completions("hello /help", 11, &entries).is_none());
    }

    #[test]
    fn replace_completion_adds_spacing_for_files_but_not_directories() {
        let mut file_text = "Print @REA".to_string();
        let mut file_cursor = file_text.len();
        replace_completion(
            &mut file_text,
            &mut file_cursor,
            &CompletionSet {
                items: vec![CompletionItem {
                    label: "@README.md".to_string(),
                    description: String::new(),
                }],
                selected: 0,
                replacement: (6, 10),
            },
        );
        assert_eq!(file_text, "Print @README.md ");
        assert_eq!(file_cursor, file_text.len());

        let mut dir_text = "@sr".to_string();
        let mut dir_cursor = dir_text.len();
        replace_completion(
            &mut dir_text,
            &mut dir_cursor,
            &CompletionSet {
                items: vec![CompletionItem {
                    label: "@src/".to_string(),
                    description: String::new(),
                }],
                selected: 0,
                replacement: (0, 3),
            },
        );
        assert_eq!(dir_text, "@src/");
        assert_eq!(dir_cursor, dir_text.len());
    }

    #[test]
    fn path_completion_lists_top_level_and_nested_children() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("README.md"), "hello").unwrap();
        fs::create_dir(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/main.rs"), "fn main() {}").unwrap();
        fs::write(dir.path().join(".env"), "TOKEN=x").unwrap();

        let top = path_completions(dir.path(), "see @", 5).unwrap();
        assert!(top.items.iter().any(|item| item.label == "@README.md"));
        assert!(top.items.iter().any(|item| item.label == "@src/"));
        assert!(!top.items.iter().any(|item| item.label == "@.env"));

        let hidden = path_completions(dir.path(), "see @.", 6).unwrap();
        assert!(hidden.items.iter().any(|item| item.label == "@.env"));

        let nested = path_completions(dir.path(), "see @src/", 9).unwrap();
        assert_eq!(nested.items[0].label, "@src/main.rs");
    }

    #[test]
    fn path_completion_respects_default_ignore_patterns() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("node_modules/pkg")).unwrap();
        fs::create_dir_all(dir.path().join("target/debug")).unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("node_modules/pkg/index.js"), "").unwrap();
        fs::write(dir.path().join("target/debug/app"), "").unwrap();
        fs::write(dir.path().join("src/main.rs"), "").unwrap();
        fs::write(dir.path().join("trace.log"), "").unwrap();

        let items = path_completions(dir.path(), "see @", 5)
            .unwrap()
            .items
            .into_iter()
            .map(|item| item.label)
            .collect::<Vec<_>>();

        assert!(items.contains(&"@src/".to_string()));
        assert!(!items.iter().any(|item| item.starts_with("@node_modules")));
        assert!(!items.iter().any(|item| item.starts_with("@target")));
        assert!(!items.contains(&"@trace.log".to_string()));
    }

    #[test]
    fn path_completion_respects_gitignore_and_negation_order() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("generated/keep")).unwrap();
        fs::write(
            dir.path().join(".gitignore"),
            "generated/\n!generated/keep/\n*.tmp\n",
        )
        .unwrap();
        fs::write(dir.path().join("generated/file.txt"), "").unwrap();
        fs::write(dir.path().join("generated/keep/file.txt"), "").unwrap();
        fs::write(dir.path().join("scratch.tmp"), "").unwrap();
        fs::write(dir.path().join("visible.txt"), "").unwrap();

        let items = path_completions(dir.path(), "see @", 5)
            .unwrap()
            .items
            .into_iter()
            .map(|item| item.label)
            .collect::<Vec<_>>();

        assert!(items.contains(&"@visible.txt".to_string()));
        assert!(!items.contains(&"@scratch.tmp".to_string()));
        assert!(!items.iter().any(|item| item.starts_with("@generated/")));
    }

    #[test]
    fn path_completion_matches_upstream_fuzzy_ranking_cases() {
        let dir = tempfile::tempdir().unwrap();
        build_fuzzy_tree(dir.path());

        assert!(labels(dir.path(), "@sr", 3).contains(&"@src/".to_string()));
        assert_eq!(labels(dir.path(), "@src", 4)[0], "@src/");
        assert_eq!(labels(dir.path(), "@src/main", 9)[0], "@src/main.py");
        assert!(labels(dir.path(), "@src/mp", 7).contains(&"@src/models.py".to_string()));
        assert!(labels(dir.path(), "@src/core/l", 11).contains(&"@src/core/logger.py".to_string()));
        assert!(labels(dir.path(), "@src/mo", 7).contains(&"@src/models.py".to_string()));
        assert!(labels(dir.path(), "@readme", 7).contains(&"@README.md".to_string()));
        assert!(labels(dir.path(), "@README", 7).contains(&"@README.md".to_string()));
        assert!(!labels(dir.path(), "@e", 2).contains(&"@.env".to_string()));
        assert!(labels(dir.path(), "@.", 2).contains(&"@.env".to_string()));
        assert!(labels(dir.path(), "@xyz123", 7).is_empty());

        let src_children = labels(dir.path(), "@src/", 5);
        assert!(src_children.contains(&"@src/main.py".to_string()));
        assert!(src_children.contains(&"@src/core/".to_string()));
        assert!(src_children.contains(&"@src/utils/".to_string()));
    }

    #[test]
    fn path_completion_matches_upstream_recursive_cases() {
        let dir = tempfile::tempdir().unwrap();
        build_recursive_tree(dir.path());

        assert_eq!(
            labels(dir.path(), "@entryp", 7)[0],
            "@vibe/acp/entrypoint.py"
        );
        assert_eq!(
            labels(dir.path(), "@acp/entry", 10)[0],
            "@vibe/acp/entrypoint.py"
        );
        assert_eq!(
            labels(dir.path(), "@acp/ent", 9)[0],
            "@vibe/acp/entrypoint.py"
        );
        let fuzzy = labels(dir.path(), "@fuzzy", 6);
        assert!(
            fuzzy
                .iter()
                .position(|item| item == "@vibe/cli/autocompletion/fuzzy.py")
                < fuzzy
                    .iter()
                    .position(|item| item == "@tests/autocompletion/test_fuzzy.py")
        );
        assert_eq!(
            labels(dir.path(), "@vibe/acp/entrypoint", 20)[0],
            "@vibe/acp/entrypoint.py"
        );
        assert_eq!(
            labels(dir.path(), "@acp", 4),
            vec![
                "@vibe/acp/",
                "@vibe/acp/agent.py",
                "@vibe/acp/entrypoint.py",
                "@vibe/cli/autocompletion/completers.py",
                "@tests/autocompletion/",
                "@tests/autocompletion/test_fuzzy.py",
                "@vibe/cli/autocompletion/",
                "@vibe/cli/autocompletion/fuzzy.py",
            ]
        );
    }

    #[test]
    fn path_completion_prefers_exact_filenames_and_source_stems() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src/features")).unwrap();
        fs::create_dir_all(dir.path().join("docs")).unwrap();
        fs::write(dir.path().join("src/chat-input.tsx"), "").unwrap();
        fs::write(dir.path().join("src/features/chat-input-state.ts"), "").unwrap();
        fs::write(dir.path().join("docs/chat-input.md"), "").unwrap();
        assert_eq!(
            labels(dir.path(), "@chat-input", 11)[0],
            "@src/chat-input.tsx"
        );

        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("pkg")).unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("pkg/foo.lock"), "").unwrap();
        fs::write(dir.path().join("src/foo.py"), "").unwrap();
        assert_eq!(labels(dir.path(), "@foo", 4)[0], "@src/foo.py");
    }

    #[test]
    fn path_completion_prefers_exact_path_children_over_unrelated_fuzzy_matches() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("scripts/zephyr/generators")).unwrap();
        fs::create_dir_all(dir.path().join("zephyr/generators/tests")).unwrap();
        fs::create_dir_all(dir.path().join("zephyr/generators/prompts")).unwrap();
        fs::write(dir.path().join("zephyr/generators/common.py"), "").unwrap();
        fs::write(dir.path().join("zephyr/generators/README.md"), "").unwrap();
        fs::create_dir_all(
            dir.path()
                .join("zephyr/datasets/synthetic_sp_up_conflict/grounded_policies/hotel"),
        )
        .unwrap();
        fs::create_dir_all(
            dir.path()
                .join("zephyr/datasets/synthetic_sp_up_conflict/grounded_policies/retail"),
        )
        .unwrap();
        fs::write(
            dir.path().join(
                "zephyr/datasets/synthetic_sp_up_conflict/grounded_policies/hotel/generators.py",
            ),
            "",
        )
        .unwrap();
        fs::write(
            dir.path().join(
                "zephyr/datasets/synthetic_sp_up_conflict/grounded_policies/retail/generators.py",
            ),
            "",
        )
        .unwrap();

        let results = labels(dir.path(), "@zephyr/generators", 18);
        let hotel = results
            .iter()
            .position(|item| item == "@zephyr/datasets/synthetic_sp_up_conflict/grounded_policies/hotel/generators.py")
            .unwrap();
        let retail = results
            .iter()
            .position(|item| item == "@zephyr/datasets/synthetic_sp_up_conflict/grounded_policies/retail/generators.py")
            .unwrap();
        assert_eq!(results[0], "@zephyr/generators/");
        assert!(
            results
                .iter()
                .position(|item| item == "@zephyr/generators/common.py")
                .unwrap()
                < hotel
        );
        assert!(
            results
                .iter()
                .position(|item| item == "@zephyr/generators/prompts/")
                .unwrap()
                < retail
        );
        assert!(
            results
                .iter()
                .position(|item| item == "@zephyr/generators/tests/")
                .unwrap()
                < hotel
        );
    }

    #[test]
    fn path_completion_handles_non_ascii_queries_like_vibe() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("café.txt"), "").unwrap();

        assert_eq!(labels(dir.path(), "@café", 5), vec!["@café.txt"]);
    }

    #[test]
    fn file_indexer_rebuilds_once_and_returns_stable_snapshots() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/main.rs"), "").unwrap();
        fs::write(dir.path().join("README.md"), "").unwrap();
        let mut indexer = FileIndexer::new();

        let first = indexer.get_index(dir.path()).unwrap();
        let second = indexer.get_index(dir.path()).unwrap();

        assert_eq!(indexer.stats().rebuilds, 1);
        assert_eq!(indexer.stats().incremental_updates, 0);
        assert_eq!(
            first
                .iter()
                .map(|entry| entry.rel.as_str())
                .collect::<Vec<_>>(),
            second
                .iter()
                .map(|entry| entry.rel.as_str())
                .collect::<Vec<_>>()
        );
        assert!(first.iter().any(|entry| entry.rel == "src/main.rs"));
    }

    #[test]
    fn file_indexer_refresh_forces_next_rebuild() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("one.txt"), "").unwrap();
        let mut indexer = FileIndexer::new();

        assert_eq!(indexer.get_index(dir.path()).unwrap().len(), 1);
        indexer.refresh();
        fs::write(dir.path().join("two.txt"), "").unwrap();
        let entries = indexer.get_index(dir.path()).unwrap();

        assert!(entries.iter().any(|entry| entry.rel == "one.txt"));
        assert!(entries.iter().any(|entry| entry.rel == "two.txt"));
        assert_eq!(indexer.stats().rebuilds, 2);
    }

    #[test]
    fn file_indexer_switching_roots_rebuilds_for_new_root() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        fs::write(first.path().join("first.py"), "").unwrap();
        fs::write(second.path().join("second.py"), "").unwrap();
        let mut indexer = FileIndexer::new();

        assert_eq!(indexer.get_index(first.path()).unwrap()[0].rel, "first.py");
        let second_entries = indexer.get_index(second.path()).unwrap();

        assert_eq!(second_entries.len(), 1);
        assert_eq!(second_entries[0].rel, "second.py");
        assert_eq!(indexer.stats().rebuilds, 2);
    }

    #[test]
    fn file_indexer_applies_incremental_file_and_directory_changes() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("seed.py"), "").unwrap();
        let mut indexer = FileIndexer::new();
        indexer.get_index(dir.path()).unwrap();

        let added = dir.path().join("new_file.py");
        fs::write(&added, "").unwrap();
        indexer
            .apply_changes(&[(FileChange::Added, added.clone())])
            .unwrap();
        assert!(
            indexer
                .get_index(dir.path())
                .unwrap()
                .iter()
                .any(|entry| entry.rel == "new_file.py")
        );

        fs::create_dir_all(dir.path().join("folder/nested")).unwrap();
        fs::write(dir.path().join("folder/nested/file.py"), "").unwrap();
        indexer
            .apply_changes(&[(FileChange::Added, dir.path().join("folder"))])
            .unwrap();
        let entries = indexer.get_index(dir.path()).unwrap();
        assert!(entries.iter().any(|entry| entry.rel == "folder"));
        assert!(
            entries
                .iter()
                .any(|entry| entry.rel == "folder/nested/file.py")
        );

        indexer
            .apply_changes(&[(FileChange::Deleted, dir.path().join("folder"))])
            .unwrap();
        let entries = indexer.get_index(dir.path()).unwrap();
        assert!(!entries.iter().any(|entry| entry.rel.starts_with("folder")));
        assert_eq!(indexer.stats().incremental_updates, 3);
    }

    #[test]
    fn file_indexer_rebuilds_when_mass_change_threshold_is_exceeded() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("seed.py"), "").unwrap();
        let mut indexer = FileIndexer::with_mass_change_threshold(1);
        indexer.get_index(dir.path()).unwrap();
        fs::write(dir.path().join("a.py"), "").unwrap();
        fs::write(dir.path().join("b.py"), "").unwrap();

        indexer
            .apply_changes(&[
                (FileChange::Added, dir.path().join("a.py")),
                (FileChange::Added, dir.path().join("b.py")),
            ])
            .unwrap();

        let entries = indexer.get_index(dir.path()).unwrap();
        assert!(entries.iter().any(|entry| entry.rel == "a.py"));
        assert!(entries.iter().any(|entry| entry.rel == "b.py"));
        assert_eq!(indexer.stats().rebuilds, 2);
    }

    #[test]
    fn file_indexer_incremental_updates_respect_ignore_rules() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(".gitignore"), "*.tmp\nignored/\n").unwrap();
        fs::write(dir.path().join("visible.py"), "").unwrap();
        let mut indexer = FileIndexer::new();
        indexer.get_index(dir.path()).unwrap();

        let ignored_file = dir.path().join("scratch.tmp");
        fs::write(&ignored_file, "").unwrap();
        fs::create_dir_all(dir.path().join("ignored")).unwrap();
        fs::write(dir.path().join("ignored/file.py"), "").unwrap();
        indexer
            .apply_changes(&[
                (FileChange::Added, ignored_file),
                (FileChange::Added, dir.path().join("ignored")),
            ])
            .unwrap();

        let entries = indexer.get_index(dir.path()).unwrap();
        assert!(entries.iter().any(|entry| entry.rel == "visible.py"));
        assert!(!entries.iter().any(|entry| entry.rel == "scratch.tmp"));
        assert!(!entries.iter().any(|entry| entry.rel.starts_with("ignored")));
    }

    #[test]
    fn file_indexer_watcher_is_disabled_by_default() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("seed.py"), "").unwrap();
        let mut indexer = FileIndexer::new();
        let baseline = index_rels(&mut indexer, dir.path());

        fs::write(dir.path().join("created_after_snapshot.py"), "").unwrap();
        let after_create = index_rels(&mut indexer, dir.path());

        assert_eq!(after_create, baseline);
        assert_eq!(indexer.stats().incremental_updates, 0);
    }

    #[test]
    fn file_indexer_watcher_updates_on_file_creation_deletion_and_rename() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("seed.py"), "").unwrap();
        let mut indexer = FileIndexer::with_watcher_enabled_getter(200, || true);
        assert_eq!(index_rels(&mut indexer, dir.path()), vec!["seed.py"]);

        fs::write(dir.path().join("new_file.py"), "").unwrap();
        assert!(index_rels(&mut indexer, dir.path()).contains(&"new_file.py".to_string()));

        fs::remove_file(dir.path().join("seed.py")).unwrap();
        assert!(!index_rels(&mut indexer, dir.path()).contains(&"seed.py".to_string()));

        fs::rename(
            dir.path().join("new_file.py"),
            dir.path().join("renamed.py"),
        )
        .unwrap();
        let entries = index_rels(&mut indexer, dir.path());
        assert!(!entries.contains(&"new_file.py".to_string()));
        assert!(entries.contains(&"renamed.py".to_string()));
        assert!(indexer.stats().incremental_updates >= 3);
    }

    #[test]
    fn file_indexer_watcher_updates_folder_rename_recursively() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("old_folder")).unwrap();
        fs::write(dir.path().join("old_folder/file1.py"), "").unwrap();
        fs::write(dir.path().join("old_folder/file2.py"), "").unwrap();
        let mut indexer = FileIndexer::with_watcher_enabled_getter(200, || true);
        assert!(index_rels(&mut indexer, dir.path()).contains(&"old_folder/file1.py".to_string()));

        fs::rename(dir.path().join("old_folder"), dir.path().join("new_folder")).unwrap();
        let entries = index_rels(&mut indexer, dir.path());

        assert!(!entries.iter().any(|entry| entry.starts_with("old_folder")));
        assert!(entries.contains(&"new_folder/file1.py".to_string()));
        assert!(entries.contains(&"new_folder/file2.py".to_string()));
    }

    #[test]
    fn file_indexer_watcher_toggle_flow_off_on_off() {
        let dir = tempfile::tempdir().unwrap();
        let enabled = Rc::new(Cell::new(false));
        let enabled_for_indexer = Rc::clone(&enabled);
        let mut indexer =
            FileIndexer::with_watcher_enabled_getter(200, move || enabled_for_indexer.get());
        assert!(index_rels(&mut indexer, dir.path()).is_empty());

        fs::write(dir.path().join("off_before.py"), "").unwrap();
        assert!(!index_rels(&mut indexer, dir.path()).contains(&"off_before.py".to_string()));

        enabled.set(true);
        assert!(!index_rels(&mut indexer, dir.path()).contains(&"off_before.py".to_string()));
        fs::write(dir.path().join("on_file.py"), "").unwrap();
        let entries = index_rels(&mut indexer, dir.path());
        assert!(entries.contains(&"on_file.py".to_string()));
        assert!(!entries.contains(&"off_before.py".to_string()));

        enabled.set(false);
        fs::write(dir.path().join("off_after.py"), "").unwrap();
        let entries = index_rels(&mut indexer, dir.path());
        assert!(entries.contains(&"on_file.py".to_string()));
        assert!(!entries.contains(&"off_after.py".to_string()));
    }

    #[test]
    fn file_indexer_shutdown_clears_and_disables_future_indexes() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("seed.py"), "").unwrap();
        let mut indexer = FileIndexer::new();
        assert_eq!(index_rels(&mut indexer, dir.path()), vec!["seed.py"]);

        indexer.shutdown();
        fs::write(dir.path().join("new_file.py"), "").unwrap();

        assert!(index_rels(&mut indexer, dir.path()).is_empty());
    }
}
