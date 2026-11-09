//! The session's desk as `embarch-api`'s default project (EMB-94).
//!
//! A tool call or CLI command that names no project — or names the literal
//! `desk` — builds the firmware repo of the desk this session works on: the
//! desk holding its folder, else its work item's (embarch-desk decision 74).
//! Several sessions on several desks build their own at the same time, and
//! the desk the UI shows changes none of them.
//!
//! Claude Code gives every MCP server and shell it starts the session's id
//! (`CLAUDE_CODE_SESSION_ID`), and runs it in the session's folder. The desk
//! service resolves the desk from both. While the service is stopped, or
//! outside a session, the desk holding the folder is read from the
//! `desks.json` the service publishes beside its state, so a build never
//! waits on another service.

use anyhow::{bail, Context, Result};
use crate::config::Config;
use serde_json::Value;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// The word a project argument uses to mean "this session's desk's".
pub const DESK: &str = "desk";

/// The headers that name the calling session and its folder to the desk
/// service (embarch-desk `client::SESSION_HEADER`, `client::CWD_HEADER`).
const SESSION_HEADER: &str = "x-embarch-session";
const CWD_HEADER: &str = "x-embarch-cwd";
const DESK_DEFAULT_PORT: u16 = 4891;
const DESK_CONNECT_TIMEOUT: Duration = Duration::from_millis(500);
const DESK_READ_TIMEOUT: Duration = Duration::from_secs(2);

fn home() -> Result<PathBuf> {
    Ok(PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?))
}

/// The desk service's directory under an XDG base, as embarch-desk's
/// `paths.rs` finds it.
fn desk_dir(own: &str, xdg: &str, fallback: &[&str]) -> Result<PathBuf> {
    if let Some(d) = std::env::var_os(own) {
        return Ok(PathBuf::from(d));
    }
    let base = match std::env::var_os(xdg).filter(|s| !s.is_empty()) {
        Some(x) => PathBuf::from(x),
        None => fallback.iter().try_fold(home()?, |p, part| Ok::<_, anyhow::Error>(p.join(part)))?,
    };
    Ok(base.join("embarch-desk"))
}

fn data_dir() -> Result<PathBuf> {
    desk_dir("EMBARCH_DESK_DATA_DIR", "XDG_DATA_HOME", &[".local", "share"])
}

fn config_dir() -> Result<PathBuf> {
    desk_dir("EMBARCH_DESK_CONFIG_DIR", "XDG_CONFIG_HOME", &[".config"])
}

fn desk_port() -> u16 {
    std::env::var("EMBARCH_DESK_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(DESK_DEFAULT_PORT)
}

/// The session this process serves, if any, and the folder it runs in.
fn caller() -> (Option<String>, PathBuf) {
    let sid = std::env::var("CLAUDE_CODE_SESSION_ID").ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    (sid, std::env::current_dir().unwrap_or_default())
}

/// `GET /desks` on the running desk service as the session `sid` in `cwd`;
/// `None` when the service does not answer. HTTP/1.0 over a plain socket:
/// one local request, and no client library that refuses to block inside
/// the MCP server's runtime.
fn ask_desk(sid: &str, cwd: &Path) -> Option<Value> {
    let token = std::fs::read_to_string(config_dir().ok()?.join("desk-token")).ok()?;
    let cwd = cwd.to_str().filter(|c| !c.chars().any(char::is_control)).unwrap_or("");
    if sid.chars().any(|c| c.is_control()) {
        return None;
    }
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], desk_port()));
    let mut s = std::net::TcpStream::connect_timeout(&addr, DESK_CONNECT_TIMEOUT).ok()?;
    s.set_read_timeout(Some(DESK_READ_TIMEOUT)).ok()?;
    s.set_write_timeout(Some(DESK_READ_TIMEOUT)).ok()?;
    let req = format!(
        "GET /desks HTTP/1.0\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {}\r\n{SESSION_HEADER}: {sid}\r\n{CWD_HEADER}: {cwd}\r\n\r\n",
        token.trim()
    );
    s.write_all(req.as_bytes()).ok()?;
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).ok()?;
    let text = String::from_utf8(raw).ok()?;
    let (head, body) = text.split_once("\r\n\r\n")?;
    head.split_whitespace().nth(1).filter(|code| *code == "200")?;
    serde_json::from_str(body).ok()
}

/// `(desk name, firmware repos)` of one desk as embarch-desk serializes it.
fn named(desk: &Value) -> (String, Vec<PathBuf>) {
    let repos = desk["firmware_repos"]
        .as_array()
        .map(|r| r.iter().filter_map(|x| x.as_str()).map(PathBuf::from).collect())
        .unwrap_or_default();
    (desk["name"].as_str().unwrap_or("?").to_string(), repos)
}

/// The desk the service says this session works on. `None` when its answer
/// names none for the session at all: a desk service from before EMB-94.
fn from_service(v: &Value) -> Option<Result<(String, Vec<PathBuf>)>> {
    let session = v.get("session")?;
    let Some(id) = session.as_str() else {
        return Some(Err(anyhow::anyhow!(
            "this session is on no desk: its folder is in no desk's repos and its work item has no desk — name a project"
        )));
    };
    let desk = v["desks"].as_array().into_iter().flatten().find(|d| d["id"].as_str() == Some(id));
    Some(desk.map(named).ok_or_else(|| anyhow::anyhow!("the desk service named desk {id:?} for this session but listed no such desk")))
}

/// The desk among `desks` (`desks.json`'s shape) whose firmware repos hold
/// `cwd`, as embarch-desk's own folder match finds it.
fn by_folder(desks: &Value, cwd: &Path) -> Result<(String, Vec<PathBuf>)> {
    let holds = |d: &&Value| {
        d["firmware_repos"].as_array().into_iter().flatten().filter_map(Value::as_str).any(|r| {
            let r = r.trim_end_matches('/');
            !r.is_empty() && cwd.starts_with(r)
        })
    };
    match desks["desks"].as_array().into_iter().flatten().find(holds) {
        Some(d) => Ok(named(d)),
        None => bail!("{} is in no desk's repos — name a project, or work in a desk's repo", cwd.display()),
    }
}

/// `(desk name, firmware repos)` of the desk this session works on, or why
/// there is none.
fn session_desk() -> Result<(String, Vec<PathBuf>)> {
    let (sid, cwd) = caller();
    if let Some(answer) = sid.as_deref().and_then(|s| ask_desk(s, &cwd)).as_ref().and_then(from_service) {
        return answer;
    }
    let path = data_dir()?.join("desks.json");
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("the desk service is not answering and no desks are published at {} — is embarch-desk installed?", path.display()))?;
    let v: Value = serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    by_folder(&v, &cwd)
}

fn same_tree(a: &Path, b: &Path) -> bool {
    let a = std::fs::canonicalize(a).unwrap_or_else(|_| a.to_path_buf());
    let b = std::fs::canonicalize(b).unwrap_or_else(|_| b.to_path_buf());
    a == b || a.starts_with(&b) || b.starts_with(&a)
}

/// A firmware repo's own `embarch/embarch.toml` (what `embarch init`
/// writes), loaded once per process and kept: the server's own config is one
/// repo's, and a desk is often another's. Leaked on purpose — a handful of
/// desks at most, held for the life of the process, so a project borrowed
/// from one lives as long as any caller needs it.
fn repo_config(repo: &Path) -> Option<&'static Config> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, &'static Config>>> = OnceLock::new();
    let path = repo.join("embarch").join("embarch.toml");
    let mut cache = CACHE.get_or_init(Default::default).lock().ok()?;
    if let Some(c) = cache.get(&path) {
        return Some(c);
    }
    let config: &'static Config = Box::leak(Box::new(Config::load_from_path(&path).ok()?));
    cache.insert(path, config);
    Some(config)
}

/// The project a name means, and the config it lives in (`None` = the
/// server's own). A named project is itself. An empty name or `desk` is this
/// session's desk's: a project in the server's config whose `source_path` is the
/// desk's firmware repo, else one in that repo's own `embarch/embarch.toml`.
/// Several candidates is an error naming them, never a guess.
pub fn resolve(config: &Config, name: &str) -> Result<(Option<&'static Config>, String)> {
    let name = name.trim();
    if !name.is_empty() && name != DESK {
        return Ok((None, name.to_string()));
    }
    let (desk, repos) = session_desk()?;
    if let Some(n) = resolve_in(config, &desk, &repos)? {
        return Ok((None, n));
    }
    for repo in &repos {
        if let Some(own) = repo_config(repo) {
            if let Some(n) = resolve_in(own, &desk, std::slice::from_ref(repo))? {
                return Ok((Some(own), n));
            }
        }
    }
    let known: Vec<String> = config.projects.iter().map(|p| format!("{} ({})", p.name, p.source_path.display())).collect();
    bail!(
        "this session's desk {desk:?} has firmware repo {}, which is not a project in embarch-api's config ({}), and it has no \
         embarch/embarch.toml of its own — run `embarch init` there, or name a project",
        repos.first().map(|r| r.display().to_string()).unwrap_or_default(),
        known.join(", ")
    )
}

/// The one project of `config` on the desk's repos; `None` when there is none.
fn resolve_in(config: &Config, desk: &str, repos: &[PathBuf]) -> Result<Option<String>> {
    if repos.is_empty() {
        bail!("this session's desk {desk:?} names no firmware repo");
    }
    for repo in repos {
        let hits: Vec<&str> = config
            .projects
            .iter()
            .filter(|p| same_tree(&p.source_path, repo))
            .map(|p| p.name.as_str())
            .collect();
        match hits.as_slice() {
            [] => continue,
            [one] => return Ok(Some(one.to_string())),
            many => bail!(
                "this session's desk {desk:?} has several projects in its repo {} — name one: {}",
                repo.display(),
                many.join(", ")
            ),
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_named_project_is_never_redirected() {
        let config: Config = toml::from_str("[core]\nbase_url = \"http://127.0.0.1:1\"\ntoken = \"x\"\n").unwrap_or_else(|_| panic!("minimal config"));
        assert_eq!(resolve(&config, "fw").unwrap().1, "fw");
    }

    #[test]
    fn the_desks_repo_finds_its_project_and_an_unknown_repo_says_so() {
        let dir = std::env::temp_dir().join(format!("api-desk-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("app")).unwrap();
        let toml_text = format!(
            "[core]\nbase_url = \"http://127.0.0.1:1\"\ntoken = \"x\"\n\n[[projects]]\nname = \"fw\"\nsource_path = \"{}\"\nchip = \"x\"\nflash_format = \"hex\"\nbuild_command = [\"true\"]\nartifact_path = \"a.hex\"\n",
            dir.display()
        );
        let config: Config = toml::from_str(&toml_text).unwrap();
        assert_eq!(resolve_in(&config, "D", std::slice::from_ref(&dir)).unwrap().as_deref(), Some("fw"));
        assert_eq!(resolve_in(&config, "D", &[dir.join("app")]).unwrap().as_deref(), Some("fw"));
        assert_eq!(resolve_in(&config, "D", &[PathBuf::from("/nowhere/else")]).unwrap(), None);
        let two = format!("{toml_text}\n[[projects]]\nname = \"fw2\"\nsource_path = \"{}\"\nchip = \"x\"\nflash_format = \"hex\"\nbuild_command = [\"true\"]\nartifact_path = \"a.hex\"\n", dir.display());
        let config2: Config = toml::from_str(&two).unwrap();
        let err = resolve_in(&config2, "D", std::slice::from_ref(&dir)).unwrap_err().to_string();
        assert!(err.contains("several projects") && err.contains("fw2"), "{err}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn desks() -> Value {
        serde_json::json!({ "desks": [
            { "id": "k1", "name": "Alpha", "firmware_repos": ["/w/alpha/"] },
            { "id": "k2", "name": "Beta", "firmware_repos": ["/w/beta", "/w/beta-tools"] },
        ] })
    }

    #[test]
    fn the_service_names_the_sessions_desk_or_says_it_is_on_none() {
        let mut v = desks();
        assert!(from_service(&v).is_none(), "a desk service from before EMB-94 names no session desk");
        v["session"] = "k2".into();
        assert_eq!(from_service(&v).unwrap().unwrap().0, "Beta");
        v["session"] = Value::Null;
        let err = from_service(&v).unwrap().unwrap_err().to_string();
        assert!(err.contains("on no desk") && err.contains("name a project"), "{err}");
        v["session"] = "k9".into();
        assert!(from_service(&v).unwrap().is_err());
    }

    #[test]
    fn without_the_service_the_desk_holding_the_folder_is_the_sessions() {
        let v = desks();
        assert_eq!(by_folder(&v, Path::new("/w/alpha/app")).unwrap().0, "Alpha");
        assert_eq!(by_folder(&v, Path::new("/w/beta-tools")).unwrap().0, "Beta");
        assert!(by_folder(&v, Path::new("/w/alphabet")).is_err(), "a path prefix is not a folder");
        let err = by_folder(&v, Path::new("/tmp/x")).unwrap_err().to_string();
        assert!(err.contains("in no desk's repos"), "{err}");
    }
}
