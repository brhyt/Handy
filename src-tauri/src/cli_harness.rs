//! Thin spawners for official coding CLIs used as post-process providers.
//!
//! Handy never reads or copies OAuth/credential files. It only optionally sets
//! well-known config-dir environment variables (`CLAUDE_CONFIG_DIR`,
//! `CODEX_HOME`, `GROK_HOME`) to directories the user already uses, then
//! parses stdout / an output file into rewritten text.

use crate::settings::CliHarnessSettings;
use log::{debug, info, warn};
use serde_json::Value;
use specta::Type;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const CLAUDE_CODE_CLI_PROVIDER_ID: &str = "claude_code_cli";
pub const CODEX_CLI_PROVIDER_ID: &str = "codex_cli";
pub const GROK_CLI_PROVIDER_ID: &str = "grok_cli";

pub const DEFAULT_CLI_TIMEOUT_SECS: u64 = 90;
pub const MIN_CLI_TIMEOUT_SECS: u64 = 10;
pub const MAX_CLI_TIMEOUT_SECS: u64 = 600;

const TRANSCRIPTION_FIELD: &str = "transcription";
const AUTH_PROBE_TIMEOUT_SECS: u64 = 12;

/// UI/status payload for the settings probe. No credential material.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Type)]
pub struct CliHarnessStatus {
    pub provider_id: String,
    pub binary_name: String,
    pub resolved_binary: Option<String>,
    pub binary_found: bool,
    pub logged_in: Option<bool>,
    pub message: String,
    pub login_hint: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CliKind {
    Claude,
    Codex,
    Grok,
}

impl CliKind {
    fn from_provider_id(id: &str) -> Option<Self> {
        match id {
            CLAUDE_CODE_CLI_PROVIDER_ID => Some(Self::Claude),
            CODEX_CLI_PROVIDER_ID => Some(Self::Codex),
            GROK_CLI_PROVIDER_ID => Some(Self::Grok),
            _ => None,
        }
    }

    fn binary_name(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Grok => "grok",
        }
    }

    fn config_dir_env(self) -> &'static str {
        match self {
            Self::Claude => "CLAUDE_CONFIG_DIR",
            Self::Codex => "CODEX_HOME",
            Self::Grok => "GROK_HOME",
        }
    }

    fn login_hint(self) -> &'static str {
        match self {
            Self::Claude => "Install the Claude Code CLI and run `claude auth login`.",
            Self::Codex => "Install the Codex CLI and run `codex login`.",
            Self::Grok => "Install the Grok CLI and run `grok login`.",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Claude => "Claude Code CLI",
            Self::Codex => "Codex CLI",
            Self::Grok => "Grok CLI",
        }
    }
}

pub fn is_cli_provider(id: &str) -> bool {
    CliKind::from_provider_id(id).is_some()
}

pub fn clamp_timeout_secs(timeout_secs: u64) -> u64 {
    timeout_secs.clamp(MIN_CLI_TIMEOUT_SECS, MAX_CLI_TIMEOUT_SECS)
}

pub fn transcription_json_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            (TRANSCRIPTION_FIELD): {
                "type": "string",
                "description": "The cleaned and processed transcription text"
            }
        },
        "required": [TRANSCRIPTION_FIELD],
        "additionalProperties": false
    })
}

/// Build the user prompt the same way legacy HTTP post-process does:
/// substitute `${output}` in the selected prompt template.
pub fn build_cli_prompt(prompt_template: &str, transcription: &str) -> String {
    if prompt_template.contains("${output}") {
        prompt_template.replace("${output}", transcription)
    } else {
        format!("{prompt_template}\n\n{transcription}")
    }
}

pub async fn rewrite_transcription(
    provider_id: &str,
    cli_settings: &CliHarnessSettings,
    model: &str,
    prompt: &str,
) -> Result<String, String> {
    let kind = CliKind::from_provider_id(provider_id)
        .ok_or_else(|| format!("'{provider_id}' is not a CLI post-process provider"))?;
    let settings = cli_settings.clone();
    let model = model.to_string();
    let prompt = prompt.to_string();
    let provider_id = provider_id.to_string();

    tokio::task::spawn_blocking(move || {
        rewrite_transcription_blocking(kind, &settings, &model, &prompt)
    })
    .await
    .map_err(|e| format!("CLI harness task failed for '{provider_id}': {e}"))?
}

pub async fn probe(provider_id: &str, cli_settings: &CliHarnessSettings) -> CliHarnessStatus {
    let settings = cli_settings.clone();
    let provider_id = provider_id.to_string();
    tokio::task::spawn_blocking({
        let provider_id = provider_id.clone();
        move || probe_blocking(&provider_id, &settings)
    })
    .await
    .unwrap_or_else(|e| CliHarnessStatus {
        provider_id,
        binary_name: String::new(),
        resolved_binary: None,
        binary_found: false,
        logged_in: None,
        message: format!("Failed to probe CLI status: {e}"),
        login_hint: String::new(),
    })
}

fn rewrite_transcription_blocking(
    kind: CliKind,
    cli_settings: &CliHarnessSettings,
    model: &str,
    prompt: &str,
) -> Result<String, String> {
    let resolved = resolve_binary(kind, cli_settings).ok_or_else(|| {
        format!(
            "{} binary '{}' was not found. Install it or set a binary path. {}",
            kind.label(),
            requested_binary(kind, cli_settings),
            kind.login_hint()
        )
    })?;

    let timeout = Duration::from_secs(clamp_timeout_secs(cli_settings.timeout_secs));
    let work_dir = make_temp_dir("handy-cli-cwd")?;
    let schema = transcription_json_schema();
    let schema_json = serde_json::to_string(&schema)
        .map_err(|e| format!("Failed to encode transcription JSON schema: {e}"))?;

    let result = match kind {
        CliKind::Claude => run_claude(
            &resolved,
            cli_settings,
            model,
            prompt,
            &schema_json,
            &work_dir,
            timeout,
        ),
        CliKind::Codex => run_codex(
            &resolved,
            cli_settings,
            model,
            prompt,
            &schema_json,
            &work_dir,
            timeout,
        ),
        CliKind::Grok => run_grok(
            &resolved,
            cli_settings,
            model,
            prompt,
            &schema_json,
            &work_dir,
            timeout,
        ),
    };

    let _ = std::fs::remove_dir_all(&work_dir);
    result
}

fn run_claude(
    binary: &Path,
    cli_settings: &CliHarnessSettings,
    model: &str,
    prompt: &str,
    schema_json: &str,
    work_dir: &Path,
    timeout: Duration,
) -> Result<String, String> {
    let mut args = vec![
        "-p".to_string(),
        "--output-format".to_string(),
        "json".to_string(),
        "--json-schema".to_string(),
        schema_json.to_string(),
        "--permission-mode".to_string(),
        "dontAsk".to_string(),
        "--tools".to_string(),
        String::new(),
    ];
    if !model.trim().is_empty() {
        args.push("--model".to_string());
        args.push(model.trim().to_string());
    }

    let output = run_command(
        binary,
        &args,
        cli_settings,
        CliKind::Claude,
        work_dir,
        Some(prompt),
        timeout,
    )?;
    ensure_success("Claude Code CLI", &output)?;
    parse_rewritten_text(&stdout_string(&output))
}

fn run_codex(
    binary: &Path,
    cli_settings: &CliHarnessSettings,
    model: &str,
    prompt: &str,
    schema_json: &str,
    work_dir: &Path,
    timeout: Duration,
) -> Result<String, String> {
    let schema_path = temp_file("handy-codex-schema", schema_json)?;
    let output_path = temp_file("handy-codex-output", "")?;

    let run = |include_ephemeral: bool| -> Result<Output, String> {
        let mut args = vec!["exec".to_string()];
        if include_ephemeral {
            args.push("--ephemeral".to_string());
        }
        args.extend([
            "--skip-git-repo-check".to_string(),
            "-s".to_string(),
            "read-only".to_string(),
            "--output-schema".to_string(),
            path_to_string(&schema_path),
            "--output-last-message".to_string(),
            path_to_string(&output_path),
            "-".to_string(),
        ]);
        if !model.trim().is_empty() {
            // Insert model flags before the stdin "-" sentinel.
            let dash = args.pop();
            args.push("--model".to_string());
            args.push(model.trim().to_string());
            if let Some(dash) = dash {
                args.push(dash);
            }
        }
        run_command(
            binary,
            &args,
            cli_settings,
            CliKind::Codex,
            work_dir,
            Some(prompt),
            timeout,
        )
    };

    let output = match run(true) {
        Ok(output) if unknown_flag(&output, "--ephemeral") => {
            info!("Codex CLI rejected --ephemeral; retrying without it");
            run(false)?
        }
        other => other?,
    };

    let file_text = std::fs::read_to_string(&output_path).unwrap_or_default();
    let _ = std::fs::remove_file(&schema_path);
    let _ = std::fs::remove_file(&output_path);

    if let Err(err) = ensure_success("Codex CLI", &output) {
        if file_text.trim().is_empty() {
            return Err(err);
        }
        warn!("Codex CLI exited with an error but wrote an output file; using that text");
    }

    if !file_text.trim().is_empty() {
        return parse_rewritten_text(&file_text);
    }
    parse_rewritten_text(&stdout_string(&output))
}

fn run_grok(
    binary: &Path,
    cli_settings: &CliHarnessSettings,
    model: &str,
    prompt: &str,
    schema_json: &str,
    work_dir: &Path,
    timeout: Duration,
) -> Result<String, String> {
    let prompt_path = temp_file("handy-grok-prompt", prompt)?;

    // Grok 1.0.5 treats `-p` as `--single <PROMPT>` (value required). The
    // structured one-shot is `--prompt-file` plus JSON flags — do not pass
    // a bare `-p` here.
    let mut args = vec![
        "--output-format".to_string(),
        "json".to_string(),
        "--json-schema".to_string(),
        schema_json.to_string(),
        "--max-turns".to_string(),
        "1".to_string(),
        "--disable-web-search".to_string(),
        "--prompt-file".to_string(),
        path_to_string(&prompt_path),
    ];
    if !model.trim().is_empty() {
        args.push("--model".to_string());
        args.push(model.trim().to_string());
    }

    let output = match run_command(
        binary,
        &args,
        cli_settings,
        CliKind::Grok,
        work_dir,
        None,
        timeout,
    ) {
        Ok(output)
            if unknown_flag(&output, "--prompt-file") || unknown_flag(&output, "--json-schema") =>
        {
            info!("Grok CLI rejected a structured flag; retrying with a plain one-shot -p");
            let mut fallback = vec![
                "-p".to_string(),
                prompt.to_string(),
                "--output-format".to_string(),
                "json".to_string(),
                "--max-turns".to_string(),
                "1".to_string(),
            ];
            if !model.trim().is_empty() {
                fallback.push("--model".to_string());
                fallback.push(model.trim().to_string());
            }
            run_command(
                binary,
                &fallback,
                cli_settings,
                CliKind::Grok,
                work_dir,
                None,
                timeout,
            )?
        }
        other => other?,
    };

    let _ = std::fs::remove_file(&prompt_path);
    ensure_success("Grok CLI", &output)?;
    parse_rewritten_text(&stdout_string(&output))
}

fn run_command(
    binary: &Path,
    args: &[String],
    cli_settings: &CliHarnessSettings,
    kind: CliKind,
    work_dir: &Path,
    stdin_text: Option<&str>,
    timeout: Duration,
) -> Result<Output, String> {
    debug!(
        "Spawning {} ({}) with {} args",
        kind.label(),
        binary.display(),
        args.len()
    );

    let mut command = Command::new(binary);
    command
        .args(args)
        .current_dir(work_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    apply_config_dir_env(&mut command, kind, cli_settings);

    let mut child = command.spawn().map_err(|e| {
        format!(
            "Failed to spawn {} ({}): {e}",
            kind.label(),
            binary.display()
        )
    })?;

    if let Some(mut stdin) = child.stdin.take() {
        if let Some(text) = stdin_text {
            stdin
                .write_all(text.as_bytes())
                .map_err(|e| format!("Failed to write prompt to {} stdin: {e}", kind.label()))?;
        }
    }

    wait_with_timeout(child, timeout, kind.label())
}

fn wait_with_timeout(
    child: std::process::Child,
    timeout: Duration,
    label: &str,
) -> Result<Output, String> {
    let pid = child.id();
    let (tx, rx) = mpsc::channel();
    let handle = std::thread::spawn(move || {
        let result = child.wait_with_output();
        let _ = tx.send(());
        result
    });

    match rx.recv_timeout(timeout) {
        Ok(()) => handle
            .join()
            .map_err(|_| format!("{label} worker thread panicked"))?
            .map_err(|e| format!("{label} failed: {e}")),
        Err(_) => {
            kill_process(pid);
            let _ = handle.join();
            Err(format!(
                "{label} timed out after {} seconds",
                timeout.as_secs()
            ))
        }
    }
}

fn kill_process(pid: u32) {
    #[cfg(unix)]
    {
        // SIGKILL: a SIGTERM can be ignored by a wrapper shell waiting on `sleep`.
        let _ = Command::new("/bin/kill")
            .args(["-9", &pid.to_string()])
            .status();
    }
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/F"])
            .status();
    }
}

fn apply_config_dir_env(command: &mut Command, kind: CliKind, cli_settings: &CliHarnessSettings) {
    let config_dir = cli_settings.config_dir.trim();
    if config_dir.is_empty() {
        return;
    }
    command.env(kind.config_dir_env(), expand_user_path(config_dir));
}

fn requested_binary(kind: CliKind, cli_settings: &CliHarnessSettings) -> String {
    let custom = cli_settings.binary_path.trim();
    if custom.is_empty() {
        kind.binary_name().to_string()
    } else {
        custom.to_string()
    }
}

fn resolve_binary(kind: CliKind, cli_settings: &CliHarnessSettings) -> Option<PathBuf> {
    let requested = requested_binary(kind, cli_settings);
    resolve_binary_path(&requested)
}

pub fn resolve_binary_path(name_or_path: &str) -> Option<PathBuf> {
    let trimmed = name_or_path.trim();
    if trimmed.is_empty() {
        return None;
    }

    let path = PathBuf::from(expand_user_path(trimmed));
    if path.is_absolute() || trimmed.contains('/') || trimmed.contains('\\') {
        return if is_executable(&path) {
            Some(path)
        } else {
            None
        };
    }

    for dir in search_path_dirs() {
        let candidate = dir.join(trimmed);
        if is_executable(&candidate) {
            return Some(candidate);
        }
        #[cfg(windows)]
        {
            let exe = dir.join(format!("{trimmed}.exe"));
            if is_executable(&exe) {
                return Some(exe);
            }
        }
    }
    None
}

fn search_path_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let mut seen = std::collections::HashSet::new();

    let push =
        |dirs: &mut Vec<PathBuf>, seen: &mut std::collections::HashSet<PathBuf>, dir: PathBuf| {
            if !dir.as_os_str().is_empty() && seen.insert(dir.clone()) {
                dirs.push(dir);
            }
        };

    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            push(&mut dirs, &mut seen, dir);
        }
    }

    let home = home_dir();
    let extras = [
        "/opt/homebrew/bin",
        "/usr/local/bin",
        "/usr/bin",
        "/bin",
        "/opt/homebrew/sbin",
    ];
    for extra in extras {
        push(&mut dirs, &mut seen, PathBuf::from(extra));
    }
    if let Some(home) = home {
        for extra in [
            ".local/bin",
            ".claude/local",
            "bin",
            ".npm-global/bin",
            "Library/Application Support/fnm/aliases/default/bin",
        ] {
            push(&mut dirs, &mut seen, home.join(extra));
        }
    }
    dirs
}

fn is_executable(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata()
            .map(|meta| meta.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

fn expand_user_path(path: &str) -> String {
    if path == "~" {
        return home_dir()
            .map(|home| home.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string());
    }
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = home_dir() {
            return home.join(rest).to_string_lossy().into_owned();
        }
    }
    if let Some(rest) = path.strip_prefix("~\\") {
        if let Some(home) = home_dir() {
            return home.join(rest).to_string_lossy().into_owned();
        }
    }
    path.to_string()
}

fn make_temp_dir(prefix: &str) -> Result<PathBuf, String> {
    let path = temp_path(prefix);
    std::fs::create_dir_all(&path).map_err(|e| format!("Failed to create temp directory: {e}"))?;
    Ok(path)
}

fn temp_file(prefix: &str, contents: &str) -> Result<PathBuf, String> {
    let path = temp_path(prefix);
    std::fs::write(&path, contents).map_err(|e| format!("Failed to write temp file: {e}"))?;
    Ok(path)
}

fn temp_path(prefix: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("{prefix}-{}-{nanos}", std::process::id()))
}

fn path_to_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn stdout_string(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr_string(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn combined_text(output: &Output) -> String {
    format!("{}\n{}", stdout_string(output), stderr_string(output))
}

fn ensure_success(label: &str, output: &Output) -> Result<(), String> {
    if output.status.success() {
        return Ok(());
    }
    let detail = first_useful_line(&combined_text(output))
        .unwrap_or_else(|| format!("exit {}", output.status));
    Err(format!("{label} failed: {detail}"))
}

fn unknown_flag(output: &Output, flag: &str) -> bool {
    let text = combined_text(output).to_lowercase();
    !output.status.success()
        && text.contains(&flag.to_lowercase())
        && (text.contains("unexpected argument")
            || text.contains("unknown")
            || text.contains("unrecognized"))
}

fn first_useful_line(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(|line| truncate_for_error(line, 240))
}

fn truncate_for_error(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars {
        return text.to_string();
    }
    let trimmed: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{trimmed}…")
}

fn looks_like_auth_failure(text: &str) -> bool {
    let text = text.to_lowercase();
    text.contains("not logged in")
        || text.contains("not authenticated")
        || text.contains("please log in")
        || text.contains("please login")
        || text.contains("run `claude auth login`")
        || text.contains("run `codex login`")
        || text.contains("run `grok login`")
        || text.contains("unauthenticated")
        || (text.contains("auth") && text.contains("required"))
}

/// Parse CLI stdout / output-file contents into rewritten transcript text.
/// Prefers `{transcription}`, then common envelopes (`structured_output`,
/// `result`, `text`), then raw trimmed text.
pub fn parse_rewritten_text(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("CLI returned empty output".to_string());
    }

    if let Some(value) = extract_json_value(trimmed) {
        if let Some(text) = transcription_from_value(&value) {
            if !text.trim().is_empty() {
                return Ok(text);
            }
        }
    }

    Ok(trimmed.to_string())
}

fn extract_json_value(raw: &str) -> Option<Value> {
    let without_fence = strip_code_fence(raw);
    if let Ok(value) = serde_json::from_str::<Value>(without_fence) {
        return Some(value);
    }
    let start = without_fence.find('{')?;
    let end = without_fence.rfind('}')?;
    serde_json::from_str(&without_fence[start..=end]).ok()
}

fn strip_code_fence(raw: &str) -> &str {
    let trimmed = raw.trim();
    let body = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```JSON"))
        .or_else(|| trimmed.strip_prefix("```"))
        .unwrap_or(trimmed);
    body.strip_suffix("```").unwrap_or(body).trim()
}

fn transcription_from_value(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Array(items) => items.iter().rev().find_map(transcription_from_value),
        Value::Object(map) => {
            if let Some(structured) = map.get("structured_output") {
                if let Some(text) = transcription_from_value(structured) {
                    return Some(text);
                }
            }
            for key in [TRANSCRIPTION_FIELD, "result", "text", "message", "content"] {
                if let Some(Value::String(text)) = map.get(key) {
                    if !text.trim().is_empty() {
                        return Some(text.clone());
                    }
                }
            }
            None
        }
        _ => None,
    }
}

fn probe_blocking(provider_id: &str, cli_settings: &CliHarnessSettings) -> CliHarnessStatus {
    let Some(kind) = CliKind::from_provider_id(provider_id) else {
        return CliHarnessStatus {
            provider_id: provider_id.to_string(),
            binary_name: String::new(),
            resolved_binary: None,
            binary_found: false,
            logged_in: None,
            message: format!("Unknown CLI provider '{provider_id}'."),
            login_hint: String::new(),
        };
    };

    let binary_name = requested_binary(kind, cli_settings);
    let resolved = resolve_binary(kind, cli_settings);
    let login_hint = kind.login_hint().to_string();

    let Some(resolved) = resolved else {
        return CliHarnessStatus {
            provider_id: provider_id.to_string(),
            binary_name: binary_name.clone(),
            resolved_binary: None,
            binary_found: false,
            logged_in: None,
            message: format!(
                "{} ('{binary_name}') was not found on PATH. Install it or set a binary path.",
                kind.label()
            ),
            login_hint,
        };
    };

    let version = run_probe_command(&resolved, kind, cli_settings, &["--version"]);
    if let Some(output) = version.as_ref() {
        if !output.status.success() && looks_like_auth_failure(&combined_text(output)) {
            return CliHarnessStatus {
                provider_id: provider_id.to_string(),
                binary_name,
                resolved_binary: Some(resolved.display().to_string()),
                binary_found: true,
                logged_in: Some(false),
                message: format!(
                    "{} is installed but does not appear to be logged in.",
                    kind.label()
                ),
                login_hint,
            };
        }
    }

    let auth_args: &[&str] = match kind {
        CliKind::Claude => &["auth", "status"],
        CliKind::Codex => &["login", "status"],
        CliKind::Grok => &["models"],
    };
    let logged_in = match run_probe_command(&resolved, kind, cli_settings, auth_args) {
        Some(output) if unknown_flag(&output, auth_args[0]) => None,
        Some(output) if looks_like_auth_failure(&combined_text(&output)) => Some(false),
        Some(output) if output.status.success() => {
            let text = combined_text(&output).to_lowercase();
            if text.contains("logged in") || text.contains("authenticated") || kind == CliKind::Grok
            {
                Some(true)
            } else if text.contains("logged out") {
                Some(false)
            } else {
                Some(true)
            }
        }
        Some(output) if !output.status.success() => {
            if looks_like_auth_failure(&combined_text(&output)) {
                Some(false)
            } else {
                None
            }
        }
        _ => None,
    };

    let message = match logged_in {
        Some(true) => format!(
            "{} is installed at {} and appears to be logged in.",
            kind.label(),
            resolved.display()
        ),
        Some(false) => format!(
            "{} is installed at {} but is not logged in.",
            kind.label(),
            resolved.display()
        ),
        None => format!(
            "{} is installed at {}. Login could not be verified automatically — try a post-process dictation, or run the login command if it fails.",
            kind.label(),
            resolved.display()
        ),
    };

    CliHarnessStatus {
        provider_id: provider_id.to_string(),
        binary_name,
        resolved_binary: Some(resolved.display().to_string()),
        binary_found: true,
        logged_in,
        message,
        login_hint,
    }
}

fn run_probe_command(
    binary: &Path,
    kind: CliKind,
    cli_settings: &CliHarnessSettings,
    args: &[&str],
) -> Option<Output> {
    let work_dir = make_temp_dir("handy-cli-probe").ok()?;
    let args: Vec<String> = args.iter().map(|s| (*s).to_string()).collect();
    let result = run_command(
        binary,
        &args,
        cli_settings,
        kind,
        &work_dir,
        None,
        Duration::from_secs(AUTH_PROBE_TIMEOUT_SECS),
    );
    let _ = std::fs::remove_dir_all(&work_dir);
    match result {
        Ok(output) => Some(output),
        Err(err) => {
            debug!("CLI probe command failed: {err}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    #[cfg(unix)]
    use std::os::unix::process::ExitStatusExt;

    #[cfg(unix)]
    fn write_script(contents: &str) -> PathBuf {
        let path = temp_file("handy-cli-test-bin", contents).expect("temp script");
        let mut perms = fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&path, perms).unwrap();
        path
    }

    #[test]
    fn build_cli_prompt_replaces_output_placeholder() {
        let prompt = build_cli_prompt("Clean: ${output}", "hello world");
        assert_eq!(prompt, "Clean: hello world");
    }

    #[test]
    fn build_cli_prompt_appends_when_placeholder_missing() {
        let prompt = build_cli_prompt("Clean this.", "hello");
        assert_eq!(prompt, "Clean this.\n\nhello");
    }

    #[test]
    fn parse_claude_structured_output() {
        let raw = r#"{
            "type": "result",
            "result": "ignored wrapper",
            "structured_output": { "transcription": "Cleaned text" }
        }"#;
        assert_eq!(parse_rewritten_text(raw).unwrap(), "Cleaned text");
    }

    #[test]
    fn parse_codex_schema_object() {
        let raw = r#"{ "transcription": "From Codex" }"#;
        assert_eq!(parse_rewritten_text(raw).unwrap(), "From Codex");
    }

    #[test]
    fn parse_grok_text_envelope() {
        let raw = r#"{ "text": "From Grok" }"#;
        assert_eq!(parse_rewritten_text(raw).unwrap(), "From Grok");
    }

    #[test]
    fn parse_result_array_uses_last_message() {
        let raw = r#"[{"type":"assistant"},{"type":"result","structured_output":{"transcription":"Last"}}]"#;
        assert_eq!(parse_rewritten_text(raw).unwrap(), "Last");
    }

    #[test]
    fn parse_plain_text_fallback() {
        assert_eq!(parse_rewritten_text("just text").unwrap(), "just text");
    }

    #[test]
    fn parse_fenced_json() {
        let raw = "```json\n{\"transcription\":\"Fenced\"}\n```";
        assert_eq!(parse_rewritten_text(raw).unwrap(), "Fenced");
    }

    #[test]
    fn empty_output_is_error() {
        assert!(parse_rewritten_text("   ").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn unknown_flag_detects_ephemeral_rejection() {
        let output = Output {
            status: std::process::ExitStatus::from_raw(2),
            stdout: Vec::new(),
            stderr: b"error: unexpected argument '--ephemeral' found".to_vec(),
        };
        assert!(unknown_flag(&output, "--ephemeral"));
        assert!(!unknown_flag(&output, "--output-schema"));
    }

    #[cfg(unix)]
    #[test]
    fn resolve_binary_path_finds_explicit_script() {
        let script = write_script("#!/bin/sh\necho ok\n");
        assert_eq!(
            resolve_binary_path(&script.to_string_lossy()).as_deref(),
            Some(script.as_path())
        );
        let _ = fs::remove_file(script);
    }

    #[cfg(unix)]
    #[test]
    fn rewrite_via_fake_claude_script() {
        let script = write_script(
            r#"#!/bin/sh
cat >/dev/null
printf '%s\n' '{"structured_output":{"transcription":"rewritten"}}'
"#,
        );
        let settings = CliHarnessSettings {
            binary_path: script.to_string_lossy().into_owned(),
            config_dir: String::new(),
            timeout_secs: 15,
        };
        let text = rewrite_transcription_blocking(CliKind::Claude, &settings, "", "Clean: hello")
            .expect("fake claude should succeed");
        assert_eq!(text, "rewritten");
        let _ = fs::remove_file(script);
    }

    #[cfg(unix)]
    #[test]
    fn rewrite_via_fake_codex_output_file() {
        let script = write_script(
            r#"#!/bin/sh
output=""
while [ $# -gt 0 ]; do
  if [ "$1" = "--output-last-message" ]; then
    shift
    output="$1"
  fi
  shift
done
printf '%s\n' '{"transcription":"codex-out"}' > "$output"
"#,
        );
        let settings = CliHarnessSettings {
            binary_path: script.to_string_lossy().into_owned(),
            config_dir: String::new(),
            timeout_secs: 15,
        };
        let text = rewrite_transcription_blocking(CliKind::Codex, &settings, "gpt-5", "prompt")
            .expect("fake codex should succeed");
        assert_eq!(text, "codex-out");
        let _ = fs::remove_file(script);
    }

    #[cfg(unix)]
    #[test]
    fn rewrite_via_fake_grok_prompt_file() {
        let script = write_script(
            r#"#!/bin/sh
# Mirror Grok 1.0.5: -p/--single requires a prompt value. The structured
# path must use --prompt-file alone (no bare -p).
prompt=""
while [ $# -gt 0 ]; do
  case "$1" in
    -p|--single)
      shift
      if [ -z "$1" ] || [ "${1#-}" != "$1" ]; then
        printf '%s\n' "error: a value is required for '--single <PROMPT>' but none was supplied" >&2
        exit 2
      fi
      printf '%s\n' "error: structured invoke must not pass --single; use --prompt-file" >&2
      exit 3
      ;;
    --prompt-file)
      shift
      prompt="$1"
      ;;
  esac
  shift
done
test -n "$prompt"
printf '%s\n' '{"text":"grok-out"}'
"#,
        );
        let settings = CliHarnessSettings {
            binary_path: script.to_string_lossy().into_owned(),
            config_dir: String::new(),
            timeout_secs: 15,
        };
        let text = rewrite_transcription_blocking(CliKind::Grok, &settings, "", "hello")
            .expect("fake grok should succeed");
        assert_eq!(text, "grok-out");
        let _ = fs::remove_file(script);
    }

    #[cfg(unix)]
    #[test]
    fn rewrite_via_fake_grok_falls_back_to_single_prompt() {
        let script = write_script(
            r#"#!/bin/sh
# Older / minimal Grok: reject --prompt-file, accept -p/--single PROMPT.
single=""
while [ $# -gt 0 ]; do
  case "$1" in
    --prompt-file|--json-schema)
      printf '%s\n' "error: unexpected argument '$1' found" >&2
      exit 2
      ;;
    -p|--single)
      shift
      if [ -z "$1" ] || [ "${1#-}" != "$1" ]; then
        printf '%s\n' "error: a value is required for '--single <PROMPT>' but none was supplied" >&2
        exit 2
      fi
      single="$1"
      ;;
  esac
  shift
done
test -n "$single"
printf '%s\n' '{"text":"grok-fallback"}'
"#,
        );
        let settings = CliHarnessSettings {
            binary_path: script.to_string_lossy().into_owned(),
            config_dir: String::new(),
            timeout_secs: 15,
        };
        let text = rewrite_transcription_blocking(CliKind::Grok, &settings, "", "hello")
            .expect("fake grok fallback should succeed");
        assert_eq!(text, "grok-fallback");
        let _ = fs::remove_file(script);
    }

    #[test]
    fn missing_binary_errors_with_login_hint() {
        let settings = CliHarnessSettings {
            binary_path: "/definitely/missing/claude-binary".to_string(),
            config_dir: String::new(),
            timeout_secs: 15,
        };
        let err = rewrite_transcription_blocking(CliKind::Claude, &settings, "", "x")
            .expect_err("missing binary should fail");
        assert!(err.contains("not found"), "{err}");
        assert!(err.contains("claude auth login"), "{err}");
    }

    #[test]
    fn probe_reports_missing_binary() {
        let settings = CliHarnessSettings {
            binary_path: "/definitely/missing/grok-binary".to_string(),
            ..CliHarnessSettings::default()
        };
        let status = probe_blocking(GROK_CLI_PROVIDER_ID, &settings);
        assert!(!status.binary_found);
        assert!(status.message.contains("not found"));
        assert!(status.login_hint.contains("grok login"));
    }

    #[cfg(unix)]
    #[test]
    fn probe_detects_not_logged_in() {
        let script = write_script(
            r#"#!/bin/sh
if [ "$1" = "auth" ]; then
  echo "Not logged in. Run claude auth login." >&2
  exit 1
fi
echo "claude 1.0.0"
"#,
        );
        let settings = CliHarnessSettings {
            binary_path: script.to_string_lossy().into_owned(),
            ..CliHarnessSettings::default()
        };
        let status = probe_blocking(CLAUDE_CODE_CLI_PROVIDER_ID, &settings);
        assert!(status.binary_found);
        assert_eq!(status.logged_in, Some(false));
        let _ = fs::remove_file(script);
    }

    #[cfg(unix)]
    #[test]
    fn timeout_kills_hung_cli() {
        let script = write_script("#!/bin/sh\nexec sleep 30\n");
        let settings = CliHarnessSettings {
            binary_path: script.to_string_lossy().into_owned(),
            config_dir: String::new(),
            timeout_secs: 10,
        };
        let started = Instant::now();
        // Override by calling run_claude with a short timeout directly.
        let work_dir = make_temp_dir("handy-cli-timeout").unwrap();
        let err = run_claude(
            &PathBuf::from(&settings.binary_path),
            &settings,
            "",
            "hi",
            "{}",
            &work_dir,
            Duration::from_millis(400),
        )
        .expect_err("hung CLI should time out");
        let _ = fs::remove_dir_all(&work_dir);
        let _ = fs::remove_file(&settings.binary_path);
        assert!(err.to_lowercase().contains("timed out"), "{err}");
        assert!(
            started.elapsed() < Duration::from_secs(8),
            "timeout took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn clamp_timeout_bounds() {
        assert_eq!(clamp_timeout_secs(1), MIN_CLI_TIMEOUT_SECS);
        assert_eq!(clamp_timeout_secs(90), 90);
        assert_eq!(clamp_timeout_secs(10_000), MAX_CLI_TIMEOUT_SECS);
    }
}
