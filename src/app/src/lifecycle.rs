//! Keep the macOS main window alive when closed, and restore it from the Dock.

use tauri::{AppHandle, Manager, RunEvent, Window, WindowEvent};

pub fn on_window_event(window: &Window, event: &WindowEvent) {
    if window.label() == "main" {
        if let WindowEvent::CloseRequested { api, .. } = event {
            // Preserve the webview and running work. Explicit Quit still exits the app.
            api.prevent_close();
            if let Err(error) = window.hide() {
                eprintln!("Blackwall could not hide its main window: {error}");
            }
        }
    }
}

pub fn on_run_event(app: &AppHandle, event: RunEvent) {
    if let RunEvent::Reopen { .. } = event {
        if let Some(window) = app.get_webview_window("main") {
            if let Err(error) = window.unminimize() {
                eprintln!("Blackwall could not unminimize its main window: {error}");
            }
            if let Err(error) = window.show() {
                eprintln!("Blackwall could not show its main window: {error}");
            }
            if let Err(error) = window.set_focus() {
                eprintln!("Blackwall could not focus its main window: {error}");
            }
        }
    }
}

/// Set once the login-shell `PATH` has been adopted or abandoned.
static LOGIN_PATH_SETTLED: std::sync::OnceLock<()> = std::sync::OnceLock::new();

/// Apps opened from the Dock start with only the system directories on `PATH`, so programs
/// installed with Homebrew or npm are missing for MCP servers, command tools, and shell commands.
/// Ask the owner's login shell for its `PATH` in the background. The shell and anything its profile
/// started are stopped after five seconds at most, leaving the default `PATH` in place.
pub fn adopt_login_path() {
    std::thread::spawn(|| {
        if let Some(path) = login_path() {
            blackwall_core::tools::set_search_path(path.into());
        }
        let _ = LOGIN_PATH_SETTLED.set(());
    });
}

/// Agent runs wait, at most five seconds, until the login-shell `PATH` is known.
pub async fn login_path_settled() {
    for _ in 0..50 {
        if LOGIN_PATH_SETTLED.get().is_some() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

fn login_path() -> Option<String> {
    use std::{
        io::BufRead,
        os::unix::process::CommandExt,
        process::{Command, Stdio},
    };
    const START: &str = "__BLACKWALL_PATH__";
    const END: &str = "__BLACKWALL_PATH_END__";
    let shell = std::env::var_os("SHELL")
        .filter(|shell| !shell.is_empty())
        .unwrap_or_else(|| "/bin/zsh".into());
    let mut command = Command::new(shell);
    command
        // awk reads the exported, colon-separated `PATH`, which fish would join with spaces.
        .args([
            "-ilc",
            r#"/usr/bin/awk 'BEGIN { printf "\n__BLACKWALL_PATH__%s__BLACKWALL_PATH_END__\n", ENVIRON["PATH"] }'"#,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0);
    let mut child = command.spawn().ok()?;
    let stdout = child.stdout.take()?;
    let (sender, receiver) = std::sync::mpsc::channel();
    // Read only until the delimited line, so profile output before or after it is ignored.
    std::thread::spawn(move || {
        let found = std::io::BufReader::new(stdout)
            .lines()
            .take(4096)
            .map_while(Result::ok)
            .find_map(|line| {
                let path = line.split_once(START)?.1.split_once(END)?.0.to_owned();
                Some(path)
            });
        let _ = sender.send(found);
    });
    let path = receiver
        .recv_timeout(std::time::Duration::from_secs(5))
        .ok()
        .flatten();
    // Fixed executable and numeric process-group argument, as for shell commands.
    let _ = Command::new("/bin/kill")
        .args(["-KILL", "--", &format!("-{}", child.id())])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = child.wait();
    path.filter(|path| !path.is_empty())
}
