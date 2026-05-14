//! Project detection from Alacritty window titles and filesystem paths.
//!
//! Handles multiple title formats:
//! - Shell prompts: `"user@host directory"` → directory name
//! - JFC/Claude titles: `"jfc · model · project"` (with optional leading
//!   status)
//! - OpenCode titles: `"OC | task description"` or plain `"OpenCode"`
//! - Filesystem-based detection by walking up to project root markers

use std::path::{Path, PathBuf};

/// Project root marker files/directories (inspired by wakatime's approach).
const PROJECT_MARKERS: &[&str] = &[
    ".git",
    "Cargo.toml",
    "package.json",
    "go.mod",
    "pom.xml",
    "build.gradle",
    "requirements.txt",
    "Pipfile",
    "composer.json",
    "Gemfile",
    "mix.exs",
];

/// Detect a project name from an Alacritty window title.
///
/// Returns `None` for empty titles, home directory (`~`), or unrecognized
/// patterns.
pub fn detect_project_from_title(title: &str) -> Option<String> {
    let title = title.trim();
    if title.is_empty() {
        return None;
    }

    // Pattern: "OC | description" → OpenCode task
    if let Some(rest) = title.strip_prefix("OC | ") {
        let rest = rest.trim();
        if !rest.is_empty() {
            return Some(format!("OC: {}", rest));
        }
        return Some("OpenCode".to_string());
    }

    // Pattern: plain "OpenCode"
    if title == "OpenCode" {
        return Some("OpenCode".to_string());
    }

    // Pattern: JFC/Claude-style "jfc · model · project" with optional leading
    // status Status prefixes: "● ", "(NNN new) ", etc.
    if title.contains(" · ") {
        let cleaned = strip_jfc_status_prefix(title);
        let parts: Vec<&str> = cleaned.split(" · ").collect();
        if parts.len() >= 3 {
            // Last segment is the project/task name
            let project = parts.last().unwrap().trim();
            if !project.is_empty() {
                return Some(project.to_string());
            }
        } else if parts.len() == 2 {
            // "jfc · model" — less common but handle it
            let project = parts.last().unwrap().trim();
            if !project.is_empty() {
                return Some(project.to_string());
            }
        }
    }

    // Pattern: "user@host directory" (shell prompt title)
    if let Some(project) = try_parse_shell_prompt(title) {
        return project;
    }

    None
}

/// Strip leading status indicators from JFC-style titles.
///
/// Handles:
/// - `"● jfc · ..."` → `"jfc · ..."`
/// - `"(142 new) jfc · ..."` → `"jfc · ..."`
fn strip_jfc_status_prefix(title: &str) -> &str {
    let mut s = title;

    // Strip leading non-ASCII status chars like ● ◉ ○ etc.
    s = s.trim_start_matches(|c: char| !c.is_ascii_alphanumeric() && c != '(');
    s = s.trim_start();

    // Strip "(NNN new) " prefix
    if s.starts_with('(') {
        if let Some(close) = s.find(')') {
            let after = &s[close + 1..];
            s = after.trim_start();
        }
    }

    s
}

/// Try to parse a shell prompt title like "user@host directory".
///
/// Returns `Some(Some(project))` for a valid project directory,
/// `Some(None)` for home dir `~`, and `None` if the pattern doesn't match.
fn try_parse_shell_prompt(title: &str) -> Option<Option<String>> {
    // Must contain @ for user@host pattern
    let at_pos = title.find('@')?;

    // Find the space separating host from directory
    let after_at = &title[at_pos + 1..];
    let space_pos = after_at.find(' ')?;
    let dir = after_at[space_pos + 1..].trim();

    if dir.is_empty() {
        return Some(None);
    }

    // Home directory is not a project
    if dir == "~" {
        return Some(None);
    }

    Some(Some(dir.to_string()))
}

/// Detect project from non-terminal app window titles based on app_id.
///
/// Handles:
/// - JetBrains IDEs: `"project – file.rs"` or `"project [~/path] – file.rs –
///   IDE Name"`
/// - VSCode: `"file.rs - project - Visual Studio Code"`
pub fn detect_project_from_app_title(app_id: &str, title: &str) -> Option<String> {
    let title = title.trim();
    if title.is_empty() {
        return None;
    }

    match app_id {
        // JetBrains IDEs: "project – file.rs" or "project [path] – file.rs – IDE Name"
        s if s.starts_with("jetbrains-") => parse_jetbrains_title(title),

        // VSCode: "file.rs - project - Visual Studio Code"
        "code" | "Code" | "code-oss" => parse_vscode_title(title),

        _ => None,
    }
}

fn parse_jetbrains_title(title: &str) -> Option<String> {
    // JetBrains uses " – " (en-dash) or " - " as separator
    // Pattern: "project_name – file.rs" or "project_name [~/path/to/project] – ..."
    // The project name is always the first segment
    let separator = if title.contains(" \u{2013} ") {
        " \u{2013} "
    } else {
        " - "
    };
    let first = title.split(separator).next()?.trim();

    // Strip any path annotation like "[~/path]"
    let name = if let Some(bracket) = first.find(" [") {
        &first[..bracket]
    } else {
        first
    };

    let name = name.trim();
    if name.is_empty() || name.contains('/') {
        return None;
    }
    Some(name.to_string())
}

fn parse_vscode_title(title: &str) -> Option<String> {
    // Pattern: "file.rs - project_name - Visual Studio Code"
    // Or: "project_name - Visual Studio Code"
    let parts: Vec<&str> = title.split(" - ").collect();
    if parts.len() >= 3 {
        // Second-to-last is project (last is "Visual Studio Code")
        let project = parts[parts.len() - 2].trim();
        if !project.is_empty() && !project.contains('/') {
            return Some(project.to_string());
        }
    } else if parts.len() == 2 {
        // "project_name - Visual Studio Code"
        let project = parts[0].trim();
        if !project.is_empty() && !project.contains('/') && !project.contains('.') {
            return Some(project.to_string());
        }
    }
    None
}

/// Detect a project by walking up from the given path looking for project root
/// markers.
///
/// Returns the directory name of the project root, or `None` if no markers
/// found. Excludes the user's home directory as a false positive.
pub fn detect_project_from_path(path: &Path) -> Option<String> {
    let home = dirs::home_dir();
    let mut dir = if path.is_dir() {
        path.to_path_buf()
    } else {
        path.parent()?.to_path_buf()
    };

    loop {
        // Skip home directory — it often has stray package.json/.git
        if let Some(ref h) = home {
            if dir == *h {
                break;
            }
        }

        for marker in PROJECT_MARKERS {
            if dir.join(marker).exists() {
                return dir.file_name()?.to_str().map(|s| s.to_string());
            }
        }

        match dir.parent() {
            Some(parent) if parent != dir => {
                dir = parent.to_path_buf();
            }
            _ => break,
        }
    }

    None
}

/// Detect the current git branch by reading .git/HEAD directly.
/// Much faster than shelling out to `git rev-parse`.
pub fn detect_git_branch(path: &Path) -> Option<String> {
    // Walk up to find .git directory
    let mut dir = if path.is_dir() {
        path.to_path_buf()
    } else {
        path.parent()?.to_path_buf()
    };

    loop {
        let git_dir = dir.join(".git");
        if git_dir.exists() {
            // Read .git/HEAD
            let head_path = if git_dir.is_file() {
                // Worktree: .git is a file with "gitdir: /path/to/real/.git/worktrees/name"
                let content = std::fs::read_to_string(&git_dir).ok()?;
                let gitdir = content.strip_prefix("gitdir: ")?.trim();
                PathBuf::from(gitdir).join("HEAD")
            } else {
                git_dir.join("HEAD")
            };

            let head_content = std::fs::read_to_string(&head_path).ok()?;
            let trimmed = head_content.trim();

            // "ref: refs/heads/branch-name" → "branch-name"
            if let Some(ref_path) = trimmed.strip_prefix("ref: refs/heads/") {
                return Some(ref_path.to_string());
            }
            // Detached HEAD (commit hash) → first 8 chars
            if trimmed.len() >= 8 && trimmed.chars().all(|c| c.is_ascii_hexdigit()) {
                return Some(trimmed[..8].to_string());
            }
            return None;
        }

        match dir.parent() {
            Some(parent) if parent != dir => dir = parent.to_path_buf(),
            _ => break,
        }
    }
    None
}

/// Given a short directory basename (e.g. "jfc") and a list of search
/// directories, try to find the full path and validate it's a real project (has
/// .git, Cargo.toml, etc). Returns the validated project name (possibly the
/// basename itself if found).
pub fn resolve_project_in_search_dirs(basename: &str, search_dirs: &[PathBuf]) -> Option<String> {
    for dir in search_dirs {
        let candidate = dir.join(basename);
        if candidate.is_dir() {
            // Use detect_project_from_path to validate it's actually a project
            if let Some(name) = detect_project_from_path(&candidate) {
                return Some(name);
            }
            // Even without markers, if the directory exists in a search_dir, trust it
            return Some(basename.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    // ─── detect_project_from_title ───────────────────────────────────────

    #[test]
    fn shell_prompt_project() {
        assert_eq!(
            detect_project_from_title("cole@gentoo-thinkpad jfc"),
            Some("jfc".to_string())
        );
    }

    #[test]
    fn shell_prompt_project_with_dashes() {
        assert_eq!(
            detect_project_from_title("cole@gentoo-p16 niri-activity-rs"),
            Some("niri-activity-rs".to_string())
        );
    }

    #[test]
    fn shell_prompt_home_dir() {
        assert_eq!(detect_project_from_title("cole@gentoo-thinkpad ~"), None);
    }

    #[test]
    fn jfc_three_parts() {
        assert_eq!(
            detect_project_from_title("jfc · bedrock-claude-4-6-opus · unlace"),
            Some("unlace".to_string())
        );
    }

    #[test]
    fn jfc_with_bullet_prefix() {
        assert_eq!(
            detect_project_from_title("● jfc · claude-opus-4-7 · jfc"),
            Some("jfc".to_string())
        );
    }

    #[test]
    fn jfc_with_new_count_prefix() {
        assert_eq!(
            detect_project_from_title("(142 new) jfc · bedrock-claude-4-6-opus · jfc"),
            Some("jfc".to_string())
        );
    }

    #[test]
    fn opencode_pipe_task() {
        assert_eq!(
            detect_project_from_title("OC | js-beautify-rs cargo publish setup"),
            Some("OC: js-beautify-rs cargo publish setup".to_string())
        );
    }

    #[test]
    fn opencode_pipe_long_title() {
        assert_eq!(
            detect_project_from_title("OC | Relationship advice after difficult f..."),
            Some("OC: Relationship advice after difficult f...".to_string())
        );
    }

    #[test]
    fn opencode_plain() {
        assert_eq!(
            detect_project_from_title("OpenCode"),
            Some("OpenCode".to_string())
        );
    }

    #[test]
    fn empty_title() {
        assert_eq!(detect_project_from_title(""), None);
    }

    #[test]
    fn whitespace_only_title() {
        assert_eq!(detect_project_from_title("   "), None);
    }

    #[test]
    fn unrecognized_title() {
        // No @ for shell prompt, no · for JFC, no OC | prefix
        assert_eq!(detect_project_from_title("Firefox"), None);
    }

    // ─── detect_project_from_app_title ───────────────────────────────────

    #[test]
    fn jetbrains_rustrover_simple_title() {
        assert_eq!(
            detect_project_from_app_title(
                "jetbrains-rustrover",
                "niri-activity-rs \u{2013} config.rs"
            ),
            Some("niri-activity-rs".to_string())
        );
    }

    #[test]
    fn jetbrains_idea_with_path() {
        assert_eq!(
            detect_project_from_app_title(
                "jetbrains-idea",
                "my-project [~/work/my-project] \u{2013} Main.java \u{2013} IntelliJ IDEA"
            ),
            Some("my-project".to_string())
        );
    }

    #[test]
    fn vscode_three_parts() {
        assert_eq!(
            detect_project_from_app_title("code", "main.rs - my-project - Visual Studio Code"),
            Some("my-project".to_string())
        );
    }

    #[test]
    fn vscode_code_oss() {
        assert_eq!(
            detect_project_from_app_title(
                "code-oss",
                "lib.rs - rust-analyzer - Visual Studio Code"
            ),
            Some("rust-analyzer".to_string())
        );
    }

    #[test]
    fn vscode_two_parts_project_only() {
        assert_eq!(
            detect_project_from_app_title("code", "my-project - Visual Studio Code"),
            Some("my-project".to_string())
        );
    }

    #[test]
    fn vscode_two_parts_file_ignored() {
        // "file.rs - Visual Studio Code" → None (has a dot, looks like a file)
        assert_eq!(
            detect_project_from_app_title("code", "file.rs - Visual Studio Code"),
            None
        );
    }

    #[test]
    fn non_project_app_vesktop() {
        assert_eq!(
            detect_project_from_app_title("vesktop", "Discord | Server - Channel"),
            None
        );
    }

    #[test]
    fn non_project_app_browser() {
        assert_eq!(
            detect_project_from_app_title("zen", "GitHub - user/repo"),
            None
        );
    }

    #[test]
    fn app_title_empty() {
        assert_eq!(detect_project_from_app_title("code", ""), None);
    }

    #[test]
    fn app_title_whitespace() {
        assert_eq!(detect_project_from_app_title("jetbrains-idea", "   "), None);
    }

    // ─── detect_project_from_path ────────────────────────────────────────

    #[test]
    fn detects_git_project_root() {
        let tmp = TempDir::new().unwrap();
        let project_dir = tmp.path().join("my-project");
        fs::create_dir_all(project_dir.join(".git")).unwrap();
        fs::create_dir_all(project_dir.join("src")).unwrap();

        // From a subdirectory, should find the project root
        let result = detect_project_from_path(&project_dir.join("src"));
        assert_eq!(result, Some("my-project".to_string()));
    }

    #[test]
    fn detects_cargo_toml_project() {
        let tmp = TempDir::new().unwrap();
        let project_dir = tmp.path().join("rust-app");
        fs::create_dir_all(project_dir.join("src/bin")).unwrap();
        fs::write(project_dir.join("Cargo.toml"), "[package]").unwrap();

        let result = detect_project_from_path(&project_dir.join("src/bin"));
        assert_eq!(result, Some("rust-app".to_string()));
    }

    #[test]
    fn detects_package_json_project() {
        let tmp = TempDir::new().unwrap();
        let project_dir = tmp.path().join("node-app");
        fs::create_dir_all(project_dir.join("src/components")).unwrap();
        fs::write(project_dir.join("package.json"), "{}").unwrap();

        let result = detect_project_from_path(&project_dir.join("src/components"));
        assert_eq!(result, Some("node-app".to_string()));
    }

    #[test]
    fn detects_go_mod_project() {
        let tmp = TempDir::new().unwrap();
        let project_dir = tmp.path().join("go-service");
        fs::create_dir_all(project_dir.join("cmd")).unwrap();
        fs::write(project_dir.join("go.mod"), "module example.com/go-service").unwrap();

        let result = detect_project_from_path(&project_dir.join("cmd"));
        assert_eq!(result, Some("go-service".to_string()));
    }

    #[test]
    fn returns_none_for_root_path() {
        // The root directory `/` has no file_name, so detection returns None.
        let result = detect_project_from_path(Path::new("/"));
        assert_eq!(result, None);
    }

    #[test]
    fn detects_from_exact_project_dir() {
        let tmp = TempDir::new().unwrap();
        let project_dir = tmp.path().join("exact");
        fs::create_dir_all(&project_dir).unwrap();
        fs::write(project_dir.join("Gemfile"), "source 'https://rubygems.org'").unwrap();

        let result = detect_project_from_path(&project_dir);
        assert_eq!(result, Some("exact".to_string()));
    }

    #[test]
    fn detects_from_file_path() {
        let tmp = TempDir::new().unwrap();
        let project_dir = tmp.path().join("file-test");
        fs::create_dir_all(&project_dir).unwrap();
        fs::write(project_dir.join("mix.exs"), "").unwrap();
        let file_path = project_dir.join("lib/app.ex");
        // file doesn't need to exist; we just need the parent dirs to exist
        fs::create_dir_all(project_dir.join("lib")).unwrap();
        fs::write(&file_path, "").unwrap();

        let result = detect_project_from_path(&file_path);
        assert_eq!(result, Some("file-test".to_string()));
    }

    // ─── detect_git_branch ───────────────────────────────────────────────

    #[test]
    fn detects_branch_from_git_head() {
        let tmp = TempDir::new().unwrap();
        let project_dir = tmp.path().join("branch-test");
        fs::create_dir_all(project_dir.join(".git")).unwrap();
        fs::write(project_dir.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();

        let result = detect_git_branch(&project_dir);
        assert_eq!(result, Some("main".to_string()));
    }

    #[test]
    fn detects_branch_from_subdirectory() {
        let tmp = TempDir::new().unwrap();
        let project_dir = tmp.path().join("sub-branch");
        fs::create_dir_all(project_dir.join(".git")).unwrap();
        fs::create_dir_all(project_dir.join("src/lib")).unwrap();
        fs::write(
            project_dir.join(".git/HEAD"),
            "ref: refs/heads/feature/my-feature\n",
        )
        .unwrap();

        let result = detect_git_branch(&project_dir.join("src/lib"));
        assert_eq!(result, Some("feature/my-feature".to_string()));
    }

    #[test]
    fn detects_detached_head() {
        let tmp = TempDir::new().unwrap();
        let project_dir = tmp.path().join("detached-test");
        fs::create_dir_all(project_dir.join(".git")).unwrap();
        fs::write(
            project_dir.join(".git/HEAD"),
            "a1b2c3d4e5f6789012345678901234567890abcd\n",
        )
        .unwrap();

        let result = detect_git_branch(&project_dir);
        assert_eq!(result, Some("a1b2c3d4".to_string()));
    }

    #[test]
    fn detects_branch_from_worktree() {
        let tmp = TempDir::new().unwrap();
        // Set up a fake worktree structure
        let main_git = tmp.path().join("main-repo/.git");
        fs::create_dir_all(main_git.join("worktrees/my-wt")).unwrap();
        fs::write(
            main_git.join("worktrees/my-wt/HEAD"),
            "ref: refs/heads/worktree-branch\n",
        )
        .unwrap();

        // The worktree directory has a .git file pointing to the worktree gitdir
        let wt_dir = tmp.path().join("worktree-dir");
        fs::create_dir_all(&wt_dir).unwrap();
        fs::write(
            wt_dir.join(".git"),
            format!("gitdir: {}\n", main_git.join("worktrees/my-wt").display()),
        )
        .unwrap();

        let result = detect_git_branch(&wt_dir);
        assert_eq!(result, Some("worktree-branch".to_string()));
    }

    #[test]
    fn returns_none_when_no_git() {
        let tmp = TempDir::new().unwrap();
        let project_dir = tmp.path().join("no-git");
        fs::create_dir_all(project_dir.join("src")).unwrap();

        let result = detect_git_branch(&project_dir.join("src"));
        assert_eq!(result, None);
    }
}
