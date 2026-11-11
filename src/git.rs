//! Git status for the header branch applet. Everything shells out to `git` (and optionally
//! `gh`) on a background thread; callers never block the UI on it.

use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Result, anyhow};

use crate::claude::hide_window;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct GitStatus {
    pub is_repo: bool,
    /// `None` when HEAD is detached.
    pub branch: Option<String>,
    pub detached_at: Option<String>,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub changed_files: u32,
    pub insertions: u32,
    pub deletions: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PullRequest {
    pub number: u64,
    pub title: String,
    pub state: String,
    pub url: String,
}

fn git(cwd: &Path, args: &[&str]) -> Result<String> {
    let mut cmd = Command::new("git");
    cmd.args(args)
        .current_dir(cwd)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    hide_window(&mut cmd);
    let out = cmd.output()?;
    if !out.status.success() {
        return Err(anyhow!(
            "{}",
            String::from_utf8_lossy(&out.stderr).trim().to_owned()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// One `git status --porcelain=v2 --branch` call gives branch, upstream, ahead/behind and the
/// dirty file list; `--numstat` against HEAD adds the +/- totals the applet shows.
pub fn status(cwd: &Path) -> GitStatus {
    let Ok(out) = git(cwd, &["status", "--porcelain=v2", "--branch", "--untracked-files=normal"])
    else {
        return GitStatus::default();
    };
    let mut status = parse_porcelain_v2(&out);
    if status.changed_files > 0
        && let Ok(numstat) = git(cwd, &["diff", "HEAD", "--numstat"])
    {
        let (ins, del) = parse_numstat(&numstat);
        status.insertions = ins;
        status.deletions = del;
    }
    status
}

pub fn parse_porcelain_v2(out: &str) -> GitStatus {
    let mut s = GitStatus { is_repo: true, ..Default::default() };
    for line in out.lines() {
        if let Some(rest) = line.strip_prefix("# branch.head ") {
            s.branch = (rest != "(detached)").then(|| rest.to_owned());
        } else if let Some(rest) = line.strip_prefix("# branch.oid ") {
            if rest != "(initial)" {
                s.detached_at = Some(rest.chars().take(7).collect());
            }
        } else if let Some(rest) = line.strip_prefix("# branch.upstream ") {
            s.upstream = Some(rest.to_owned());
        } else if let Some(rest) = line.strip_prefix("# branch.ab ") {
            for part in rest.split_whitespace() {
                if let Some(n) = part.strip_prefix('+') {
                    s.ahead = n.parse().unwrap_or(0);
                } else if let Some(n) = part.strip_prefix('-') {
                    s.behind = n.parse().unwrap_or(0);
                }
            }
        } else if !line.starts_with('#') && !line.is_empty() && !line.starts_with('!') {
            s.changed_files += 1;
        }
    }
    if s.branch.is_some() {
        s.detached_at = None;
    }
    s
}

fn parse_numstat(out: &str) -> (u32, u32) {
    out.lines().fold((0, 0), |(i, d), line| {
        let mut parts = line.split('\t');
        let a = parts.next().and_then(|n| n.parse::<u32>().ok()).unwrap_or(0);
        let b = parts.next().and_then(|n| n.parse::<u32>().ok()).unwrap_or(0);
        (i + a, d + b)
    })
}

/// Local branches, most recently committed first.
pub fn local_branches(cwd: &Path) -> Result<Vec<String>> {
    let out = git(
        cwd,
        &["for-each-ref", "--sort=-committerdate", "--format=%(refname:short)", "refs/heads"],
    )?;
    Ok(out.lines().map(str::to_owned).filter(|l| !l.is_empty()).collect())
}

/// Rejects names git would parse as options, as upstream's driver does.
fn check_branch_name(branch: &str) -> Result<()> {
    if branch.is_empty() || branch.starts_with('-') {
        return Err(anyhow!("invalid branch name: {branch:?}"));
    }
    Ok(())
}

pub fn switch_branch(cwd: &Path, branch: &str) -> Result<()> {
    check_branch_name(branch)?;
    git(cwd, &["switch", branch]).map(|_| ())
}

pub fn create_branch(cwd: &Path, branch: &str) -> Result<()> {
    check_branch_name(branch)?;
    git(cwd, &["switch", "-c", branch]).map(|_| ())
}

/// The open PR for the current branch via the GitHub CLI, if `gh` is installed and authed.
pub fn pull_request(cwd: &Path) -> Option<PullRequest> {
    let mut cmd = Command::new("gh");
    cmd.args(["pr", "view", "--json", "number,title,state,url"])
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    hide_window(&mut cmd);
    let out = cmd.output().ok()?;
    if !out.status.success() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    Some(PullRequest {
        number: v["number"].as_u64()?,
        title: v["title"].as_str()?.to_owned(),
        state: v["state"].as_str()?.to_owned(),
        url: v["url"].as_str()?.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_branch_tracking_and_dirty_files() {
        let s = parse_porcelain_v2(
            "# branch.oid 1234567890abcdef\n# branch.head feature/x\n# branch.upstream origin/feature/x\n# branch.ab +2 -1\n1 .M N... 100644 100644 100644 a b src/main.rs\n? new.txt\n",
        );
        assert_eq!(s.branch.as_deref(), Some("feature/x"));
        assert_eq!(s.upstream.as_deref(), Some("origin/feature/x"));
        assert_eq!((s.ahead, s.behind, s.changed_files), (2, 1, 2));
        assert_eq!(s.detached_at, None);
    }

    #[test]
    fn parses_detached_head() {
        let s = parse_porcelain_v2("# branch.oid abcdef1234\n# branch.head (detached)\n");
        assert_eq!(s.branch, None);
        assert_eq!(s.detached_at.as_deref(), Some("abcdef1"));
    }

    #[test]
    fn refuses_option_like_branch_names() {
        let cwd = std::env::temp_dir();
        assert!(switch_branch(&cwd, "--orphan").is_err());
        assert!(create_branch(&cwd, "-f").is_err());
    }

    #[test]
    fn sums_numstat_skipping_binary() {
        assert_eq!(parse_numstat("3\t1\ta.rs\n-\t-\tbin.png\n10\t0\tb.rs\n"), (13, 1));
    }
}
