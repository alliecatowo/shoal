//! Repository discovery and the once-per-command Git status snapshot.

use super::*;

const GIT_STATUS_TIMEOUT: Duration = Duration::from_millis(300);
const GIT_STATUS_OUTPUT_CAP: usize = 256 * 1024;

// ---------------------------------------------------------------------------
// Pure-Rust git reader — branch + in-progress state, zero subprocess (site/content/internals/prompt-editor-lsp.md)
// ---------------------------------------------------------------------------

/// Read branch + repo state from `.git` directly (no subprocess, no git lib),
/// then fill in status counts with exactly one `git status --porcelain=v2
/// --branch` subprocess (site/content/internals/prompt-editor-lsp.md) — the one deliberate exception to "no
/// subprocess" in this reader, budgeted because [`read_git`] itself only ever
/// runs once per command (site/content/internals/prompt-editor-lsp.md), never per keystroke. A repo whose git binary
/// can't run (missing, non-zero exit, unparseable output) still gets an
/// accurate branch/state; only the counts are flagged `degraded` and left at
/// zero — an honest gap, not a lie (site/content/internals/language-conformance-contract.md).
pub fn read_git(cwd: &Path) -> Option<GitSnapshot> {
    let (repo_root, git_dir) = discover_repo(cwd)?;
    let repo_relative = cwd
        .strip_prefix(&repo_root)
        .map(|p| p.to_path_buf())
        .unwrap_or_default();

    let (branch, detached_at) = read_head(&git_dir);
    let state = read_state(&git_dir);
    let counts = git_status_counts(cwd);
    let degraded = counts.is_none();
    let counts = counts.unwrap_or_default();

    Some(GitSnapshot {
        repo_root,
        repo_relative,
        branch,
        detached_at,
        state,
        ahead: counts.ahead,
        behind: counts.behind,
        staged: counts.staged,
        unstaged: counts.unstaged,
        untracked: counts.untracked,
        conflicted: counts.conflicted,
        // Not derivable from a single `git status`; a second subprocess
        // (`git stash list`) would be needed and the budget here is one call
        // per command. Left at zero — an honest gap (site/content/internals/prompt-editor-lsp.md fuller engine can
        // add it later without breaking this contract).
        stashed: 0,
        degraded,
        age: Duration::ZERO,
    })
}

/// Status counts parsed out of `git status --porcelain=v2 --branch`.
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct GitCounts {
    pub(super) ahead: u32,
    pub(super) behind: u32,
    pub(super) staged: u32,
    pub(super) unstaged: u32,
    pub(super) untracked: u32,
    pub(super) conflicted: u32,
}

/// Run the one status subprocess this reader budgets and parse it into
/// counts. `None` on any failure to run or a non-zero exit — callers treat
/// that as `degraded`, never as "clean".
pub(super) fn git_status_counts(cwd: &Path) -> Option<GitCounts> {
    git_status_counts_with_program(cwd, std::ffi::OsStr::new("git"))
}

fn git_status_counts_with_program(cwd: &Path, program: &std::ffi::OsStr) -> Option<GitCounts> {
    let mut command = std::process::Command::new(program);
    command
        .arg("-C")
        .arg(cwd)
        .args(["status", "--porcelain=v2", "--branch"]);
    let output =
        shoal_exec::run_bounded_command(&mut command, GIT_STATUS_TIMEOUT, GIT_STATUS_OUTPUT_CAP)
            .ok()?;
    if output.timed_out || output.truncated || !output.status.success() {
        return None;
    }
    Some(parse_porcelain_v2_counts(&output.stdout))
}

/// Parse `git status --porcelain=v2 --branch` bytes into [`GitCounts`].
///
/// Line shapes (git-status(1)): `# branch.ab +<ahead> -<behind>` (absent with
/// no upstream); `1 <XY> …` ordinary changed entry; `2 <XY> …` renamed/copied
/// entry (`X`/`Y` = index/worktree status chars, `.` means unchanged); `u …`
/// unmerged/conflicted entry; `? <path>` untracked; `! <path>` ignored
/// (skipped). Every count only ever needs the marker byte and, for `1`/`2`,
/// the `XY` field — never the path — so this never needs to special-case
/// paths containing spaces or tabs the way a path-extracting parser would
/// (contrast `shoal-adapters`' `parse_porcelain_v2`, which does extract paths
/// for the `git status` *adapter* and is private to that crate). An
/// unparseable or missing `branch.ab`/count field degrades to `0`, never a
/// guess.
pub(super) fn parse_porcelain_v2_counts(bytes: &[u8]) -> GitCounts {
    let text = String::from_utf8_lossy(bytes);
    let mut c = GitCounts::default();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("# branch.ab ") {
            for tok in rest.split_whitespace() {
                if let Some(n) = tok.strip_prefix('+') {
                    c.ahead = n.parse().unwrap_or(0);
                } else if let Some(n) = tok.strip_prefix('-') {
                    c.behind = n.parse().unwrap_or(0);
                }
            }
            continue;
        }
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        let mut fields = line.split(' ');
        match fields.next() {
            Some("1") | Some("2") => {
                let Some(xy) = fields.next() else { continue };
                let mut chars = xy.chars();
                let x = chars.next().unwrap_or('.');
                let y = chars.next().unwrap_or('.');
                if x != '.' {
                    c.staged += 1;
                }
                if y != '.' {
                    c.unstaged += 1;
                }
            }
            Some("u") => c.conflicted += 1,
            Some("?") => c.untracked += 1,
            _ => {}
        }
    }
    c
}

pub(super) fn discover_repo(cwd: &Path) -> Option<(PathBuf, PathBuf)> {
    let mut dir = cwd;
    loop {
        let candidate = dir.join(".git");
        if candidate.is_dir() {
            return Some((dir.to_path_buf(), candidate));
        }
        if candidate.is_file() {
            // Worktree: `.git` file contains `gitdir: <path>`.
            if let Ok(content) = std::fs::read_to_string(&candidate)
                && let Some(rest) = content.trim().strip_prefix("gitdir:")
            {
                let gd = PathBuf::from(rest.trim());
                let gd = if gd.is_absolute() { gd } else { dir.join(gd) };
                return Some((dir.to_path_buf(), gd));
            }
        }
        dir = dir.parent()?;
    }
}

pub(super) fn read_head(git_dir: &Path) -> (Option<String>, Option<String>) {
    let Ok(content) = std::fs::read_to_string(git_dir.join("HEAD")) else {
        return (None, None);
    };
    let content = content.trim();
    if let Some(rest) = content.strip_prefix("ref:") {
        let refname = rest.trim();
        let branch = refname
            .rsplit('/')
            .next()
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());
        (branch, None)
    } else if content.len() >= 7 && content.chars().all(|c| c.is_ascii_hexdigit()) {
        (None, Some(content[..7].to_string()))
    } else {
        (None, None)
    }
}

pub(super) fn read_state(git_dir: &Path) -> RepoState {
    let exists = |name: &str| git_dir.join(name).exists();
    if exists("rebase-merge") || exists("rebase-apply") {
        RepoState::Rebasing
    } else if exists("MERGE_HEAD") {
        RepoState::Merging
    } else if exists("CHERRY_PICK_HEAD") {
        RepoState::CherryPicking
    } else if exists("REVERT_HEAD") {
        RepoState::Reverting
    } else if exists("BISECT_LOG") {
        RepoState::Bisecting
    } else {
        RepoState::Clean
    }
}

#[cfg(test)]
mod bounded_probe_tests {
    use super::*;
    use std::fs;
    use std::io;
    use std::os::unix::fs::PermissionsExt;
    use std::thread;
    use std::time::Instant;

    fn executable_script(contents: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("git");
        fs::write(&path, format!("#!/bin/sh\n{contents}\n")).expect("write fake git");
        let mut permissions = fs::metadata(&path)
            .expect("fake git metadata")
            .permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&path, permissions).expect("make fake git executable");
        (dir, path)
    }

    #[test]
    fn bounded_git_probe_parses_complete_output() {
        let (_dir, git) = executable_script(
            "printf '# branch.ab +2 -3\\n1 M. N... 100644 100644 100644 x x file\\n? new\\n'",
        );
        let counts = git_status_counts_with_program(Path::new("/"), git.as_os_str())
            .expect("complete status");
        assert_eq!(counts.ahead, 2);
        assert_eq!(counts.behind, 3);
        assert_eq!(counts.staged, 1);
        assert_eq!(counts.untracked, 1);
    }

    #[test]
    fn truncated_status_is_degraded_instead_of_parsed_as_clean() {
        let (_dir, git) =
            executable_script("printf '# branch.ab +0 -0\\n'; head -c 300000 /dev/zero");
        let start = Instant::now();
        assert!(git_status_counts_with_program(Path::new("/"), git.as_os_str()).is_none());
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn hung_status_is_degraded_and_forked_descendant_is_killed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let descendant_path = dir.path().join("descendant.pid");
        let script = format!(
            "printf '# branch.ab +0 -0\\n'; sleep 30 & echo $! > '{}'; wait",
            descendant_path.display()
        );
        let (_git_dir, git) = executable_script(&script);

        let start = Instant::now();
        assert!(git_status_counts_with_program(Path::new("/"), git.as_os_str()).is_none());
        assert!(start.elapsed() < Duration::from_secs(1));

        let descendant: libc::pid_t = fs::read_to_string(descendant_path)
            .expect("descendant pid")
            .trim()
            .parse()
            .expect("numeric pid");
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            // SAFETY: signal 0 only checks whether this recorded pid exists.
            let result = unsafe { libc::kill(descendant, 0) };
            if result == -1 && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "descendant {descendant} survived"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }
}
