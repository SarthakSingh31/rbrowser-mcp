use std::{path::PathBuf, time::Duration};

use chromiumoxide::{
    Browser, BrowserConfig, Page,
    browser::HeadlessMode,
    cdp::browser_protocol::{
        network::{CookieParam, TimeSinceEpoch},
        target::{CreateTargetParams, EventTargetDestroyed},
    },
};
use futures::{Stream, StreamExt};
use tempfile::TempDir;
use tokio::task::JoinHandle;

use crate::error::Error;

const WINDOW_SIZE: (u32, u32) = (1280, 800);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Headless,
    Visible,
}

impl std::fmt::Display for Mode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Mode::Headless => "headless",
            Mode::Visible => "visible",
        })
    }
}

/// A running Chrome process. Headless and visible pages live in separate processes because
/// Chrome decides headless-ness at launch.
#[derive(Debug)]
pub struct BrowserInstance {
    browser: Browser,
    handler: JoinHandle<()>,
    user_agent: String,
    /// Each process gets its own profile so several servers (and both modes) can run at once.
    _profile: TempDir,
}

impl BrowserInstance {
    pub async fn launch(mode: Mode, executable: Option<&PathBuf>) -> Result<Self, Error> {
        let profile = tempfile::Builder::new().prefix("rbrowser-mcp-").tempdir()?;

        let chrome = crate::reaper::chrome_path(executable).map_err(Error::FailedToBuildConfig)?;
        let config = BrowserConfig::builder()
            .user_data_dir(profile.path())
            .window_size(WINDOW_SIZE.0, WINDOW_SIZE.1)
            .viewport(None)
            .headless_mode(match mode {
                Mode::Headless => HeadlessMode::New,
                Mode::Visible => HeadlessMode::False,
            })
            // Pages are opened explicitly, so don't show an empty startup window.
            .arg("--no-startup-window");
        let config = crate::reaper::wrap(config, &chrome);

        let (browser, mut handler) =
            Browser::launch(config.build().map_err(Error::FailedToBuildConfig)?).await?;

        let handler = tokio::spawn(async move {
            while let Some(r) = handler.next().await {
                if let Err(e) = r {
                    tracing::warn!("Browser handler error: {e}");
                }
            }
        });

        let user_agent = browser
            .user_agent()
            .await?
            .replace("HeadlessChrome", "Chrome");

        tracing::info!("Launched {mode} browser: {user_agent}");

        Ok(Self {
            browser,
            handler,
            user_agent,
            _profile: profile,
        })
    }

    /// Whether the browser process is still connected. A visible browser exits when the user
    /// closes its last window.
    pub fn is_alive(&self) -> bool {
        !self.handler.is_finished()
    }

    /// Open `url` in a new window with stealth patches applied before the first navigation.
    pub async fn open_window(&self, url: &str) -> Result<Page, Error> {
        let page = self
            .browser
            .new_page(
                CreateTargetParams::builder()
                    .url("about:blank")
                    .new_window(true)
                    .width(WINDOW_SIZE.0 as i64)
                    .height(WINDOW_SIZE.1 as i64)
                    .build()
                    .map_err(Error::FailedToBuildConfig)?,
            )
            .await?;
        page.enable_stealth_mode_with_agent(&self.user_agent)
            .await?;
        if url != "about:blank" {
            page.goto(url).await?;
        }

        Ok(page)
    }

    /// Stream of target ids destroyed in this browser, e.g. a window closed by the user.
    pub async fn destroyed_targets(&self) -> Result<impl Stream<Item = String> + use<>, Error> {
        Ok(self
            .browser
            .event_listener::<EventTargetDestroyed>()
            .await?
            .map(|e| e.target_id.inner().clone()))
    }

    pub async fn cookies(&self) -> Result<Vec<CookieParam>, Error> {
        Ok(self
            .browser
            .get_cookies()
            .await?
            .into_iter()
            .map(|c| CookieParam {
                name: c.name,
                value: c.value,
                url: None,
                domain: Some(c.domain),
                path: Some(c.path),
                secure: Some(c.secure),
                http_only: Some(c.http_only),
                same_site: c.same_site,
                expires: (!c.session).then_some(TimeSinceEpoch::new(c.expires)),
                priority: Some(c.priority),
                source_scheme: Some(c.source_scheme),
                source_port: Some(c.source_port),
                partition_key: c.partition_key,
            })
            .collect())
    }

    pub async fn set_cookies(&self, cookies: Vec<CookieParam>) -> Result<(), Error> {
        if !cookies.is_empty() {
            self.browser.set_cookies(cookies).await?;
        }
        Ok(())
    }

    /// Close the browser and wait for the process to exit.
    pub async fn shutdown(mut self) {
        if let Err(e) = self.browser.close().await {
            tracing::debug!("Failed to close browser gracefully: {e}");
        }
        if tokio::time::timeout(Duration::from_secs(5), self.browser.wait())
            .await
            .is_err()
        {
            tracing::warn!("Browser did not exit in time, killing it");
            self.browser.kill().await;
        }
        self.handler.abort();
    }
}
