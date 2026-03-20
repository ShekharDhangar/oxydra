use std::collections::{BTreeSet, HashMap};
use std::ffi::CStr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use types::RunnerGlobalConfig;

/// Shared state for the web configurator server.
#[derive(Debug, Clone)]
pub struct WebState {
    /// The parsed global config (used for user registry, workspace root, etc.).
    pub global_config: RunnerGlobalConfig,
    /// Absolute path to the runner global config file.
    pub config_path: PathBuf,
    /// Resolved workspace root directory (absolute).
    pub workspace_root: PathBuf,
    /// Effective bind address used by the running web server.
    pub bind_address: String,
    /// Host header values accepted by the DNS rebinding guard.
    allowed_hosts: BTreeSet<String>,
    /// Resolved bearer token when token auth mode is enabled.
    auth_token: Option<String>,
    /// Path to the runner executable used for detached lifecycle spawns.
    runner_executable: PathBuf,
    /// PIDs of daemon processes spawned via web lifecycle endpoints.
    spawned_daemon_pids: Arc<Mutex<HashMap<String, u32>>>,
}

impl WebState {
    pub fn new(
        global_config: RunnerGlobalConfig,
        config_path: PathBuf,
        bind_address: String,
    ) -> Self {
        let config_dir = config_path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));
        let configured_root = PathBuf::from(&global_config.workspace_root);
        let workspace_root = if configured_root.is_absolute() {
            configured_root
        } else {
            config_dir.join(configured_root)
        };
        let allowed_hosts = allowed_host_values(&bind_address, local_host_aliases());
        let auth_token = global_config.web.resolve_auth_token();
        let runner_executable = std::env::var_os("OXYDRA_WEB_RUNNER_EXECUTABLE")
            .map(PathBuf::from)
            .or_else(|| std::env::current_exe().ok())
            .unwrap_or_else(|| PathBuf::from("oxydra"));
        Self {
            global_config,
            config_path,
            workspace_root,
            bind_address,
            allowed_hosts,
            auth_token,
            runner_executable,
            spawned_daemon_pids: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Directory containing the runner config file (used to resolve relative paths).
    pub fn config_dir(&self) -> PathBuf {
        self.config_path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."))
    }

    /// Loads the latest global config from disk. Callers should use this for
    /// read-after-write consistency instead of relying on startup-time state.
    pub fn load_latest_global_config(&self) -> Result<RunnerGlobalConfig, crate::RunnerError> {
        crate::load_runner_global_config(&self.config_path)
    }

    /// Loads the latest global config, falling back to the startup snapshot if
    /// the file cannot be reloaded.
    pub fn latest_global_config_or_cached(&self) -> RunnerGlobalConfig {
        self.load_latest_global_config()
            .unwrap_or_else(|_| self.global_config.clone())
    }

    /// Resolves a user config path against the runner config directory.
    pub fn resolve_user_config_path(&self, configured_path: &str) -> PathBuf {
        let configured = PathBuf::from(configured_path);
        if configured.is_absolute() {
            configured
        } else {
            self.config_dir().join(configured)
        }
    }

    /// Returns true when the request Host header matches the configured bind
    /// address allow-list.
    ///
    /// When the server is bound to a wildcard address (`0.0.0.0` / `::`), any
    /// host header whose host part is a bare IP address at the correct port is
    /// also accepted. The local machine hostname is also accepted on the same
    /// port so operators can reach the configurator via the host's own name
    /// without opening the rebinding guard to arbitrary external hostnames.
    pub fn allows_host_header(&self, host_header: &str) -> bool {
        let normalized = host_header.trim().to_ascii_lowercase();
        if self.allowed_hosts.contains(&normalized) {
            return true;
        }
        // Additional check for wildcard binds: accept any IP-based host header
        // at the configured port.
        if let Ok(bind_addr) = self.bind_address.parse::<std::net::SocketAddr>()
            && bind_addr.ip().is_unspecified()
        {
            // host_header for IPv4 looks like "1.2.3.4:port";
            // for IPv6 it looks like "[::1]:port" — both are handled by
            // SocketAddr's FromStr.
            if let Ok(host_addr) = normalized.parse::<std::net::SocketAddr>()
                && host_addr.port() == bind_addr.port()
            {
                return true;
            }
        }
        false
    }

    /// Returns the resolved web bearer token (if token auth is enabled).
    pub fn auth_token(&self) -> Option<&str> {
        self.auth_token.as_deref()
    }

    /// Returns the executable path used to spawn detached `oxydra start` daemons.
    pub fn runner_executable(&self) -> &Path {
        &self.runner_executable
    }

    /// Record a daemon PID for a user started by the web control API.
    pub fn record_spawned_daemon_pid(&self, user_id: &str, pid: u32) {
        if let Ok(mut tracked) = self.spawned_daemon_pids.lock() {
            tracked.insert(user_id.to_owned(), pid);
        }
    }

    /// Remove and return any tracked daemon PID for the given user.
    pub fn remove_spawned_daemon_pid(&self, user_id: &str) -> Option<u32> {
        self.spawned_daemon_pids
            .lock()
            .ok()
            .and_then(|mut tracked| tracked.remove(user_id))
    }

    /// Snapshot tracked daemon PIDs for shutdown logging.
    pub fn spawned_daemon_pids_snapshot(&self) -> Vec<(String, u32)> {
        let mut pairs = self
            .spawned_daemon_pids
            .lock()
            .map(|tracked| {
                tracked
                    .iter()
                    .map(|(user_id, pid)| (user_id.clone(), *pid))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        pairs.sort_by(|a, b| a.0.cmp(&b.0));
        pairs
    }

    /// Compute the control socket path for a user daemon.
    pub fn control_socket_path(&self, user_id: &str) -> PathBuf {
        self.workspace_root
            .join(user_id)
            .join("ipc")
            .join(crate::RUNNER_CONTROL_SOCKET_NAME)
    }
}

fn allowed_host_values(
    bind_address: &str,
    local_host_aliases: impl IntoIterator<Item = String>,
) -> BTreeSet<String> {
    let local_host_aliases = local_host_aliases.into_iter().collect::<Vec<_>>();
    let mut allowed = BTreeSet::new();
    if let Ok(addr) = bind_address.parse::<std::net::SocketAddr>() {
        match addr {
            std::net::SocketAddr::V4(v4) => {
                let host = format!("{}:{}", v4.ip(), v4.port()).to_ascii_lowercase();
                allowed.insert(host);
                // Loopback (127.0.0.1) and unspecified/wildcard (0.0.0.0)
                // both listen on the loopback interface, so `localhost` is a
                // valid way to reach them.
                if v4.ip().is_loopback() || v4.ip().is_unspecified() {
                    allowed.insert(format!("localhost:{}", v4.port()).to_ascii_lowercase());
                }
                if v4.ip().is_unspecified() {
                    allowed.extend(
                        local_host_aliases
                            .iter()
                            .filter(|host| !host.is_empty())
                            .map(|host| format!("{host}:{}", v4.port()).to_ascii_lowercase()),
                    );
                }
            }
            std::net::SocketAddr::V6(v6) => {
                let host = format!("[{}]:{}", v6.ip(), v6.port()).to_ascii_lowercase();
                allowed.insert(host);
                if v6.ip().is_loopback() || v6.ip().is_unspecified() {
                    allowed.insert(format!("localhost:{}", v6.port()).to_ascii_lowercase());
                }
                if v6.ip().is_unspecified() {
                    allowed.extend(
                        local_host_aliases
                            .iter()
                            .filter(|host| !host.is_empty())
                            .map(|host| format!("{host}:{}", v6.port()).to_ascii_lowercase()),
                    );
                }
            }
        }
    } else {
        allowed.insert(bind_address.to_ascii_lowercase());
    }

    allowed
}

fn local_host_aliases() -> BTreeSet<String> {
    let mut aliases = BTreeSet::new();
    if let Some(hostname) = read_local_hostname() {
        let normalized = hostname.trim().trim_end_matches('.').to_ascii_lowercase();
        if !normalized.is_empty() {
            aliases.insert(normalized.clone());
            if let Some((short, _)) = normalized.split_once('.')
                && !short.is_empty()
            {
                aliases.insert(short.to_owned());
            }
        }
    }
    aliases
}

fn read_local_hostname() -> Option<String> {
    let mut buffer = [0 as libc::c_char; 256];
    // SAFETY: `buffer` is valid writable memory and its length is passed
    // correctly. `gethostname` writes at most `buffer.len()` bytes.
    let result = unsafe { libc::gethostname(buffer.as_mut_ptr(), buffer.len()) };
    if result != 0 {
        return None;
    }
    buffer[buffer.len() - 1] = 0;
    let hostname = unsafe { CStr::from_ptr(buffer.as_ptr()) };
    hostname.to_str().ok().map(ToOwned::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use types::RunnerGlobalConfig;

    fn make_state(bind: &str) -> WebState {
        make_state_with_aliases(bind, BTreeSet::new())
    }

    fn make_state_with_aliases(bind: &str, aliases: BTreeSet<String>) -> WebState {
        let global_config = RunnerGlobalConfig {
            workspace_root: std::env::temp_dir().to_string_lossy().to_string(),
            ..Default::default()
        };
        let config_path = std::path::PathBuf::from("/tmp/config.toml");
        let config_dir = config_path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));
        let configured_root = PathBuf::from(&global_config.workspace_root);
        let workspace_root = if configured_root.is_absolute() {
            configured_root
        } else {
            config_dir.join(configured_root)
        };
        WebState {
            global_config,
            config_path,
            workspace_root,
            bind_address: bind.to_owned(),
            allowed_hosts: allowed_host_values(bind, aliases),
            auth_token: None,
            runner_executable: PathBuf::from("oxydra"),
            spawned_daemon_pids: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    // --- loopback bind (127.0.0.1) -----------------------------------------

    #[test]
    fn loopback_bind_accepts_localhost_header() {
        let state = make_state("127.0.0.1:8881");
        assert!(state.allows_host_header("localhost:8881"));
    }

    #[test]
    fn loopback_bind_accepts_ip_header() {
        let state = make_state("127.0.0.1:8881");
        assert!(state.allows_host_header("127.0.0.1:8881"));
    }

    #[test]
    fn loopback_bind_rejects_external_ip_header() {
        let state = make_state("127.0.0.1:8881");
        assert!(!state.allows_host_header("192.168.40.6:8881"));
    }

    #[test]
    fn loopback_bind_rejects_arbitrary_hostname() {
        let state = make_state("127.0.0.1:8881");
        assert!(!state.allows_host_header("evil.example.com:8881"));
    }

    // --- wildcard bind (0.0.0.0) -------------------------------------------

    #[test]
    fn wildcard_bind_accepts_localhost_header() {
        // 0.0.0.0 also listens on the loopback interface, so localhost must work.
        let state = make_state("0.0.0.0:8881");
        assert!(state.allows_host_header("localhost:8881"));
    }

    #[test]
    fn wildcard_bind_accepts_external_ip_header() {
        let state = make_state("0.0.0.0:8881");
        assert!(state.allows_host_header("192.168.40.6:8881"));
    }

    #[test]
    fn wildcard_bind_accepts_loopback_ip_header() {
        let state = make_state("0.0.0.0:8881");
        assert!(state.allows_host_header("127.0.0.1:8881"));
    }

    #[test]
    fn wildcard_bind_rejects_wrong_port() {
        let state = make_state("0.0.0.0:8881");
        assert!(!state.allows_host_header("192.168.40.6:9999"));
    }

    #[test]
    fn wildcard_bind_accepts_local_hostname_alias() {
        let state = make_state_with_aliases("0.0.0.0:8881", BTreeSet::from(["arachnoid".into()]));
        assert!(state.allows_host_header("arachnoid:8881"));
    }

    #[test]
    fn wildcard_bind_rejects_arbitrary_hostname_header() {
        let state = make_state_with_aliases("0.0.0.0:8881", BTreeSet::from(["arachnoid".into()]));
        assert!(!state.allows_host_header("evil.example.com:8881"));
    }

    #[test]
    fn wildcard_bind_accepts_literal_0000_header() {
        // The literal bind address itself is also in the set.
        let state = make_state("0.0.0.0:8881");
        assert!(state.allows_host_header("0.0.0.0:8881"));
    }

    // --- case / whitespace --------------------------------------------------

    #[test]
    fn host_matching_is_case_insensitive() {
        let state = make_state("127.0.0.1:8881");
        assert!(state.allows_host_header("Localhost:8881"));
        assert!(state.allows_host_header("LOCALHOST:8881"));
    }

    #[test]
    fn host_matching_trims_whitespace() {
        let state = make_state("127.0.0.1:8881");
        assert!(state.allows_host_header("  localhost:8881  "));
    }
}
