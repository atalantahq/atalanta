// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

//! `atalanta mcp install` / `atalanta mcp config`: register Atalanta's MCP
//! server with an agent client, and teach the agent the Atalanta workflow.
//!
//! Per client, the global MCP config is merged (other servers are kept, the
//! file is backed up to `<file>.bak` before it changes, and a second run
//! changes nothing), and the agent rules (`assets/atalanta-agent-rules.md`)
//! are installed where that client reads global instructions:
//!
//! | Client | MCP config | Rules |
//! |---|---|---|
//! | Claude Code | `claude mcp add --scope user` (else `~/.claude.json` `mcpServers`) | skill `~/.claude/skills/atalanta/SKILL.md` |
//! | Cursor | `~/.cursor/mcp.json` `mcpServers` | none global: printed guidance (User Rules live in Cursor's settings) |
//! | Codex | `~/.codex/config.toml` `[mcp_servers.atalanta]` | skill `~/.agents/skills/atalanta/SKILL.md` |
//! | Windsurf | `~/.codeium/windsurf/mcp_config.json` `mcpServers` | a marked block in `~/.codeium/windsurf/memories/global_rules.md` |
//! | VS Code | user `mcp.json` `servers` (`type: stdio`) | none global: printed guidance |
//! | Cline | `…/globalStorage/saoudrizwan.claude-dev/settings/cline_mcp_settings.json` `mcpServers` (and `~/.cline/data/settings/` for the CLI) | `~/Documents/Cline/Rules/atalanta.md` |
//! | Gemini CLI | `~/.gemini/settings.json` `mcpServers` | skill `~/.gemini/skills/atalanta/SKILL.md` |
//!
//! Everything is relative to an [`InstallEnv`], whose `home` tests point at
//! a temporary directory.

use std::path::{Path, PathBuf};

use serde_json::Value;

/// The agent rules, one source for every client.
pub const AGENT_RULES: &str = include_str!("../assets/atalanta-agent-rules.md");

/// The name the server is registered under.
pub const SERVER_NAME: &str = "atalanta";

const BLOCK_BEGIN: &str =
    "<!-- atalanta:begin (managed by `atalanta mcp install`; edits here are replaced) -->";
const BLOCK_END: &str = "<!-- atalanta:end -->";

/// An agent client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Client {
    Claude,
    Cursor,
    Codex,
    Windsurf,
    Vscode,
    Cline,
    Gemini,
}

impl Client {
    pub const ALL: [Client; 7] = [
        Client::Claude,
        Client::Cursor,
        Client::Codex,
        Client::Windsurf,
        Client::Vscode,
        Client::Cline,
        Client::Gemini,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Cursor => "cursor",
            Self::Codex => "codex",
            Self::Windsurf => "windsurf",
            Self::Vscode => "vscode",
            Self::Cline => "cline",
            Self::Gemini => "gemini",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim().to_ascii_lowercase();
        let s = match s.as_str() {
            "claude-code" | "claudecode" => "claude",
            "code" | "vs-code" | "copilot" => "vscode",
            "gemini-cli" => "gemini",
            other => other,
        };
        Self::ALL.into_iter().find(|c| c.name() == s)
    }
}

/// Where and what to install.
#[derive(Debug, Clone)]
pub struct InstallEnv {
    pub home: PathBuf,
    /// `macos`, `linux`, `windows`.
    pub os: &'static str,
    /// `%APPDATA%` (Windows).
    pub appdata: Option<PathBuf>,
    /// The `atalanta` binary the clients will start.
    pub atalanta: PathBuf,
    /// Arguments after it (`mcp`, plus any server flags).
    pub args: Vec<String>,
    /// The `claude` CLI, when installed.
    pub claude_cli: Option<PathBuf>,
}

impl InstallEnv {
    /// This machine: `$HOME`, this binary, `claude` from PATH.
    pub fn current(args: Vec<String>) -> std::io::Result<Self> {
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from)
            .ok_or_else(|| std::io::Error::other("HOME is not set"))?;
        let exe = std::env::current_exe()?;
        let atalanta = std::fs::canonicalize(&exe).unwrap_or(exe);
        Ok(Self {
            home,
            os: std::env::consts::OS,
            appdata: std::env::var_os("APPDATA").map(PathBuf::from),
            atalanta,
            args,
            claude_cli: find_on_path("claude"),
        })
    }

    /// VS Code's user directory.
    fn vscode_user(&self) -> PathBuf {
        match self.os {
            "macos" => self.home.join("Library/Application Support/Code/User"),
            "windows" => self
                .appdata
                .clone()
                .unwrap_or_else(|| self.home.join("AppData/Roaming"))
                .join("Code/User"),
            _ => self.home.join(".config/Code/User"),
        }
    }

    fn command(&self) -> String {
        self.atalanta.to_string_lossy().into_owned()
    }
}

/// `program` on PATH.
fn find_on_path(program: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|d| d.join(program))
            .find(|p| p.is_file())
    })
}

/// What to do.
#[derive(Debug, Clone, Copy, Default)]
pub struct Options {
    pub dry_run: bool,
    pub uninstall: bool,
}

/// What happened (or, in a dry run, would).
#[derive(Debug, Clone, PartialEq)]
pub struct Change {
    pub client: Client,
    pub what: String,
}

/// Where a client's MCP servers are configured.
enum McpTarget {
    /// A JSON file, servers under `key`, each entry `entry`.
    Json {
        path: PathBuf,
        key: &'static str,
        entry: Value,
    },
    /// Codex's TOML.
    CodexToml(PathBuf),
    /// `claude mcp add` (with the JSON file as the fallback).
    ClaudeCli(PathBuf),
}

/// Where a client's rules go.
enum RulesTarget {
    /// A file of our own (a skill or a rules file); `frontmatter`: keep the
    /// skill frontmatter.
    File { path: PathBuf, frontmatter: bool },
    /// A marked block inside a file the user shares with other rules.
    Block(PathBuf),
    /// No global place: tell the user.
    Guidance(String),
}

/// The rules without the skill frontmatter.
fn rules_body() -> &'static str {
    AGENT_RULES
        .strip_prefix("---\n")
        .and_then(|rest| rest.split_once("\n---\n"))
        .map(|(_, body)| body.trim_start_matches('\n'))
        .unwrap_or(AGENT_RULES)
}

fn stdio_entry(env: &InstallEnv, with_type: bool) -> Value {
    let mut v = serde_json::json!({ "command": env.command(), "args": env.args });
    if with_type {
        v["type"] = "stdio".into();
    }
    v
}

fn mcp_targets(env: &InstallEnv, client: Client) -> Vec<McpTarget> {
    let h = &env.home;
    let json = |path: PathBuf, key, entry| McpTarget::Json { path, key, entry };
    match client {
        Client::Claude => {
            let file = h.join(".claude.json");
            if env.claude_cli.is_some() {
                vec![McpTarget::ClaudeCli(file)]
            } else {
                vec![json(file, "mcpServers", stdio_entry(env, true))]
            }
        }
        Client::Cursor => vec![json(
            h.join(".cursor/mcp.json"),
            "mcpServers",
            stdio_entry(env, false),
        )],
        Client::Codex => vec![McpTarget::CodexToml(h.join(".codex/config.toml"))],
        Client::Windsurf => vec![json(
            h.join(".codeium/windsurf/mcp_config.json"),
            "mcpServers",
            stdio_entry(env, false),
        )],
        Client::Vscode => vec![json(
            env.vscode_user().join("mcp.json"),
            "servers",
            stdio_entry(env, true),
        )],
        Client::Cline => {
            let mut entry = stdio_entry(env, false);
            entry["disabled"] = false.into();
            let mut out = vec![json(
                env.vscode_user()
                    .join("globalStorage/saoudrizwan.claude-dev/settings/cline_mcp_settings.json"),
                "mcpServers",
                entry.clone(),
            )];
            // The Cline CLI keeps its own settings.
            if h.join(".cline").is_dir() {
                out.push(json(
                    h.join(".cline/data/settings/cline_mcp_settings.json"),
                    "mcpServers",
                    entry,
                ));
            }
            out
        }
        Client::Gemini => vec![json(
            h.join(".gemini/settings.json"),
            "mcpServers",
            stdio_entry(env, false),
        )],
    }
}

fn rules_target(env: &InstallEnv, client: Client) -> RulesTarget {
    let h = &env.home;
    let skill = |dir: PathBuf| RulesTarget::File {
        path: dir.join("atalanta").join("SKILL.md"),
        frontmatter: true,
    };
    match client {
        Client::Claude => skill(h.join(".claude/skills")),
        Client::Codex => skill(h.join(".agents/skills")),
        Client::Gemini => skill(h.join(".gemini/skills")),
        Client::Windsurf => RulesTarget::Block(h.join(".codeium/windsurf/memories/global_rules.md")),
        Client::Cline => RulesTarget::File {
            path: h.join("Documents/Cline/Rules/atalanta.md"),
            frontmatter: false,
        },
        Client::Cursor => RulesTarget::Guidance(
            "Cursor has no global rules file: paste the rules (`atalanta mcp rules`) into Cursor Settings > Rules > User Rules, or save them as .cursor/rules/atalanta.mdc in a project".into(),
        ),
        Client::Vscode => RulesTarget::Guidance(
            "VS Code has no global rules file here: save the rules (`atalanta mcp rules`) as .github/copilot-instructions.md (or a *.instructions.md) in a project".into(),
        ),
    }
}

/// Whether `client` looks installed on this machine (for `all`).
pub fn detected(env: &InstallEnv, client: Client) -> bool {
    let h = &env.home;
    match client {
        Client::Claude => {
            env.claude_cli.is_some()
                || h.join(".claude").is_dir()
                || h.join(".claude.json").is_file()
        }
        Client::Cursor => h.join(".cursor").is_dir(),
        Client::Codex => h.join(".codex").is_dir(),
        Client::Windsurf => h.join(".codeium/windsurf").is_dir(),
        Client::Vscode => env.vscode_user().is_dir(),
        Client::Cline => {
            env.vscode_user()
                .join("globalStorage/saoudrizwan.claude-dev")
                .is_dir()
                || h.join(".cline").is_dir()
        }
        Client::Gemini => h.join(".gemini").is_dir(),
    }
}

/// The config snippet for `client`, to paste by hand.
pub fn config_snippet(env: &InstallEnv, client: Client) -> String {
    let mut out = Vec::new();
    for t in mcp_targets(env, client) {
        match t {
            McpTarget::Json { path, key, entry } => {
                let doc = serde_json::json!({ key: { SERVER_NAME: entry } });
                out.push(format!(
                    "// {}\n{}",
                    path.display(),
                    serde_json::to_string_pretty(&doc).unwrap_or_default()
                ));
            }
            McpTarget::CodexToml(path) => {
                let mut doc = toml_edit::DocumentMut::new();
                set_codex(&mut doc, env);
                out.push(format!("# {}\n{doc}", path.display()));
            }
            McpTarget::ClaudeCli(path) => {
                out.push(format!(
                    "{}\n\n// or in {}:\n{}",
                    claude_add_line(env),
                    path.display(),
                    serde_json::to_string_pretty(&serde_json::json!({
                        "mcpServers": { SERVER_NAME: stdio_entry(env, true) }
                    }))
                    .unwrap_or_default()
                ));
            }
        }
    }
    out.join("\n\n")
}

fn claude_add_line(env: &InstallEnv) -> String {
    let mut parts = vec![
        "claude".to_string(),
        "mcp".into(),
        "add".into(),
        "--scope".into(),
        "user".into(),
        SERVER_NAME.into(),
        "--".into(),
        env.command(),
    ];
    parts.extend(env.args.iter().cloned());
    parts
        .iter()
        .map(|p| shell_quote(p))
        .collect::<Vec<_>>()
        .join(" ")
}

/// `p` quoted for a POSIX shell when it needs it (display only).
fn shell_quote(p: &str) -> String {
    if !p.is_empty()
        && p.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./=:@".contains(c))
    {
        p.to_string()
    } else {
        format!("'{}'", p.replace('\'', r"'\''"))
    }
}

/// Install (or uninstall) Atalanta for `client`.
pub fn install(env: &InstallEnv, client: Client, opts: Options) -> Result<Vec<Change>, String> {
    let mut changes = Vec::new();
    let mut note = |what: String| changes.push(Change { client, what });
    for t in mcp_targets(env, client) {
        match t {
            McpTarget::Json { path, key, entry } => note(merge_json(&path, key, &entry, opts)?),
            McpTarget::CodexToml(path) => note(merge_codex(&path, env, opts)?),
            McpTarget::ClaudeCli(file) => note(claude_cli(env, &file, opts)?),
        }
    }
    match rules_target(env, client) {
        RulesTarget::File { path, frontmatter } => {
            let text = if frontmatter {
                AGENT_RULES
            } else {
                rules_body()
            };
            note(write_own_file(&path, text, opts)?);
        }
        RulesTarget::Block(path) => note(write_block(&path, rules_body(), opts)?),
        RulesTarget::Guidance(g) if !opts.uninstall => note(format!("rules: {g}")),
        RulesTarget::Guidance(_) => {}
    }
    Ok(changes)
}

fn verb(opts: Options, done: &str, would: &str) -> String {
    if opts.dry_run {
        format!("would {would}")
    } else {
        done.to_string()
    }
}

/// Copy `path` to `path.bak` before changing it.
fn backup(path: &Path) -> Result<(), String> {
    if path.is_file() {
        let mut bak = path.as_os_str().to_owned();
        bak.push(".bak");
        std::fs::copy(path, PathBuf::from(bak))
            .map_err(|e| format!("backing up {}: {e}", path.display()))?;
    }
    Ok(())
}

fn write(path: &Path, text: &str) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    atalanta_common::fsutil::write_atomic(path, text.as_bytes())
        .map_err(|e| format!("writing {}: {e}", path.display()))
}

fn merge_json(path: &Path, key: &str, entry: &Value, opts: Options) -> Result<String, String> {
    let shown = path.display();
    let text = match std::fs::read_to_string(path) {
        Ok(t) => Some(t),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(format!("reading {shown}: {e}")),
    };
    let mut doc: Value = match text.as_deref().map(str::trim) {
        None | Some("") => serde_json::json!({}),
        Some(t) => serde_json::from_str(t).map_err(|e| {
            format!(
                "{shown} is not plain JSON ({e}; comments or trailing commas?): left unchanged. \
                 Add this under \"{key}\" by hand:\n\"{SERVER_NAME}\": {}",
                serde_json::to_string_pretty(entry).unwrap_or_default()
            )
        })?,
    };
    let Some(root) = doc.as_object_mut() else {
        return Err(format!(
            "{shown}: the top level is not an object; left unchanged"
        ));
    };
    let servers = root
        .entry(key.to_string())
        .or_insert_with(|| serde_json::json!({}));
    let Some(servers) = servers.as_object_mut() else {
        return Err(format!(
            "{shown}: \"{key}\" is not an object; left unchanged"
        ));
    };
    if opts.uninstall {
        if servers.remove(SERVER_NAME).is_none() {
            return Ok(format!("{shown}: not installed"));
        }
    } else {
        if servers.get(SERVER_NAME) == Some(entry) {
            return Ok(format!("{shown}: already up to date"));
        }
        servers.insert(SERVER_NAME.to_string(), entry.clone());
    }
    let out = serde_json::to_string_pretty(&doc).map_err(|e| e.to_string())? + "\n";
    let what = if opts.uninstall {
        verb(opts, "removed the server from", "remove the server from")
    } else {
        verb(opts, "registered the server in", "register the server in")
    };
    if !opts.dry_run {
        backup(path)?;
        write(path, &out)?;
    }
    Ok(format!("{what} {shown} (\"{key}\".{SERVER_NAME})"))
}

fn set_codex(doc: &mut toml_edit::DocumentMut, env: &InstallEnv) {
    let servers = doc.entry("mcp_servers").or_insert_with(|| {
        let mut t = toml_edit::Table::new();
        t.set_implicit(true);
        toml_edit::Item::Table(t)
    });
    let mut table = toml_edit::Table::new();
    table["command"] = toml_edit::value(env.command());
    let mut args = toml_edit::Array::new();
    for a in &env.args {
        args.push(a.as_str());
    }
    table["args"] = toml_edit::value(args);
    servers[SERVER_NAME] = toml_edit::Item::Table(table);
}

fn codex_matches(doc: &toml_edit::DocumentMut, env: &InstallEnv) -> bool {
    let Some(t) = doc
        .get("mcp_servers")
        .and_then(|s| s.get(SERVER_NAME))
        .and_then(|v| v.as_table_like())
    else {
        return false;
    };
    let command = t.get("command").and_then(|c| c.as_str());
    let args: Option<Vec<&str>> = t
        .get("args")
        .and_then(|a| a.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect());
    command == Some(env.command().as_str())
        && args == Some(env.args.iter().map(String::as_str).collect())
}

fn merge_codex(path: &Path, env: &InstallEnv, opts: Options) -> Result<String, String> {
    let shown = path.display();
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("reading {shown}: {e}")),
    };
    let mut doc: toml_edit::DocumentMut = text
        .parse()
        .map_err(|e| format!("{shown} is not valid TOML ({e}): left unchanged"))?;
    if opts.uninstall {
        let removed = doc
            .get_mut("mcp_servers")
            .and_then(|s| s.as_table_like_mut())
            .and_then(|s| s.remove(SERVER_NAME))
            .is_some();
        if !removed {
            return Ok(format!("{shown}: not installed"));
        }
    } else {
        if codex_matches(&doc, env) {
            return Ok(format!("{shown}: already up to date"));
        }
        set_codex(&mut doc, env);
    }
    let what = if opts.uninstall {
        verb(opts, "removed the server from", "remove the server from")
    } else {
        verb(opts, "registered the server in", "register the server in")
    };
    if !opts.dry_run {
        backup(path)?;
        write(path, &doc.to_string())?;
    }
    Ok(format!("{what} {shown} ([mcp_servers.{SERVER_NAME}])"))
}

/// `claude mcp …` with `args`, as argv; its output, or why it failed.
fn run_claude(env: &InstallEnv, args: &[String]) -> Result<String, String> {
    let cli = env.claude_cli.as_ref().ok_or("no claude CLI")?;
    let out = std::process::Command::new(cli)
        .args(args)
        .env("HOME", &env.home)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("{}: {e}", cli.display()))?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    if out.status.success() {
        Ok(text)
    } else {
        Err(text.trim().to_string())
    }
}

fn claude_cli(env: &InstallEnv, file: &Path, opts: Options) -> Result<String, String> {
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    let current = run_claude(env, &s(&["mcp", "get", SERVER_NAME])).ok();
    let installed_here = current.as_deref().is_some_and(|out| {
        out.contains(&env.command()) && env.args.iter().all(|a| out.contains(a.as_str()))
    });
    if opts.uninstall {
        if current.is_none() {
            return Ok("claude: not installed".into());
        }
        if opts.dry_run {
            return Ok(format!(
                "would run: claude mcp remove --scope user {SERVER_NAME}"
            ));
        }
        run_claude(env, &s(&["mcp", "remove", "--scope", "user", SERVER_NAME]))
            .map_err(|e| format!("claude mcp remove: {e}"))?;
        return Ok(format!("ran claude mcp remove --scope user {SERVER_NAME}"));
    }
    if installed_here {
        return Ok("claude: already up to date (claude mcp get atalanta)".into());
    }
    if opts.dry_run {
        return Ok(format!("would run: {}", claude_add_line(env)));
    }
    if current.is_some() {
        let _ = run_claude(env, &s(&["mcp", "remove", "--scope", "user", SERVER_NAME]));
    }
    let mut add = s(&["mcp", "add", "--scope", "user", SERVER_NAME, "--"]);
    add.push(env.command());
    add.extend(env.args.iter().cloned());
    match run_claude(env, &add) {
        Ok(_) => Ok(format!("ran {}", claude_add_line(env))),
        // The CLI failed: edit its config file instead.
        Err(e) => merge_json(file, "mcpServers", &stdio_entry(env, true), opts)
            .map(|done| format!("{done} (claude mcp add failed: {e})")),
    }
}

fn write_own_file(path: &Path, text: &str, opts: Options) -> Result<String, String> {
    let shown = path.display();
    let existing = std::fs::read_to_string(path).ok();
    if opts.uninstall {
        if existing.is_none() {
            return Ok(format!("{shown}: not installed"));
        }
        if !opts.dry_run {
            std::fs::remove_file(path).map_err(|e| format!("{shown}: {e}"))?;
            // The skill's own folder, when now empty.
            if path.file_name().is_some_and(|n| n == "SKILL.md") {
                if let Some(dir) = path.parent() {
                    let _ = std::fs::remove_dir(dir);
                }
            }
        }
        return Ok(format!("{} {shown}", verb(opts, "removed", "remove")));
    }
    if existing.as_deref() == Some(text) {
        return Ok(format!("{shown}: already up to date"));
    }
    if !opts.dry_run {
        backup(path)?;
        write(path, text)?;
    }
    Ok(format!(
        "{} {shown}",
        verb(opts, "wrote the agent rules to", "write the agent rules to")
    ))
}

/// `text` between the markers, replacing an earlier block or appended.
fn with_block(existing: &str, text: Option<&str>) -> String {
    let block = text.map(|t| format!("{BLOCK_BEGIN}\n{}\n{BLOCK_END}\n", t.trim_end()));
    if let (Some(start), Some(end)) = (existing.find(BLOCK_BEGIN), existing.find(BLOCK_END)) {
        if start < end {
            let after = &existing[end + BLOCK_END.len()..];
            let after = after.strip_prefix('\n').unwrap_or(after);
            return format!("{}{}{after}", &existing[..start], block.unwrap_or_default());
        }
    }
    match block {
        None => existing.to_string(),
        Some(b) if existing.trim().is_empty() => b,
        Some(b) => format!("{}\n\n{b}", existing.trim_end()),
    }
}

fn write_block(path: &Path, text: &str, opts: Options) -> Result<String, String> {
    let shown = path.display();
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    let new = with_block(&existing, (!opts.uninstall).then_some(text));
    if new == existing {
        return Ok(format!(
            "{shown}: {}",
            if opts.uninstall {
                "not installed"
            } else {
                "already up to date"
            }
        ));
    }
    if !opts.dry_run {
        backup(path)?;
        write(path, &new)?;
    }
    Ok(format!(
        "{} {shown}",
        if opts.uninstall {
            verb(
                opts,
                "removed the agent rules block from",
                "remove the agent rules block from",
            )
        } else {
            verb(
                opts,
                "wrote the agent rules block to",
                "write the agent rules block to",
            )
        }
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(home: &Path) -> InstallEnv {
        InstallEnv {
            home: home.to_path_buf(),
            os: "macos",
            appdata: None,
            atalanta: PathBuf::from("/opt/atalanta/bin/atalanta"),
            args: vec!["mcp".into()],
            claude_cli: None,
        }
    }

    fn run(e: &InstallEnv, c: Client, opts: Options) -> Vec<String> {
        install(e, c, opts)
            .unwrap_or_else(|err| panic!("{err}"))
            .into_iter()
            .map(|c| c.what)
            .collect()
    }

    #[test]
    fn json_clients_merge_back_up_and_are_idempotent() {
        let home = tempfile::tempdir().unwrap();
        let e = env(home.path());
        let cursor = home.path().join(".cursor/mcp.json");
        std::fs::create_dir_all(cursor.parent().unwrap()).unwrap();
        let other = r#"{"mcpServers":{"github":{"command":"gh-mcp"}},"theme":"dark"}"#;
        std::fs::write(&cursor, other).unwrap();

        let dry = run(
            &e,
            Client::Cursor,
            Options {
                dry_run: true,
                ..Default::default()
            },
        );
        assert!(dry[0].starts_with("would register"), "{dry:?}");
        assert_eq!(
            std::fs::read_to_string(&cursor).unwrap(),
            other,
            "dry run wrote nothing"
        );

        run(&e, Client::Cursor, Options::default());
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&cursor).unwrap()).unwrap();
        assert_eq!(v["mcpServers"]["github"]["command"], "gh-mcp", "kept");
        assert_eq!(v["theme"], "dark");
        assert_eq!(
            v["mcpServers"]["atalanta"]["command"],
            "/opt/atalanta/bin/atalanta"
        );
        assert_eq!(
            v["mcpServers"]["atalanta"]["args"],
            serde_json::json!(["mcp"])
        );
        assert_eq!(
            std::fs::read_to_string(home.path().join(".cursor/mcp.json.bak")).unwrap(),
            other
        );

        let again = run(&e, Client::Cursor, Options::default());
        assert!(again[0].contains("already up to date"), "{again:?}");
        assert!(
            again[1].starts_with("rules: Cursor has no global rules file"),
            "{again:?}"
        );

        run(
            &e,
            Client::Cursor,
            Options {
                uninstall: true,
                ..Default::default()
            },
        );
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&cursor).unwrap()).unwrap();
        assert!(v["mcpServers"].get("atalanta").is_none());
        assert_eq!(v["mcpServers"]["github"]["command"], "gh-mcp");
    }

    #[test]
    fn json_with_comments_is_left_alone() {
        let home = tempfile::tempdir().unwrap();
        let e = env(home.path());
        let path = home
            .path()
            .join("Library/Application Support/Code/User/mcp.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let jsonc = "{\n  // mine\n  \"servers\": {}\n}\n";
        std::fs::write(&path, jsonc).unwrap();
        let err = install(&e, Client::Vscode, Options::default()).unwrap_err();
        assert!(
            err.contains("not plain JSON") && err.contains("\"atalanta\""),
            "{err}"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), jsonc);
    }

    #[test]
    fn vscode_uses_servers_with_a_type() {
        let home = tempfile::tempdir().unwrap();
        let e = InstallEnv {
            os: "linux",
            ..env(home.path())
        };
        run(&e, Client::Vscode, Options::default());
        let v: Value = serde_json::from_str(
            &std::fs::read_to_string(home.path().join(".config/Code/User/mcp.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(v["servers"]["atalanta"]["type"], "stdio");
    }

    #[test]
    fn codex_toml_is_merged_keeping_the_rest() {
        let home = tempfile::tempdir().unwrap();
        let e = env(home.path());
        let path = home.path().join(".codex/config.toml");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "model = \"o4\" # mine\n\n[mcp_servers.other]\ncommand = \"x\"\n",
        )
        .unwrap();
        let out = run(&e, Client::Codex, Options::default());
        assert!(
            out[1].contains(".agents/skills/atalanta/SKILL.md"),
            "{out:?}"
        );
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("model = \"o4\" # mine"), "{text}");
        assert!(text.contains("[mcp_servers.other]"), "{text}");
        let doc: toml_edit::DocumentMut = text.parse().unwrap();
        assert_eq!(
            doc["mcp_servers"]["atalanta"]["command"].as_str(),
            Some("/opt/atalanta/bin/atalanta")
        );
        assert!(run(&e, Client::Codex, Options::default())[0].contains("already up to date"));
        run(
            &e,
            Client::Codex,
            Options {
                uninstall: true,
                ..Default::default()
            },
        );
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            !text.contains("atalanta") && text.contains("[mcp_servers.other]"),
            "{text}"
        );
        assert!(!home.path().join(".agents/skills/atalanta").exists());
    }

    #[test]
    fn claude_gets_a_skill_and_uses_its_cli_when_present() {
        let home = tempfile::tempdir().unwrap();
        let mut e = env(home.path());
        // Without the CLI: ~/.claude.json.
        run(&e, Client::Claude, Options::default());
        let v: Value = serde_json::from_str(
            &std::fs::read_to_string(home.path().join(".claude.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(v["mcpServers"]["atalanta"]["type"], "stdio");
        let skill =
            std::fs::read_to_string(home.path().join(".claude/skills/atalanta/SKILL.md")).unwrap();
        assert!(
            skill.starts_with("---\nname: atalanta\ndescription: "),
            "{skill}"
        );

        #[cfg(unix)]
        {
            // A stand-in `claude` that logs its argv (under the HOME it is
            // given) and says nothing is installed.
            let cli = crate::test_util::script(
                "fake-claude",
                "#!/bin/sh\necho \"$@\" >> \"$HOME/claude.log\"\n[ \"$2\" = get ] && exit 1\nexit 0\n",
            );
            let log = home.path().join("claude.log");
            e.claude_cli = Some(cli);
            let out = run(&e, Client::Claude, Options::default());
            assert!(
                out[0].starts_with(
                    "ran claude mcp add --scope user atalanta -- /opt/atalanta/bin/atalanta mcp"
                ),
                "{out:?}"
            );
            let calls = std::fs::read_to_string(&log).unwrap();
            assert!(
                calls.contains("mcp add --scope user atalanta -- /opt/atalanta/bin/atalanta mcp"),
                "{calls}"
            );
        }
    }

    #[test]
    fn windsurf_rules_are_a_replaceable_block_in_the_shared_file() {
        let home = tempfile::tempdir().unwrap();
        let e = env(home.path());
        let rules = home
            .path()
            .join(".codeium/windsurf/memories/global_rules.md");
        std::fs::create_dir_all(rules.parent().unwrap()).unwrap();
        std::fs::write(&rules, "Always use tabs.\n").unwrap();
        run(&e, Client::Windsurf, Options::default());
        run(&e, Client::Windsurf, Options::default());
        let text = std::fs::read_to_string(&rules).unwrap();
        assert!(text.starts_with("Always use tabs.\n"), "{text}");
        assert_eq!(text.matches(BLOCK_BEGIN).count(), 1, "{text}");
        assert!(
            !text.contains("name: atalanta"),
            "no skill frontmatter: {text}"
        );
        run(
            &e,
            Client::Windsurf,
            Options {
                uninstall: true,
                ..Default::default()
            },
        );
        assert_eq!(
            std::fs::read_to_string(&rules).unwrap().trim(),
            "Always use tabs."
        );
    }

    #[test]
    fn every_client_installs_and_uninstalls_into_a_fresh_home() {
        let home = tempfile::tempdir().unwrap();
        let e = env(home.path());
        for c in Client::ALL {
            run(&e, c, Options::default());
            assert!(!config_snippet(&e, c).is_empty());
        }
        for c in Client::ALL {
            let again = run(&e, c, Options::default());
            assert!(
                again
                    .iter()
                    .all(|l| l.contains("already up to date") || l.starts_with("rules:")),
                "{c:?}: {again:?}"
            );
        }
        for c in Client::ALL {
            run(
                &e,
                c,
                Options {
                    uninstall: true,
                    ..Default::default()
                },
            );
        }
        assert!(!home.path().join(".gemini/skills/atalanta").exists());
        assert!(!home
            .path()
            .join("Documents/Cline/Rules/atalanta.md")
            .exists());
    }

    #[test]
    fn the_rules_teach_the_workflow_and_their_yaml_is_a_valid_suite() {
        for needle in [
            "export_session_flow",
            "atalanta run",
            "describe_failure",
            "${",
            "snapshot",
        ] {
            assert!(AGENT_RULES.contains(needle), "{needle}");
        }
        let yaml = AGENT_RULES
            .split("```yaml\n")
            .nth(1)
            .and_then(|s| s.split("```").next())
            .unwrap();
        let suite =
            atalanta_core::parser::parse_suite_from_str(yaml).unwrap_or_else(|e| panic!("{e}"));
        atalanta_core::validate_suite(&suite).unwrap();
        assert!(rules_body().starts_with("# Atalanta"));
        assert!(
            rules_body().len() < 6000,
            "Windsurf caps global rules at 6000 characters"
        );
    }

    #[test]
    fn client_names_parse() {
        assert_eq!(Client::parse("Claude-Code"), Some(Client::Claude));
        assert_eq!(Client::parse("vs-code"), Some(Client::Vscode));
        assert_eq!(Client::parse("antigravity"), None);
    }
}
