use std::{borrow::Cow, ops::Deref, sync::Arc, time::Duration};

use base64::Engine;
use chaser_oxide::{
    Browser, BrowserConfig, ChaserPage, cdp::browser_protocol::page::CaptureScreenshotParams,
};
use futures::StreamExt;
use rmcp::{
    ServiceExt,
    handler::server::wrapper::Parameters,
    model::{CallToolResult, ContentBlock},
    schemars,
};
use tokio::{
    sync::{RwLock, RwLockReadGuard},
    task::JoinHandle,
};

mod error;
mod snapshot;

use error::Error;

pub struct Guard<'g, A, T> {
    #[allow(unused)]
    lock: RwLockReadGuard<'g, A>,
    data: T,
}

impl<'g, A, T> Deref for Guard<'g, A, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.data
    }
}

#[derive(Debug, Clone)]
pub struct BrowserMcp {
    browser: Arc<Browser>,
    current_tab: Arc<RwLock<ChaserPage>>,
}

impl BrowserMcp {
    pub async fn new() -> Result<(Self, JoinHandle<anyhow::Result<()>>), Error> {
        let (browser, mut handler) = Browser::launch(
            BrowserConfig::builder()
                .viewport(None)
                .build()
                .map_err(|e| Error::FailedToBuildConfig(e))?,
        )
        .await?;

        let handle = tokio::spawn(async move {
            while let Some(r) = handler.next().await {
                r?;
            }

            Ok(())
        });

        // Wait for window to open

        let version = browser.version().await?;
        tracing::info!("Running version: {version:?}");

        let start_tab = loop {
            if let Some(page) = browser.pages().await?.pop() {
                break page;
            }

            tokio::time::sleep(Duration::from_millis(100)).await;
        };

        let chaser = ChaserPage::new(start_tab.clone());
        chaser
            .apply_native_profile()
            .await
            .expect("Failed to apply native profile");

        Ok((
            BrowserMcp {
                browser: Arc::new(browser),
                current_tab: Arc::new(RwLock::const_new(chaser)),
            },
            handle,
        ))
    }
}

impl Drop for BrowserMcp {
    fn drop(&mut self) {
        Arc::get_mut(&mut self.browser).map(|b| b.close());
    }
}

#[derive(Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
struct NavigateParams {
    /// The url to naviagte to
    url: String,
}

#[derive(Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
struct EvaluateToolArg {
    /// The script we want to evaluate on the current page
    script: Cow<'static, str>,
}

#[rmcp::tool_router]
impl BrowserMcp {
    #[rmcp::tool(description = "Navigate to a url on the current tab")]
    async fn navigate(
        &self,
        Parameters(args): Parameters<NavigateParams>,
    ) -> Result<String, Error> {
        let page = self.current_tab.read().await;
        page.goto(&args.url).await?;

        Ok("Success".into())
    }

    #[rmcp::tool(
        description = "Evaluate a expression or function on the current page and get its response"
    )]
    async fn evaluate(
        &self,
        Parameters(arg): Parameters<EvaluateToolArg>,
    ) -> Result<CallToolResult, Error> {
        let page = self.current_tab.read().await;

        if let Some(r) = page.evaluate_stealth(&*arg.script).await? {
            Ok(CallToolResult::structured(r))
        } else {
            Ok(CallToolResult::success(vec![]))
        }
    }

    #[rmcp::tool(description = "Get a screenshot of the current page")]
    async fn screenshot(&self) -> Result<CallToolResult, Error> {
        let page = self.current_tab.read().await;
        let image = page
            .raw_page()
            .screenshot(CaptureScreenshotParams {
                format: None,
                quality: Some(100),
                clip: None,
                from_surface: None,
                capture_beyond_viewport: None,
                optimize_for_speed: None,
            })
            .await?;

        Ok(CallToolResult::success(vec![ContentBlock::image(
            base64::engine::general_purpose::STANDARD.encode(image),
            "image/png",
        )]))
    }
}

#[rmcp::tool_handler(
    name = "rbrowser-mcp",
    version = "0.0.1",
    instructions = "A MCP to allow the agent to interact with the browser"
)]
impl rmcp::ServerHandler for BrowserMcp {}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    tracing::info!("Starting MCP server!");

    let (mcp, handle) = BrowserMcp::new().await?;

    tracing::info!("Launched the browser!");

    let service = mcp.serve(rmcp::transport::stdio()).await?;

    tracing::info!("MCP server started!");

    service.waiting().await?;

    handle.await??;

    Ok(())
}
