#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::io::{BufRead, BufReader};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tauri::{AppHandle, Emitter, Manager};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

const PORT: u16 = 3080;

fn url() -> String {
    format!("http://127.0.0.1:{PORT}")
}

struct ServerState {
    pid: Mutex<Option<u32>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
enum LaunchSource {
    Npx,
    Local,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct AppSettings {
    source: LaunchSource,
    local_path: String,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            source: LaunchSource::Local,
            local_path: default_local_path(),
        }
    }
}

fn default_local_path() -> String {
    #[cfg(windows)]
    {
        r"H:\github\deepseek-harness".to_string()
    }
    #[cfg(not(windows))]
    {
        let home = std::env::var("HOME").unwrap_or_default();
        format!("{home}/github/deepseek-harness")
    }
}

#[derive(Clone, serde::Serialize)]
struct Status {
    running: bool,
    port: u16,
    url: String,
    source: LaunchSource,
}

fn status_of(app: &AppHandle, running: bool) -> Status {
    Status {
        running,
        port: PORT,
        url: url(),
        source: load_settings(app).source,
    }
}

#[derive(Clone, serde::Serialize)]
struct UpgradeResult {
    ok: bool,
    version: String,
    restarted: bool,
    message: String,
}

fn settings_path(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("无法解析应用数据目录：{e}"))?;
    Ok(dir.join("settings.json"))
}

fn load_settings(app: &AppHandle) -> AppSettings {
    let Ok(path) = settings_path(app) else {
        return AppSettings::default();
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return AppSettings::default();
    };
    serde_json::from_str(&text).unwrap_or_default()
}

fn save_settings(app: &AppHandle, settings: &AppSettings) -> Result<(), String> {
    let path = settings_path(app)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("无法创建设置目录：{e}"))?;
    }
    let text = serde_json::to_string_pretty(settings)
        .map_err(|e| format!("无法序列化设置：{e}"))?;
    std::fs::write(&path, text).map_err(|e| format!("无法写入设置：{e}"))
}

fn validate_local_repo(path: &str) -> Result<PathBuf, String> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return Err("本地仓库路径不能为空".to_string());
    }
    let dir = PathBuf::from(trimmed);
    if !dir.is_dir() {
        return Err(format!("本地仓库不存在：{trimmed}"));
    }
    if !dir.join("package.json").is_file() || !dir.join("apps").join("cli").is_dir() {
        return Err(format!(
            "不是有效的 DeepSeek Harness 仓库（缺少 package.json 或 apps/cli）：{trimmed}"
        ));
    }
    Ok(dir)
}

/// Base tool launcher for non-Windows (macOS/Linux). Prefers an explicit
/// fnm path so the app also works when launched from Finder/Dock, where the
/// GUI process has a minimal PATH without node/npx/pnpm; falls back to PATH.
#[cfg(not(windows))]
fn base_tool_cmd(tool: &str) -> Command {
    for fnm in [
        "/opt/homebrew/bin/fnm",
        "/opt/homebrew/opt/fnm/bin/fnm",
        "/usr/local/bin/fnm",
    ] {
        if Path::new(fnm).is_file() {
            let mut c = Command::new(fnm);
            c.args(["exec", "--using", "default", "--", tool]);
            return c;
        }
    }
    Command::new(tool)
}

/// Windows: keep the child console hidden so no cmd window flashes up.
#[cfg(windows)]
fn hide_console(c: &mut Command) {
    use std::os::windows::process::CommandExt;
    c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
}

/// Windows: GUI-launched processes may inherit a stale PATH (Node.js not
/// visible), and Rust's Command::new("npx") cannot resolve the .cmd batch
/// shim the way cmd.exe does. So run through "cmd /C <tool> ..." — exactly
/// like a user typing it in a terminal — and merge standard Node/pnpm/git
/// install directories into PATH as a safety net.
#[cfg(windows)]
fn base_tool_cmd(tool: &str) -> Command {
    let mut c = Command::new("cmd");
    c.args(["/C", tool]);
    augment_path_for_tools(&mut c);
    hide_console(&mut c);
    c
}

#[cfg(windows)]
fn augment_path_for_tools(c: &mut Command) {
    let mut dirs: Vec<String> = Vec::new();
    for var in ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"] {
        if let Ok(base) = std::env::var(var) {
            let candidate = if var == "LOCALAPPDATA" {
                format!("{base}\\Programs\\nodejs")
            } else {
                format!("{base}\\nodejs")
            };
            if Path::new(&candidate).is_dir() {
                dirs.push(candidate);
            }
        }
    }
    if let Ok(appdata) = std::env::var("APPDATA") {
        let npm = format!("{appdata}\\npm");
        if Path::new(&npm).is_dir() {
            dirs.push(npm);
        }
    }
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        let pnpm = format!("{local}\\pnpm");
        if Path::new(&pnpm).is_dir() {
            dirs.push(pnpm);
        }
    }
    for git in [r"C:\Program Files\Git\cmd", r"C:\Program Files (x86)\Git\cmd"] {
        if Path::new(git).is_dir() {
            dirs.push(git.to_string());
        }
    }
    if dirs.is_empty() {
        return;
    }
    let mut path = dirs.join(";");
    if let Ok(p) = std::env::var("PATH") {
        if !p.is_empty() {
            path.push(';');
            path.push_str(&p);
        }
    }
    c.env("PATH", path);
}

fn git_cmd() -> Command {
    #[cfg(windows)]
    {
        base_tool_cmd("git")
    }
    #[cfg(not(windows))]
    {
        Command::new("git")
    }
}

fn dsh_command(settings: &AppSettings) -> Result<Command, String> {
    match settings.source {
        LaunchSource::Npx => {
            let mut c = base_tool_cmd("npx");
            c.args(["--yes", "@deepseek-ai/dsh", "web"]);
            Ok(c)
        }
        LaunchSource::Local => {
            let dir = validate_local_repo(&settings.local_path)?;
            let mut c = base_tool_cmd("pnpm");
            c.args(["dsh", "web"]);
            c.current_dir(dir);
            Ok(c)
        }
    }
}

fn version_command(settings: &AppSettings) -> Result<Command, String> {
    match settings.source {
        LaunchSource::Npx => {
            // NOTE: no "--" separator — npx passes everything after the package
            // name to the binary, and dsh would receive "-- --version".
            let mut c = base_tool_cmd("npx");
            c.args(["--yes", "@deepseek-ai/dsh", "--version"]);
            c.env("npm_config_update_notifier", "false");
            Ok(c)
        }
        LaunchSource::Local => {
            let dir = validate_local_repo(&settings.local_path)?;
            let mut c = base_tool_cmd("pnpm");
            c.args(["dsh", "--version"]);
            c.current_dir(dir);
            Ok(c)
        }
    }
}

fn npx_upgrade_command() -> Command {
    // @latest tag makes npx fetch the newest release (the --latest flag is
    // not a valid npx option and only produces npm warnings).
    let mut c = base_tool_cmd("npx");
    c.args(["--yes", "@deepseek-ai/dsh@latest", "--version"]);
    c.env("npm_config_update_notifier", "false");
    c
}

fn local_upgrade_steps(dir: &Path) -> [(&'static str, Command); 3] {
    let mut pull = git_cmd();
    pull.args(["pull"]);
    pull.current_dir(dir);

    let mut install = base_tool_cmd("pnpm");
    install.args(["install"]);
    install.current_dir(dir);

    let mut build = base_tool_cmd("pnpm");
    build.args(["run", "build"]);
    build.current_dir(dir);

    [("git pull", pull), ("pnpm install", install), ("pnpm run build", build)]
}

fn extract_version(lines: &[String]) -> Option<String> {
    // Scan from the last line backwards for the first semver pattern, so it
    // works with bare "0.1.0", "v0.1.0", "dsh 0.1.0-rc.6", npm download lines
    // ("...dsh-0.1.0.tgz"), etc.
    for line in lines.iter().rev() {
        if let Some(v) = find_semver(line) {
            return Some(v);
        }
    }
    None
}

/// Find a semver-like pattern (digits.digits.digits with optional
/// -prerelease / +build suffix) anywhere inside a line.
fn find_semver(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let n = b.len();
    let mut i = 0;
    while i < n {
        if !b[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let start = i;
        let mut j = i;
        let mut dots = 0u32;
        while j < n {
            if b[j].is_ascii_digit() {
                j += 1;
            } else if b[j] == b'.' && dots < 2 && j + 1 < n && b[j + 1].is_ascii_digit() {
                dots += 1;
                j += 1;
            } else {
                break;
            }
        }
        if dots == 2 {
            let mut end = j;
            if end < n && b[end] == b'-' {
                let mut k = end + 1;
                while k < n
                    && (b[k].is_ascii_alphanumeric() || b[k] == b'.' || b[k] == b'-')
                {
                    k += 1;
                }
                end = k;
            }
            if end > start {
                return Some(s[start..end].to_string());
            }
        }
        i = if j > i { j } else { i + 1 };
    }
    None
}

fn is_port_open(port: u16) -> bool {
    TcpStream::connect(("127.0.0.1", port)).is_ok()
}

#[cfg(unix)]
fn kill_process_group(pid: u32) {
    unsafe { libc::kill(-(pid as i32), libc::SIGTERM); }
    std::thread::sleep(Duration::from_millis(400));
    unsafe { libc::kill(-(pid as i32), libc::SIGKILL); }
}

#[cfg(not(unix))]
fn kill_process_group(pid: u32) {
    let mut c = Command::new("taskkill");
    c.args(["/PID", &pid.to_string(), "/T", "/F"]);
    hide_console(&mut c);
    let _ = c.status();
}

fn pipe_lines<R: std::io::Read + Send + 'static>(
    reader: R,
    app: AppHandle,
    event: &'static str,
    collected: Option<std::sync::Arc<Mutex<Vec<String>>>>,
) {
    std::thread::spawn(move || {
        for line in BufReader::new(reader).lines() {
            match line {
                Ok(l) => {
                    let _ = app.emit(event, &l);
                    if let Some(buf) = &collected {
                        buf.lock().unwrap().push(l);
                    }
                }
                Err(_) => break,
            }
        }
    });
}

fn start_internal(app: &AppHandle) -> Result<Status, String> {
    {
        let state = app.state::<ServerState>();
        if state.pid.lock().unwrap().is_some() {
            return Ok(status_of(app, true));
        }
    }

    // Port already serving (e.g. the user's browser dsh session)? Reuse it
    // instead of spawning a duplicate that would fail with EADDRINUSE.
    if is_port_open(PORT) {
        let _ = app.emit("server:ready", ());
        return Ok(status_of(app, true));
    }

    let settings = load_settings(app);
    let mut cmd = dsh_command(&settings)?;
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(unix)]
    {
        cmd.process_group(0);
    }

    let spawn_err = match settings.source {
        LaunchSource::Npx => "无法启动 npx @deepseek-ai/dsh web".to_string(),
        LaunchSource::Local => format!(
            "无法启动 pnpm dsh web（{}）",
            settings.local_path.trim()
        ),
    };
    let mut child = cmd.spawn().map_err(|e| format!("{spawn_err}：{e}"))?;
    let pid = child.id();

    if let Some(stdout) = child.stdout.take() {
        pipe_lines(stdout, app.clone(), "server:stdout", None);
    }
    if let Some(stderr) = child.stderr.take() {
        pipe_lines(stderr, app.clone(), "server:stderr", None);
    }

    {
        let state = app.state::<ServerState>();
        let mut guard = state.pid.lock().unwrap();
        *guard = Some(pid);
    }

    // watcher: clear state and notify on exit
    {
        let app = app.clone();
        std::thread::spawn(move || {
            let code = child.wait().ok().and_then(|s| s.code());
            let state = app.state::<ServerState>();
            let mut guard = state.pid.lock().unwrap();
            *guard = None;
            drop(guard);
            let _ = app.emit("server:exited", code);
        });
    }

    // readiness polling; aborts early when the process exits before the
    // port opens (e.g. node/npx missing) so the UI reports failure at once
    {
        let app = app.clone();
        std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(90);
            loop {
                if is_port_open(PORT) {
                    let _ = app.emit("server:ready", ());
                    return;
                }
                if app.state::<ServerState>().pid.lock().unwrap().is_none() {
                    return;
                }
                if Instant::now() >= deadline {
                    let _ = app.emit("server:timeout", ());
                    return;
                }
                std::thread::sleep(Duration::from_millis(250));
            }
        });
    }

    Ok(status_of(app, true))
}

#[tauri::command]
fn get_settings(app: AppHandle) -> AppSettings {
    load_settings(&app)
}

#[tauri::command]
fn set_settings(app: AppHandle, settings: AppSettings) -> Result<AppSettings, String> {
    if settings.source == LaunchSource::Local {
        validate_local_repo(&settings.local_path)?;
    }
    save_settings(&app, &settings)?;
    Ok(settings)
}

#[tauri::command]
fn start_server(app: AppHandle) -> Result<Status, String> {
    start_internal(&app)
}

#[tauri::command]
fn stop_server(app: AppHandle) -> Status {
    let state = app.state::<ServerState>();
    let pid = {
        let mut guard = state.pid.lock().unwrap();
        guard.take()
    };
    match pid {
        Some(pid) => {
            kill_process_group(pid);
            let _ = app.emit("server:stopped", ());
            status_of(&app, false)
        }
        // Nothing we own: reflect whether 3080 is still up (reused server).
        None => status_of(&app, is_port_open(PORT)),
    }
}

#[tauri::command]
fn restart_server(app: AppHandle) -> Result<Status, String> {
    stop_server(app.clone());
    std::thread::sleep(Duration::from_millis(600));
    start_internal(&app)
}

fn run_logged_command(
    app: &AppHandle,
    mut cmd: Command,
    collected: &std::sync::Arc<Mutex<Vec<String>>>,
) -> Result<bool, String> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(unix)]
    {
        cmd.process_group(0);
    }
    let mut child = cmd.spawn().map_err(|e| format!("无法启动命令：{e}"))?;
    if let Some(stdout) = child.stdout.take() {
        pipe_lines(
            stdout,
            app.clone(),
            "upgrade:stdout",
            Some(collected.clone()),
        );
    }
    if let Some(stderr) = child.stderr.take() {
        pipe_lines(
            stderr,
            app.clone(),
            "upgrade:stderr",
            Some(collected.clone()),
        );
    }
    let exit = child.wait();
    Ok(matches!(exit, Ok(s) if s.success()))
}

fn restart_after_upgrade(app: &AppHandle) -> (bool, String) {
    let owns = app.state::<ServerState>().pid.lock().unwrap().is_some();
    if owns {
        stop_server(app.clone());
        std::thread::sleep(Duration::from_millis(600));
        match start_internal(app) {
            Ok(_) => (true, "升级完成，服务已用新版本重启".to_string()),
            Err(e) => (false, format!("升级成功，但重启失败：{e}，请手动点击重启")),
        }
    } else {
        (
            false,
            "升级完成，已安装最新版（当前无本应用运行的服务，下次启动即生效）".to_string(),
        )
    }
}

fn upgrade_dsh_inner(app: AppHandle) -> Result<UpgradeResult, String> {
    let settings = load_settings(&app);
    let collected: std::sync::Arc<Mutex<Vec<String>>> =
        std::sync::Arc::new(Mutex::new(Vec::new()));

    let ok = match settings.source {
        LaunchSource::Npx => {
            let cmd = npx_upgrade_command();
            run_logged_command(&app, cmd, &collected).map_err(|e| {
                format!("无法启动升级命令（npx --yes @deepseek-ai/dsh@latest --version）：{e}")
            })?
        }
        LaunchSource::Local => {
            let dir = validate_local_repo(&settings.local_path)?;
            let mut all_ok = true;
            for (label, cmd) in local_upgrade_steps(&dir) {
                let _ = app.emit("upgrade:stdout", format!("$ {label}"));
                collected.lock().unwrap().push(format!("$ {label}"));
                match run_logged_command(&app, cmd, &collected) {
                    Ok(true) => {}
                    Ok(false) => {
                        all_ok = false;
                        break;
                    }
                    Err(e) => return Err(format!("无法启动 {label}：{e}")),
                }
            }
            all_ok
        }
    };

    std::thread::sleep(Duration::from_millis(150));
    let lines = collected.lock().unwrap().clone();
    let version = extract_version(&lines).unwrap_or_else(|| "unknown".to_string());

    let (restarted, message) = if ok {
        restart_after_upgrade(&app)
    } else {
        let fail = match settings.source {
            LaunchSource::Npx => "升级失败，请检查网络后重试".to_string(),
            LaunchSource::Local => {
                "升级失败（git pull / pnpm install / pnpm run build），请查看 CLI 日志".to_string()
            }
        };
        (false, fail)
    };

    Ok(UpgradeResult {
        ok,
        version,
        restarted,
        message,
    })
}

#[tauri::command]
async fn upgrade_dsh(app: AppHandle) -> Result<UpgradeResult, String> {
    tauri::async_runtime::spawn_blocking(move || upgrade_dsh_inner(app))
        .await
        .map_err(|e| format!("升级任务失败：{e}"))?
}

/// Report the DeepSeek Harness (dsh) CLI version that will actually run.
#[tauri::command]
async fn dsh_version(app: AppHandle) -> Result<String, String> {
    let settings = load_settings(&app);
    let mut cmd = version_command(&settings)?;
    cmd.stdout(Stdio::piped()).stderr(Stdio::null());
    #[cfg(unix)]
    {
        cmd.process_group(0);
    }
    let child = cmd
        .spawn()
        .map_err(|e| format!("无法获取 dsh 版本：{e}"))?;
    let out = child
        .wait_with_output()
        .map_err(|e| format!("读取 dsh 版本失败：{e}"))?;
    let text = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<String> = text.lines().map(|s| s.to_string()).collect();
    let version = extract_version(&lines).unwrap_or_else(|| "unknown".to_string());
    Ok(version)
}

#[tauri::command]
fn server_status(app: AppHandle) -> Status {
    let state = app.state::<ServerState>();
    let running = state.pid.lock().unwrap().is_some() || is_port_open(PORT);
    status_of(&app, running)
}

fn main() {
    tauri::Builder::default()
        .manage(ServerState { pid: Mutex::new(None) })
        .invoke_handler(tauri::generate_handler![
            get_settings,
            set_settings,
            start_server,
            stop_server,
            restart_server,
            server_status,
            upgrade_dsh,
            dsh_version
        ])
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { .. } = event {
                window.app_handle().exit(0);
            }
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app_handle, event| {
            if let tauri::RunEvent::ExitRequested { .. } | tauri::RunEvent::Exit = event {
                let state = app_handle.state::<ServerState>();
                let pid = {
                    let mut guard = state.pid.lock().unwrap();
                    guard.take()
                };
                if let Some(pid) = pid {
                    kill_process_group(pid);
                }
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_roundtrip_uses_camel_case() {
        let settings = AppSettings {
            source: LaunchSource::Local,
            local_path: r"H:\github\deepseek-harness".to_string(),
        };
        let json = serde_json::to_string(&settings).unwrap();
        assert!(json.contains("\"source\":\"local\""));
        assert!(json.contains("\"localPath\""));
        let parsed: AppSettings = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.source, LaunchSource::Local);
        assert_eq!(parsed.local_path, settings.local_path);
    }

    #[test]
    fn validate_local_repo_rejects_empty_and_missing() {
        assert!(validate_local_repo("").is_err());
        assert!(validate_local_repo("   ").is_err());
        assert!(validate_local_repo("Z:\\definitely-not-a-dsh-repo").is_err());
    }
}
