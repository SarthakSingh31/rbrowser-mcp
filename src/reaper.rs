//! Make sure Chrome dies with the MCP server, even if the server is killed without a chance to
//! clean up (SIGKILL, crash).
//!
//! chromey spawns Chrome itself, so we can't configure the child directly. Instead chromey is
//! pointed at this binary with [`REAL_CHROME_ENV`] set. In that mode [`exec_chrome_if_wrapper`]
//! asks the kernel to SIGKILL the process when its parent (the server) dies, then `exec`s the
//! real Chrome, which keeps that setting.

use std::path::{Path, PathBuf};

use chromiumoxide::browser::BrowserConfigBuilder;

const REAL_CHROME_ENV: &str = "RBROWSER_MCP_REAL_CHROME";
const SERVER_PID_ENV: &str = "RBROWSER_MCP_SERVER_PID";

/// The Chrome executable to use: `explicit` or the auto-detected one.
pub fn chrome_path(explicit: Option<&PathBuf>) -> Result<PathBuf, String> {
    match explicit {
        Some(path) => Ok(path.clone()),
        None => chromiumoxide::detection::default_executable(Default::default()),
    }
}

/// Configure `config` to launch `chrome` through the parent-death wrapper where supported.
pub fn wrap(config: BrowserConfigBuilder, chrome: &Path) -> BrowserConfigBuilder {
    #[cfg(target_os = "linux")]
    if let Ok(me) = std::env::current_exe() {
        return config
            .chrome_executable(me)
            .env(REAL_CHROME_ENV, chrome.to_string_lossy())
            .env(SERVER_PID_ENV, std::process::id().to_string());
    }
    config.chrome_executable(chrome)
}

/// If this process was started as the Chrome wrapper, become Chrome. Returns only otherwise.
pub fn exec_chrome_if_wrapper() {
    let Some(chrome) = std::env::var_os(REAL_CHROME_ENV) else {
        return;
    };

    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::CommandExt;

        let server_pid: Option<libc::pid_t> = std::env::var(SERVER_PID_ENV)
            .ok()
            .and_then(|pid| pid.parse().ok());
        // SAFETY: plain syscalls with constant arguments.
        unsafe {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
            // The server may have died before prctl took effect, in which case we were reparented.
            if server_pid.is_some_and(|pid| libc::getppid() != pid) {
                std::process::exit(1);
            }
        }

        let err = std::process::Command::new(chrome)
            .args(std::env::args_os().skip(1))
            .env_remove(REAL_CHROME_ENV)
            .env_remove(SERVER_PID_ENV)
            .exec();
        eprintln!("Failed to exec Chrome: {err}");
        std::process::exit(1);
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = chrome;
        unreachable!("the Chrome wrapper is only used on Linux");
    }
}
