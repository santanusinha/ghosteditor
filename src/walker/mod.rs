//! Built-in content walker (P2.7).
//!
//! Literal or regex search over workspace files. Skips ignored paths
//! (.git, target, node_modules, plus .gitignore patterns), binary-looking
//! files, and oversized files. Results are capped; the reply carries a
//! `next_offset` cursor when more matches exist.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

/// A single match. Lines and columns are 1-based, like every ghostedit tool.
#[derive(Debug, Clone, PartialEq)]
pub struct Match {
    pub path: String,
    pub line: usize,
    pub column: usize,
    pub text: String,
}

/// What to search for.
#[derive(Debug, Clone)]
pub enum Pattern {
    /// Substring, case-insensitive.
    Literal(String),
    /// Compiled regex, case-insensitive by default.
    Regex(regex::Regex),
}

/// The search outcome. `truncated` is true when matches exist past the cap
/// or the offset; the caller passes `offset + matches.len()` as the next
/// cursor.
#[derive(Debug, Clone, Default)]
pub struct SearchResult {
    pub matches: Vec<Match>,
    pub files_scanned: usize,
    pub truncated: bool,
}

/// Hard cap: no answer may flood the agent context (plan rule 4.5.9).
pub const MAX_MATCHES: usize = 200;
/// Files larger than this are skipped, like an editor would.
const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;

/// Directories never entered, regardless of gitignore.
const SKIP_DIRS: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    ".venv",
    "venv",
    "__pycache__",
    "dist",
    "build",
    ".mypy_cache",
    ".pytest_cache",
];

/// Matches kept in memory per search. Counting always covers every
/// match; storage stops past this bound so a pathological tree cannot
/// balloon memory. Realistic pages (offset + 200 up to this bound) stay
/// exact.
const STORE_LIMIT: usize = 100_000;

/// Search the workspace under `root`. `query` is a case-insensitive
/// substring when `regex` is false, else a regex pattern. `offset` skips
/// the first `offset` matches for pagination. `glob` restricts the walk
/// to files whose root-relative path matches. A regex compile error is
/// returned as a string.
///
/// Phase 1 collects candidate files (metadata only). Phase 2 scans them
/// on the thread pool. Phase 3 sorts by (path, line, column) and applies
/// the offset window, so pagination is deterministic across runs.
pub fn search(
    root: &Path,
    query: &str,
    regex: bool,
    offset: usize,
    glob: Option<&str>,
) -> Result<SearchResult, String> {
    let pattern = if regex {
        Pattern::Regex(
            regex::RegexBuilder::new(query)
                .case_insensitive(true)
                .build()
                .map_err(|e| format!("invalid regex: {e}"))?,
        )
    } else {
        Pattern::Literal(query.to_string())
    };
    let ignore = GitignoreSet::load(root);

    let mut files = Vec::new();
    collect_files(root, root, &ignore, &mut files)?;

    let total = AtomicUsize::new(0);
    let scanned = AtomicUsize::new(0);
    let next_file = AtomicUsize::new(0);
    let stored = Mutex::new(Vec::new());

    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .clamp(1, 16);
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| loop {
                let index = next_file.fetch_add(1, Ordering::Relaxed);
                if index >= files.len() {
                    break;
                }
                scan_file(
                    root,
                    &files[index],
                    &pattern,
                    glob,
                    &total,
                    &scanned,
                    &stored,
                );
            });
        }
    });

    let mut all = stored.into_inner().unwrap_or_default();
    all.sort_by(|a, b| (&a.path, a.line, a.column).cmp(&(&b.path, b.line, b.column)));
    let page: Vec<Match> = all.into_iter().skip(offset).take(MAX_MATCHES).collect();
    let truncated = total.load(Ordering::Relaxed) > offset + page.len();
    Ok(SearchResult {
        matches: page,
        files_scanned: scanned.load(Ordering::Relaxed),
        truncated,
    })
}

/// Collect candidate files under `dir`. Metadata only; no file is read
/// here. `root` stays the workspace root so relative paths stay stable.
fn collect_files(
    _root: &Path,
    dir: &Path,
    parent_ignore: &GitignoreSet,
    files: &mut Vec<PathBuf>,
) -> Result<(), String> {
    // Load this directory's .gitignore before matching its children.
    let ignore = parent_ignore.clone_for_dir(dir);
    let entries = fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if path.is_dir() {
            if SKIP_DIRS.contains(&name) || ignore.matches(&path, true) {
                continue;
            }
            collect_files(dir, &path, &ignore, files)?;
        } else if path.is_file() {
            if ignore.matches(&path, false) {
                continue;
            }
            files.push(path);
        }
    }
    Ok(())
}

/// Scan one file: count every match, and store the ones that fit the
/// storage window. The glob filter runs before any I/O so a scoped
/// search never reads files it will not scan (P3.5). Infallible: an
/// unreadable file contributes nothing.
fn scan_file(
    root: &Path,
    path: &Path,
    pattern: &Pattern,
    glob: Option<&str>,
    total: &AtomicUsize,
    scanned: &AtomicUsize,
    stored: &Mutex<Vec<Match>>,
) {
    let relative = path
        .strip_prefix(root)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| path.display().to_string());
    if let Some(glob) = glob {
        if !glob_matches(glob, &relative) {
            return;
        }
    }
    let Ok(metadata) = fs::metadata(path) else {
        return;
    };
    if metadata.len() > MAX_FILE_BYTES {
        return;
    }
    let Ok(bytes) = fs::read(path) else {
        return;
    };
    // A NUL byte in the first 8 KiB marks a binary file.
    if bytes[..bytes.len().min(8192)].contains(&0) {
        return;
    }
    let Ok(text) = String::from_utf8(bytes) else {
        return;
    };
    scanned.fetch_add(1, Ordering::Relaxed);
    let mut local: Vec<Match> = Vec::new();
    for (index, line) in text.lines().enumerate() {
        match pattern {
            Pattern::Literal(needle) => {
                if let Some(at) = find_substring(line, needle) {
                    local.push(Match {
                        path: relative.clone(),
                        line: index + 1,
                        column: at + 1,
                        text: truncate_line(line),
                    });
                }
            }
            Pattern::Regex(re) => {
                for found in re.find_iter(line) {
                    local.push(Match {
                        path: relative.clone(),
                        line: index + 1,
                        column: found.start() + 1,
                        text: truncate_line(line),
                    });
                }
            }
        }
    }
    if local.is_empty() {
        return;
    }
    let start = total.fetch_add(local.len(), Ordering::Relaxed);
    if start >= STORE_LIMIT {
        return;
    }
    let keep = local.len().min(STORE_LIMIT - start);
    local.truncate(keep);
    stored.lock().expect("walker store poisoned").extend(local);
}

/// Minimal glob for the search filter: `*` matches within one path
/// segment, `**` crosses segments, `?` is one non-slash char. `a/**/b`
/// also matches `a/b`. Paths use forward slashes (root-relative).
pub fn glob_matches(glob: &str, path: &str) -> bool {
    fn go(p: &[char], t: &[char]) -> bool {
        match (p.first(), t.first()) {
            (None, None) => true,
            (Some('*'), _) => {
                if p.get(1) == Some(&'*') {
                    let rest = &p[2..];
                    (0..=t.len()).any(|i| go(rest, &t[i..]))
                } else {
                    let rest = &p[1..];
                    for i in 0..=t.len() {
                        if i > 0 && t[i - 1] == '/' {
                            break;
                        }
                        if go(rest, &t[i..]) {
                            return true;
                        }
                    }
                    false
                }
            }
            (Some('?'), Some(&c)) if c != '/' => go(&p[1..], &t[1..]),
            (Some(&a), Some(&b)) if a == b => go(&p[1..], &t[1..]),
            _ => false,
        }
    }
    let p: Vec<char> = glob.chars().collect();
    let t: Vec<char> = path.chars().collect();
    if go(&p, &t) {
        return true;
    }
    // `a/**/b` also matches `a/b`: retry with the `/**/` collapsed.
    // `a/**/b` also matches `a/b`, and a leading `**/` is optional:
    // retry with each form collapsed.
    if glob.contains("/**/") {
        let collapsed: Vec<char> = glob.replace("/**/", "/").chars().collect();
        if go(&collapsed, &t) {
            return true;
        }
    }
    if let Some(rest) = glob.strip_prefix("**/") {
        let without: Vec<char> = rest.chars().collect();
        if go(&without, &t) {
            return true;
        }
    }
    false
}

/// Case-insensitive substring search. Returns the byte offset in the
/// original line. Both sides lowercase once (one allocation, not one per
/// start position); the offset maps back through a char walk because
/// lowercase can change byte lengths.
fn find_substring(hay: &str, needle: &str) -> Option<usize> {
    if needle.is_empty() {
        return None;
    }
    let needle_lower = needle.to_lowercase();
    if needle_lower.is_empty() {
        return None;
    }
    let hay_lower = hay.to_lowercase();
    let byte_at = hay_lower.find(&needle_lower)?;
    // Walk the original chars; each one produced `lower_len` lowercase
    // bytes. The match starts at the char whose lowercase range holds
    // `byte_at`.
    let mut acc = 0usize;
    for (orig_byte, ch) in hay.char_indices() {
        let lower_len: usize = ch.to_lowercase().map(|c| c.len_utf8()).sum();
        if acc + lower_len > byte_at {
            return Some(orig_byte);
        }
        acc += lower_len;
    }
    None
}

/// Trim a matched line and cap it so one long line cannot flood context.
fn truncate_line(line: &str) -> String {
    let trimmed = line.trim();
    if trimmed.chars().count() <= 200 {
        trimmed.to_string()
    } else {
        let cut: String = trimmed.chars().take(200).collect();
        format!("{cut}…")
    }
}

/// A minimal .gitignore matcher. Patterns from a .gitignore apply to
/// paths under that directory.
#[derive(Default)]
pub struct GitignoreSet {
    dirs: Vec<(std::path::PathBuf, Vec<GitPattern>)>,
}

impl GitignoreSet {
    /// Load the root .gitignore. Nested ones load during the walk.
    pub fn load(root: &Path) -> Self {
        let mut set = GitignoreSet::default();
        set.load_dir(root);
        set
    }

    /// Load one directory's .gitignore when present.
    pub fn load_dir(&mut self, dir: &Path) {
        let Ok(text) = fs::read_to_string(dir.join(".gitignore")) else {
            return;
        };
        let patterns: Vec<GitPattern> = text
            .lines()
            .filter_map(|line| {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    return None;
                }
                Some(GitPattern::parse(line))
            })
            .collect();
        if !patterns.is_empty() {
            self.dirs.push((dir.to_path_buf(), patterns));
        }
    }

    /// A copy that also loads `dir`'s own .gitignore, for descending.
    pub fn clone_for_dir(&self, dir: &Path) -> Self {
        let mut next = GitignoreSet {
            dirs: self.dirs.clone(),
        };
        next.load_dir(dir);
        next
    }

    /// True when a path matches any loaded pattern from a parent directory.
    pub fn matches(&self, path: &Path, is_dir: bool) -> bool {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        for (dir, patterns) in &self.dirs {
            if !path.starts_with(dir) {
                continue;
            }
            for pattern in patterns {
                if pattern.matches(name, is_dir) {
                    return true;
                }
            }
        }
        false
    }
}

/// One gitignore pattern. Supported: literals, `*`, `**`, trailing slash
/// (dir only), leading slash (anchored to the base name).
#[derive(Debug, Clone)]
struct GitPattern {
    text: String,
    dir_only: bool,
}

impl GitPattern {
    fn parse(line: &str) -> Self {
        let dir_only = line.ends_with('/');
        let text = line.trim_matches('/').to_string();
        Self { text, dir_only }
    }

    fn matches(&self, name: &str, is_dir: bool) -> bool {
        if self.dir_only && !is_dir {
            return false;
        }
        glob_match(&self.text, name)
    }
}

/// Small glob: `*` matches any run without `/`; `**` matches anything.
fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    glob_rec(&p, 0, &t, 0)
}

fn glob_rec(p: &[char], mut pi: usize, t: &[char], mut ti: usize) -> bool {
    while pi < p.len() {
        match p[pi] {
            '*' if p.get(pi + 1) == Some(&'*') => {
                // `**` consumes anything; try the rest at every position.
                for skip in ti..=t.len() {
                    if glob_rec(p, pi + 2, t, skip) {
                        return true;
                    }
                }
                return false;
            }
            '*' => {
                // `*` consumes any run without `/`.
                for skip in ti..=t.len() {
                    if t[ti..skip].contains(&'/') {
                        break;
                    }
                    if glob_rec(p, pi + 1, t, skip) {
                        return true;
                    }
                }
                return false;
            }
            c => {
                if ti >= t.len() || t[ti] != c {
                    return false;
                }
                pi += 1;
                ti += 1;
            }
        }
    }
    ti == t.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_with_files(files: &[(&str, &str)]) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        for (name, content) in files {
            let path = dir.path().join(name);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("mkdir");
            }
            std::fs::write(path, content).expect("write");
        }
        let root = dir.path().to_path_buf();
        (dir, root)
    }

    #[test]
    fn literal_search_finds_case_insensitive_matches() {
        let (_dir, root) = tmp_with_files(&[
            ("a.txt", "Hello World\nnope\nWORLD ahead"),
            ("sub/b.txt", "world"),
        ]);
        let result = search(&root, "world", false, 0, None).expect("search");
        assert_eq!(result.matches.len(), 3);
        assert!(result.matches.iter().any(|m| m.line == 3));
        assert!(!result.truncated);
    }

    #[test]
    fn regex_search_finds_pattern() {
        let (_dir, root) = tmp_with_files(&[("a.txt", "fn add(a, b)\nfn sub()")]);
        let result = search(&root, "fn [a-z]+\\(", true, 0, None).expect("search");
        assert_eq!(result.matches.len(), 2);
        assert_eq!(result.matches[0].column, 1);
    }

    #[test]
    fn search_skips_gitignore_dirs_and_files() {
        let (_dir, root) = tmp_with_files(&[
            (".gitignore", "secrets/\n*.log\n"),
            ("secrets/key.txt", "matchme"),
            ("app.log", "matchme"),
            ("src/main.rs", "matchme here"),
        ]);
        let result = search(&root, "matchme", false, 0, None).expect("search");
        assert_eq!(result.matches.len(), 1);
        assert_eq!(result.matches[0].path, "src/main.rs");
    }

    #[test]
    fn search_skips_build_dirs_and_binaries() {
        let (_dir, root) = tmp_with_files(&[
            ("target/debug/x.txt", "matchme"),
            ("node_modules/pkg/index.js", "matchme"),
        ]);
        std::fs::write(root.join("data.bin"), b"matchme\x00binary").expect("write");
        let result = search(&root, "matchme", false, 0, None).expect("search");
        assert_eq!(result.matches.len(), 0);
    }

    #[test]
    fn pagination_offsets_and_truncation() {
        let (_dir, root) = tmp_with_files(&[("a.txt", "hit\nhit\nhit\n"), ("b.txt", "hit\n")]);
        let page1 = search(&root, "hit", false, 0, None).expect("search");
        assert_eq!(page1.matches.len(), 4);
        assert!(!page1.truncated);
        let page2 = search(&root, "hit", false, 2, None).expect("search");
        assert_eq!(page2.matches.len(), 2);
        // Order across files follows the directory read; assert on content.
        let lines: Vec<usize> = page2.matches.iter().map(|m| m.line).collect();
        assert!(lines.contains(&3));
    }

    #[test]
    fn bad_regex_is_an_error() {
        let (_dir, root) = tmp_with_files(&[("a.txt", "x")]);
        let err = search(&root, "(unclosed", true, 0, None).unwrap_err();
        assert!(err.contains("invalid regex"), "got: {err}");
    }

    #[test]
    fn long_lines_are_truncated() {
        let long = format!("{} needle {}", "x".repeat(500), "y".repeat(500));
        let (_dir, root) = tmp_with_files(&[("a.txt", &long)]);
        let result = search(&root, "needle", false, 0, None).expect("search");
        assert_eq!(result.matches.len(), 1);
        assert!(result.matches[0].text.chars().count() <= 201);
    }

    #[test]
    fn glob_basics() {
        assert!(glob_match("*.log", "app.log"));
        assert!(!glob_match("*.log", "app.log.txt"));
        assert!(glob_match("docs/**", "docs/a/b.md"));
        assert!(!glob_match("docs/*", "docs/a/b.md"));
        assert!(glob_match("build", "build"));
    }

    #[test]
    fn substring_offset_maps_case_insensitive_hits() {
        assert_eq!(find_substring("Hello World", "world"), Some(6));
        assert_eq!(find_substring("ÄÖÜ x", "x"), Some(7));
    }

    #[test]
    fn glob_matches_segments_and_crosses_dirs() {
        assert!(glob_matches("*.txt", "a.txt"));
        assert!(!glob_matches("*.txt", "sub/a.txt"));
        assert!(glob_matches("src/*.rs", "src/main.rs"));
        assert!(!glob_matches("src/*.rs", "src/deep/main.rs"));
        assert!(glob_matches("src/**/*.rs", "src/deep/main.rs"));
        // `a/**/b` also matches `a/b`.
        assert!(glob_matches("src/**/main.rs", "src/main.rs"));
        assert!(glob_matches("?.txt", "a.txt"));
        assert!(!glob_matches("?.txt", "ab.txt"));
        assert!(!glob_matches("*.rs", "a.txt"));
    }

    #[test]
    fn glob_filter_restricts_the_walk() {
        let (_dir, root) = tmp_with_files(&[
            ("a.py", "matchme"),
            ("sub/b.py", "matchme"),
            ("c.rs", "matchme"),
        ]);
        let result = search(&root, "matchme", false, 0, Some("*.py")).expect("search");
        assert_eq!(result.matches.len(), 1);
        assert_eq!(result.matches[0].path, "a.py");
        let result = search(&root, "matchme", false, 0, Some("**/*.py")).expect("search");
        assert_eq!(result.matches.len(), 2);
        assert!(result.files_scanned <= 2);
    }
}
