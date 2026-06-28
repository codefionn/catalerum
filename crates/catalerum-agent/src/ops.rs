//! Executing the operations the server issues, confined to the granted directories.
//!
//! Every filesystem op resolves its target through [`AgentState::scoped_path`],
//! which canonicalises the path (resolving `..` and symlinks) and requires it to
//! sit under one of the served directories at the right access level — so no op can
//! escape the configured scope, even via a symlink or `../`. Command execution goes
//! through [`crate::sandbox`] for a second (OS-level) wall.

use std::io::{BufRead, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use base64::Engine;
use catalerum_core::computer::{
    ComputerCapabilities, ComputerOp, ComputerPlatform, DesktopAction, DirGrant, DirMode,
    SandboxKind, WriteMode, DEFAULT_EXEC_TIMEOUT_SECS, DEFAULT_SEARCH_TIMEOUT_SECS,
    MAX_EXEC_TIMEOUT_SECS, MAX_SEARCH_TIMEOUT_SECS, PROTOCOL_VERSION,
};
use serde_json::{json, Value};

use crate::config::Config;

/// Max bytes returned by a single `read_file` / captured per exec stream.
const MAX_READ_BYTES: u64 = 256 * 1024;
/// Files larger than this are skipped by `search` (they're unlikely text corpora).
const MAX_SEARCH_FILE_BYTES: u64 = 4 * 1024 * 1024;
/// Default / ceiling on `search` matches.
const DEFAULT_SEARCH_MATCHES: usize = 200;
const MAX_SEARCH_MATCHES: usize = 2000;
/// Cap on `list_dir` entries.
const MAX_DIR_ENTRIES: usize = 4000;

struct SearchOptions<'a> {
    cwd: Option<&'a str>,
    root: Option<&'a str>,
    query: &'a str,
    regex: bool,
    max_results: Option<u64>,
    include_hidden: bool,
    timeout: Duration,
}

/// Matcher compiled once per search and shared by all search workers. Plain text
/// uses the regex engine too: unlike lowercasing every filename and line, its
/// case-insensitive literal search does not allocate in the hot loop.
struct SearchMatcher(regex::Regex);

impl SearchMatcher {
    fn new(query: &str, is_regex: bool) -> Result<Self, String> {
        let pattern = if is_regex {
            query.to_string()
        } else {
            regex::escape(query)
        };
        regex::RegexBuilder::new(&pattern)
            .case_insensitive(!is_regex)
            .build()
            .map(Self)
            .map_err(|e| format!("invalid regex: {e}"))
    }

    fn is_match(&self, text: &str) -> bool {
        self.0.is_match(text)
    }
}

/// State shared by the bounded set of filesystem search workers.
struct SearchShared<'a> {
    matcher: &'a SearchMatcher,
    started: Instant,
    timeout: Duration,
    cap: usize,
    claimed: AtomicUsize,
    capped: AtomicBool,
    timed_out: AtomicBool,
    matches: Mutex<Vec<Value>>,
}

impl SearchShared<'_> {
    fn should_stop(&self) -> bool {
        if self.claimed.load(Ordering::Relaxed) >= self.cap {
            return true;
        }
        if self.started.elapsed() >= self.timeout {
            self.timed_out.store(true, Ordering::Relaxed);
            return true;
        }
        false
    }

    /// Reserve one of the globally capped result slots, then append the hit.
    fn push(&self, hit: Value) -> bool {
        let slot = self.claimed.fetch_add(1, Ordering::Relaxed);
        if slot >= self.cap {
            self.capped.store(true, Ordering::Relaxed);
            return false;
        }
        if slot + 1 == self.cap {
            self.capped.store(true, Ordering::Relaxed);
        }
        self.matches.lock().unwrap().push(hit);
        true
    }
}

/// The running daemon's state: its immutable config plus the session-scoped runtime
/// directory grants (added via an approved `GrantAccess`).
pub struct AgentState {
    pub config: Config,
    /// Directories granted at runtime this session (never persisted).
    runtime_grants: Mutex<Vec<DirGrant>>,
}

impl AgentState {
    pub fn new(config: Config) -> Self {
        Self {
            config,
            runtime_grants: Mutex::new(Vec::new()),
        }
    }

    /// The machine capabilities to announce on connect (config + live sandbox kind).
    pub fn capabilities(&self) -> ComputerCapabilities {
        ComputerCapabilities {
            platform: platform(),
            hostname: hostname::get()
                .ok()
                .and_then(|h| h.into_string().ok())
                .unwrap_or_default(),
            os: std::env::consts::OS.to_string(),
            arch: std::env::consts::ARCH.to_string(),
            agent_version: env!("CARGO_PKG_VERSION").to_string(),
            dirs: self.all_grants(),
            grantable_roots: self.config.grantable_roots.clone(),
            exec_policy: self.config.exec_policy,
            desktop: self.config.desktop,
            sandbox: crate::sandbox::active_kind(self.config.sandbox),
            protocol: PROTOCOL_VERSION,
        }
    }

    /// The config dirs plus any runtime grants.
    fn all_grants(&self) -> Vec<DirGrant> {
        let mut grants = self.config.dir_grants();
        grants.extend(self.runtime_grants.lock().unwrap().iter().cloned());
        grants
    }

    /// Resolve `requested` to a canonical path confined to a served directory at the
    /// required access level, or a human-readable error explaining the refusal.
    fn scoped_path(&self, requested: &str, need_write: bool) -> Result<PathBuf, String> {
        let canon = canonicalize_lenient(Path::new(requested))?;
        let grants = self.all_grants();
        let mut covered_read_only = false;
        for g in &grants {
            let Ok(base) = std::fs::canonicalize(&g.path) else {
                continue; // a configured dir that doesn't exist can't cover anything
            };
            if canon.starts_with(&base) {
                if !need_write || g.mode.can_write() {
                    return Ok(canon);
                }
                covered_read_only = true;
            }
        }
        if covered_read_only {
            Err(format!(
                "`{requested}` is inside a read-only directory; this agent isn't allowed to write there"
            ))
        } else {
            Err(format!(
                "`{requested}` is not inside any directory this agent serves"
            ))
        }
    }

    /// A directory usable as a working directory / search root: any served dir
    /// (read is enough).
    fn scoped_dir(&self, requested: &str) -> Result<PathBuf, String> {
        let path = self.scoped_path(requested, false)?;
        if path.is_dir() {
            Ok(path)
        } else {
            Err(format!("`{requested}` is not a directory"))
        }
    }

    /// Validate an optional working directory and canonicalise it inside the
    /// agent's served scope.
    fn scoped_cwd(&self, cwd: Option<&str>) -> Result<Option<PathBuf>, String> {
        match cwd {
            Some(path) if !Path::new(path).is_absolute() => {
                Err("`cwd` must be an absolute path".to_string())
            }
            Some(path) => self.scoped_dir(path).map(Some),
            None => Ok(None),
        }
    }

    /// Resolve a tool path against `cwd`. Absolute paths remain valid without a
    /// working directory; relative paths require one. The operation itself still
    /// applies its read/write scope check to the resolved path.
    fn resolve_input_path(&self, cwd: Option<&str>, requested: &str) -> Result<PathBuf, String> {
        let cwd = self.scoped_cwd(cwd)?;
        if Path::new(requested).is_absolute() {
            Ok(PathBuf::from(requested))
        } else {
            cwd.map(|base| base.join(requested))
                .ok_or_else(|| "a relative `path` requires `cwd`".to_string())
        }
    }

    /// Execute one op, returning its success `data` or an error message.
    pub async fn execute(&self, op: ComputerOp) -> Result<Value, String> {
        match op {
            ComputerOp::ListDir { cwd, path } => {
                let path = self.resolve_input_path(cwd.as_deref(), &path)?;
                self.list_dir(&path.to_string_lossy())
            }
            ComputerOp::ReadFile {
                cwd,
                path,
                offset,
                limit,
                media_content_type,
            } => {
                let path = self.resolve_input_path(cwd.as_deref(), &path)?;
                self.read_file(
                    &path.to_string_lossy(),
                    offset,
                    limit,
                    media_content_type.as_deref(),
                )
            }
            ComputerOp::WriteFile {
                cwd,
                path,
                content,
                mode,
            } => {
                let path = self.resolve_input_path(cwd.as_deref(), &path)?;
                self.write_file(&path.to_string_lossy(), &content, mode)
            }
            ComputerOp::Search {
                cwd,
                root,
                query,
                regex,
                max_results,
                include_hidden,
                timeout_secs,
            } => self.search(SearchOptions {
                cwd: cwd.as_deref(),
                root: root.as_deref(),
                query: &query,
                regex,
                max_results,
                include_hidden,
                timeout: Duration::from_secs(
                    timeout_secs
                        .unwrap_or(DEFAULT_SEARCH_TIMEOUT_SECS)
                        .clamp(1, MAX_SEARCH_TIMEOUT_SECS),
                ),
            }),
            ComputerOp::Stat { cwd, path } => {
                let path = self.resolve_input_path(cwd.as_deref(), &path)?;
                self.stat(&path.to_string_lossy())
            }
            ComputerOp::GrantAccess { cwd, path, mode } => {
                let path = self.resolve_input_path(cwd.as_deref(), &path)?;
                self.grant_access(&path.to_string_lossy(), mode)
            }
            ComputerOp::Exec {
                command,
                cwd,
                timeout_secs,
                stdin,
            } => {
                self.exec(&command, cwd.as_deref(), timeout_secs, stdin)
                    .await
            }
            ComputerOp::Desktop { action } => self.desktop(action).await,
        }
    }

    fn list_dir(&self, path: &str) -> Result<Value, String> {
        let dir = self.scoped_dir(path)?;
        let mut entries = Vec::new();
        let mut truncated = false;
        for entry in std::fs::read_dir(&dir).map_err(|e| e.to_string())? {
            let Ok(entry) = entry else { continue };
            if entries.len() >= MAX_DIR_ENTRIES {
                truncated = true;
                break;
            }
            let meta = entry.metadata().ok();
            let kind = meta
                .as_ref()
                .map(|m| {
                    if m.is_dir() {
                        "dir"
                    } else if m.file_type().is_symlink() {
                        "symlink"
                    } else {
                        "file"
                    }
                })
                .unwrap_or("file");
            entries.push(json!({
                "name": entry.file_name().to_string_lossy(),
                "path": entry.path().to_string_lossy(),
                "kind": kind,
                "size": meta.as_ref().map(|m| m.len()),
            }));
        }
        Ok(json!({ "path": dir.to_string_lossy(), "entries": entries, "truncated": truncated }))
    }

    fn read_file(
        &self,
        path: &str,
        offset: Option<u64>,
        limit: Option<u64>,
        media_content_type: Option<&str>,
    ) -> Result<Value, String> {
        let file_path = self.scoped_path(path, false)?;
        let mut file = std::fs::File::open(&file_path).map_err(|e| e.to_string())?;
        let total = file.metadata().map(|m| m.len()).unwrap_or(0);
        if let Some(off) = offset {
            use std::io::Seek;
            file.seek(std::io::SeekFrom::Start(off))
                .map_err(|e| e.to_string())?;
        }
        let cap = limit.unwrap_or(MAX_READ_BYTES).min(MAX_READ_BYTES);
        let mut buf = Vec::new();
        file.take(cap)
            .read_to_end(&mut buf)
            .map_err(|e| e.to_string())?;
        let read_from = offset.unwrap_or(0);
        let truncated = read_from + (buf.len() as u64) < total;
        if let Some(content_type) = media_content_type {
            if offset.is_some() || limit.is_some() {
                return Err(
                    "`offset`/`limit` cannot be used when ingesting binary media".to_string(),
                );
            }
            if truncated {
                return Err(format!(
                    "media file is too large for native model ingestion (maximum {MAX_READ_BYTES} bytes)"
                ));
            }
            return Ok(json!({
                "path": file_path.to_string_lossy(),
                "content_base64": base64::engine::general_purpose::STANDARD.encode(&buf),
                "content_type": content_type,
                "size": total,
            }));
        }
        let content = decode_text_file(buf, truncated)?;
        Ok(json!({
            "path": file_path.to_string_lossy(),
            "content": content,
            "size": total,
            "truncated": truncated,
        }))
    }

    fn write_file(&self, path: &str, content: &str, mode: WriteMode) -> Result<Value, String> {
        let file_path = self.scoped_path(path, true)?;
        use std::io::Write;
        let mut opts = std::fs::OpenOptions::new();
        match mode {
            WriteMode::Overwrite => opts.write(true).create(true).truncate(true),
            WriteMode::CreateNew => opts.write(true).create_new(true),
            WriteMode::Append => opts.append(true).create(true),
        };
        let mut file = opts.open(&file_path).map_err(|e| e.to_string())?;
        file.write_all(content.as_bytes())
            .map_err(|e| e.to_string())?;
        Ok(json!({ "path": file_path.to_string_lossy(), "bytes_written": content.len() }))
    }

    fn stat(&self, path: &str) -> Result<Value, String> {
        // Stat is a read; a path that doesn't exist yet is still reported (exists:false)
        // as long as it's within scope.
        let scoped = self.scoped_path(path, false)?;
        match std::fs::symlink_metadata(&scoped) {
            Ok(meta) => {
                let kind = if meta.is_dir() {
                    "dir"
                } else if meta.file_type().is_symlink() {
                    "symlink"
                } else {
                    "file"
                };
                let modified = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs());
                Ok(json!({
                    "path": scoped.to_string_lossy(),
                    "exists": true,
                    "kind": kind,
                    "size": meta.len(),
                    "modified": modified,
                }))
            }
            Err(_) => Ok(json!({
                "path": scoped.to_string_lossy(),
                "exists": false,
            })),
        }
    }

    /// Broad search: matches file/directory *names* as well as file *contents*.
    /// Plain (non-regex) queries match case-insensitively; hidden (dot-prefixed)
    /// entries are skipped unless `include_hidden`.
    fn search(&self, options: SearchOptions<'_>) -> Result<Value, String> {
        let SearchOptions {
            cwd,
            root,
            query,
            regex,
            max_results,
            include_hidden,
            timeout,
        } = options;
        let started = Instant::now();
        let cap = max_results
            .map(|n| (n as usize).min(MAX_SEARCH_MATCHES))
            .unwrap_or(DEFAULT_SEARCH_MATCHES)
            .max(1);
        let cwd = self.scoped_cwd(cwd)?;
        // Roots to walk: an explicit root (relative to cwd when applicable), cwd
        // itself, or every served directory when neither was provided.
        let roots: Vec<PathBuf> = match (root, cwd) {
            (Some(root), Some(cwd)) if Path::new(root).is_relative() => {
                vec![self.scoped_dir(&cwd.join(root).to_string_lossy())?]
            }
            (Some(root), None) if Path::new(root).is_relative() => {
                return Err("a relative `root` requires `cwd`".to_string())
            }
            (Some(root), _) => vec![self.scoped_dir(root)?],
            (None, Some(cwd)) => vec![cwd],
            (None, None) => self
                .all_grants()
                .iter()
                .filter_map(|g| std::fs::canonicalize(&g.path).ok())
                .collect(),
        };
        if roots.is_empty() {
            return Err("no readable directory to search".to_string());
        }

        let matcher = SearchMatcher::new(query, regex)?;

        // A parent grant subsumes a nested grant. Removing overlap avoids walking
        // and reading the same tree twice when users configure both.
        let mut unique_roots: Vec<PathBuf> = Vec::with_capacity(roots.len());
        for root in roots {
            if unique_roots
                .iter()
                .any(|existing| root.starts_with(existing))
            {
                continue;
            }
            unique_roots.retain(|existing| !existing.starts_with(&root));
            unique_roots.push(root);
        }

        // Split at each granted root's first level. This gives workers independent
        // subtrees while ensuring the walk root itself is never reported as a name
        // hit (a granted directory may itself be hidden and remains searchable).
        let mut tasks = Vec::new();
        'roots: for root in unique_roots {
            if started.elapsed() >= timeout {
                break;
            }
            let Ok(entries) = std::fs::read_dir(root) else {
                continue;
            };
            for entry in entries.flatten() {
                if started.elapsed() >= timeout {
                    break 'roots;
                }
                if include_hidden || !is_hidden(&entry.file_name()) {
                    tasks.push(entry.path());
                }
            }
        }

        let shared = SearchShared {
            matcher: &matcher,
            started,
            timeout,
            cap,
            claimed: AtomicUsize::new(0),
            capped: AtomicBool::new(false),
            timed_out: AtomicBool::new(started.elapsed() >= timeout),
            matches: Mutex::new(Vec::with_capacity(cap)),
        };
        let worker_count = std::thread::available_parallelism()
            .map_or(1, usize::from)
            .min(8)
            .min(tasks.len().max(1));
        let next_task = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..worker_count {
                scope.spawn(|| loop {
                    if shared.should_stop() {
                        break;
                    }
                    let index = next_task.fetch_add(1, Ordering::Relaxed);
                    let Some(path) = tasks.get(index) else { break };
                    search_task(path, include_hidden, &shared);
                });
            }
        });

        let timed_out = shared.timed_out.load(Ordering::Relaxed);
        let truncated = timed_out || shared.capped.load(Ordering::Relaxed);
        let matches = shared.matches.into_inner().unwrap();
        Ok(json!({
            "matches": matches,
            "truncated": truncated,
            "timed_out": timed_out,
        }))
    }

    fn grant_access(&self, path: &str, mode: DirMode) -> Result<Value, String> {
        // The requested dir must live under one of the advertised grantable roots.
        let canon = canonicalize_lenient(Path::new(path))?;
        let under_root = self.config.grantable_roots.iter().any(|r| {
            std::fs::canonicalize(r)
                .map(|base| canon.starts_with(&base))
                .unwrap_or(false)
        });
        if !under_root {
            return Err(format!(
                "`{path}` is not under any grantable root this agent advertises"
            ));
        }
        let grant = DirGrant {
            path: canon.to_string_lossy().to_string(),
            mode,
        };
        let mut grants = self.runtime_grants.lock().unwrap();
        // De-dupe / upgrade: replace an existing grant for the same path.
        grants.retain(|g| g.path != grant.path);
        grants.push(grant.clone());
        Ok(json!({
            "path": grant.path,
            "mode": if mode.can_write() { "read_write" } else { "read" },
        }))
    }

    async fn exec(
        &self,
        command: &str,
        cwd: Option<&str>,
        timeout_secs: Option<u64>,
        stdin: Option<String>,
    ) -> Result<Value, String> {
        // Resolve the working directory: the requested (scoped) dir, or the first
        // served directory as a sensible default.
        let workdir = match cwd {
            Some(c) => self.scoped_dir(c)?,
            None => self
                .all_grants()
                .first()
                .and_then(|g| std::fs::canonicalize(&g.path).ok())
                .ok_or("this agent serves no directory to run a command in")?,
        };
        let dirs = self.all_grants();
        let mut cmd = crate::sandbox::build_command(command, &workdir, &dirs, self.config.sandbox);
        cmd.stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .stdin(std::process::Stdio::piped())
            .kill_on_drop(true);

        let mut child = cmd.spawn().map_err(|e| format!("spawn failed: {e}"))?;
        if let Some(input) = stdin {
            if let Some(mut sink) = child.stdin.take() {
                use tokio::io::AsyncWriteExt;
                let _ = sink.write_all(input.as_bytes()).await;
                let _ = sink.shutdown().await;
            }
        } else {
            drop(child.stdin.take());
        }

        let timeout = Duration::from_secs(
            timeout_secs
                .unwrap_or(DEFAULT_EXEC_TIMEOUT_SECS)
                .clamp(1, MAX_EXEC_TIMEOUT_SECS),
        );
        let output = match tokio::time::timeout(timeout, child.wait_with_output()).await {
            Ok(Ok(out)) => out,
            Ok(Err(e)) => return Err(format!("command failed: {e}")),
            Err(_) => {
                return Ok(json!({
                    "stdout": "",
                    "stderr": "",
                    "exit_code": Value::Null,
                    "timed_out": true,
                }))
            }
        };
        Ok(json!({
            "stdout": cap_utf8(&output.stdout),
            "stderr": cap_utf8(&output.stderr),
            "exit_code": output.status.code(),
            "timed_out": false,
        }))
    }

    async fn desktop(&self, action: DesktopAction) -> Result<Value, String> {
        if !self.config.desktop {
            return Err("desktop control is disabled on this agent".to_string());
        }
        match action {
            DesktopAction::OpenUrl { url } => {
                run_detached(open_url_command(&url)).await?;
                Ok(json!({ "opened": url }))
            }
            DesktopAction::Notify { title, body } => {
                run_detached(notify_command(&title, &body)).await?;
                Ok(json!({ "notified": true }))
            }
            DesktopAction::Screenshot => screenshot().await,
        }
    }
}

fn decode_text_file(bytes: Vec<u8>, truncated: bool) -> Result<String, String> {
    let content = match String::from_utf8(bytes) {
        Ok(content) => content,
        Err(error) if truncated && error.utf8_error().error_len().is_none() => {
            let valid_up_to = error.utf8_error().valid_up_to();
            let mut bytes = error.into_bytes();
            bytes.truncate(valid_up_to);
            String::from_utf8(bytes).expect("prefix ending at valid_up_to is valid UTF-8")
        }
        Err(_) => {
            return Err(
                "file is binary or is not valid UTF-8; read_file reads text files only".to_string(),
            )
        }
    };
    if content
        .chars()
        .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
    {
        return Err(
            "file contains binary control bytes; read_file reads text files only".to_string(),
        );
    }
    Ok(content)
}

/// The daemon's platform, from the compile target.
fn platform() -> ComputerPlatform {
    match std::env::consts::OS {
        "linux" => ComputerPlatform::Linux,
        "macos" => ComputerPlatform::Macos,
        "windows" => ComputerPlatform::Windows,
        _ => ComputerPlatform::Other,
    }
}

/// Whether a file/directory name counts as hidden (dot-prefixed, unix convention).
fn is_hidden(name: &std::ffi::OsStr) -> bool {
    name.to_string_lossy().starts_with('.')
}

/// Walk and scan one independent subtree. Search results intentionally have no
/// ordering guarantee, so workers can append as soon as they find a hit.
fn search_task(path: &Path, include_hidden: bool, shared: &SearchShared<'_>) {
    for entry in walkdir::WalkDir::new(path)
        .follow_links(false)
        .into_iter()
        // The task root is already filtered by its parent. Below it, prune hidden
        // directories at traversal time so their contents are never opened.
        .filter_entry(|e| include_hidden || e.depth() == 0 || !is_hidden(e.file_name()))
        .filter_map(Result::ok)
    {
        if shared.should_stop() {
            break;
        }

        if shared
            .matcher
            .is_match(&entry.file_name().to_string_lossy())
            && !shared.push(json!({
                "path": entry.path().to_string_lossy(),
                "kind": "name",
            }))
        {
            break;
        }

        if !entry.file_type().is_file()
            || entry.metadata().map(|m| m.len()).unwrap_or(0) > MAX_SEARCH_FILE_BYTES
        {
            continue;
        }
        search_file_contents(entry.path(), shared);
    }
}

/// Stream a file through a reusable line buffer. The previous implementation
/// allocated the whole file and then another lowercase String for every line.
fn search_file_contents(path: &Path, shared: &SearchShared<'_>) {
    let Ok(file) = std::fs::File::open(path) else {
        return;
    };
    let mut reader = std::io::BufReader::with_capacity(64 * 1024, file);
    let mut line = String::new();
    let mut line_number = 0usize;
    loop {
        line.clear();
        let Ok(read) = reader.read_line(&mut line) else {
            return; // binary / non-UTF-8 / unreadable
        };
        if read == 0 {
            return;
        }
        line_number += 1;
        if shared.should_stop() {
            return;
        }
        let text = match line.strip_suffix('\n') {
            Some(without_lf) => without_lf.strip_suffix('\r').unwrap_or(without_lf),
            None => &line,
        };
        if shared.matcher.is_match(text)
            && !shared.push(json!({
                "path": path.to_string_lossy(),
                "kind": "content",
                "line": line_number,
                "text": text.chars().take(500).collect::<String>(),
            }))
        {
            return;
        }
    }
}

/// Canonicalise `p`, resolving `..`/symlinks. For a not-yet-existing target, resolve
/// its parent and re-attach the final component (so a new file under an allowed dir
/// is writable, while `..` can't escape).
fn canonicalize_lenient(p: &Path) -> Result<PathBuf, String> {
    if let Ok(c) = std::fs::canonicalize(p) {
        return Ok(c);
    }
    let parent = p
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| format!("cannot resolve `{}`", p.display()))?;
    let name = p
        .file_name()
        .ok_or_else(|| format!("cannot resolve `{}`", p.display()))?;
    let cp = std::fs::canonicalize(parent)
        .map_err(|e| format!("parent of `{}` not accessible: {e}", p.display()))?;
    Ok(cp.join(name))
}

/// Cap a captured stream to [`MAX_READ_BYTES`] and lossy-decode it.
fn cap_utf8(bytes: &[u8]) -> String {
    let end = (MAX_READ_BYTES as usize).min(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

/// Spawn a "fire and forget" desktop command, waiting only for it to start/exit
/// quickly; a non-zero exit is reported.
async fn run_detached(mut cmd: tokio::process::Command) -> Result<(), String> {
    cmd.stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());
    let out = cmd
        .output()
        .await
        .map_err(|e| format!("desktop command failed to start: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "desktop command exited with {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

#[cfg(target_os = "macos")]
fn open_url_command(url: &str) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("open");
    cmd.arg(url);
    cmd
}
#[cfg(target_os = "linux")]
fn open_url_command(url: &str) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("xdg-open");
    cmd.arg(url);
    cmd
}
#[cfg(target_os = "windows")]
fn open_url_command(url: &str) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("cmd");
    cmd.arg("/C").arg("start").arg("").arg(url);
    cmd
}

#[cfg(target_os = "macos")]
fn notify_command(title: &str, body: &str) -> tokio::process::Command {
    let script = format!(
        "display notification \"{}\" with title \"{}\"",
        body.replace('"', "'"),
        title.replace('"', "'")
    );
    let mut cmd = tokio::process::Command::new("osascript");
    cmd.arg("-e").arg(script);
    cmd
}
#[cfg(target_os = "linux")]
fn notify_command(title: &str, body: &str) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("notify-send");
    cmd.arg(title).arg(body);
    cmd
}
#[cfg(target_os = "windows")]
fn notify_command(title: &str, body: &str) -> tokio::process::Command {
    // Best-effort msgbox via PowerShell.
    let script = format!(
        "[void][System.Reflection.Assembly]::LoadWithPartialName('System.Windows.Forms'); \
         [System.Windows.Forms.MessageBox]::Show('{}','{}')",
        body.replace('\'', "`'"),
        title.replace('\'', "`'")
    );
    let mut cmd = tokio::process::Command::new("powershell");
    cmd.arg("-NoProfile").arg("-Command").arg(script);
    cmd
}

/// Capture the primary screen as a base64 PNG, using the first available platform
/// tool. Returns `{ image_base64, mime }` or an error when no capture tool exists.
async fn screenshot() -> Result<Value, String> {
    let dir = std::env::temp_dir();
    let path = dir.join(format!("catalerum-agent-shot-{}.png", std::process::id()));
    let path_str = path.to_string_lossy().to_string();

    let attempts: Vec<(&str, Vec<String>)> = screenshot_commands(&path_str);
    let mut last_err = String::from("no screenshot tool found");
    let mut captured = false;
    for (program, args) in attempts {
        let mut cmd = tokio::process::Command::new(program);
        cmd.args(&args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        match cmd.output().await {
            Ok(out) if out.status.success() && path.exists() => {
                captured = true;
                break;
            }
            Ok(_) => last_err = format!("`{program}` did not produce an image"),
            Err(e) => last_err = format!("`{program}`: {e}"),
        }
    }
    if !captured {
        return Err(format!("could not take a screenshot ({last_err})"));
    }
    let bytes = std::fs::read(&path).map_err(|e| e.to_string())?;
    let _ = std::fs::remove_file(&path);
    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
    Ok(json!({ "image_base64": b64, "mime": "image/png" }))
}

#[cfg(target_os = "macos")]
fn screenshot_commands(path: &str) -> Vec<(&'static str, Vec<String>)> {
    vec![(
        "screencapture",
        vec!["-x".into(), "-t".into(), "png".into(), path.to_string()],
    )]
}
#[cfg(target_os = "linux")]
fn screenshot_commands(path: &str) -> Vec<(&'static str, Vec<String>)> {
    vec![
        ("grim", vec![path.to_string()]),
        ("scrot", vec!["-o".into(), path.to_string()]),
        (
            "import",
            vec!["-window".into(), "root".into(), path.to_string()],
        ),
    ]
}
#[cfg(target_os = "windows")]
fn screenshot_commands(_path: &str) -> Vec<(&'static str, Vec<String>)> {
    Vec::new()
}

// Keep `SandboxKind` referenced on platforms that don't otherwise use it in this
// module (it is used via `crate::sandbox::active_kind` in `capabilities`).
const _: fn() -> SandboxKind = || SandboxKind::None;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DirConfig;

    fn state_for(dir: &Path, mode: DirMode) -> AgentState {
        AgentState::new(Config {
            server_url: "https://x".into(),
            token: "t".into(),
            name: "m".into(),
            dirs: vec![DirConfig {
                path: dir.to_string_lossy().to_string(),
                mode,
            }],
            grantable_roots: vec![],
            exec_policy: catalerum_core::computer::ExecPolicy::Auto,
            desktop: false,
            sandbox: false,
        })
    }

    #[test]
    fn scoping_blocks_escape_and_allows_within() {
        let tmp = std::env::temp_dir().join(format!("ca-scope-{}", std::process::id()));
        std::fs::create_dir_all(tmp.join("sub")).unwrap();
        std::fs::write(tmp.join("sub/a.txt"), b"hi").unwrap();
        let state = state_for(&tmp, DirMode::ReadWrite);

        // Inside the served dir → ok.
        assert!(state
            .scoped_path(&tmp.join("sub/a.txt").to_string_lossy(), false)
            .is_ok());
        // A new file under the dir (write) → ok (parent resolves).
        assert!(state
            .scoped_path(&tmp.join("sub/new.txt").to_string_lossy(), true)
            .is_ok());
        // `..` escape → refused.
        assert!(state
            .scoped_path(&tmp.join("sub/../../etc/passwd").to_string_lossy(), false)
            .is_err());
        // A completely outside path → refused.
        assert!(state.scoped_path("/etc/hostname", false).is_err());

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn read_only_dir_refuses_write() {
        let tmp = std::env::temp_dir().join(format!("ca-ro-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let state = state_for(&tmp, DirMode::Read);
        let target = tmp.join("x.txt");
        assert!(state.scoped_path(&target.to_string_lossy(), false).is_ok());
        assert!(state.scoped_path(&target.to_string_lossy(), true).is_err());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn cwd_resolves_relative_paths_for_file_operations() {
        let tmp = std::env::temp_dir().join(format!("ca-cwd-{}", std::process::id()));
        let work = tmp.join("work");
        let grantable = tmp.join("grantable");
        let requested = grantable.join("requested");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::create_dir_all(&requested).unwrap();
        let mut state = state_for(&work, DirMode::ReadWrite);
        state
            .config
            .grantable_roots
            .push(grantable.to_string_lossy().to_string());
        let cwd = work.to_string_lossy().to_string();

        state
            .execute(ComputerOp::WriteFile {
                cwd: Some(cwd.clone()),
                path: "notes.txt".into(),
                content: "hello".into(),
                mode: WriteMode::Overwrite,
            })
            .await
            .expect("relative write");

        let read = state
            .execute(ComputerOp::ReadFile {
                cwd: Some(cwd.clone()),
                path: "notes.txt".into(),
                offset: None,
                limit: None,
                media_content_type: None,
            })
            .await
            .expect("relative read");
        assert_eq!(read["content"], "hello");

        let stat = state
            .execute(ComputerOp::Stat {
                cwd: Some(cwd.clone()),
                path: "notes.txt".into(),
            })
            .await
            .expect("relative stat");
        assert_eq!(stat["kind"], "file");

        let listing = state
            .execute(ComputerOp::ListDir {
                cwd: Some(cwd.clone()),
                path: ".".into(),
            })
            .await
            .expect("relative list");
        assert!(listing["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["name"] == "notes.txt"));

        let granted = state
            .execute(ComputerOp::GrantAccess {
                cwd: Some(cwd),
                path: "../grantable/requested".into(),
                mode: DirMode::Read,
            })
            .await
            .expect("relative access request");
        assert_eq!(
            Path::new(granted["path"].as_str().unwrap()),
            requested.as_path()
        );

        let err = state
            .execute(ComputerOp::Stat {
                cwd: None,
                path: "notes.txt".into(),
            })
            .await
            .expect_err("relative path without cwd");
        assert!(err.contains("requires `cwd`"));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn search_is_broad_and_gates_hidden_files() {
        let tmp = std::env::temp_dir().join(format!("ca-search-{}", std::process::id()));
        std::fs::create_dir_all(tmp.join(".hidden")).unwrap();
        std::fs::write(tmp.join("notes.txt"), b"the Alpha secret line\n").unwrap();
        std::fs::write(tmp.join("Alpha_plan.md"), b"nothing relevant\n").unwrap();
        std::fs::write(tmp.join(".hidden/inner.txt"), b"alpha too\n").unwrap();
        let state = state_for(&tmp, DirMode::Read);

        let search = |include_hidden| ComputerOp::Search {
            cwd: None,
            root: None,
            query: "alpha".into(),
            regex: false,
            max_results: None,
            include_hidden,
            timeout_secs: None,
        };

        let out = state.execute(search(false)).await.expect("search");
        let matches = out["matches"].as_array().unwrap();
        // Case-insensitive content match…
        assert!(matches.iter().any(|m| m["kind"] == "content"
            && m["path"].as_str().unwrap().ends_with("notes.txt")
            && m["line"] == 1));
        // …and a name match, both despite the differing case.
        assert!(
            matches
                .iter()
                .any(|m| m["kind"] == "name"
                    && m["path"].as_str().unwrap().ends_with("Alpha_plan.md"))
        );
        // Hidden entries are skipped by default.
        assert!(!matches
            .iter()
            .any(|m| m["path"].as_str().unwrap().contains(".hidden")));

        let out = state.execute(search(true)).await.expect("search hidden");
        let matches = out["matches"].as_array().unwrap();
        assert!(matches
            .iter()
            .any(|m| m["kind"] == "content" && m["path"].as_str().unwrap().contains(".hidden")));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn expired_search_returns_partial_result_instead_of_an_error() {
        let tmp = std::env::temp_dir().join(format!("ca-search-timeout-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("notes.txt"), b"needle\n").unwrap();
        let state = state_for(&tmp, DirMode::Read);

        let out = state
            .search(SearchOptions {
                cwd: None,
                root: None,
                query: "needle",
                regex: false,
                max_results: None,
                include_hidden: false,
                timeout: Duration::ZERO,
            })
            .expect("a deadline returns accumulated results");

        assert!(out["timed_out"].as_bool().unwrap());
        assert!(out["truncated"].as_bool().unwrap());
        assert!(out["matches"].as_array().unwrap().is_empty());

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn parallel_search_enforces_one_global_result_cap() {
        let tmp = std::env::temp_dir().join(format!("ca-search-cap-{}", std::process::id()));
        for dir in 0..4 {
            let path = tmp.join(format!("tree-{dir}"));
            std::fs::create_dir_all(&path).unwrap();
            for file in 0..10 {
                std::fs::write(path.join(format!("file-{file}.txt")), b"needle\nneedle\n").unwrap();
            }
        }
        let state = state_for(&tmp, DirMode::Read);

        let out = state
            .search(SearchOptions {
                cwd: None,
                root: None,
                query: "needle",
                regex: false,
                max_results: Some(7),
                include_hidden: false,
                timeout: Duration::from_secs(10),
            })
            .expect("search");

        assert_eq!(out["matches"].as_array().unwrap().len(), 7);
        assert!(out["truncated"].as_bool().unwrap());
        assert!(!out["timed_out"].as_bool().unwrap());

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn search_cwd_is_the_default_root_and_resolves_relative_roots() {
        let tmp = std::env::temp_dir().join(format!("ca-search-cwd-{}", std::process::id()));
        std::fs::create_dir_all(tmp.join("work/sub")).unwrap();
        std::fs::write(tmp.join("outside.txt"), b"needle outside\n").unwrap();
        std::fs::write(tmp.join("work/inside.txt"), b"needle inside\n").unwrap();
        std::fs::write(tmp.join("work/sub/nested.txt"), b"needle nested\n").unwrap();
        let state = state_for(&tmp, DirMode::Read);
        let cwd = tmp.join("work").to_string_lossy().to_string();

        let search = |root| ComputerOp::Search {
            cwd: Some(cwd.clone()),
            root,
            query: "needle".into(),
            regex: false,
            max_results: None,
            include_hidden: false,
            timeout_secs: None,
        };

        let out = state.execute(search(None)).await.expect("cwd search");
        let matches = out["matches"].as_array().unwrap();
        assert!(matches
            .iter()
            .all(|hit| !hit["path"].as_str().unwrap().ends_with("outside.txt")));
        assert!(matches
            .iter()
            .any(|hit| hit["path"].as_str().unwrap().ends_with("inside.txt")));

        let out = state
            .execute(search(Some("sub".into())))
            .await
            .expect("relative root search");
        let matches = out["matches"].as_array().unwrap();
        let expected_root = tmp.join("work/sub");
        assert!(matches
            .iter()
            .all(|hit| Path::new(hit["path"].as_str().unwrap()).starts_with(&expected_root)));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn read_file_rejects_binary_unless_server_selects_native_media() {
        let tmp = std::env::temp_dir().join(format!("ca-media-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("pixel.png"), [1, 2, 3]).unwrap();
        let state = state_for(&tmp, DirMode::Read);
        let cwd = tmp.to_string_lossy().to_string();

        let text_err = state
            .execute(ComputerOp::ReadFile {
                cwd: Some(cwd.clone()),
                path: "pixel.png".into(),
                offset: None,
                limit: None,
                media_content_type: None,
            })
            .await
            .expect_err("binary data must not be lossily decoded");
        assert!(text_err.contains("binary"));

        let media = state
            .execute(ComputerOp::ReadFile {
                cwd: Some(cwd),
                path: "pixel.png".into(),
                offset: None,
                limit: None,
                media_content_type: Some("image/png".into()),
            })
            .await
            .expect("server-authorized media read");
        assert_eq!(media["content_type"], "image/png");
        assert_eq!(media["content_base64"], "AQID");
        assert!(media.get("content").is_none());
        assert_eq!(decode_text_file(vec![b'a', 0xe2, 0x82], true).unwrap(), "a");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn exec_runs_a_simple_command() {
        if cfg!(target_family = "windows") {
            return; // /bin/sh path is unix-only in this test
        }
        let tmp = std::env::temp_dir().join(format!("ca-exec-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        // No sandbox in the test (Landlock may be unavailable in CI containers).
        let mut state = state_for(&tmp, DirMode::ReadWrite);
        state.config.sandbox = false;
        let out = state
            .execute(ComputerOp::Exec {
                command: "echo hello".into(),
                cwd: None,
                timeout_secs: Some(10),
                stdin: None,
            })
            .await
            .expect("exec");
        assert_eq!(out["exit_code"], 0);
        assert!(out["stdout"].as_str().unwrap().contains("hello"));
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
