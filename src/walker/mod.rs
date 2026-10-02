//! Built-in content walker (P2.7).
//!
//! Literal or regex search over workspace files. Skips ignored paths
//! (.git, target, node_modules, plus .gitignore patterns), binary-looking
//! files, and oversized files. Results are capped; the reply carries a
//! `next_offset` cursor when more matches exist.

use std::fs;
use std::path::Path;

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

/// Search the workspace under `root`. `query` is a case-insensitive
/// substring when `regex` is false, else a regex pattern. `offset` skips
/// the first `offset` matches for pagination. A regex compile error is
/// returned as a string.
pub fn search(
    root: &Path,
    query: &str,
    regex: bool,
    offset: usize,
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
    let mut result = SearchResult::default();
    let mut cursor = 0usize; // Index of the next match overall.
    walk(
        root,
        root,
        &ignore,
        &pattern,
        offset,
        &mut cursor,
        &mut result,
    )?;
    result.truncated = cursor > offset + result.matches.len();
    Ok(result)
}

/// Depth-first walk. `root` stays the workspace root so relative paths
/// stay stable at any depth.
fn walk(
    root: &Path,
    dir: &Path,
    parent_ignore: &GitignoreSet,
    pattern: &Pattern,
    offset: usize,
    cursor: &mut usize,
    result: &mut SearchResult,
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
            walk(root, &path, &ignore, pattern, offset, cursor, result)?;
        } else if path.is_file() {
            if ignore.matches(&path, false) {
                continue;
            }
            scan_file(root, &path, pattern, offset, cursor, result)?;
        }
    }
    Ok(())
}

/// Scan one file for matches, applying the pagination cursor.
fn scan_file(
    root: &Path,
    path: &Path,
    pattern: &Pattern,
    offset: usize,
    cursor: &mut usize,
    result: &mut SearchResult,
) -> Result<(), String> {
    let Ok(metadata) = fs::metadata(path) else {
        return Ok(());
    };
    if metadata.len() > MAX_FILE_BYTES {
        return Ok(());
    }
    let Ok(bytes) = fs::read(path) else {
        return Ok(());
    };
    // A NUL byte in the first 8 KiB marks a binary file.
    if bytes[..bytes.len().min(8192)].contains(&0) {
        return Ok(());
    }
    let Ok(text) = String::from_utf8(bytes) else {
        return Ok(());
    };
    let relative = path
        .strip_prefix(root)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| path.display().to_string());
    for (index, line) in text.lines().enumerate() {
        match pattern {
            Pattern::Literal(needle) => {
                let Some(at) = find_substring(line, needle) else {
                    continue;
                };
                emit(&relative, index, at, line, offset, cursor, result);
            }
            Pattern::Regex(re) => {
                for found in re.find_iter(line) {
                    emit(
                        &relative,
                        index,
                        found.start(),
                        line,
                        offset,
                        cursor,
                        result,
                    );
                }
            }
        }
    }
    result.files_scanned += 1;
    Ok(())
}

/// Record one match unless past the cap. Every match advances the cursor
/// so `truncated` stays correct across pages.
fn emit(
    relative: &str,
    line_index: usize,
    column: usize,
    line: &str,
    offset: usize,
    cursor: &mut usize,
    result: &mut SearchResult,
) {
    let index = *cursor;
    *cursor += 1;
    if index >= offset && result.matches.len() < MAX_MATCHES {
        result.matches.push(Match {
            path: relative.to_string(),
            line: line_index + 1,
            column: column + 1,
            text: truncate_line(line),
        });
    }
}

/// Case-insensitive substring search. Returns the byte offset in the
/// original line. The lowercase copies can change byte lengths, so the
/// offset maps back through a char walk.
fn find_substring(hay: &str, needle: &str) -> Option<usize> {
    if needle.is_empty() {
        return None;
    }
    let needle_lower = needle.to_lowercase();
    let chars: Vec<(usize, char)> = hay.char_indices().collect();
    for start in 0..chars.len() {
        let slice: String = chars[start..]
            .iter()
            .map(|(_, c)| *c)
            .take(needle.chars().count())
            .collect();
        if slice.to_lowercase() == needle_lower {
            return Some(chars[start].0);
        }
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
        let result = search(&root, "world", false, 0).expect("search");
        assert_eq!(result.matches.len(), 3);
        assert!(result.matches.iter().any(|m| m.line == 3));
        assert!(!result.truncated);
    }

    #[test]
    fn regex_search_finds_pattern() {
        let (_dir, root) = tmp_with_files(&[("a.txt", "fn add(a, b)\nfn sub()")]);
        let result = search(&root, "fn [a-z]+\\(", true, 0).expect("search");
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
        let result = search(&root, "matchme", false, 0).expect("search");
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
        let result = search(&root, "matchme", false, 0).expect("search");
        assert_eq!(result.matches.len(), 0);
    }

    #[test]
    fn pagination_offsets_and_truncation() {
        let (_dir, root) = tmp_with_files(&[("a.txt", "hit\nhit\nhit\n"), ("b.txt", "hit\n")]);
        let page1 = search(&root, "hit", false, 0).expect("search");
        assert_eq!(page1.matches.len(), 4);
        assert!(!page1.truncated);
        let page2 = search(&root, "hit", false, 2).expect("search");
        assert_eq!(page2.matches.len(), 2);
        // Order across files follows the directory read; assert on content.
        let lines: Vec<usize> = page2.matches.iter().map(|m| m.line).collect();
        assert!(lines.contains(&3));
    }

    #[test]
    fn bad_regex_is_an_error() {
        let (_dir, root) = tmp_with_files(&[("a.txt", "x")]);
        let err = search(&root, "(unclosed", true, 0).unwrap_err();
        assert!(err.contains("invalid regex"), "got: {err}");
    }

    #[test]
    fn long_lines_are_truncated() {
        let long = format!("{} needle {}", "x".repeat(500), "y".repeat(500));
        let (_dir, root) = tmp_with_files(&[("a.txt", &long)]);
        let result = search(&root, "needle", false, 0).expect("search");
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
}
