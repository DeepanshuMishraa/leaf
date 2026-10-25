//! GitHub diffs and review comments: pull requests, commits and comparisons.

use std::process::{Command, Stdio};
use std::sync::LazyLock;

use crate::comment::{Anchor, Side};
use crate::diff::Repo;

/// A diff larger than this is refused rather than held in memory.
const MAX_BYTES: u64 = 96 * 1024 * 1024;

static GH: LazyLock<bool> = LazyLock::new(|| {
    Command::new("gh")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
});

#[derive(Debug, thiserror::Error)]
pub enum GithubFailed {
    #[error(
        "That link isn't a GitHub pull request, commit or comparison: {url}. Use a link like https://github.com/owner/repo/pull/12."
    )]
    NotGithub { url: String },
    #[error(
        "Could not reach GitHub ({cause}). Check your connection and the link, then try again."
    )]
    Network { cause: String },
    #[error("GitHub returned HTTP {status} while {what}. {hint}")]
    Http {
        status: u16,
        what: String,
        hint: String,
    },
    #[error(
        "GitHub rejected the comment: {detail} Check that the line is still part of the pull request, then try again."
    )]
    Post { detail: String },
    #[error(
        "No pull request found for {branch}. Open one on GitHub first, or comment on a pull request link directly."
    )]
    NoPullRequest { branch: String },
    #[error(
        "Posting to GitHub needs the GitHub CLI (`gh`) or a GH_TOKEN/GITHUB_TOKEN environment variable. Install it from https://cli.github.com, run `gh auth login`, then try again."
    )]
    NoAuth,
    #[error("`gh {command}` failed: {cause}")]
    Gh { command: String, cause: String },
}

/// A GitHub page that holds a diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resource {
    Pull {
        owner: String,
        repo: String,
        number: u64,
    },
    Commit {
        owner: String,
        repo: String,
        sha: String,
    },
    Compare {
        owner: String,
        repo: String,
        base: String,
        head: String,
    },
}

impl Resource {
    pub fn parse_url(url: &str) -> Option<Self> {
        let url = url.trim();
        let rest = url
            .strip_prefix("https://")
            .or_else(|| url.strip_prefix("http://"))
            .unwrap_or(url);
        let rest = rest.strip_prefix("github.com/")?;
        let rest = rest.split(['?', '#']).next().unwrap_or(rest);
        let rest = rest.strip_suffix('/').unwrap_or(rest);
        let mut parts = rest.split('/');
        let owner = clean(parts.next()?)?;
        let repo = clean(parts.next()?)?;
        let kind = parts.next()?;

        match kind {
            "pull" | "pulls" => {
                let number = parts.next()?.trim_end_matches(".diff").parse().ok()?;
                Some(Resource::Pull {
                    owner,
                    repo,
                    number,
                })
            }
            "commit" | "commits" => {
                let sha = parts.next()?.trim_end_matches(".diff");
                is_sha(sha).then(|| Resource::Commit {
                    owner,
                    repo,
                    sha: sha.to_owned(),
                })
            }
            "compare" => {
                let range = parts.next()?.trim_end_matches(".diff");
                let (base, head) = range.split_once("...")?;
                let (base, head) = (clean(base)?, clean(head)?);
                Some(Resource::Compare {
                    owner,
                    repo,
                    base,
                    head,
                })
            }
            _ => None,
        }
    }

    pub fn label(&self) -> String {
        match self {
            Resource::Pull {
                owner,
                repo,
                number,
            } => format!("{owner}/{repo}#{number}"),
            Resource::Commit { owner, repo, sha } => {
                format!("{owner}/{repo}@{}", &sha[..sha.len().min(7)])
            }
            Resource::Compare {
                owner,
                repo,
                base,
                head,
            } => format!("{owner}/{repo} {base}...{head}"),
        }
    }

    pub fn key(&self) -> String {
        match self {
            Resource::Pull {
                owner,
                repo,
                number,
            } => format!("{owner}/{repo}/pull/{number}"),
            Resource::Commit { owner, repo, sha } => format!("{owner}/{repo}/commit/{sha}"),
            Resource::Compare {
                owner,
                repo,
                base,
                head,
            } => format!("{owner}/{repo}/compare/{base}...{head}"),
        }
    }

    fn api_path(&self) -> String {
        match self {
            Resource::Pull {
                owner,
                repo,
                number,
            } => format!("repos/{owner}/{repo}/pulls/{number}"),
            Resource::Commit { owner, repo, sha } => {
                format!("repos/{owner}/{repo}/commits/{sha}")
            }
            Resource::Compare {
                owner,
                repo,
                base,
                head,
            } => format!("repos/{owner}/{repo}/compare/{base}...{head}"),
        }
    }

    fn web_url(&self) -> String {
        match self {
            Resource::Pull {
                owner,
                repo,
                number,
            } => format!("https://github.com/{owner}/{repo}/pull/{number}"),
            Resource::Commit { owner, repo, sha } => {
                format!("https://github.com/{owner}/{repo}/commit/{sha}")
            }
            Resource::Compare {
                owner,
                repo,
                base,
                head,
            } => format!("https://github.com/{owner}/{repo}/compare/{base}...{head}"),
        }
    }

    fn diff_url(&self) -> String {
        format!("{}.diff", self.web_url())
    }
}

fn clean(part: &str) -> Option<String> {
    let part = part.trim();
    (!part.is_empty()).then(|| part.to_owned())
}

fn is_sha(sha: &str) -> bool {
    (7..=40).contains(&sha.len()) && sha.chars().all(|c| c.is_ascii_hexdigit())
}

/// A diff fetched from GitHub, and where it came from.
pub struct Fetched {
    pub patch: String,
    pub resource: Resource,
}

/// Downloads the diff for a pull request, commit or comparison.
pub fn fetch(url: &str) -> Result<Fetched, GithubFailed> {
    let resource = Resource::parse_url(url).ok_or_else(|| GithubFailed::NotGithub {
        url: url.to_owned(),
    })?;
    let path = resource.api_path();

    let mut gh_error = None;
    if *GH {
        match gh(&[
            "api",
            "--method",
            "GET",
            "-H",
            "Accept: application/vnd.github.v3.diff",
            &path,
        ]) {
            Ok(patch)
                if patch.trim().is_empty()
                    || patch.starts_with("diff --git")
                    || patch.starts_with("From ") =>
            {
                return Ok(Fetched { patch, resource });
            }
            Ok(other) => {
                gh_error = Some(format!(
                    "gh returned something that isn't a diff: {}",
                    first_line(&other)
                ));
            }
            Err(e) => gh_error = Some(e.to_string()),
        }
    }

    let patch =
        http_get(&resource.diff_url(), "application/vnd.github.v3.diff").map_err(
            |e| match gh_error {
                Some(gh) => GithubFailed::Network {
                    cause: format!("{e}; gh said: {gh}"),
                },
                None => e,
            },
        )?;
    Ok(Fetched { patch, resource })
}

struct Target {
    owner: String,
    repo: String,
    number: u64,
    head: String,
    label: String,
}

/// Posts a review comment on the pull request behind `resource`, or on the
/// current branch's pull request when `resource` isn't a pull request itself.
pub fn post_review(
    resource: Option<&Resource>,
    repo: Option<&Repo>,
    branch: Option<&str>,
    anchor: &Anchor,
    body: &str,
) -> Result<String, GithubFailed> {
    let target = target(resource, repo, branch)?;
    let side = match anchor.side {
        Side::Old => "LEFT",
        Side::New => "RIGHT",
    };
    let path = format!(
        "repos/{}/{}/pulls/{}/comments",
        target.owner, target.repo, target.number
    );

    let mut args: Vec<String> = vec![
        "api".to_owned(),
        "--method".to_owned(),
        "POST".to_owned(),
        path,
        "-f".to_owned(),
        format!("body={body}"),
        "-f".to_owned(),
        format!("commit_id={}", target.head),
        "-f".to_owned(),
        format!("path={}", anchor.file),
        "-f".to_owned(),
        format!("side={side}"),
        "-F".to_owned(),
        format!("line={}", anchor.end_line),
    ];
    if anchor.start_line != anchor.end_line {
        args.push("-F".to_owned());
        args.push(format!("start_line={}", anchor.start_line));
        args.push("-f".to_owned());
        args.push(format!("start_side={side}"));
    }

    let mut gh_error = None;
    if *GH {
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        match gh(&refs) {
            Ok(_) => return Ok(target.label),
            Err(e) => gh_error = Some(e),
        }
    }
    if token().is_some() {
        http_post_comment(&target, anchor, body, side)?;
        return Ok(target.label);
    }
    Err(gh_error.unwrap_or(GithubFailed::NoAuth))
}

fn target(
    resource: Option<&Resource>,
    repo: Option<&Repo>,
    branch: Option<&str>,
) -> Result<Target, GithubFailed> {
    match resource {
        Some(Resource::Pull {
            owner,
            repo,
            number,
        }) => pull_target(owner, repo, *number),
        Some(Resource::Commit {
            owner,
            repo: name,
            sha,
        }) => match commit_pull_request(owner, name, sha)? {
            Some(number) => pull_target(owner, name, number),
            None => branch_target(repo, branch),
        },
        Some(Resource::Compare { .. }) | None => branch_target(repo, branch),
    }
}

fn pull_target(owner: &str, repo: &str, number: u64) -> Result<Target, GithubFailed> {
    let path = format!("repos/{owner}/{repo}/pulls/{number}");
    let mut head = None;
    if *GH && let Ok(json) = gh(&["api", &path]) {
        head = json_head_sha(&json);
    }
    let head = match head {
        Some(head) => head,
        None => {
            let json = http_get(&format!("https://api.github.com/{path}"), JSON_ACCEPT)?;
            json_head_sha(&json).ok_or_else(|| GithubFailed::NoPullRequest {
                branch: format!("{owner}/{repo}#{number}"),
            })?
        }
    };
    Ok(Target {
        owner: owner.to_owned(),
        repo: repo.to_owned(),
        number,
        head,
        label: format!("{owner}/{repo}#{number}"),
    })
}

fn commit_pull_request(owner: &str, repo: &str, sha: &str) -> Result<Option<u64>, GithubFailed> {
    let path = format!("repos/{owner}/{repo}/commits/{sha}/pulls");
    if *GH && let Ok(json) = gh(&["api", &path]) {
        return Ok(json_number_after(&json, "number"));
    }
    let json = http_get(&format!("https://api.github.com/{path}"), JSON_ACCEPT)?;
    Ok(json_number_after(&json, "number"))
}

fn branch_target(repo: Option<&Repo>, branch: Option<&str>) -> Result<Target, GithubFailed> {
    let Some(repo) = repo else {
        return Err(GithubFailed::NoPullRequest {
            branch: branch.unwrap_or("this diff").to_owned(),
        });
    };
    if !*GH {
        return Err(GithubFailed::NoAuth);
    }
    let text = gh_in(
        Some(&repo.root),
        &[
            "pr",
            "view",
            "--json",
            "url,headRefOid",
            "--jq",
            ".url + \" \" + .headRefOid",
        ],
    )
    .map_err(|e| {
        let cause = e.to_string();
        if cause.contains("no pull request") {
            GithubFailed::NoPullRequest {
                branch: branch.unwrap_or("this branch").to_owned(),
            }
        } else {
            e
        }
    })?;
    let (url, head) = text.split_once(' ').ok_or(GithubFailed::NoPullRequest {
        branch: branch.unwrap_or("this branch").to_owned(),
    })?;
    let Some(Resource::Pull {
        owner,
        repo: name,
        number,
    }) = Resource::parse_url(url)
    else {
        return Err(GithubFailed::NoPullRequest {
            branch: branch.unwrap_or("this branch").to_owned(),
        });
    };
    Ok(Target {
        owner: owner.clone(),
        repo: name.clone(),
        number,
        head: head.trim().to_owned(),
        label: format!("{owner}/{name}#{number}"),
    })
}

const JSON_ACCEPT: &str = "application/vnd.github+json";

fn http_get(url: &str, accept: &str) -> Result<String, GithubFailed> {
    let mut request = ureq::get(url)
        .header("user-agent", "leaf-reader (diff)")
        .header("accept", accept);
    if let Some(token) = token() {
        request = request.header("authorization", &format!("Bearer {token}"));
    }
    let response = request
        .call()
        .map_err(|e| http_failure(e, "fetching from GitHub"))?;
    let bytes = response
        .into_body()
        .with_config()
        .limit(MAX_BYTES)
        .read_to_vec()
        .map_err(|e| GithubFailed::Network {
            cause: e.to_string(),
        })?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn http_post_comment(
    target: &Target,
    anchor: &Anchor,
    body: &str,
    side: &str,
) -> Result<(), GithubFailed> {
    let token = token().ok_or(GithubFailed::NoAuth)?;
    let url = format!(
        "https://api.github.com/repos/{}/{}/pulls/{}/comments",
        target.owner, target.repo, target.number
    );
    let mut json = format!(
        "{{\"body\":\"{}\",\"commit_id\":\"{}\",\"path\":\"{}\",\"line\":{},\"side\":\"{side}\"",
        json_escape(body),
        json_escape(&target.head),
        json_escape(&anchor.file),
        anchor.end_line
    );
    if anchor.start_line != anchor.end_line {
        let _ = std::fmt::Write::write_fmt(
            &mut json,
            format_args!(
                ",\"start_line\":{},\"start_side\":\"{side}\"",
                anchor.start_line
            ),
        );
    }
    json.push('}');

    let response = ureq::post(&url)
        .header("user-agent", "leaf-reader (diff)")
        .header("accept", JSON_ACCEPT)
        .header("x-github-api-version", "2022-11-28")
        .header("authorization", &format!("Bearer {token}"))
        .header("content-type", "application/json")
        .send(json.as_str());
    match response {
        Ok(_) => Ok(()),
        Err(ureq::Error::StatusCode(status)) => Err(GithubFailed::Post {
            detail: format!("HTTP {status}. {}", hint(status)),
        }),
        Err(other) => Err(GithubFailed::Network {
            cause: other.to_string(),
        }),
    }
}

fn gh(args: &[&str]) -> Result<String, GithubFailed> {
    gh_in(None, args)
}

fn gh_in(dir: Option<&std::path::Path>, args: &[&str]) -> Result<String, GithubFailed> {
    let mut command = Command::new("gh");
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(dir) = dir {
        command.current_dir(dir);
    }
    let output = command.output().map_err(|e| GithubFailed::Gh {
        command: args.join(" "),
        cause: e.to_string(),
    })?;
    if !output.status.success() {
        let cause = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(GithubFailed::Gh {
            command: args.join(" "),
            cause: if cause.is_empty() {
                format!("exited with {}", output.status)
            } else {
                cause
            },
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn http_failure(error: ureq::Error, what: &str) -> GithubFailed {
    match error {
        ureq::Error::StatusCode(status) => GithubFailed::Http {
            status,
            what: what.to_owned(),
            hint: hint(status).to_owned(),
        },
        other => GithubFailed::Network {
            cause: other.to_string(),
        },
    }
}

fn hint(status: u16) -> &'static str {
    match status {
        401 | 403 => {
            "The repository may be private or GitHub may be rate limiting. Run `gh auth login`, or set GH_TOKEN, then try again."
        }
        404 => {
            "The link may be wrong, or the repository may be private. Run `gh auth login`, or set GH_TOKEN, then try again."
        }
        422 => "The line may no longer be part of the pull request's diff.",
        429 => "GitHub is rate limiting requests. Wait a minute, then try again.",
        _ => "Try again in a moment.",
    }
}

fn token() -> Option<String> {
    ["GH_TOKEN", "GITHUB_TOKEN"].into_iter().find_map(|name| {
        std::env::var(name)
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
    })
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or("").trim()
}

fn json_head_sha(json: &str) -> Option<String> {
    let head = json.find("\"head\"")?;
    json_string_after(&json[head..], "sha")
}

fn json_string_after(text: &str, key: &str) -> Option<String> {
    let quoted = format!("\"{key}\"");
    let at = text.find(&quoted)?;
    let rest = &text[at + quoted.len()..];
    let colon = rest.find(':')?;
    parse_json_string(rest[colon + 1..].trim_start())
}

fn json_number_after(text: &str, key: &str) -> Option<u64> {
    let quoted = format!("\"{key}\"");
    let at = text.find(&quoted)?;
    let rest = &text[at + quoted.len()..];
    let colon = rest.find(':')?;
    let digits: String = rest[colon + 1..]
        .trim_start()
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse().ok()
}

fn parse_json_string(text: &str) -> Option<String> {
    let mut chars = text.strip_prefix('"')?.chars();
    let mut out = String::new();
    while let Some(c) = chars.next() {
        match c {
            '"' => return Some(out),
            '\\' => match chars.next()? {
                'n' => out.push('\n'),
                't' => out.push('\t'),
                'r' => out.push('\r'),
                'b' => out.push('\u{8}'),
                'f' => out.push('\u{c}'),
                '"' => out.push('"'),
                '\\' => out.push('\\'),
                '/' => out.push('/'),
                'u' => {
                    let hex: String = (0..4).filter_map(|_| chars.next()).collect();
                    let value = u32::from_str_radix(&hex, 16).ok()?;
                    out.push(char::from_u32(value)?);
                }
                other => out.push(other),
            },
            c => out.push(c),
        }
    }
    None
}

fn json_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => {
                let _ = std::fmt::Write::write_fmt(&mut out, format_args!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_pages_parse() {
        assert_eq!(
            Resource::parse_url("https://github.com/o/r/pull/12"),
            Some(Resource::Pull {
                owner: "o".into(),
                repo: "r".into(),
                number: 12
            })
        );
        assert_eq!(
            Resource::parse_url("http://github.com/o/r/pull/12/files#diff-abc"),
            Some(Resource::Pull {
                owner: "o".into(),
                repo: "r".into(),
                number: 12
            })
        );
        assert_eq!(
            Resource::parse_url("https://github.com/o/r/pull/12.diff"),
            Some(Resource::Pull {
                owner: "o".into(),
                repo: "r".into(),
                number: 12
            })
        );
        assert_eq!(
            Resource::parse_url("https://github.com/o/r/commit/0123456789abcdef"),
            Some(Resource::Commit {
                owner: "o".into(),
                repo: "r".into(),
                sha: "0123456789abcdef".into()
            })
        );
        assert_eq!(
            Resource::parse_url("https://github.com/o/r/compare/main...feat?expand=1"),
            Some(Resource::Compare {
                owner: "o".into(),
                repo: "r".into(),
                base: "main".into(),
                head: "feat".into()
            })
        );
        assert_eq!(Resource::parse_url("https://github.com/o/r/issues/1"), None);
        assert_eq!(Resource::parse_url("https://gitlab.com/o/r/pull/1"), None);
        assert_eq!(
            Resource::parse_url("https://github.com/o/r/commit/not-a-sha"),
            None
        );
    }

    #[test]
    fn labels_and_diff_urls_are_built_from_the_page() {
        let pull = Resource::Pull {
            owner: "o".into(),
            repo: "r".into(),
            number: 12,
        };
        assert_eq!(pull.label(), "o/r#12");
        assert_eq!(pull.key(), "o/r/pull/12");
        assert_eq!(pull.diff_url(), "https://github.com/o/r/pull/12.diff");
        let commit = Resource::Commit {
            owner: "o".into(),
            repo: "r".into(),
            sha: "0123456789abcdef".into(),
        };
        assert_eq!(commit.label(), "o/r@0123456");
    }

    #[test]
    fn pull_metadata_yields_the_head_sha() {
        let json = r#"{"url":"https://github.com/o/r/pull/2","head":{"ref":"feat","sha":"abc1234def"},"base":{"sha":"0000000"}}"#;
        assert_eq!(json_head_sha(json).as_deref(), Some("abc1234def"));
        let pulls = r#"[{"number":42,"title":"x"},{"number":7}]"#;
        assert_eq!(json_number_after(pulls, "number"), Some(42));
        assert_eq!(json_number_after("[]", "number"), None);
    }

    #[test]
    fn json_strings_are_escaped() {
        assert_eq!(json_escape("a\"b\nc\\d"), "a\\\"b\\nc\\\\d");
        assert_eq!(json_escape("tab\there"), "tab\\there");
    }
}
