//! Minimal GitLab REST API client for the digest (read-only, needs a `read_api` token).

use std::env;

use serde_json::Value;

use crate::digest::{format_date, format_utc, parse_time};
use crate::{AGENT, Result};

const PER_PAGE: usize = 100;
const MAX_PAGES: usize = 50;

pub struct Gitlab {
    api: String,
    token: String,
    group: String,
    /// Project paths inside the group to count; empty = every project.
    only: Vec<String>,
}

pub struct Project {
    pub id: u64,
    pub name: String,
}

#[derive(Debug, PartialEq)]
pub struct RawCommit {
    pub title: String,
    pub author_name: String,
    pub author_email: String,
    pub authored: i64,
    pub committed: i64,
    pub merge: bool,
}

impl Gitlab {
    pub fn from_env() -> Result<Self> {
        let get = |k: &str| -> Result<String> {
            env::var(k)
                .ok()
                .filter(|v| !v.is_empty())
                .ok_or_else(|| format!("{k} is not set (in the environment or .env)").into())
        };
        Ok(Gitlab {
            api: format!("{}/api/v4", get("GITLAB_URL")?.trim_end_matches('/')),
            token: get("GITLAB_TOKEN")?,
            group: get("GITLAB_GROUP")?,
            only: parse_project_list(&env::var("GITLAB_PROJECTS").unwrap_or_default()),
        })
    }

    fn get(&self, path: &str, query: &[(&str, String)], page: usize) -> Result<Value> {
        let mut req = AGENT
            .get(format!("{}{path}", self.api))
            .header("PRIVATE-TOKEN", self.token.as_str())
            .query("per_page", PER_PAGE.to_string())
            .query("page", page.to_string());
        for (k, v) in query {
            req = req.query(*k, v);
        }
        let mut resp = req.call()?;
        let status = resp.status();
        let body = resp.body_mut().read_to_string()?;
        if !status.is_success() {
            let snippet: String = body.chars().take(200).collect();
            return Err(format!("GitLab {path}: HTTP {status}: {snippet}").into());
        }
        serde_json::from_str(&body).map_err(|_| {
            format!("GitLab {path}: not JSON (VPN down or a login page?)").into()
        })
    }

    fn get_all(&self, path: &str, query: &[(&str, String)]) -> Result<Vec<Value>> {
        let mut out = Vec::new();
        for page in 1..=MAX_PAGES {
            let items = match self.get(path, query, page)? {
                Value::Array(items) => items,
                _ => return Err(format!("GitLab {path}: expected a list").into()),
            };
            let n = items.len();
            out.extend(items);
            if n < PER_PAGE {
                return Ok(out);
            }
        }
        // Better no report than a silently incomplete one.
        Err(format!("GitLab {path}: more than {} results, refusing to report a partial list", MAX_PAGES * PER_PAGE).into())
    }

    fn group_path(&self) -> String {
        format!("/groups/{}", encode_segment(&self.group))
    }

    pub fn group_name(&self) -> Result<String> {
        let g = self.get(&self.group_path(), &[("with_projects", "false".into())], 1)?;
        Ok(g["name"].as_str().unwrap_or(&self.group).to_string())
    }

    /// Projects of the group (and subgroups) with any activity since `since`.
    pub fn projects(&self, since: i64) -> Result<Vec<Project>> {
        let items = self.get_all(
            &format!("{}/projects", self.group_path()),
            &[
                ("include_subgroups", "true".into()),
                ("archived", "false".into()),
                ("simple", "true".into()),
                ("order_by", "last_activity_at".into()),
            ],
        )?;
        Ok(parse_projects(&items, since, &self.only))
    }

    /// Commits on all branches of a project, committed in [since, until].
    pub fn commits(&self, p: &Project, since: i64, until: i64) -> Result<Vec<RawCommit>> {
        let items = self.get_all(
            &format!("/projects/{}/repository/commits", p.id),
            &[
                ("all", "true".into()),
                ("since", format_utc(since)),
                ("until", format_utc(until)),
            ],
        )?;
        Ok(items.iter().filter_map(parse_commit).collect())
    }

    /// Tags pushed to a project in [from, to).
    pub fn tags(&self, p: &Project, from: i64, to: i64) -> Result<Vec<String>> {
        let items = self.get_all(
            &format!("/projects/{}/events", p.id),
            // `after` is a date and exclusive, so start one day earlier and filter by time.
            &[("action", "pushed".into()), ("after", format_date(from - 86400))],
        )?;
        Ok(tag_pushes(&items, from, to))
    }

    /// Merge requests of the given projects merged in [from, to).
    pub fn merged_mrs(&self, projects: &[Project], from: i64, to: i64) -> Result<usize> {
        let items = self.get_all(
            &format!("{}/merge_requests", self.group_path()),
            &[
                ("state", "merged".into()),
                ("scope", "all".into()),
                ("updated_after", format_utc(from)),
            ],
        )?;
        let ids: Vec<u64> = projects.iter().map(|p| p.id).collect();
        Ok(count_merged(&items, &ids, from, to))
    }
}

/// Percent-encode a path segment ("group/sub" -> "group%2Fsub").
fn encode_segment(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// "frontend, backend" -> ["frontend", "backend"]; "*" or empty -> every project.
fn parse_project_list(s: &str) -> Vec<String> {
    s.split(',')
        .map(str::trim)
        .filter(|p| !p.is_empty() && *p != "*")
        .map(String::from)
        .collect()
}

fn time_of(v: &Value, key: &str) -> Option<i64> {
    v[key].as_str().and_then(parse_time)
}

fn parse_projects(items: &[Value], since: i64, only: &[String]) -> Vec<Project> {
    items
        .iter()
        .filter(|p| only.is_empty() || p["path"].as_str().is_some_and(|path| only.iter().any(|o| o == path)))
        .filter(|p| time_of(p, "last_activity_at").is_none_or(|t| t >= since))
        .filter_map(|p| {
            Some(Project {
                id: p["id"].as_u64()?,
                name: p["path"].as_str().or(p["name"].as_str())?.to_string(),
            })
        })
        .collect()
}

fn parse_commit(v: &Value) -> Option<RawCommit> {
    Some(RawCommit {
        title: v["title"].as_str()?.to_string(),
        author_name: v["author_name"].as_str().unwrap_or("").to_string(),
        author_email: v["author_email"].as_str().unwrap_or("").to_string(),
        authored: time_of(v, "authored_date")?,
        committed: time_of(v, "committed_date")?,
        merge: v["parent_ids"].as_array().is_some_and(|p| p.len() > 1),
    })
}

fn tag_pushes(items: &[Value], from: i64, to: i64) -> Vec<String> {
    let mut tags: Vec<(i64, String)> = items
        .iter()
        .filter(|e| e["push_data"]["ref_type"] == "tag" && e["push_data"]["action"] == "created")
        .filter_map(|e| {
            let t = time_of(e, "created_at")?;
            (from <= t && t < to).then(|| (t, e["push_data"]["ref"].as_str().unwrap_or("?").to_string()))
        })
        .collect();
    tags.sort();
    tags.into_iter().map(|(_, name)| name).collect()
}

fn count_merged(items: &[Value], project_ids: &[u64], from: i64, to: i64) -> usize {
    items
        .iter()
        .filter(|mr| mr["project_id"].as_u64().is_some_and(|id| project_ids.contains(&id)))
        .filter_map(|mr| time_of(mr, "merged_at"))
        .filter(|t| from <= *t && *t < to)
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const T: i64 = 1793980933; // 2026-11-06T16:02:13Z

    #[test]
    fn encodes_group_paths() {
        assert_eq!(encode_segment("group"), "group");
        assert_eq!(encode_segment("group/sub group"), "group%2Fsub%20group");
    }

    #[test]
    fn parses_commits_and_flags_merges() {
        let c = json!({
            "id": "abc", "title": "fix(auth): x", "author_name": "Jane Doe",
            "author_email": "jane.doe@example.com", "authored_date": "2026-11-06T21:02:13.000+05:00",
            "committed_date": "2026-11-06T16:02:13Z", "parent_ids": ["p1"]
        });
        assert_eq!(
            parse_commit(&c),
            Some(RawCommit {
                title: "fix(auth): x".into(),
                author_name: "Jane Doe".into(),
                author_email: "jane.doe@example.com".into(),
                authored: T,
                committed: T,
                merge: false,
            })
        );
        let mut m = c.clone();
        m["parent_ids"] = json!(["p1", "p2"]);
        assert!(parse_commit(&m).unwrap().merge);
        assert_eq!(parse_commit(&json!({"title": "no dates"})), None);
    }

    #[test]
    fn keeps_only_recently_active_projects() {
        let items = vec![
            json!({"id": 1, "path": "backend", "last_activity_at": "2026-11-06T16:02:13Z"}),
            json!({"id": 2, "path": "old", "last_activity_at": "2020-01-01T00:00:00Z"}),
            json!({"id": 3, "name": "no-date"}),
        ];
        let names: Vec<String> = parse_projects(&items, T - 10, &[]).into_iter().map(|p| p.name).collect();
        assert_eq!(names, vec!["backend", "no-date"]);
    }

    #[test]
    fn counts_only_listed_projects() {
        let only = parse_project_list("frontend, backend,worker,auth");
        assert_eq!(only, vec!["frontend", "backend", "worker", "auth"]);
        assert!(parse_project_list("").is_empty());
        assert!(parse_project_list(" * ").is_empty());
        let items = vec![
            json!({"id": 1, "path": "auth", "last_activity_at": "2026-11-06T16:02:13Z"}),
            json!({"id": 2, "path": "deployment", "last_activity_at": "2026-11-06T16:02:13Z"}),
        ];
        let names: Vec<String> = parse_projects(&items, T - 10, &only).into_iter().map(|p| p.name).collect();
        assert_eq!(names, vec!["auth"]);
    }

    #[test]
    fn picks_new_tags_in_window() {
        let ev = |t: &str, kind: &str, action: &str, r: &str| {
            json!({"created_at": t, "push_data": {"ref_type": kind, "action": action, "ref": r}})
        };
        let items = vec![
            ev("2026-11-06T16:02:13Z", "tag", "created", "0.0.158"),
            ev("2026-11-06T16:02:14Z", "branch", "pushed", "main"),
            ev("2026-11-06T16:02:15Z", "tag", "removed", "0.0.157"),
            ev("2026-11-05T10:00:00Z", "tag", "created", "0.0.156"),
        ];
        assert_eq!(tag_pushes(&items, T - 60, T + 60), vec!["0.0.158"]);
    }

    #[test]
    fn counts_merges_in_window() {
        let items = vec![
            json!({"project_id": 1, "merged_at": "2026-11-06T16:02:13Z"}),
            json!({"project_id": 1, "merged_at": "2026-11-06T18:00:00Z"}),
            json!({"project_id": 9, "merged_at": "2026-11-06T16:02:13Z"}),
            json!({"project_id": 1, "merged_at": null}),
        ];
        assert_eq!(count_merged(&items, &[1], T - 60, T + 60), 1);
    }
}
