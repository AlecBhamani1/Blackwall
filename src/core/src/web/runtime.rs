//! Owner configuration and adapter boundaries shared by Chat and Agent.
use super::{WebError, WebErrorCategory};
use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

#[derive(Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    #[default]
    Duckduckgo,
    Brave,
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WebSettings {
    pub provider: Provider,
    pub brave_fallback: bool,
    pub rendering: bool,
}
impl WebSettings {
    pub async fn load(directory: &Path) -> Result<Self, String> {
        match tokio::fs::read(directory.join("web.json")).await {
            Ok(bytes) if bytes.len() <= 4096 => serde_json::from_slice(&bytes).map_err(|_| {
                "Web settings are invalid. Open Settings → Web access and save them again.".into()
            }),
            Ok(_) => Err("Web settings exceed their size limit.".into()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(_) => Err("Web settings could not be read.".into()),
        }
    }
    pub async fn save(&self, directory: &Path) -> Result<(), String> {
        tokio::fs::create_dir_all(directory)
            .await
            .map_err(|_| "Web settings could not be saved.")?;
        let bytes =
            serde_json::to_vec_pretty(self).map_err(|_| "Web settings could not be encoded.")?;
        let temporary = directory.join("web.json.tmp");
        tokio::fs::write(&temporary, bytes)
            .await
            .map_err(|_| "Web settings could not be saved.")?;
        tokio::fs::rename(temporary, directory.join("web.json"))
            .await
            .map_err(|_| "Web settings could not be saved.".into())
    }
}

/// Credentials stay inside the runtime; neither settings nor IPC ever return a saved key.
pub trait SearchCredentials: Send + Sync {
    fn brave_key(&self) -> BoxFuture<'_, Result<Option<String>, String>>;
}

#[derive(Debug, Deserialize, Serialize)]
pub struct RenderedPage {
    pub url: String,
    pub html: String,
}
#[derive(Serialize, Deserialize)]
pub struct Resource {
    pub status: u16,
    pub content_type: String,
    pub body: String,
    pub location: Option<String>,
}
/// The adapter must deny browser-process network access at the OS boundary. Every allowed
/// request is fulfilled through this broker, with no browser headers, cookies or credentials.
pub trait RenderNetwork: Send + Sync {
    fn get<'a>(&'a self, url: &'a str) -> BoxFuture<'a, Result<Resource, crate::tools::ToolError>>;
}
pub trait PageRenderer: Send + Sync {
    fn render<'a>(
        &'a self,
        url: &'a str,
        network: &'a dyn RenderNetwork,
    ) -> BoxFuture<'a, Result<RenderedPage, WebError>>;
}
pub struct WebRuntime {
    pub directory: PathBuf,
    pub credentials: Arc<dyn SearchCredentials>,
    pub renderer: Option<Arc<dyn PageRenderer>>,
    challenged: Mutex<HashSet<String>>,
    #[cfg(test)]
    pub(crate) test_brave: Option<Arc<dyn super::SearchProvider>>,
    #[cfg(test)]
    pub(crate) test_duckduckgo: Option<Arc<dyn super::SearchProvider>>,
}
impl WebRuntime {
    pub fn new(
        directory: PathBuf,
        credentials: Arc<dyn SearchCredentials>,
        renderer: Option<Arc<dyn PageRenderer>>,
    ) -> Self {
        Self {
            directory,
            credentials,
            renderer,
            challenged: Mutex::default(),
            #[cfg(test)]
            test_brave: None,
            #[cfg(test)]
            test_duckduckgo: None,
        }
    }
    pub(crate) async fn search_brave(
        &self,
        query: &str,
    ) -> Result<Vec<super::SearchResult>, WebError> {
        #[cfg(test)]
        if let Some(provider) = &self.test_brave {
            return provider.search(query).await;
        }
        super::search::configured_brave(self.credentials.as_ref(), query).await
    }
    pub(crate) fn challenged(&self, run: &str) -> bool {
        self.challenged
            .lock()
            .map(|s| s.contains(run))
            .unwrap_or(true)
    }
    pub(crate) fn block_challenge(&self, run: &str) {
        if let Ok(mut state) = self.challenged.lock() {
            state.insert(run.to_owned());
        }
    }
    pub(crate) fn clear_run(&self, run: &str) {
        if let Ok(mut state) = self.challenged.lock() {
            state.remove(run);
        }
    }
}
pub fn renderer_unavailable(url: &str) -> WebError {
    WebError { category: WebErrorCategory::RendererUnavailable, source: reqwest::Url::parse(url).map(|u| super::error::source_url(&u)).unwrap_or_default(), message: "Browser rendering is unavailable. Open Settings → Web access to install/update the isolated browser on macOS, or use another public source. Opening the source in your browser does not share its login with Blackwall.".into() }
}
