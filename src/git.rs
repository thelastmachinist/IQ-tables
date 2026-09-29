//! Live links to IQ git repositories.
//!
//! IQ Labs' git keeps each repository's history in an IQ table of the
//! database "iq-git-v1": the table `git_commits:<owner>:<repo>` with columns
//! id, message, treeTxId, parentCommitId, timestamp (ms) and author. A
//! commit's tree is a JSON inscription (`iqgit-tree`) mapping each path to
//! `{txId, hash}`, and each file is an inscription named `iqgit-blob:<path>`
//! holding its bytes in base64. IQ's browser shows a repository at
//! `https://browser.iqlabs.dev/<commit table address>`.
//!
//! A cell holding that link shows the repository's newest commit, read when
//! the cell is on screen, so a table of projects stays current while the
//! projects change and the table never does. A commit's tree inscription is
//! the pinned alternative: `iq://tx/<treeTxId>` never changes.

use std::collections::HashMap;

use crate::app::{fetch_err, App, Load, P};
use crate::crypto::base58;
use crate::host;
use crate::iq;
use crate::json::{self, Json};
use crate::solana::b58;
use crate::ui::esc;

/// IQ git's database id.
pub const GIT_DB: &str = "iq-git-v1";
pub const BROWSER: &str = "https://browser.iqlabs.dev/";
/// How long a repository's commit list is reused before a cell re-reads it.
const FRESH_MS: f64 = 120_000.0;
const COMMITS_READ: usize = 50;

#[derive(Clone, Debug)]
pub struct Commit {
    pub id: String,
    pub message: String,
    pub time_ms: i64,
    pub tree: String,
    pub parent: String,
}

pub struct Repo {
    pub owner: String,
    pub name: String,
    /// Newest first; only commits signed by the owner.
    pub commits: Load<Vec<Commit>>,
    /// When the commit list was last read (ms).
    pub at: f64,
    pub busy: bool,
}

/// What the app knows about an address behind an IQ browser link.
#[derive(Default)]
pub struct Git {
    /// `Ready(None)`: not a git repository (the link stays a plain link).
    pub repos: HashMap<String, Load<Option<Repo>>>,
    /// Commit trees: tree inscription -> [(path, file inscription)].
    pub trees: HashMap<String, Load<Vec<(String, String)>>>,
    /// Repositories and trees the last render showed ("tree:<sig>" for trees).
    pub wanted: std::cell::RefCell<Vec<String>>,
    /// The open repository panel: (commit table, selected commit id).
    pub panel: Option<(String, Option<String>)>,
}

/// The address in an IQ browser link (`https://browser.iqlabs.dev/<address>`).
pub fn browser_pda(s: &str) -> Option<&str> {
    let rest = s.strip_prefix("https://").or_else(|| s.strip_prefix("http://")).unwrap_or(s);
    let pda = rest.strip_prefix("browser.iqlabs.dev/")?.trim_end_matches('/');
    (base58::decode(pda).map(|b| b.len()) == Some(32)).then_some(pda)
}

/// The commit table of `owner`'s repository `repo`.
pub fn commits_pda(owner: &str, repo: &str) -> String {
    let root = iq::db_root_pda(&iq::seed_bytes(GIT_DB));
    b58(&iq::table_pda(&root, &iq::seed_bytes(&format!("git_commits:{}:{}", owner, repo))))
}

/// (owner, repo) when a table named `name` at `pda` is an IQ git commit
/// table — the address must be the one IQ git derives for that name, so a
/// look-alike table elsewhere isn't mistaken for the repository.
pub fn repo_of(pda: &str, name: &str) -> Option<(String, String)> {
    let (owner, repo) = name.strip_prefix("git_commits:")?.split_once(':')?;
    crate::solana::parse_pk(owner)?;
    (commits_pda(owner, repo) == pda).then(|| (owner.to_string(), repo.to_string()))
}

/// Commits from gateway rows, newest first. Only the owner's own commits
/// count: anyone can inscribe a row, so a row signed by someone else isn't
/// the project's history.
pub fn parse_commits(rows: &[Json], owner: &str) -> Vec<Commit> {
    let mut v: Vec<Commit> = rows
        .iter()
        .filter(|r| r.get("__signer").str() == Some(owner))
        .filter_map(|r| {
            let tree = r.get("treeTxId").str_or("");
            (base58::decode(&tree).map(|b| b.len()) == Some(64)).then(|| Commit {
                id: r.get("id").str_or(""),
                message: r.get("message").str_or(""),
                time_ms: r.get("timestamp").f64().map(|t| t as i64).or_else(|| r.get("__blockTime").f64().map(|t| t as i64 * 1000)).unwrap_or(0),
                tree,
                parent: r.get("parentCommitId").str_or(""),
            })
        })
        .collect();
    v.sort_by_key(|c| std::cmp::Reverse(c.time_ms));
    v
}

/// Files of a commit tree (`{"path": {"txId": …, "hash": …}}`), by path.
pub fn parse_tree(data: &str) -> Option<Vec<(String, String)>> {
    let j = json::parse(data).ok()?;
    let mut v: Vec<(String, String)> = j
        .obj()
        .iter()
        .filter_map(|(p, f)| f.get("txId").str().filter(|s| base58::decode(s).map(|b| b.len()) == Some(64)).map(|s| (p.clone(), s.to_string())))
        .collect();
    if v.is_empty() && !j.obj().is_empty() {
        return None;
    }
    v.sort();
    Some(v)
}

/// A file type from a path's extension (git blobs are stored as
/// application/octet-stream).
pub fn filetype_of(path: &str) -> &'static str {
    let ext = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "js" | "mjs" | "cjs" => "application/javascript",
        "json" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "md" | "markdown" => "text/markdown",
        "csv" => "text/csv",
        "xml" => "application/xml",
        "wasm" => "application/wasm",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        _ => "text/plain",
    }
}

/// "3 days ago".
pub fn ago(now_ms: f64, t_ms: i64) -> String {
    let s = ((now_ms as i64 - t_ms) / 1000).max(0);
    let (n, unit) = match s {
        0..=59 => return "just now".into(),
        60..=3599 => (s / 60, "minute"),
        3600..=86_399 => (s / 3600, "hour"),
        86_400..=2_591_999 => (s / 86_400, "day"),
        2_592_000..=31_535_999 => (s / 2_592_000, "month"),
        _ => (s / 31_536_000, "year"),
    };
    format!("{} {}{} ago", n, unit, if n == 1 { "" } else { "s" })
}

fn clip(s: &str, n: usize) -> String {
    let line = s.lines().next().unwrap_or("");
    let t: String = line.chars().take(n).collect();
    if line.chars().count() > n || s.lines().nth(1).is_some() {
        format!("{}…", t)
    } else {
        t
    }
}

/// A cell showing an IQ browser link: the repository and its newest commit
/// once known; `None` keeps the plain link (not a repository, or not read yet).
pub fn cell(app: &App, pda: &str, raw: &str) -> Option<String> {
    if app.use_rpc() {
        return None;
    }
    app.git.wanted.borrow_mut().push(pda.to_string());
    let Some(Load::Ready(Some(r))) = app.git.repos.get(pda) else { return None };
    // the newest commit's message goes in the tooltip so the cell stays narrow
    let (latest, msg) = match &r.commits {
        Load::Ready(v) => match v.first() {
            Some(c) => (format!(" <span class=\"muted\">· {}</span>", ago(host::now_ms(), c.time_ms)), format!("Latest: {}\n", clip(&c.message, 80))),
            None => (" <span class=\"muted\">· no commits yet</span>".into(), String::new()),
        },
        Load::Err(_) => (String::new(), String::new()),
        _ => (" <span class=\"muted\">· reading…</span>".into(), String::new()),
    };
    Some(format!(
        "<button class=\"link git\" data-a=\"git-open\" data-arg=\"{}\" title=\"{}{} — IQ git repository; always shows its newest commit\">📦 {}{}</button>",
        esc(pda),
        esc(&msg),
        esc(raw),
        esc(&r.name),
        latest
    ))
}

/// The repository panel.
pub fn panel(app: &App, h: &mut String) {
    let Some((pda, sel)) = app.git.panel.as_ref() else { return };
    let Some(Load::Ready(Some(r))) = app.git.repos.get(pda) else { return };
    let live = format!("{}{}", BROWSER, pda);
    h.push_str("<div class=\"modal\" role=\"dialog\" aria-modal=\"true\" aria-label=\"Repository\"><div class=\"card sheet gitpanel\">");
    h.push_str(&format!(
        "<div class=\"tablehead\"><h3 class=\"grow\">📦 {}</h3><button class=\"x\" data-a=\"git-close\" aria-label=\"Close\">×</button></div>",
        esc(&r.name)
    ));
    h.push_str(&format!(
        "<p class=\"small muted\">An <a href=\"{}\" target=\"_blank\" rel=\"noopener\">IQ git</a> repository by {}. The link in the cell always shows the newest commit — the table doesn't change when the project does.</p>",
        esc(&live),
        crate::views_account::who(app, &r.owner)
    ));
    h.push_str(&format!(
        "<div class=\"row\"><a class=\"btn\" href=\"{0}\" target=\"_blank\" rel=\"noopener\">Open in IQ's browser ↗</a><button class=\"btn\" data-a=\"copy\" data-arg=\"{0}\">Copy live link</button><button class=\"btn\" data-a=\"git-refresh\" data-arg=\"{1}\"{2}>{3}</button></div>",
        esc(&live),
        esc(pda),
        if r.busy { " disabled" } else { "" },
        if r.busy { "Reading…" } else { "Refresh" }
    ));
    let commits = match &r.commits {
        Load::Ready(v) => v,
        Load::Err(e) => {
            h.push_str(&format!("<div class=\"card bad\">Couldn't read the commits: {}</div></div></div>", esc(e)));
            return;
        }
        _ => {
            h.push_str("<div class=\"loading\">Reading the commits…</div></div></div>");
            return;
        }
    };
    if commits.is_empty() {
        h.push_str("<p class=\"muted\">No commits yet.</p></div></div>");
        return;
    }
    let cur = sel.as_ref().and_then(|id| commits.iter().find(|c| &c.id == id)).unwrap_or(&commits[0]);
    let now = host::now_ms();
    h.push_str("<div class=\"gitcols\"><div><h4>Commits</h4><ul class=\"commits\">");
    for (i, c) in commits.iter().enumerate() {
        h.push_str(&format!(
            "<li class=\"{}\"><button class=\"link\" data-a=\"git-commit\" data-arg=\"{}\">{}</button>{} <span class=\"small muted\">{}</span></li>",
            if c.id == cur.id { "on" } else { "" },
            esc(&c.id),
            esc(&clip(&c.message, 60)),
            if i == 0 { " <span class=\"pill off\">latest</span>" } else { "" },
            ago(now, c.time_ms)
        ));
    }
    h.push_str("</ul></div><div>");
    h.push_str(&format!("<h4>Files in “{}”</h4>", esc(&clip(&cur.message, 40))));
    app.git.wanted.borrow_mut().push(format!("tree:{}", cur.tree));
    match app.git.trees.get(&cur.tree) {
        Some(Load::Ready(files)) => {
            h.push_str("<ul class=\"files\">");
            for (path, tx) in files {
                h.push_str(&format!(
                    "<li><button class=\"link file\" data-a=\"open-tx\" data-arg=\"{}\" data-val=\"{}\">📄 {}</button></li>",
                    esc(tx),
                    esc(path),
                    esc(path)
                ));
            }
            if files.is_empty() {
                h.push_str("<li class=\"muted\">No files.</li>");
            }
            h.push_str("</ul>");
        }
        Some(Load::Err(e)) => h.push_str(&format!("<p class=\"bad\">{}</p>", esc(e))),
        _ => h.push_str("<div class=\"loading\">Reading the file list…</div>"),
    }
    let pinned = pinned_link(&r.name, cur);
    h.push_str(&format!(
        "<p class=\"small\">This version never changes at <code>{}</code></p><div class=\"row\"><button class=\"btn\" data-a=\"copy\" data-arg=\"{}\">Copy link to this version</button></div>",
        esc(&pinned),
        esc(&pinned)
    ));
    h.push_str("</div></div></div></div>");
}

/// A link to one commit's files: its tree inscription, named repo@commit.
pub fn pinned_link(repo: &str, c: &Commit) -> String {
    let short: String = c.id.chars().filter(|ch| *ch != '-').take(8).collect();
    // `@` is allowed in a URL fragment, so it stays readable
    format!("iq://tx/{}#{}@{}", c.tree, crate::app::pct_encode(repo), short)
}

/// The file list of a tree inscription shown in the viewer.
pub fn tree_html(files: &[(String, String)], h: &mut String) {
    h.push_str(&format!("<p class=\"small\"><span class=\"pill\">IQ git</span> a commit's files ({})</p><ul class=\"files\">", files.len()));
    for (path, tx) in files {
        h.push_str(&format!(
            "<li><button class=\"link file\" data-a=\"open-tx\" data-arg=\"{}\" data-val=\"{}\">📄 {}</button></li>",
            esc(tx),
            esc(path),
            esc(path)
        ));
    }
    h.push_str("</ul>");
}

impl App {
    /// Read what the last render showed and isn't known (or is stale).
    pub fn git_fetch_wanted(&mut self) {
        let wanted: Vec<String> = std::mem::take(&mut *self.git.wanted.borrow_mut());
        if wanted.is_empty() || self.use_rpc() {
            return;
        }
        let now = host::now_ms();
        let mut seen = std::collections::HashSet::new();
        for w in wanted {
            if !seen.insert(w.clone()) {
                continue;
            }
            if let Some(tree) = w.strip_prefix("tree:") {
                if !self.git.trees.contains_key(tree) {
                    self.git.trees.insert(tree.to_string(), Load::Loading);
                    self.get(&format!("/data/{}", tree), P::GitTree(tree.to_string()));
                }
                continue;
            }
            let stale = match self.git.repos.get_mut(&w) {
                None => {
                    self.git.repos.insert(w.clone(), Load::Loading);
                    self.get(&format!("/table/{}/meta", w), P::GitMeta(w));
                    continue;
                }
                Some(Load::Ready(Some(r))) if !r.busy && now - r.at > FRESH_MS => {
                    r.busy = true;
                    true
                }
                _ => false,
            };
            if stale {
                self.git_rows(&w);
            }
        }
    }

    fn git_rows(&mut self, pda: &str) {
        self.get(&format!("/table/{}/rows?limit={}", pda, COMMITS_READ), P::GitRows(pda.to_string()));
    }

    pub fn git_action(&mut self, a: &str, arg: &str) {
        match a {
            "git-open" => self.git.panel = Some((arg.to_string(), None)),
            "git-close" => self.git.panel = None,
            "git-commit" => {
                if let Some(p) = self.git.panel.as_mut() {
                    p.1 = Some(arg.to_string());
                }
            }
            "git-refresh" => {
                if let Some(Load::Ready(Some(r))) = self.git.repos.get_mut(arg) {
                    if r.busy {
                        return;
                    }
                    r.busy = true;
                }
                self.git_rows(arg);
            }
            _ => {}
        }
    }

    pub fn git_async(&mut self, p: P, ok: bool, status: u32, data: Vec<u8>) -> bool {
        let text = String::from_utf8_lossy(&data).into_owned();
        let http_ok = ok && (200..300).contains(&status);
        match p {
            P::GitMeta(pda) => {
                let v = if http_ok {
                    let name = json::parse(&text).ok().and_then(|m| m.get("name").str().map(String::from)).unwrap_or_default();
                    Load::Ready(repo_of(&pda, &name).map(|(owner, name)| Repo { owner, name, commits: Load::Loading, at: 0.0, busy: true }))
                } else if ok && status == 404 {
                    Load::Ready(None)
                } else {
                    Load::Err(fetch_err(ok, status, &text))
                };
                let is_repo = matches!(v, Load::Ready(Some(_)));
                self.git.repos.insert(pda.clone(), v);
                if is_repo {
                    self.git_rows(&pda);
                }
            }
            P::GitRows(pda) => {
                if let Some(Load::Ready(Some(r))) = self.git.repos.get_mut(&pda) {
                    r.busy = false;
                    r.at = host::now_ms();
                    let got =
                        if http_ok { json::parse(&text).map(|v| parse_commits(v.get("rows").arr(), &r.owner)) } else { Err(fetch_err(ok, status, &text)) };
                    match got {
                        Ok(v) => r.commits = Load::Ready(v),
                        // keep what was shown if a refresh fails
                        Err(e) if !matches!(r.commits, Load::Ready(_)) => r.commits = Load::Err(e),
                        Err(e) => self.toast = Some((false, format!("Couldn't refresh {}: {}", r.name, e))),
                    }
                }
            }
            P::GitTree(tree) => {
                let v = if http_ok {
                    json::parse(&text)
                        .ok()
                        .and_then(|v| match v.get("data") {
                            Json::Str(d) => parse_tree(d),
                            other => parse_tree(&other.to_string()),
                        })
                        .map(Load::Ready)
                        .unwrap_or_else(|| Load::Err("That commit's file list isn't readable.".into()))
                } else {
                    Load::Err(fetch_err(ok, status, &text))
                };
                self.git.trees.insert(tree, v);
            }
            _ => return false,
        }
        true
    }
}
