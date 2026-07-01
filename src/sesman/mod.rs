//! Lamco session manager (`lamco-sesman`).
//!
//! This module owns the lifecycle of a headless compositor stack for Lamco RDP:
//! Weston headless → nested compositor (niri by default) → `lamco-rdp-server`.
//! It intentionally follows the useful parts of xrdp-sesman (session registry,
//! reuse/reconnect, process health, teardown) without depending on xrdp's X11-
//! specific daemon internals.

use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::Write as _,
    net::SocketAddr,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Utc};
use nix::{
    sys::signal::{Signal, kill},
    unistd::Pid,
};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};
use uuid::Uuid;

const STATE_VERSION: u32 = 1;
const DEFAULT_SESSION_NAME: &str = "default";
const DEFAULT_WIDTH: u32 = 1920;
const DEFAULT_HEIGHT: u32 = 1080;
const DEFAULT_START_TIMEOUT_MS: u64 = 15_000;
const DEFAULT_STOP_TIMEOUT_MS: u64 = 10_000;

/// Requested client geometry tracked by sesman.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionSize {
    pub width: u32,
    pub height: u32,
}

impl Default for SessionSize {
    fn default() -> Self {
        Self {
            width: DEFAULT_WIDTH,
            height: DEFAULT_HEIGHT,
        }
    }
}

/// External client metadata persisted for reconnect decisions.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClientInfo {
    pub peer: String,
    pub connected_at: DateTime<Utc>,
    pub requested_size: Option<SessionSize>,
}

/// A managed process component: weston, niri, lamco, or site-specific extras.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComponentConfig {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub working_dir: Option<PathBuf>,
    #[serde(default)]
    pub log_path: Option<PathBuf>,
    #[serde(default)]
    pub readiness: ReadinessCheck,
    #[serde(default = "default_true")]
    pub required: bool,
    #[serde(default)]
    pub startup_delay_ms: u64,
}

/// Readiness signal for a component.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ReadinessCheck {
    /// Process is considered ready once it is still alive after spawn.
    #[default]
    ProcessAlive,
    /// Wait for a Unix-domain Wayland socket path.
    UnixSocket { path: PathBuf },
    /// Do not wait for this component.
    None,
}

/// Sesman configuration file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SesmanConfig {
    #[serde(default = "default_session_name")]
    pub session_name: String,
    #[serde(default = "default_user")]
    pub user: String,
    #[serde(default = "default_xdg_runtime_dir")]
    pub xdg_runtime_dir: PathBuf,
    #[serde(default = "default_state_dir")]
    pub state_dir: PathBuf,
    #[serde(default = "default_log_dir")]
    pub log_dir: PathBuf,
    #[serde(default = "default_size")]
    pub default_size: SessionSize,
    #[serde(default = "default_start_timeout_ms")]
    pub start_timeout_ms: u64,
    #[serde(default = "default_stop_timeout_ms")]
    pub stop_timeout_ms: u64,
    #[serde(default)]
    pub environment: BTreeMap<String, String>,
    #[serde(default)]
    pub cleanup_paths: Vec<PathBuf>,
    #[serde(default)]
    pub cleanup_globs: Vec<String>,
    #[serde(default = "default_components")]
    pub components: Vec<ComponentConfig>,
}

impl Default for SesmanConfig {
    fn default() -> Self {
        Self {
            session_name: default_session_name(),
            user: default_user(),
            xdg_runtime_dir: default_xdg_runtime_dir(),
            state_dir: default_state_dir(),
            log_dir: default_log_dir(),
            default_size: default_size(),
            start_timeout_ms: default_start_timeout_ms(),
            stop_timeout_ms: default_stop_timeout_ms(),
            environment: default_environment(),
            cleanup_paths: default_cleanup_paths(),
            cleanup_globs: default_cleanup_globs(),
            components: default_components(),
        }
    }
}

impl SesmanConfig {
    /// Load sesman TOML config, or return Lamco's nested niri defaults if absent.
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let Some(path) = path else {
            return Ok(Self::default());
        };

        match fs::read_to_string(path) {
            Ok(content) => {
                let mut config: Self = toml::from_str(&content)
                    .with_context(|| format!("failed to parse {}", path.display()))?;
                config.apply_runtime_defaults();
                config.validate()?;
                Ok(config)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("failed to read {}", path.display())),
        }
    }

    fn apply_runtime_defaults(&mut self) {
        if self.environment.is_empty() {
            self.environment = default_environment();
        }
        if self.cleanup_paths.is_empty() {
            self.cleanup_paths = default_cleanup_paths();
        }
        if self.cleanup_globs.is_empty() {
            self.cleanup_globs = default_cleanup_globs();
        }
        if self.components.is_empty() {
            self.components = default_components();
        }
    }

    fn validate(&self) -> Result<()> {
        if self.session_name.trim().is_empty() {
            bail!("session_name cannot be empty");
        }
        if self.components.is_empty() {
            bail!("at least one component is required");
        }
        for component in &self.components {
            if component.name.trim().is_empty() {
                bail!("component name cannot be empty");
            }
            if component.command.trim().is_empty() {
                bail!("component {} has empty command", component.name);
            }
        }
        Ok(())
    }

    fn state_path(&self) -> PathBuf {
        self.state_dir
            .join(format!("{}.state.json", self.session_name))
    }

    fn lock_path(&self) -> PathBuf {
        self.state_dir.join(format!("{}.lock", self.session_name))
    }
}

/// PID record for a managed component.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ComponentState {
    pub name: String,
    pub pid: i32,
    pub command: Vec<String>,
    pub started_at: DateTime<Utc>,
    pub required: bool,
}

/// Persistent session registry entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionState {
    pub version: u32,
    pub id: Uuid,
    pub name: String,
    pub user: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub xdg_runtime_dir: PathBuf,
    pub default_size: SessionSize,
    pub requested_size: Option<SessionSize>,
    pub last_client: Option<ClientInfo>,
    pub components: Vec<ComponentState>,
}

impl SessionState {
    fn new(config: &SesmanConfig) -> Self {
        let now = Utc::now();
        Self {
            version: STATE_VERSION,
            id: Uuid::new_v4(),
            name: config.session_name.clone(),
            user: config.user.clone(),
            created_at: now,
            updated_at: now,
            xdg_runtime_dir: config.xdg_runtime_dir.clone(),
            default_size: config.default_size,
            requested_size: None,
            last_client: None,
            components: Vec::new(),
        }
    }

    fn mark_updated(&mut self) {
        self.updated_at = Utc::now();
    }
}

/// Health classification for the persisted session.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum SessionHealth {
    Missing,
    Healthy,
    Degraded,
    Dead,
}

/// Status output for CLI/automation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionStatus {
    pub health: SessionHealth,
    pub state_path: PathBuf,
    pub state: Option<SessionState>,
    pub dead_components: Vec<String>,
}

/// Result of an ensure/start operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnsureResult {
    pub reused_existing: bool,
    pub status: SessionStatus,
}

/// Options passed when an RDP frontend asks for a session.
#[derive(Debug, Clone, Default)]
pub struct EnsureOptions {
    pub force_restart: bool,
    pub requested_size: Option<SessionSize>,
    pub client_peer: Option<String>,
}

/// Lamco session manager.
pub struct SessionManager {
    config: SesmanConfig,
}

impl SessionManager {
    pub fn new(config: SesmanConfig) -> Self {
        Self { config }
    }

    pub fn config(&self) -> &SesmanConfig {
        &self.config
    }

    /// Ensure a usable session exists, reusing healthy processes when possible.
    pub fn ensure(&self, options: EnsureOptions) -> Result<EnsureResult> {
        let _lock = FileLock::acquire(&self.config.lock_path())?;
        fs::create_dir_all(&self.config.state_dir).context("failed to create sesman state dir")?;
        fs::create_dir_all(&self.config.log_dir).context("failed to create sesman log dir")?;
        fs::create_dir_all(&self.config.xdg_runtime_dir)
            .context("failed to create XDG runtime dir")?;

        let mut status = self.status()?;
        if options.force_restart {
            info!(
                "force restart requested for session {}",
                self.config.session_name
            );
            self.stop_locked()?;
            status = self.status()?;
        }

        if status.health == SessionHealth::Healthy {
            let mut state = status
                .state
                .clone()
                .ok_or_else(|| anyhow!("healthy status without state"))?;
            self.update_reconnect_state(&mut state, &options);
            self.write_state(&state)?;
            return Ok(EnsureResult {
                reused_existing: true,
                status: self.status()?,
            });
        }

        if matches!(status.health, SessionHealth::Degraded | SessionHealth::Dead) {
            warn!(
                "stopping stale/degraded session before restart: {:?}",
                status.dead_components
            );
            self.stop_locked()?;
        }

        self.cleanup_runtime_paths();
        let mut state = SessionState::new(&self.config);
        self.update_reconnect_state(&mut state, &options);

        for component in &self.config.components {
            let component_state = self.spawn_component(component)?;
            state.components.push(component_state);
            state.mark_updated();
            self.write_state(&state)?;
            self.wait_until_ready(component, &state)?;
            if component.startup_delay_ms > 0 {
                thread::sleep(Duration::from_millis(component.startup_delay_ms));
            }
        }

        Ok(EnsureResult {
            reused_existing: false,
            status: self.status()?,
        })
    }

    /// Return current persisted session status.
    pub fn status(&self) -> Result<SessionStatus> {
        let state_path = self.config.state_path();
        let state = match fs::read_to_string(&state_path) {
            Ok(content) => Some(
                serde_json::from_str::<SessionState>(&content)
                    .with_context(|| format!("failed to parse {}", state_path.display()))?,
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                return Err(e).with_context(|| format!("failed to read {}", state_path.display()));
            }
        };

        let Some(ref session_state) = state else {
            return Ok(SessionStatus {
                health: SessionHealth::Missing,
                state_path,
                state,
                dead_components: Vec::new(),
            });
        };

        let mut dead_components = Vec::new();
        for component in &session_state.components {
            if !process_alive(component.pid) {
                dead_components.push(component.name.clone());
            }
        }

        let health = if dead_components.is_empty() {
            SessionHealth::Healthy
        } else if dead_components.len() == session_state.components.len() {
            SessionHealth::Dead
        } else {
            SessionHealth::Degraded
        };

        Ok(SessionStatus {
            health,
            state_path,
            state,
            dead_components,
        })
    }

    /// Stop the persisted session and remove the registry entry.
    pub fn stop(&self) -> Result<SessionStatus> {
        let _lock = FileLock::acquire(&self.config.lock_path())?;
        self.stop_locked()?;
        self.status()
    }

    fn stop_locked(&self) -> Result<()> {
        let status = self.status()?;
        let Some(state) = status.state else {
            return Ok(());
        };

        for component in state.components.iter().rev() {
            if process_alive(component.pid) {
                debug!("SIGTERM {} pid {}", component.name, component.pid);
                signal_pid(component.pid, Signal::SIGTERM)?;
            }
        }

        let deadline = Instant::now() + Duration::from_millis(self.config.stop_timeout_ms);
        while Instant::now() < deadline {
            if state
                .components
                .iter()
                .all(|component| !process_alive(component.pid))
            {
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }

        for component in state.components.iter().rev() {
            if process_alive(component.pid) {
                warn!("SIGKILL {} pid {}", component.name, component.pid);
                signal_pid(component.pid, Signal::SIGKILL)?;
            }
        }

        let state_path = self.config.state_path();
        match fs::remove_file(&state_path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("failed to remove {}", state_path.display()));
            }
        }
        self.cleanup_runtime_paths();
        Ok(())
    }

    fn update_reconnect_state(&self, state: &mut SessionState, options: &EnsureOptions) {
        if let Some(size) = options.requested_size {
            state.requested_size = Some(size);
        }
        if let Some(peer) = options.client_peer.clone() {
            state.last_client = Some(ClientInfo {
                peer,
                connected_at: Utc::now(),
                requested_size: options.requested_size,
            });
        }
        state.mark_updated();
    }

    fn spawn_component(&self, component: &ComponentConfig) -> Result<ComponentState> {
        let log_path = component
            .log_path
            .clone()
            .unwrap_or_else(|| self.config.log_dir.join(format!("{}.log", component.name)));
        let stdout = open_log(&log_path)?;
        let stderr = stdout
            .try_clone()
            .with_context(|| format!("failed to clone log handle for {}", log_path.display()))?;

        let mut command = Command::new(&component.command);
        command.args(&component.args);
        command.env("XDG_RUNTIME_DIR", &self.config.xdg_runtime_dir);
        command.envs(&self.config.environment);
        command.envs(&component.env);
        if let Some(cwd) = &component.working_dir {
            command.current_dir(cwd);
        }
        command.stdin(Stdio::null());
        command.stdout(Stdio::from(stdout));
        command.stderr(Stdio::from(stderr));

        info!(
            "starting component {}: {} {}",
            component.name,
            component.command,
            component.args.join(" ")
        );
        let child = command
            .spawn()
            .with_context(|| format!("failed to spawn component {}", component.name))?;
        let pid = i32::try_from(child.id()).context("child pid did not fit i32")?;

        Ok(ComponentState {
            name: component.name.clone(),
            pid,
            command: std::iter::once(component.command.clone())
                .chain(component.args.clone())
                .collect(),
            started_at: Utc::now(),
            required: component.required,
        })
    }

    fn wait_until_ready(&self, component: &ComponentConfig, state: &SessionState) -> Result<()> {
        let deadline = Instant::now() + Duration::from_millis(self.config.start_timeout_ms);
        loop {
            let alive = state
                .components
                .iter()
                .find(|c| c.name == component.name)
                .is_some_and(|c| process_alive(c.pid));
            if !alive && component.required {
                bail!("component {} exited before becoming ready", component.name);
            }

            match &component.readiness {
                ReadinessCheck::None => return Ok(()),
                ReadinessCheck::ProcessAlive if alive => return Ok(()),
                ReadinessCheck::ProcessAlive => {}
                ReadinessCheck::UnixSocket { path } if path.exists() => return Ok(()),
                ReadinessCheck::UnixSocket { .. } => {}
            }

            if Instant::now() >= deadline {
                bail!(
                    "timed out waiting for component {} readiness",
                    component.name
                );
            }
            thread::sleep(Duration::from_millis(100));
        }
    }

    fn cleanup_runtime_paths(&self) {
        for path in &self.config.cleanup_paths {
            remove_stale_path(path);
        }
        for pattern in &self.config.cleanup_globs {
            cleanup_simple_glob(pattern);
        }
    }

    fn write_state(&self, state: &SessionState) -> Result<()> {
        let path = self.config.state_path();
        let tmp_path = path.with_extension("state.json.tmp");
        let data = serde_json::to_vec_pretty(state).context("failed to serialize session state")?;
        fs::write(&tmp_path, data)
            .with_context(|| format!("failed to write {}", tmp_path.display()))?;
        fs::rename(&tmp_path, &path).with_context(|| {
            format!(
                "failed to atomically replace {} with {}",
                path.display(),
                tmp_path.display()
            )
        })?;
        Ok(())
    }
}

struct FileLock {
    path: PathBuf,
}

impl FileLock {
    fn acquire(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).context("failed to create sesman lock dir")?;
        }

        match OpenOptions::new().write(true).create_new(true).open(path) {
            Ok(mut file) => {
                writeln!(file, "{}", std::process::id()).context("failed to write lock pid")?;
                Ok(Self {
                    path: path.to_path_buf(),
                })
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if stale_lock(path)? {
                    warn!("removing stale sesman lock {}", path.display());
                    fs::remove_file(path).with_context(|| {
                        format!("failed to remove stale lock {}", path.display())
                    })?;
                    Self::acquire(path)
                } else {
                    Err(anyhow!("session is already locked: {}", path.display()))
                }
            }
            Err(e) => Err(e).with_context(|| format!("failed to create lock {}", path.display())),
        }
    }
}

fn stale_lock(path: &Path) -> Result<bool> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("failed to read lock {}", path.display()))?;
    let Ok(pid) = content.trim().parse::<i32>() else {
        return Ok(true);
    };
    Ok(!process_alive(pid))
}

impl Drop for FileLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn process_alive(pid: i32) -> bool {
    kill(Pid::from_raw(pid), None).is_ok()
}

fn signal_pid(pid: i32, signal: Signal) -> Result<()> {
    kill(Pid::from_raw(pid), Some(signal)).with_context(|| format!("failed to signal pid {pid}"))
}

fn open_log(path: &Path) -> Result<File> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("failed to open {}", path.display()))
}

fn default_true() -> bool {
    true
}

fn default_session_name() -> String {
    DEFAULT_SESSION_NAME.to_string()
}

fn default_start_timeout_ms() -> u64 {
    DEFAULT_START_TIMEOUT_MS
}

fn default_stop_timeout_ms() -> u64 {
    DEFAULT_STOP_TIMEOUT_MS
}

fn default_size() -> SessionSize {
    SessionSize::default()
}

fn default_user() -> String {
    std::env::var("USER").unwrap_or_else(|_| "rui".to_string())
}

fn default_xdg_runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map_or_else(|| PathBuf::from("/run/user/1000"), PathBuf::from)
}

fn default_state_dir() -> PathBuf {
    default_xdg_runtime_dir().join("lamco-sesman")
}

fn default_log_dir() -> PathBuf {
    PathBuf::from("/tmp")
}

fn default_environment() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("XDG_CURRENT_DESKTOP".to_string(), "niri".to_string()),
        ("XDG_SESSION_DESKTOP".to_string(), "niri".to_string()),
        ("WLR_NO_HARDWARE_CURSORS".to_string(), "1".to_string()),
        ("LIBVA_DRIVER_NAME".to_string(), "iHD".to_string()),
        ("RUST_BACKTRACE".to_string(), "1".to_string()),
        (
            "PATH".to_string(),
            "/usr/local/bin:/usr/bin:/bin".to_string(),
        ),
    ])
}

fn default_cleanup_paths() -> Vec<PathBuf> {
    let runtime = default_xdg_runtime_dir();
    vec![
        runtime.join("wayland-weston"),
        runtime.join("wayland-weston.lock"),
        runtime.join("wayland-1"),
        runtime.join("wayland-1.lock"),
        PathBuf::from("/tmp/lamco-rdp-server.log"),
        PathBuf::from("/tmp/niri-nested.log"),
        PathBuf::from("/tmp/weston-headless.log"),
    ]
}

fn default_cleanup_globs() -> Vec<String> {
    vec![format!(
        "{}/niri.wayland-1.*.sock",
        default_xdg_runtime_dir().display()
    )]
}

fn remove_stale_path(path: &Path) {
    if let Err(e) = fs::remove_file(path)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        warn!(
            "failed to remove stale runtime path {}: {e}",
            path.display()
        );
    }
}

fn cleanup_simple_glob(pattern: &str) {
    let Some(star) = pattern.find('*') else {
        remove_stale_path(Path::new(pattern));
        return;
    };
    let (prefix, suffix_with_star) = pattern.split_at(star);
    let suffix = &suffix_with_star[1..];
    let prefix_path = Path::new(prefix);
    let parent = prefix_path.parent().unwrap_or_else(|| Path::new("."));
    let Some(name_prefix) = prefix_path.file_name().and_then(|name| name.to_str()) else {
        return;
    };
    let Ok(entries) = fs::read_dir(parent) else {
        return;
    };
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let Some(file_name) = file_name.to_str() else {
            continue;
        };
        if file_name.starts_with(name_prefix) && file_name.ends_with(suffix) {
            remove_stale_path(&entry.path());
        }
    }
}

fn default_components() -> Vec<ComponentConfig> {
    let runtime = default_xdg_runtime_dir();
    vec![
        ComponentConfig {
            name: "weston".to_string(),
            command: "weston".to_string(),
            args: vec![
                "--backend=headless".to_string(),
                "--shell=kiosk-shell.so".to_string(),
                "--socket=wayland-weston".to_string(),
                format!("--width={DEFAULT_WIDTH}"),
                format!("--height={DEFAULT_HEIGHT}"),
                "--use-gl".to_string(),
                "--idle-time=0".to_string(),
            ],
            env: BTreeMap::new(),
            working_dir: None,
            log_path: Some(PathBuf::from("/tmp/weston-headless.log")),
            readiness: ReadinessCheck::UnixSocket {
                path: runtime.join("wayland-weston"),
            },
            required: true,
            startup_delay_ms: 0,
        },
        ComponentConfig {
            name: "niri".to_string(),
            command: "/usr/local/bin/niri".to_string(),
            args: Vec::new(),
            env: BTreeMap::from([("WAYLAND_DISPLAY".to_string(), "wayland-weston".to_string())]),
            working_dir: None,
            log_path: Some(PathBuf::from("/tmp/niri-nested.log")),
            readiness: ReadinessCheck::UnixSocket {
                path: runtime.join("wayland-1"),
            },
            required: true,
            startup_delay_ms: 5_000,
        },
        ComponentConfig {
            name: "lamco-rdp-server".to_string(),
            command: "/usr/bin/lamco-rdp-server".to_string(),
            args: vec![
                "-c".to_string(),
                "/home/rui/.config/lamco-rdp-server/config.toml".to_string(),
            ],
            env: BTreeMap::from([
                ("LAMCO_RDP_ROTATE_180".to_string(), "0".to_string()),
                ("LAMCO_RDP_FLIP_VERTICAL".to_string(), "1".to_string()),
                ("WAYLAND_DISPLAY".to_string(), "wayland-1".to_string()),
            ]),
            working_dir: None,
            log_path: Some(PathBuf::from("/tmp/lamco-rdp-server.log")),
            readiness: ReadinessCheck::ProcessAlive,
            required: true,
            startup_delay_ms: 0,
        },
    ]
}

/// Convert an optional socket address into stable client metadata.
pub fn peer_to_string(peer: Option<SocketAddr>) -> Option<String> {
    peer.map(|addr| addr.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_matches_live_nested_stack() {
        let config = SesmanConfig::default();
        assert_eq!(config.components.len(), 3);
        assert_eq!(config.components[0].name, "weston");
        assert_eq!(config.components[1].name, "niri");
        assert_eq!(config.components[2].name, "lamco-rdp-server");
        assert_eq!(
            config.components[1].env.get("WAYLAND_DISPLAY"),
            Some(&"wayland-weston".to_string())
        );
        assert_eq!(
            config.components[2].env.get("WAYLAND_DISPLAY"),
            Some(&"wayland-1".to_string())
        );
    }

    #[test]
    fn toml_config_can_override_components() {
        let toml = r#"
            session_name = "test"
            user = "alice"
            xdg_runtime_dir = "/tmp/runtime-alice"
            state_dir = "/tmp/state"
            log_dir = "/tmp/logs"

            [[components]]
            name = "dummy"
            command = "/bin/sleep"
            args = ["60"]
            readiness = { type = "process_alive" }
        "#;
        let mut config: SesmanConfig = toml::from_str(toml).expect("test TOML should parse");
        config.apply_runtime_defaults();
        config.validate().expect("test config should validate");
        assert_eq!(config.session_name, "test");
        assert_eq!(config.components.len(), 1);
        assert_eq!(config.components[0].command, "/bin/sleep");
    }
}
