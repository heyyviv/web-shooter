//! Headless Chrome, for pages that build their content with JavaScript.
//!
//! Chrome is the one installed on the machine (`WEB_SHOOTER_CHROME` or
//! `CHROME` to choose, else the usual install locations); nothing is
//! downloaded. It starts on the first `render` call and closes after
//! `IDLE` without one. Each start gets a fresh profile in a temporary
//! folder that is deleted when Chrome closes, so no cookies, cache or
//! history outlive it. Images are not loaded and the disk cache is kept
//! minimal, to keep memory and disk use low.

use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use chromiumoxide::{
    Page,
    browser::{Browser, BrowserConfig},
    cdp::browser_protocol::browser::CloseParams,
};
use futures::StreamExt;
use tokio::{
    sync::{Mutex, Semaphore},
    task::JoinHandle,
};

/// Chrome closes after this long without a page being rendered.
const IDLE: Duration = Duration::from_secs(120);

/// Tabs open at once, across all calls: each costs tens of MB.
pub const MAX_TABS: usize = 4;

/// Tries per page when Chrome has died (crashed or was quit) and must be
/// started again. A page that fails to load is not retried.
const MAX_ATTEMPTS: usize = 3;

/// Longest wait for a page's load event.
const LOAD_TIMEOUT: Duration = Duration::from_secs(15);

/// Longest a page may take in all, so nothing can hang a call.
const PAGE_MAX: Duration = Duration::from_secs(40);

/// After the load event, scripts often fetch the content; the text is
/// checked every `SETTLE_POLL` until it is unchanged `STABLE_POLLS` times
/// in a row, for at most `SETTLE_MAX`. Oracle HCM's job search pauses for
/// more than half a second before its results arrive. A page still blank
/// after `BLANK_MAX` is taken as it is.
const SETTLE_POLL: Duration = Duration::from_millis(300);
const STABLE_POLLS: usize = 5;
const SETTLE_MAX: Duration = Duration::from_secs(10);
const BLANK_MAX: Duration = Duration::from_secs(3);

/// How long Chrome gets to exit after being asked to, before it is killed.
const CLOSE_WAIT: Duration = Duration::from_secs(3);

/// Prefix of the temporary profile folders, followed by `<pid>-<n>`.
const PROFILE_PREFIX: &str = "web-shooter-chrome-";

/// A rendered page: the URL after redirects and the HTML of the live DOM.
pub struct Rendered {
    pub url: String,
    pub html: String,
}

/// The shared Chrome, started on demand.
#[derive(Clone)]
pub struct Chrome {
    state: Arc<Mutex<Option<Running>>>,
    /// One permit per open tab. Permits are returned when dropped, so a
    /// cancelled call can't leave the count wrong.
    tabs: Arc<Semaphore>,
    last_used: Arc<std::sync::Mutex<Instant>>,
}

impl Default for Chrome {
    fn default() -> Self {
        Self {
            state: Arc::default(),
            tabs: Arc::new(Semaphore::new(MAX_TABS)),
            last_used: Arc::new(std::sync::Mutex::new(Instant::now())),
        }
    }
}

struct Running {
    browser: Arc<Browser>,
    /// Drives the DevTools connection; it ends when Chrome exits.
    handler: JoinHandle<()>,
    /// Closes Chrome when idle.
    watcher: JoinHandle<()>,
    profile: PathBuf,
}

impl Chrome {
    /// Load `url` (http or https) in a new tab, wait for its content, and
    /// return the HTML. Chrome is started, or started again if it has died.
    pub async fn render(&self, url: &str) -> Result<Rendered, String> {
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return Err("only http and https URLs can be rendered".into());
        }
        let _tab = self
            .tabs
            .acquire()
            .await
            .map_err(|_| "Chrome is shutting down".to_string())?;
        let mut error = String::new();
        for _ in 0..MAX_ATTEMPTS {
            let browser = match self.browser().await {
                Ok(browser) => browser,
                Err(Launch::NoChrome(e)) => return Err(e),
                Err(Launch::Failed(e)) => {
                    error = e;
                    continue;
                }
            };
            let result = tokio::time::timeout(PAGE_MAX, render_in_tab(&browser, url))
                .await
                .unwrap_or_else(|_| Err("the page took too long to render".into()));
            self.touch();
            match result {
                Ok(page) => return Ok(page),
                // Chrome died under the page: start it again and retry.
                Err(e) if !self.alive().await => error = e,
                Err(e) => return Err(e),
            }
        }
        Err(format!("{error} (Chrome failed {MAX_ATTEMPTS} times)"))
    }

    fn touch(&self) {
        *self.last_used.lock().unwrap() = Instant::now();
    }

    /// Whether Chrome is running and its connection is open.
    async fn alive(&self) -> bool {
        self.state
            .lock()
            .await
            .as_ref()
            .is_some_and(|r| !r.handler.is_finished())
    }

    /// The running browser, started if there is none or it has died.
    async fn browser(&self) -> Result<Arc<Browser>, Launch> {
        let mut state = self.state.lock().await;
        if let Some(running) = state.as_ref() {
            if !running.handler.is_finished() {
                return Ok(running.browser.clone());
            }
            // Chrome exited: clean up what it left before starting anew.
            if let Some(dead) = state.take() {
                dead.watcher.abort();
                shut_down(dead).await;
            }
        }
        let (browser, handler, profile) = launch().await?;
        let watcher = self.watch_idle();
        *state = Some(Running {
            browser: browser.clone(),
            handler,
            watcher,
            profile,
        });
        Ok(browser)
    }

    /// Close Chrome once no tab is open and none has been for `IDLE`.
    fn watch_idle(&self) -> JoinHandle<()> {
        let this = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(IDLE / 4).await;
                let mut state = this.state.lock().await;
                // Checked under the lock: a render takes its tab permit
                // before it asks for the browser.
                let unused = this.tabs.available_permits() == MAX_TABS
                    && this.last_used.lock().unwrap().elapsed() >= IDLE;
                if unused {
                    if let Some(running) = state.take() {
                        drop(state);
                        shut_down(running).await;
                    }
                    return;
                }
            }
        })
    }

    /// Close Chrome now, even mid-render, and delete its profile.
    pub async fn close(&self) {
        let running = self.state.lock().await.take();
        if let Some(running) = running {
            running.watcher.abort();
            shut_down(running).await;
        }
    }
}

/// Why Chrome couldn't be started: no browser installed (not worth
/// retrying), or a start that failed.
enum Launch {
    NoChrome(String),
    Failed(String),
}

/// Ask Chrome to exit, kill it if it doesn't, and delete its profile.
async fn shut_down(running: Running) {
    let Running {
        browser,
        mut handler,
        profile,
        ..
    } = running;
    let _ = tokio::time::timeout(CLOSE_WAIT, browser.execute(CloseParams::default())).await;
    let exited = tokio::time::timeout(CLOSE_WAIT, &mut handler)
        .await
        .is_ok();
    handler.abort();
    // The last handle kills the process when dropped; one left in a
    // cancelled render is dropped with it. Kill now if this is the last.
    if !exited && let Ok(mut browser) = Arc::try_unwrap(browser) {
        let _ = browser.kill().await;
        let _ = browser.wait().await;
    }
    remove_profile(&profile).await;
}

/// Delete a profile folder, retrying while Chrome finishes writing to it.
async fn remove_profile(profile: &Path) {
    for _ in 0..5 {
        if std::fs::remove_dir_all(profile).is_ok() || !profile.exists() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    eprintln!(
        "web-shooter: could not delete Chrome profile {}",
        profile.display()
    );
}

/// Start headless Chrome with a fresh profile.
async fn launch() -> Result<(Arc<Browser>, JoinHandle<()>, PathBuf), Launch> {
    static STARTS: AtomicUsize = AtomicUsize::new(0);
    remove_stale_profiles();
    let profile = std::env::temp_dir().join(format!(
        "{PROFILE_PREFIX}{}-{}",
        std::process::id(),
        STARTS.fetch_add(1, Ordering::SeqCst)
    ));
    let mut config = BrowserConfig::builder()
        .user_data_dir(&profile)
        .launch_timeout(Duration::from_secs(20))
        .request_timeout(LOAD_TIMEOUT + Duration::from_secs(5))
        .args([
            "--mute-audio",
            // Text is all that is read: skip images and keep caches tiny.
            "--blink-settings=imagesEnabled=false",
            "--disk-cache-size=1",
            "--media-cache-size=1",
        ]);
    if let Some(path) = std::env::var_os("WEB_SHOOTER_CHROME") {
        config = config.chrome_executable(PathBuf::from(path));
    }
    let config = config.build().map_err(|e| {
        Launch::NoChrome(format!(
            "no Chrome found ({e}); install Chrome or set WEB_SHOOTER_CHROME to its path"
        ))
    })?;
    let started = Instant::now();
    let (browser, mut handler) = match Browser::launch(config).await {
        Ok(launched) => launched,
        Err(e) => {
            remove_profile(&profile).await;
            return Err(Launch::Failed(format!("could not start Chrome: {e}")));
        }
    };
    eprintln!(
        "web-shooter: started Chrome in {} ms",
        started.elapsed().as_millis()
    );
    let handler = tokio::spawn(async move {
        // The handler never ends by itself when Chrome dies: it reports a
        // connection error and then waits forever. Ending here drops it,
        // which fails the requests waiting on it, and marks Chrome dead.
        while let Some(Ok(())) = handler.next().await {}
    });
    Ok((Arc::new(browser), handler, profile))
}

/// Delete profiles left by web-shooter processes that have exited (killed
/// before they could clean up). Profiles of running processes are kept.
fn remove_stale_profiles() {
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name
            .to_str()
            .and_then(|n| n.strip_prefix(PROFILE_PREFIX))
            .and_then(|rest| rest.split('-').next())
            .and_then(|pid| pid.parse::<u32>().ok())
        else {
            continue;
        };
        if pid != std::process::id() && !process_alive(pid) {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// Whether a process exists (Unix: `kill -0`). Elsewhere assumed alive,
/// so nothing that might be in use is deleted.
fn process_alive(pid: u32) -> bool {
    if !cfg!(unix) {
        return true;
    }
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .map_or(true, |s| s.success())
}

/// A tab that closes itself if the call rendering it is cancelled.
struct Tab(Option<Page>);

impl Tab {
    async fn close(mut self) {
        if let Some(page) = self.0.take() {
            let _ = page.close().await;
        }
    }
}

impl Drop for Tab {
    fn drop(&mut self) {
        if let Some(page) = self.0.take()
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            runtime.spawn(async move {
                let _ = page.close().await;
            });
        }
    }
}

async fn render_in_tab(browser: &Browser, url: &str) -> Result<Rendered, String> {
    let page = browser
        .new_page("about:blank")
        .await
        .map_err(|e| format!("Chrome could not open a tab: {e}"))?;
    let tab = Tab(Some(page.clone()));
    // A page that never finishes loading is read as it is.
    if let Ok(Err(e)) = tokio::time::timeout(LOAD_TIMEOUT, page.goto(url)).await {
        tab.close().await;
        return Err(format!("could not load the page: {e}"));
    }
    settle(&page).await;
    let html = page.content().await;
    let final_url = page.url().await.ok().flatten();
    tab.close().await;
    Ok(Rendered {
        url: final_url.unwrap_or_else(|| url.to_string()),
        html: html.map_err(|e| format!("could not read the rendered page: {e}"))?,
    })
}

/// Wait until the page's visible text stops growing: content fetched by
/// scripts after the load event arrives a little later.
async fn settle(page: &Page) {
    let started = Instant::now();
    let mut last: Option<u64> = None;
    let mut stable = 0;
    while started.elapsed() < SETTLE_MAX {
        let len = page
            .evaluate("document.body ? document.body.innerText.length : 0")
            .await
            .ok()
            .and_then(|r| r.into_value::<u64>().ok())
            .unwrap_or(0);
        if len == 0 && started.elapsed() >= BLANK_MAX {
            return;
        }
        if len > 0 && last == Some(len) {
            stable += 1;
            if stable >= STABLE_POLLS {
                return;
            }
        } else {
            stable = 0;
        }
        last = Some(len);
        tokio::time::sleep(SETTLE_POLL).await;
    }
}
