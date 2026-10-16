use std::{
    collections::{BTreeMap, HashMap},
    path::PathBuf,
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use chromiumoxide::{
    Page,
    cdp::{
        browser_protocol::page::{CaptureScreenshotParams, CreateIsolatedWorldParams},
        js_protocol::runtime::EvaluateParams as RuntimeEvaluateParams,
    },
};
use futures::StreamExt;
use rmcp::{
    handler::server::wrapper::Parameters,
    model::{CallToolResult, ContentBlock},
    schemars,
};
use tokio::sync::Mutex;

use crate::{
    browser::{BrowserInstance, Mode},
    error::Error,
};

/// Upper bound for any single page operation, so one stuck CDP call can't wedge a page forever.
const OP_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone)]
pub struct Config {
    pub default_mode: Mode,
    pub max_pages: usize,
    pub chrome_path: Option<PathBuf>,
}

#[derive(Debug)]
struct PageSlot {
    page: Page,
    mode: Mode,
}

/// A page the agent knows by id. The underlying CDP page is swapped out when it moves between
/// the headless and visible browsers; the per-page lock also serialises operations on it.
#[derive(Debug)]
struct PageHandle {
    slot: Mutex<PageSlot>,
}

#[derive(Debug, Default)]
struct Registry {
    pages: BTreeMap<u64, Arc<PageHandle>>,
    /// CDP target id -> (page id, mode), used to forget pages whose window was closed.
    targets: HashMap<String, (u64, Mode)>,
}

impl Registry {
    fn count(&self, mode: Mode) -> usize {
        self.targets.values().filter(|(_, m)| *m == mode).count()
    }
}

#[derive(Debug)]
struct Inner {
    config: Config,
    headless: Mutex<Option<BrowserInstance>>,
    visible: Mutex<Option<BrowserInstance>>,
    registry: StdMutex<Registry>,
    next_id: AtomicU64,
}

#[derive(Debug, Clone)]
pub struct BrowserMcp {
    inner: Arc<Inner>,
}

fn page_label(id: u64) -> String {
    format!("p{id}")
}

fn parse_page_id(id: &str) -> Result<u64, Error> {
    id.strip_prefix('p')
        .unwrap_or(id)
        .parse()
        .map_err(|_| Error::PageNotFound(id.to_owned()))
}

async fn timed<T>(fut: impl Future<Output = Result<T, Error>>) -> Result<T, Error> {
    tokio::time::timeout(OP_TIMEOUT, fut)
        .await
        .map_err(|_| Error::Timeout)?
}

/// Evaluate `script` in an isolated world so page scripts can't observe it and `Runtime.enable`
/// is never sent.
pub async fn evaluate_isolated(
    page: &Page,
    script: &str,
) -> Result<Option<serde_json::Value>, Error> {
    let frame_id = page.mainframe().await?.ok_or(Error::NotFound)?;
    let world = page
        .execute(
            CreateIsolatedWorldParams::builder()
                .frame_id(frame_id)
                .world_name("rbrowser")
                .grant_univeral_access(true)
                .build()
                .map_err(Error::FailedToBuildConfig)?,
        )
        .await?;
    let res = page
        .execute(
            RuntimeEvaluateParams::builder()
                .expression(script)
                .context_id(world.result.execution_context_id)
                .await_promise(true)
                .return_by_value(true)
                .build()
                .map_err(Error::FailedToBuildConfig)?,
        )
        .await?;

    if let Some(exception) = &res.result.exception_details {
        return Err(Error::JavascriptException(Box::new(exception.clone())));
    }

    Ok(res.result.result.value.clone())
}

async fn describe(page: &Page) -> String {
    let url = page.url().await.ok().flatten().unwrap_or_default();
    match page.get_title().await.ok().flatten() {
        Some(title) if !title.is_empty() => format!("{url} \"{title}\""),
        _ => url,
    }
}

impl BrowserMcp {
    pub fn new(config: Config) -> Self {
        Self {
            inner: Arc::new(Inner {
                config,
                headless: Mutex::new(None),
                visible: Mutex::new(None),
                registry: StdMutex::new(Registry::default()),
                next_id: AtomicU64::new(1),
            }),
        }
    }

    fn browser_slot(&self, mode: Mode) -> &Mutex<Option<BrowserInstance>> {
        match mode {
            Mode::Headless => &self.inner.headless,
            Mode::Visible => &self.inner.visible,
        }
    }

    fn registry(&self) -> std::sync::MutexGuard<'_, Registry> {
        self.inner.registry.lock().expect("registry lock poisoned")
    }

    fn page(&self, id: &str) -> Result<(u64, Arc<PageHandle>), Error> {
        let num = parse_page_id(id)?;
        let handle = self
            .registry()
            .pages
            .get(&num)
            .cloned()
            .ok_or_else(|| Error::PageNotFound(id.to_owned()))?;
        Ok((num, handle))
    }

    /// Launch the browser for `mode` if it isn't running. Must be called with its slot locked.
    async fn ensure_browser<'a>(
        &self,
        slot: &'a mut Option<BrowserInstance>,
        mode: Mode,
    ) -> Result<&'a BrowserInstance, Error> {
        if slot.as_ref().is_some_and(|b| !b.is_alive()) {
            tracing::info!("{mode} browser exited, relaunching");
            if let Some(dead) = slot.take() {
                dead.shutdown().await;
            }
            let mut registry = self.registry();
            registry.targets.retain(|_, (_, m)| *m != mode);
            let Registry { pages, targets } = &mut *registry;
            pages.retain(|id, _| targets.values().any(|(p, _)| p == id));
        }

        if slot.is_none() {
            let instance =
                BrowserInstance::launch(mode, self.inner.config.chrome_path.as_ref()).await?;
            self.watch_targets(&instance, mode).await?;
            *slot = Some(instance);
        }

        Ok(slot.as_ref().expect("browser was just launched"))
    }

    /// Forget pages whose window was closed outside of close_page (e.g. by the user).
    async fn watch_targets(&self, instance: &BrowserInstance, mode: Mode) -> Result<(), Error> {
        let mut destroyed = instance.destroyed_targets().await?;
        let this = self.clone();
        tokio::spawn(async move {
            while let Some(target) = destroyed.next().await {
                let removed = {
                    let mut registry = this.registry();
                    match registry.targets.remove(&target) {
                        Some((id, _)) => registry.pages.remove(&id).map(|_| id),
                        None => None,
                    }
                };
                if let Some(id) = removed {
                    tracing::info!("Page {} was closed by the user", page_label(id));
                    this.release_if_idle(mode).await;
                }
            }
        });
        Ok(())
    }

    /// Shut down the browser for `mode` once it has no pages left.
    async fn release_if_idle(&self, mode: Mode) {
        let mut slot = self.browser_slot(mode).lock().await;
        if self.registry().count(mode) == 0
            && let Some(instance) = slot.take()
        {
            tracing::info!("No {mode} pages left, closing the {mode} browser");
            instance.shutdown().await;
        }
    }

    /// Open a window for `url` in the `mode` browser and register it as a new page. Registration
    /// happens with the browser slot locked so `release_if_idle` can't close the browser under it.
    async fn open_in(&self, mode: Mode, url: &str) -> Result<(u64, Arc<PageHandle>), Error> {
        let mut slot = self.browser_slot(mode).lock().await;
        let browser = self.ensure_browser(&mut slot, mode).await?;
        let page = match browser.open_window(url).await {
            Ok(page) => page,
            Err(e) => {
                if self.registry().count(mode) == 0
                    && let Some(instance) = slot.take()
                {
                    instance.shutdown().await;
                }
                return Err(e);
            }
        };

        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let target = page.target_id().inner().clone();
        let handle = Arc::new(PageHandle {
            slot: Mutex::new(PageSlot { page, mode }),
        });
        let mut registry = self.registry();
        registry.targets.insert(target, (id, mode));
        registry.pages.insert(id, handle.clone());
        Ok((id, handle))
    }

    fn summary(&self) -> String {
        let registry = self.registry();
        let mut lines = Vec::new();
        for (id, handle) in &registry.pages {
            let mode = registry
                .targets
                .values()
                .find(|(p, _)| p == id)
                .map(|(_, m)| m.to_string())
                .unwrap_or_default();
            let busy = if handle.slot.try_lock().is_err() {
                " (busy)"
            } else {
                ""
            };
            lines.push(format!("- {} [{mode}]{busy}", page_label(*id)));
        }
        lines.join("\n")
    }

    pub async fn shutdown(&self) {
        for mode in [Mode::Headless, Mode::Visible] {
            if let Some(instance) = self.browser_slot(mode).lock().await.take() {
                instance.shutdown().await;
            }
        }
        let mut registry = self.registry();
        registry.pages.clear();
        registry.targets.clear();
    }

    /// Move a page to the browser for `to`, carrying over its URL, cookies and localStorage.
    async fn move_page(&self, id: &str, to: Mode) -> Result<String, Error> {
        let (num, handle) = self.page(id)?;
        let mut slot = handle.slot.lock().await;
        if slot.mode == to {
            return Ok(format!("Page {} is already {to}.", page_label(num)));
        }
        let from = slot.mode;

        let url = slot
            .page
            .url()
            .await?
            .unwrap_or_else(|| "about:blank".into());
        let local_storage =
            evaluate_isolated(&slot.page, "JSON.stringify(Object.entries(localStorage))")
                .await
                .ok()
                .flatten()
                .and_then(|v| v.as_str().map(str::to_owned))
                .filter(|s| s != "[]");
        let cookies = match self.browser_slot(from).lock().await.as_ref() {
            Some(browser) => browser.cookies().await?,
            None => Vec::new(),
        };

        let new_page = {
            let mut target_slot = self.browser_slot(to).lock().await;
            let browser = self.ensure_browser(&mut target_slot, to).await?;
            browser.set_cookies(cookies).await?;
            let page = browser.open_window(&url).await?;

            if let Some(entries) = local_storage {
                let script =
                    format!("for (const [k, v] of {entries}) localStorage.setItem(k, v); true");
                if evaluate_isolated(&page, &script).await.is_ok() {
                    page.reload().await?;
                }
            }

            let mut registry = self.registry();
            registry.targets.remove(slot.page.target_id().inner());
            registry
                .targets
                .insert(page.target_id().inner().clone(), (num, to));
            page
        };

        if to == Mode::Visible {
            new_page.bring_to_front().await?;
        }
        let old = std::mem::replace(&mut slot.page, new_page);
        slot.mode = to;
        drop(slot);

        if let Err(e) = old.close().await {
            tracing::debug!("Failed to close old page: {e}");
        }
        self.release_if_idle(from).await;

        Ok(format!(
            "Page {} is now {to} at {url}. In-memory page state (sessionStorage, form input, \
             history, running scripts) was not carried over; cookies and localStorage were.",
            page_label(num)
        ))
    }
}

#[derive(Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
struct OpenPageParams {
    /// The url to open in the new page
    url: String,
}

#[derive(Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
struct PageParams {
    /// The id of the page, as returned by open_page (e.g. "p1")
    page_id: String,
}

#[derive(Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
struct NavigateParams {
    /// The id of the page, as returned by open_page (e.g. "p1")
    page_id: String,
    /// The url to navigate to
    url: String,
}

#[derive(Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
struct EvaluateToolArgs {
    /// The id of the page, as returned by open_page (e.g. "p1")
    page_id: String,
    /// The expression to evaluate. Promises are awaited.
    script: String,
}

#[rmcp::tool_router]
impl BrowserMcp {
    #[rmcp::tool(
        description = "Open a url in a new browser window and return its page id. Pages are \
                       headless (not visible to the user) unless shown with show_page. Every call \
                       opens another window: close pages with close_page once you no longer need \
                       them."
    )]
    async fn open_page(
        &self,
        Parameters(args): Parameters<OpenPageParams>,
    ) -> Result<String, Error> {
        let mode = self.inner.config.default_mode;
        let (id, handle) = timed(self.open_in(mode, &args.url)).await?;
        let description = describe(&handle.slot.lock().await.page).await;
        let open = self.registry().pages.len();

        let mut message = format!(
            "Opened page {} ({mode}): {description}\n\nOpen pages:\n{}\n\nClose any page you are \
             no longer using with close_page.",
            page_label(id),
            self.summary()
        );
        if open > self.inner.config.max_pages {
            message.push_str(&format!(
                "\n\nWarning: {open} pages are open (soft limit {}). Close pages you are done \
                 with.",
                self.inner.config.max_pages
            ));
        }
        Ok(message)
    }

    #[rmcp::tool(description = "Close a page and its window")]
    async fn close_page(&self, Parameters(args): Parameters<PageParams>) -> Result<String, Error> {
        let num = parse_page_id(&args.page_id)?;
        let handle = self
            .registry()
            .pages
            .remove(&num)
            .ok_or_else(|| Error::PageNotFound(args.page_id.clone()))?;

        // Wait for in-flight operations on this page before closing it.
        let slot = handle.slot.lock().await;
        let mode = slot.mode;
        self.registry()
            .targets
            .remove(slot.page.target_id().inner());
        let page = slot.page.clone();
        drop(slot);
        if let Err(e) = page.close().await {
            tracing::debug!("Failed to close page: {e}");
        }
        self.release_if_idle(mode).await;

        let remaining = self.summary();
        Ok(if remaining.is_empty() {
            format!("Closed page {}. No pages are open.", page_label(num))
        } else {
            format!("Closed page {}. Open pages:\n{remaining}", page_label(num))
        })
    }

    #[rmcp::tool(description = "List the open pages with their url, title and visibility")]
    async fn list_pages(&self) -> Result<String, Error> {
        let pages: Vec<_> = self
            .registry()
            .pages
            .iter()
            .map(|(id, h)| (*id, h.clone()))
            .collect();
        if pages.is_empty() {
            return Ok("No pages are open. Use open_page to open one.".into());
        }

        let mut lines = Vec::new();
        for (id, handle) in pages {
            let line = match handle.slot.try_lock() {
                Ok(slot) => format!(
                    "- {} [{}] {}",
                    page_label(id),
                    slot.mode,
                    describe(&slot.page).await
                ),
                Err(_) => format!("- {} (busy)", page_label(id)),
            };
            lines.push(line);
        }
        Ok(lines.join("\n"))
    }

    #[rmcp::tool(description = "Navigate a page to a url")]
    async fn navigate(
        &self,
        Parameters(args): Parameters<NavigateParams>,
    ) -> Result<String, Error> {
        let (_, handle) = self.page(&args.page_id)?;
        let slot = handle.slot.lock().await;
        timed(async {
            slot.page.goto(&args.url).await?;
            Ok(format!("Navigated to {}", describe(&slot.page).await))
        })
        .await
    }

    #[rmcp::tool(
        description = "Evaluate a JavaScript expression on a page and return its JSON result. \
                       Runs in an isolated world: the DOM is shared, but globals defined by the \
                       page's own scripts are not visible."
    )]
    async fn evaluate(
        &self,
        Parameters(args): Parameters<EvaluateToolArgs>,
    ) -> Result<CallToolResult, Error> {
        let (_, handle) = self.page(&args.page_id)?;
        let slot = handle.slot.lock().await;
        let value = timed(evaluate_isolated(&slot.page, &args.script)).await?;

        Ok(match value {
            Some(v @ serde_json::Value::Object(_)) => CallToolResult::structured(v),
            Some(v) => CallToolResult::success(vec![ContentBlock::text(v.to_string())]),
            None => CallToolResult::success(vec![ContentBlock::text("undefined")]),
        })
    }

    #[rmcp::tool(description = "Get a screenshot of a page")]
    async fn screenshot(
        &self,
        Parameters(args): Parameters<PageParams>,
    ) -> Result<CallToolResult, Error> {
        let (_, handle) = self.page(&args.page_id)?;
        let slot = handle.slot.lock().await;
        let image = timed(async {
            Ok(slot
                .page
                .execute(CaptureScreenshotParams::default())
                .await?
                .result
                .data)
        })
        .await?;

        Ok(CallToolResult::success(vec![ContentBlock::image(
            AsRef::<str>::as_ref(&image),
            "image/png",
        )]))
    }

    #[rmcp::tool(
        description = "Show a page in a visible window so the user can interact with it (e.g. to \
                       log in or solve a CAPTCHA). The page is reopened at its current url in a \
                       visible browser with its cookies and localStorage; sessionStorage, form \
                       input and other in-memory state are lost, so call this before the user \
                       needs to act. Use hide_page to make it headless again."
    )]
    async fn show_page(&self, Parameters(args): Parameters<PageParams>) -> Result<String, Error> {
        timed(self.move_page(&args.page_id, Mode::Visible)).await
    }

    #[rmcp::tool(
        description = "Move a visible page back to headless once the user is done with it. \
                       Cookies and localStorage are carried over."
    )]
    async fn hide_page(&self, Parameters(args): Parameters<PageParams>) -> Result<String, Error> {
        timed(self.move_page(&args.page_id, Mode::Headless)).await
    }
}

#[rmcp::tool_handler(
    name = "rbrowser-mcp",
    instructions = "Control a Chrome browser. open_page opens a url in a new window and returns a \
                    page id (e.g. \"p1\") that the other tools take. Every open_page creates \
                    another window, so close pages with close_page once you no longer need them. \
                    Pages are headless by default; call show_page when the user needs to see or \
                    interact with a page, and hide_page when they're done."
)]
impl rmcp::ServerHandler for BrowserMcp {}
