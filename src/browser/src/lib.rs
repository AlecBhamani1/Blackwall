//! The browser process has no network capability. Core brokers each resource over stdio.
use blackwall_core::web::{
    renderer_unavailable, PageRenderer, RenderNetwork, RenderedPage, WebError,
};
use futures_util::future::BoxFuture;
use std::{
    path::{Path, PathBuf},
    process::Stdio,
};
#[cfg(unix)]
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
const SCRIPT: &str = include_str!("renderer.cjs");
pub const PLAYWRIGHT_VERSION: &str = "1.64.1";
static RENDER_SLOT: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);

pub struct PlaywrightRenderer {
    directory: PathBuf,
}
impl PlaywrightRenderer {
    pub fn new(directory: PathBuf) -> Self {
        Self { directory }
    }
    pub fn supported() -> bool {
        cfg!(target_os = "macos") && Path::new("/usr/bin/sandbox-exec").is_file()
    }
    pub fn installed(&self) -> bool {
        std::fs::read_to_string(self.directory.join("installed-version"))
            .ok()
            .is_some_and(|version| version == PLAYWRIGHT_VERSION)
            && std::fs::read(self.directory.join("node_modules/playwright/package.json"))
                .ok()
                .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
                .is_some_and(|package| package["version"] == PLAYWRIGHT_VERSION)
    }
    /// Explicit owner action only: installs a pinned runtime and its matching Chromium revision.
    pub async fn install(&self) -> Result<(), String> {
        if !Self::supported() {
            return Err("Isolated rendering currently requires macOS sandbox-exec.".into());
        }
        let _slot = RENDER_SLOT
            .acquire()
            .await
            .map_err(|_| "Browser installation is unavailable.")?;
        tokio::fs::create_dir_all(&self.directory)
            .await
            .map_err(|_| "Could not create browser runtime directory.")?;
        for (program, args) in [
            (
                "npm",
                vec![
                    "install".to_owned(),
                    "--ignore-scripts".into(),
                    "--no-audit".into(),
                    "--no-fund".into(),
                    format!("playwright@{PLAYWRIGHT_VERSION}"),
                ],
            ),
            (
                "node",
                vec![
                    "node_modules/playwright/cli.js".into(),
                    "install".into(),
                    "chromium".into(),
                ],
            ),
        ] {
            let mut command = tokio::process::Command::new(program);
            command
                .args(args)
                .current_dir(&self.directory)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .kill_on_drop(true);
            blackwall_core::apply_owner_env(&mut command, &[]);
            command.env("PLAYWRIGHT_BROWSERS_PATH", self.directory.join("browsers"));
            let mut child = command
                .spawn()
                .map_err(|_| "Install Node.js 22+ and npm, then retry browser installation.")?;
            let status = tokio::time::timeout(std::time::Duration::from_secs(240), child.wait())
                .await
                .map_err(|_| "Browser installation timed out. Retry in Settings → Web access.")?
                .map_err(|_| "Browser installation could not finish.")?;
            if !status.success() {
                return Err("Browser installation failed. Check your network and Node.js/npm installation, then retry.".into());
            }
        }
        tokio::fs::write(self.directory.join("installed-version"), PLAYWRIGHT_VERSION)
            .await
            .map_err(|_| "Browser installation could not be recorded.")?;
        Ok(())
    }
}
// The process group is never reaped before Drop, so its ID cannot be recycled while owned.
#[cfg(unix)]
struct Process {
    child: tokio::process::Child,
    group: rustix::process::Pid,
}
#[cfg(unix)]
impl Drop for Process {
    fn drop(&mut self) {
        let _ = rustix::process::kill_process_group(self.group, rustix::process::Signal::KILL);
        let _ = self.child.start_kill();
    }
}
impl PageRenderer for PlaywrightRenderer {
    fn render<'a>(
        &'a self,
        url: &'a str,
        network: &'a dyn RenderNetwork,
    ) -> BoxFuture<'a, Result<RenderedPage, WebError>> {
        Box::pin(async move {
            let fail = || renderer_unavailable(url);
            if !Self::supported() || !self.installed() {
                return Err(fail());
            }
            let _permit = RENDER_SLOT.try_acquire().map_err(|_| {
                let mut error = fail();
                error.message = "Another browser render or installation is running. Wait for it to finish before rendering this page.".into();
                error
            })?;
            #[cfg(unix)]
            {
                let mut command = tokio::process::Command::new("/usr/bin/sandbox-exec");
                command
                    .args([
                        "-p",
                        "(version 1)(allow default)(deny network*)",
                        "node",
                        "-e",
                        SCRIPT,
                    ])
                    .arg(url)
                    .current_dir(&self.directory)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null())
                    .kill_on_drop(true);
                blackwall_core::apply_owner_env(&mut command, &[]);
                command.env("PLAYWRIGHT_BROWSERS_PATH", self.directory.join("browsers"));
                command.process_group(0);
                let child = command.spawn().map_err(|_| fail())?;
                let group = child
                    .id()
                    .and_then(|id| i32::try_from(id).ok())
                    .and_then(rustix::process::Pid::from_raw)
                    .ok_or_else(fail)?;
                let mut process = Process { child, group };
                let mut stdin = process.child.stdin.take().ok_or_else(fail)?;
                let stdout = process.child.stdout.take().ok_or_else(fail)?;
                let mut reader = BufReader::new(stdout);
                // Adapter protocol: <=100 resource requests plus one final bounded DOM.
                for _ in 0..=100 {
                    let mut line = Vec::new();
                    let count = (&mut reader)
                        .take(3 * 1024 * 1024)
                        .read_until(b'\n', &mut line)
                        .await
                        .map_err(|_| fail())?;
                    if count == 0 || line.last() != Some(&b'\n') {
                        return Err(fail());
                    }
                    let message: serde_json::Value =
                        serde_json::from_slice(&line).map_err(|_| fail())?;
                    if message["type"] == "done" {
                        return serde_json::from_value(message["page"].clone()).map_err(|_| fail());
                    }
                    if message["type"] == "failure" {
                        let (category, message) = match message["status"].as_u64() {
                            Some(401) => (blackwall_core::web::WebErrorCategory::Authentication, "The rendered page requires login. Use a public source; Blackwall does not share browser logins."),
                            Some(403) => (blackwall_core::web::WebErrorCategory::SiteDenied, "The rendered page refused access. Use another public source."),
                            Some(429) => (blackwall_core::web::WebErrorCategory::RateLimited, "The rendered page is rate-limited. Wait before trying again."),
                            Some(408) => (blackwall_core::web::WebErrorCategory::Timeout, "Browser rendering exceeded its bounded wait. Use another public source."),
                            Some(509) => (blackwall_core::web::WebErrorCategory::SizeLimit, "Browser rendering exceeded 100 resource requests. Use another public source."),
                            Some(508) => (blackwall_core::web::WebErrorCategory::RedirectLoop, "The rendered resource redirects in a loop. Use another source."),
                            Some(310) => (blackwall_core::web::WebErrorCategory::RedirectLimit, "The rendered resource exceeded eight redirects. Use its final public URL."),
                            Some(413) => (blackwall_core::web::WebErrorCategory::SizeLimit, "The rendered DOM exceeded 2 MiB. Use another source."),
                            _ => (blackwall_core::web::WebErrorCategory::Http, "The rendered source returned an unsuccessful response."),
                        };
                        let mut error = fail();
                        error.category = category;
                        error.message = message.into();
                        return Err(error);
                    }
                    if message["type"] != "request" {
                        return Err(fail());
                    }
                    let resource_url = message["url"]
                        .as_str()
                        .filter(|u| u.len() <= 4096)
                        .ok_or_else(fail)?;
                    let response = match network.get(resource_url).await {
                        Ok(resource) => {
                            serde_json::json!({"id": message["id"], "resource": resource})
                        }
                        Err(_) => serde_json::json!({"id": message["id"], "denied": true}),
                    };
                    let mut bytes = serde_json::to_vec(&response).map_err(|_| fail())?;
                    bytes.push(b'\n');
                    stdin.write_all(&bytes).await.map_err(|_| fail())?;
                }
                Err(fail())
            }
            #[cfg(not(unix))]
            {
                let _ = (network, SCRIPT, Stdio::null());
                Err(fail())
            }
        })
    }
}
