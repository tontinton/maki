use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use maki_agent::AgentConfig;
use maki_sandbox::Sandbox;
use maki_sandbox::ipc::DirEntry;
use maki_sandbox::namespace::{EnvEntry, NamespaceConfig};
use maki_sandbox::profiles::{self, MountUsage, SandboxProfile};

use crate::components::ModalScroll;
use crate::components::Overlay;
use crate::components::modal::{CHROME_LINES, Modal};
use crate::components::scrollbar::render_vertical_scrollbar;
use crate::theme;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

const TITLE: &str = " Sandbox ";
const WIDTH_PERCENT: u16 = 65;
const MAX_HEIGHT_PERCENT: u16 = 85;
const YOLO_LABEL: &str = "Skip permission prompts for all tools";

#[derive(Clone, Debug, PartialEq)]
enum Mode {
    Info,
    Browse,
    Shell,
}

/// Focus position in the info tab options list: the YOLO checkbox (only
/// present while the sandbox is enabled), then each profile in order.
#[derive(Clone, Copy, Debug, PartialEq)]
enum InfoFocus {
    Yolo,
    Profile(usize),
}

/// Directory listing delivered by a background job.
pub(crate) struct DirSnapshot {
    pub pwd: String,
    pub entries: Result<Vec<DirEntry>, String>,
}

/// Event produced by a background sandbox job and applied on the UI thread.
pub(crate) enum SandboxModalEvent {
    /// Browse tab requested a fresh child; carries the initial listing.
    BrowserReady(DirSnapshot),
    /// A cd+ls round-trip finished for the open browser.
    Navigated(DirSnapshot),
    /// Shell tab requested a fresh child; carries the working directory.
    ShellReady(String),
    /// A shell command finished.
    Executed {
        command: String,
        result: Result<(String, bool), String>,
    },
    /// Child respawn or setup failed.
    Failed(String),
}

/// Long-running modal job tracked so late key presses don't stack requests
/// and stale replies can be recognized.
#[derive(Clone, Copy, Debug, PartialEq)]
enum PendingJob {
    Browser,
    Navigate,
    Shell,
}

/// Snapshot of sandbox configuration for display.
pub struct SandboxInfo {
    pub enabled: bool,
    pub env_entries: Vec<EnvEntry>,
    /// User-configured environment allow list, kept separate so profile
    /// toggling can rebuild `env_entries` without losing it.
    pub allowed_env: Vec<String>,
    pub workspace_dir: String,
    pub workspace_name: String,
    pub home_mounts: Vec<(String, String)>,
    /// (profile, enabled) pairs shown on the info tab.
    pub profiles: Vec<(SandboxProfile, bool)>,
    /// Extra host directories to bind-mount into the workspace.
    pub extra_workspace_dirs: Vec<(String, String)>,
}

struct FileEntry {
    name: String,
    is_dir: bool,
}

/// Sandbox filesystem browser state. All IPC happens in background jobs;
/// this struct only holds the last applied listing for rendering.
struct SandboxFileBrowser {
    cwd: String,
    entries: Vec<FileEntry>,
    cursor: usize,
    scroll: usize,
    viewport_entries: usize,
    error: Option<String>,
}

impl SandboxFileBrowser {
    fn new(pwd: String, entries: Vec<DirEntry>) -> Self {
        let mut browser = Self {
            cwd: pwd,
            entries: Vec::new(),
            cursor: 0,
            scroll: 0,
            viewport_entries: 0,
            error: None,
        };
        browser.apply_listing(entries);
        browser
    }

    /// Replace the listing after a successful cd+ls round-trip.
    fn apply_listing(&mut self, listed: Vec<DirEntry>) {
        self.entries.clear();
        for e in listed {
            self.entries.push(FileEntry {
                name: e.name,
                is_dir: e.is_dir,
            });
        }
        self.entries.sort_by(|a, b| match (a.is_dir, b.is_dir) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.name.cmp(&b.name),
        });
        if self.cwd != "/" {
            self.entries.insert(
                0,
                FileEntry {
                    name: "..".into(),
                    is_dir: true,
                },
            );
        }
        self.cursor = 0;
        self.scroll = 0;
    }

    fn clamp_cursor(&mut self) {
        let max = self.entries.len().saturating_sub(1);
        self.cursor = self.cursor.min(max);
        if self.cursor < self.scroll {
            self.scroll = self.cursor;
        } else if self.entries.len() > self.viewport_entries
            && self.cursor >= self.scroll + self.viewport_entries
        {
            self.scroll = self
                .cursor
                .saturating_add(1)
                .saturating_sub(self.viewport_entries);
        }
    }

    fn visible_entries(&self) -> &[FileEntry] {
        let start = self.scroll.min(self.entries.len());
        let end = (self.scroll + self.viewport_entries).min(self.entries.len());
        &self.entries[start..end]
    }

    fn scroll_offset(&self) -> usize {
        self.scroll
    }

    fn total_entries(&self) -> usize {
        self.entries.len()
    }
}

fn parent_dir(path: &str) -> Option<String> {
    if path == "/" {
        return None;
    }
    let p = Path::new(path).parent()?;
    let result = p.to_string_lossy().to_string();
    if result.is_empty() {
        Some("/".into())
    } else {
        Some(result)
    }
}

/// A single entry in the shell output history.
struct ShellEntry {
    command: String,
    output: String,
    is_error: bool,
}

/// Sandbox interactive shell state. Commands run in background jobs; results
/// arrive via [`SandboxModalEvent::Executed`].
struct SandboxShellState {
    cwd: String,
    input: String,
    entries: Vec<ShellEntry>,
    history: Vec<String>,
    history_pos: Option<usize>,
    error: Option<String>,
}

impl SandboxShellState {
    fn new(pwd: String) -> Self {
        Self {
            cwd: pwd,
            input: String::new(),
            entries: Vec::new(),
            history: Vec::new(),
            history_pos: None,
            error: None,
        }
    }

    fn push_result(&mut self, command: String, result: Result<(String, bool), String>) {
        match result {
            Ok((output, is_error)) => {
                self.error = None;
                self.entries.push(ShellEntry {
                    command,
                    output,
                    is_error,
                });
            }
            Err(e) => {
                self.error = Some(e);
            }
        }
    }

    fn submit(&mut self) -> Option<String> {
        let command = std::mem::take(&mut self.input);
        if command.is_empty() {
            return None;
        }
        self.history.push(command.clone());
        self.history_pos = None;
        Some(command)
    }

    fn history_up(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let pos = self.history_pos.unwrap_or(self.history.len());
        if pos > 0 {
            let new_pos = pos - 1;
            self.history_pos = Some(new_pos);
            self.input = self.history[new_pos].clone();
        }
    }

    fn history_down(&mut self) {
        match self.history_pos {
            Some(pos) if pos + 1 < self.history.len() => {
                let new_pos = pos + 1;
                self.history_pos = Some(new_pos);
                self.input = self.history[new_pos].clone();
            }
            _ => {
                self.history_pos = None;
                self.input.clear();
            }
        }
    }
}

pub struct SandboxModal {
    open: bool,
    mode: Mode,
    scroll: ModalScroll,
    h_scroll: usize,
    info: SandboxInfo,
    sandbox: Option<Arc<Sandbox>>,
    browser: Option<SandboxFileBrowser>,
    shell: Option<SandboxShellState>,
    shell_entry_count: usize,
    /// In-flight spawn/navigate job; execs run untracked.
    pending: Option<PendingJob>,
    event_tx: Option<flume::Sender<SandboxModalEvent>>,
    info_focus: Option<InfoFocus>,
    spawn_error: Option<String>,
    /// Set when the user toggles `enabled` via the UI.
    enabled_changed: bool,
    /// Set when the user toggles any profile via the UI.
    profiles_changed: bool,
    /// YOLO state as shown in the checkbox (owned by the app's permissions,
    /// mirrored here for display).
    yolo: bool,
    /// Set when the user toggles YOLO via the UI.
    yolo_changed: bool,
}

impl SandboxModal {
    pub fn new(info: SandboxInfo, sandbox: Option<Arc<Sandbox>>) -> Self {
        let info_focus = Self::first_focus(&info);
        Self {
            open: false,
            mode: Mode::Info,
            scroll: ModalScroll::new_top(),
            h_scroll: 0,
            info,
            sandbox,
            browser: None,
            shell: None,
            shell_entry_count: 0,
            pending: None,
            event_tx: None,
            info_focus,
            spawn_error: None,
            enabled_changed: false,
            profiles_changed: false,
            yolo: false,
            yolo_changed: false,
        }
    }

    /// Build the modal from the agent config, deriving the namespace
    /// layout (mounts, env) shown on the info tab. `event_tx` receives
    /// results from background jobs; it is wired into the session's event
    /// loop by the caller.
    pub fn from_config(
        config: &AgentConfig,
        sandbox: Option<Arc<Sandbox>>,
        event_tx: flume::Sender<SandboxModalEvent>,
    ) -> Self {
        let workspace_dir = std::env::current_dir().ok();
        let workspace_name = workspace_dir
            .as_ref()
            .and_then(|d| d.file_name().map(|n| n.to_string_lossy().to_string()))
            .unwrap_or_default();
        let enabled_profiles = profiles::select_profiles(&config.sandbox_profiles);
        let mut ns_config = NamespaceConfig::from_agent_config(
            config.sandbox_allowed_env.clone(),
            &config.sandbox_allowed_paths,
            &config.sandbox_extra_dirs,
            &enabled_profiles,
            workspace_dir.clone().unwrap_or_default(),
            workspace_name.clone(),
        );
        ns_config.prune_missing_mounts();
        let home_mounts: Vec<(String, String)> = ns_config
            .home_mounts
            .iter()
            .map(|(p, name)| (p.display().to_string(), name.clone()))
            .collect();
        let extra_workspace_dirs: Vec<(String, String)> = ns_config
            .extra_workspace_dirs
            .iter()
            .map(|(p, name)| (p.display().to_string(), name.clone()))
            .collect();
        let env_entries = ns_config.effective_env();
        let mut modal = Self::new(
            SandboxInfo {
                enabled: config.sandbox_enabled,
                env_entries,
                allowed_env: config.sandbox_allowed_env.clone(),
                workspace_dir: workspace_dir
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default(),
                workspace_name,
                home_mounts,
                profiles: profiles::builtin_profiles()
                    .into_iter()
                    .map(|p| {
                        let enabled = config.sandbox_profiles.contains(&p.name);
                        (p, enabled)
                    })
                    .collect(),
                extra_workspace_dirs,
            },
            sandbox,
        );
        // Enabled profile toggles contribute PATH entries on top of the
        // base allow list.
        modal.rebuild_env_entries();
        modal.event_tx = Some(event_tx);
        modal
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn scroll(&mut self, delta: i32) {
        self.scroll.scroll(delta);
    }

    pub fn toggle(&mut self) {
        self.open = !self.open;
        if self.open {
            self.mode = Mode::Info;
            self.close_browser();
            self.close_shell();
            self.pending = None;
            self.spawn_error = None;
            self.enabled_changed = false;
            self.profiles_changed = false;
            self.yolo_changed = false;
            self.info_focus = Self::first_focus(&self.info);
        } else {
            self.close_browser();
            self.close_shell();
        }
        self.scroll.reset();
    }

    /// Returns and resets the `enabled_changed` flag.
    /// The caller should check this after `handle_key` to persist the setting.
    pub fn take_enabled_changed(&mut self) -> bool {
        std::mem::take(&mut self.enabled_changed)
    }

    /// Returns and resets the `profiles_changed` flag.
    /// The caller should check this after `handle_key` to persist the setting.
    pub fn take_profiles_changed(&mut self) -> bool {
        std::mem::take(&mut self.profiles_changed)
    }

    /// Names of the currently enabled profiles (config order = built-in order).
    pub fn enabled_profile_names(&self) -> Vec<String> {
        self.info
            .profiles
            .iter()
            .filter(|(_, enabled)| *enabled)
            .map(|(p, _)| p.name.clone())
            .collect()
    }

    /// Whether the sandbox is currently enabled.
    pub fn is_enabled(&self) -> bool {
        self.info.enabled
    }

    /// Mirror the app's live YOLO state into the checkbox for display.
    pub fn set_yolo(&mut self, yolo: bool) {
        self.yolo = yolo;
    }

    /// Returns and resets the `yolo_changed` flag.
    /// The caller should check this after `handle_key` to apply to permissions.
    pub fn take_yolo_changed(&mut self) -> bool {
        std::mem::take(&mut self.yolo_changed)
    }

    fn toggle_yolo(&mut self) {
        self.yolo = !self.yolo;
        self.yolo_changed = true;
    }

    fn first_focus(info: &SandboxInfo) -> Option<InfoFocus> {
        if info.enabled {
            Some(InfoFocus::Yolo)
        } else if info.profiles.is_empty() {
            None
        } else {
            Some(InfoFocus::Profile(0))
        }
    }

    fn focus_count(info: &SandboxInfo) -> usize {
        info.profiles.len() + usize::from(info.enabled)
    }

    fn focus_index(info: &SandboxInfo, focus: &InfoFocus) -> usize {
        match focus {
            InfoFocus::Yolo => 0,
            InfoFocus::Profile(i) => usize::from(info.enabled) + i,
        }
    }

    fn focus_at(info: &SandboxInfo, idx: usize) -> Option<InfoFocus> {
        let base = usize::from(info.enabled);
        if idx < base {
            Some(InfoFocus::Yolo)
        } else if idx < base + info.profiles.len() {
            Some(InfoFocus::Profile(idx - base))
        } else {
            None
        }
    }

    fn move_focus(&mut self, delta: i32) {
        let Some(focus) = self.info_focus else {
            return;
        };
        let idx = Self::focus_index(&self.info, &focus) as i32 + delta;
        let count = Self::focus_count(&self.info) as i32;
        if idx < 0 || idx >= count {
            return;
        }
        self.info_focus = Self::focus_at(&self.info, idx as usize);
    }

    fn toggle_focus(&mut self) {
        let Some(focus) = self.info_focus else {
            return;
        };
        match focus {
            InfoFocus::Yolo => self.toggle_yolo(),
            InfoFocus::Profile(i) => {
                if let Some((_, enabled)) = self.info.profiles.get_mut(i) {
                    *enabled = !*enabled;
                    self.profiles_changed = true;
                    self.rebuild_env_entries();
                }
            }
        }
    }

    /// Re-homes the focus when the options list no longer contains it
    /// (sandbox toggled off with the YOLO checkbox focused, or a profile
    /// removed).
    fn clamp_focus(&mut self) {
        let Some(focus) = self.info_focus else {
            return;
        };
        let info = &self.info;
        let gone = matches!(focus, InfoFocus::Yolo) && !info.enabled;
        let out_of_range = Self::focus_index(info, &focus) >= Self::focus_count(info);
        if gone || out_of_range {
            self.info_focus = Self::first_focus(info);
        }
    }

    fn option_line(&self, selected: bool, checked: bool, label: &str) -> Line<'static> {
        let t = theme::current();
        let sel_style = if selected {
            t.item_selected
        } else {
            Style::default()
        };
        let prefix = if selected { "  ▸ " } else { "    " };
        let toggle = if checked { "x" } else { " " };
        Line::from(vec![
            Span::styled(prefix, sel_style),
            Span::styled("[", Style::default()),
            Span::styled(
                toggle,
                if checked {
                    t.item_selected
                } else {
                    Style::default()
                },
            ),
            Span::styled("] ", Style::default()),
            Span::styled(label.to_owned(), sel_style),
        ])
    }

    pub fn close(&mut self) {
        self.open = false;
        self.close_browser();
        self.close_shell();
        self.pending = None;
        self.spawn_error = None;
        self.scroll.reset();
        self.info_focus = None;
    }

    fn close_browser(&mut self) {
        self.browser.take();
    }

    fn close_shell(&mut self) {
        self.shell.take();
        self.shell_entry_count = 0;
    }

    fn rebuild_env_entries(&mut self) {
        let path_dirs: Vec<String> = self
            .info
            .profiles
            .iter()
            .filter(|(_, enabled)| *enabled)
            .flat_map(|(p, _)| &p.mounts)
            .filter(|m| m.usage == MountUsage::OnlyPath)
            .map(|m| m.sandbox_internal_path())
            .collect();
        let cfg = NamespaceConfig::new(
            self.info.allowed_env.clone(),
            vec![],
            std::path::PathBuf::new(),
            String::new(),
            vec![],
            vec![],
            path_dirs,
            vec![],
            vec![],
        );
        self.info.env_entries = cfg.effective_env();
    }

    fn toggle_sandbox_enabled(&mut self) {
        if !self.info.enabled {
            // Validate kernel support before enabling.
            if let Err(e) = maki_sandbox::namespace::probe() {
                self.spawn_error = Some(e.to_string());
                return;
            }
        }
        self.info.enabled = !self.info.enabled;
        self.enabled_changed = true;
        self.spawn_error = None;
        self.clamp_focus();
    }

    fn spawn_browser(&mut self) {
        if self.browser.is_some() || self.pending.is_some() {
            return;
        }
        self.close_shell();
        self.request_child(PendingJob::Browser, |sandbox| match sandbox.pwd() {
            Ok(pwd) => {
                let entries = sandbox.ls(&pwd).map_err(|e| e.to_string());
                SandboxModalEvent::BrowserReady(DirSnapshot { pwd, entries })
            }
            Err(e) => SandboxModalEvent::Failed(e.to_string()),
        });
    }

    fn spawn_shell(&mut self) {
        if self.shell.is_some() || self.pending.is_some() {
            return;
        }
        self.close_browser();
        self.shell_entry_count = 0;
        self.request_child(PendingJob::Shell, |sandbox| {
            SandboxModalEvent::ShellReady(sandbox.pwd().unwrap_or_default())
        });
    }

    /// Reinit the sandbox child with the modal's config on a blocking thread,
    /// then run `query` against it and deliver the result to the UI thread.
    fn request_child(
        &mut self,
        job: PendingJob,
        query: impl FnOnce(&Sandbox) -> SandboxModalEvent + Send + 'static,
    ) {
        let Some(tx) = self.event_tx.clone() else {
            self.spawn_error = Some("sandbox event channel unavailable".into());
            return;
        };
        let Some(sandbox) = self.sandbox.clone() else {
            self.spawn_error = Some("sandbox not available".into());
            return;
        };
        let config = self.build_spawn_config();
        self.pending = Some(job);
        smol::unblock(move || {
            if let Err(e) = sandbox.reinit(config) {
                tracing::error!(error = %e, "failed to reinit sandbox for modal");
                let _ = tx.send(SandboxModalEvent::Failed(e.to_string()));
                return;
            }
            let _ = tx.send(query(&sandbox));
        })
        .detach();
    }

    /// cd to `target` and refresh the listing, off the UI thread.
    fn navigate(&mut self, target: String) {
        if self.pending.is_some() {
            return;
        }
        let Some(tx) = self.event_tx.clone() else {
            return;
        };
        let Some(sandbox) = self.sandbox.clone() else {
            return;
        };
        let fallback_cwd = match &self.browser {
            Some(b) => b.cwd.clone(),
            None => return,
        };
        self.pending = Some(PendingJob::Navigate);
        smol::unblock(move || {
            if let Err(e) = sandbox.cd(&target) {
                let _ = tx.send(SandboxModalEvent::Navigated(DirSnapshot {
                    pwd: fallback_cwd,
                    entries: Err(e.to_string()),
                }));
                return;
            }
            let pwd = sandbox.pwd().unwrap_or(fallback_cwd);
            let entries = sandbox.ls(&pwd).map_err(|e| e.to_string());
            let _ = tx.send(SandboxModalEvent::Navigated(DirSnapshot { pwd, entries }));
        })
        .detach();
    }

    /// Run a shell command off the UI thread. Unlike spawns and navigations,
    /// execs are untracked: several may be in flight and results append in
    /// arrival order.
    fn exec_command(&mut self, command: String) {
        let (Some(tx), Some(sandbox)) = (self.event_tx.clone(), self.sandbox.clone()) else {
            if let Some(shell) = &mut self.shell {
                shell.error = Some("sandbox unavailable".into());
            }
            return;
        };
        smol::unblock(move || {
            let result = sandbox.exec(&command).map_err(|e| e.to_string());
            let _ = tx.send(SandboxModalEvent::Executed { command, result });
        })
        .detach();
    }

    /// Apply a finished background job to the modal state. Stale events
    /// (job superseded by a tab switch or a newer navigation) are dropped.
    pub(crate) fn apply(&mut self, event: SandboxModalEvent) {
        match event {
            SandboxModalEvent::BrowserReady(snapshot) => {
                if self.pending != Some(PendingJob::Browser) || self.mode != Mode::Browse {
                    return;
                }
                self.pending = None;
                match snapshot.entries {
                    Ok(entries) => {
                        self.spawn_error = None;
                        self.browser = Some(SandboxFileBrowser::new(snapshot.pwd, entries));
                    }
                    Err(e) => self.spawn_error = Some(e),
                }
            }
            SandboxModalEvent::Navigated(snapshot) => {
                if self.pending != Some(PendingJob::Navigate) {
                    return;
                }
                self.pending = None;
                let Some(browser) = &mut self.browser else {
                    return;
                };
                browser.cwd = snapshot.pwd;
                match snapshot.entries {
                    Ok(entries) => {
                        browser.error = None;
                        browser.apply_listing(entries);
                    }
                    Err(e) => browser.error = Some(e),
                }
            }
            SandboxModalEvent::ShellReady(pwd) => {
                if self.pending != Some(PendingJob::Shell) || self.mode != Mode::Shell {
                    return;
                }
                self.pending = None;
                self.shell = Some(SandboxShellState::new(pwd));
                self.spawn_error = None;
            }
            SandboxModalEvent::Failed(error) => {
                self.pending = None;
                self.spawn_error = Some(error);
            }
            SandboxModalEvent::Executed { command, result } => {
                if let Some(shell) = &mut self.shell {
                    shell.push_result(command, result);
                }
            }
        }
    }

    fn build_namespace_config(&self) -> NamespaceConfig {
        let extra_home_mounts: Vec<(PathBuf, String)> = self
            .info
            .home_mounts
            .iter()
            .map(|(host, name)| (PathBuf::from(host), name.clone()))
            .collect();
        let extra_workspace_dirs: Vec<(PathBuf, String)> = self
            .info
            .extra_workspace_dirs
            .iter()
            .map(|(host, name)| (PathBuf::from(host), name.clone()))
            .collect();
        let enabled_profiles: Vec<profiles::SandboxProfile> = self
            .info
            .profiles
            .iter()
            .filter(|(_, enabled)| *enabled)
            .map(|(p, _)| p.clone())
            .collect();
        profiles::build_namespace_config(
            &enabled_profiles,
            PathBuf::from(&self.info.workspace_dir),
            self.info.workspace_name.clone(),
            extra_home_mounts,
            extra_workspace_dirs,
        )
    }

    /// Config for a real child spawn: like [`Self::build_namespace_config`],
    /// but drops mount sources missing on this machine so an enabled profile
    /// with absent directories cannot fail the spawn.
    fn build_spawn_config(&self) -> NamespaceConfig {
        let mut config = self.build_namespace_config();
        config.prune_missing_mounts();
        config
    }

    pub fn handle_key(&mut self, key_event: KeyEvent) -> bool {
        match key_event.code {
            KeyCode::Esc => {
                self.close();
                true
            }
            KeyCode::Tab => {
                self.mode = match self.mode {
                    Mode::Info => {
                        if self.info.enabled {
                            self.spawn_browser();
                            Mode::Browse
                        } else {
                            Mode::Info
                        }
                    }
                    Mode::Browse => {
                        self.close_browser();
                        if self.info.enabled {
                            self.spawn_shell();
                            self.scroll.scroll_to_bottom();
                            Mode::Shell
                        } else {
                            Mode::Info
                        }
                    }
                    Mode::Shell => {
                        self.close_shell();
                        Mode::Info
                    }
                };
                self.scroll.reset();
                self.h_scroll = 0;
                true
            }
            _ if self.mode == Mode::Browse => self.handle_browse_key(key_event),
            _ if self.mode == Mode::Shell => self.handle_shell_key(key_event),
            _ if self.mode == Mode::Info && self.info_focus.is_some() => match key_event.code {
                KeyCode::Char('s') => {
                    self.toggle_sandbox_enabled();
                    true
                }
                KeyCode::Up => {
                    self.move_focus(-1);
                    true
                }
                KeyCode::Down => {
                    self.move_focus(1);
                    true
                }
                KeyCode::Enter | KeyCode::Char(' ') => {
                    self.toggle_focus();
                    true
                }
                _ => {
                    self.scroll.handle_key(key_event);
                    true
                }
            },
            _ => {
                self.scroll.handle_key(key_event);
                true
            }
        }
    }

    fn handle_browse_key(&mut self, key_event: KeyEvent) -> bool {
        let Some(browser) = &mut self.browser else {
            return true;
        };
        match key_event.code {
            KeyCode::Enter | KeyCode::Right => {
                let target = match browser.entries.get(browser.cursor) {
                    Some(entry) if entry.name == ".." => {
                        parent_dir(&browser.cwd).unwrap_or_else(|| "/".into())
                    }
                    Some(entry) if entry.is_dir => {
                        let sep = if browser.cwd.ends_with('/') { "" } else { "/" };
                        format!("{}{}{}", browser.cwd, sep, entry.name)
                    }
                    _ => return true,
                };
                self.navigate(target);
                true
            }
            KeyCode::Backspace | KeyCode::Left => {
                if let Some(parent) = parent_dir(&browser.cwd) {
                    self.navigate(parent);
                }
                true
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if browser.cursor > 0 {
                    browser.cursor -= 1;
                    browser.clamp_cursor();
                }
                true
            }
            KeyCode::Down | KeyCode::Char('j') => {
                let max = browser.entries.len().saturating_sub(1);
                if browser.cursor < max {
                    browser.cursor += 1;
                    browser.clamp_cursor();
                }
                true
            }
            _ => true,
        }
    }

    fn handle_shell_key(&mut self, key_event: KeyEvent) -> bool {
        let Some(shell) = &mut self.shell else {
            return true;
        };
        match key_event.code {
            KeyCode::Enter => {
                if let Some(command) = shell.submit() {
                    self.exec_command(command);
                    self.scroll.scroll_to_bottom();
                    self.h_scroll = 0;
                }
                true
            }
            KeyCode::Up => {
                shell.history_up();
                true
            }
            KeyCode::Down => {
                shell.history_down();
                true
            }
            KeyCode::Backspace => {
                shell.input.pop();
                true
            }
            KeyCode::Char(ch) => {
                shell.input.push(ch);
                true
            }
            KeyCode::Left => {
                self.h_scroll = self.h_scroll.saturating_sub(1);
                true
            }
            KeyCode::Right => {
                self.h_scroll = self.h_scroll.saturating_add(1);
                true
            }
            KeyCode::PageUp | KeyCode::PageDown | KeyCode::Home | KeyCode::End => {
                self.scroll.handle_key(key_event);
                true
            }
            _ => true,
        }
    }

    pub fn handle_paste(&mut self, text: &str) -> bool {
        if !self.open || self.mode != Mode::Shell {
            return false;
        }
        let mut submitted = Vec::new();
        if let Some(shell) = &mut self.shell {
            for ch in text.chars() {
                if ch == '\n' || ch == '\r' {
                    if let Some(command) = shell.submit() {
                        submitted.push(command);
                    }
                    self.scroll.scroll_to_bottom();
                } else {
                    shell.input.push(ch);
                }
            }
        }
        for command in submitted {
            self.exec_command(command);
        }
        self.shell.is_some()
    }

    fn render_info(&mut self, lines: &mut Vec<Line>) {
        let t = theme::current();
        self.clamp_focus();
        let info = &self.info;

        // Status
        lines.push(Line::from(Span::styled(
            "  Status (press s to toggle)",
            t.keybind_section,
        )));
        let status_text = if info.enabled { "enabled" } else { "disabled" };
        lines.push(Line::from(format!("    {status_text}")));

        // Options — YOLO checkbox (only while the sandbox is enabled),
        // then profiles. One list, navigable with ↑/↓.
        if info.enabled || !info.profiles.is_empty() {
            lines.push(Line::default());
            lines.push(Line::from(Span::styled(
                "  Options (navigate with ↑/↓, toggle with Enter/Space)",
                t.keybind_section,
            )));
            if info.enabled {
                let selected = self.info_focus == Some(InfoFocus::Yolo);
                lines.push(self.option_line(selected, self.yolo, YOLO_LABEL));
            }
            for (i, (profile, enabled)) in info.profiles.iter().enumerate() {
                let selected = self.info_focus == Some(InfoFocus::Profile(i));
                lines.push(self.option_line(selected, *enabled, &profile.name));
                // Show mount details for the focused profile
                if selected {
                    let mount_parts: Vec<String> = profile
                        .mounts
                        .iter()
                        .map(|m| format!("{} ({})", m.path, m.usage.label()))
                        .collect();
                    lines.push(Line::from(Span::styled(
                        format!("      Mounts: {}", mount_parts.join(", ")),
                        t.tool_dim,
                    )));
                }
            }
        }

        // Filesystem layout
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            "  Filesystem layout",
            t.keybind_section,
        )));

        let ws_label = format!("    /home/maki/workspace  ←  {}  (rw)", info.workspace_dir,);
        lines.push(Line::from(Span::styled(ws_label, Style::default())));

        lines.push(Line::from(Span::styled(
            "    /usr                       (ro, system)",
            t.tool_dim,
        )));
        lines.push(Line::from(Span::styled(
            "    /etc                       (tmpfs, empty)",
            t.tool_dim,
        )));
        lines.push(Line::from(Span::styled(
            "    /tmp                       (tmpfs, scratch)",
            t.tool_dim,
        )));
        lines.push(Line::from(Span::styled(
            "    /bin → /usr/bin            (symlink)",
            t.tool_dim,
        )));
        lines.push(Line::from(Span::styled(
            "    /sbin → /usr/sbin          (symlink)",
            t.tool_dim,
        )));
        lines.push(Line::from(Span::styled(
            "    /lib → /usr/lib            (symlink)",
            t.tool_dim,
        )));
        lines.push(Line::from(Span::styled(
            "    /lib64 → /usr/lib64        (symlink)",
            t.tool_dim,
        )));

        // Mounts from enabled profiles
        for (profile, enabled) in &info.profiles {
            if !enabled {
                continue;
            }
            for mount in &profile.mounts {
                if mount.usage == MountUsage::OnlyPath {
                    continue;
                }
                let label = if mount.usage == MountUsage::SymLink {
                    format!(
                        "    {}  →  {}  (symlink)  [{}]",
                        mount.sandbox_internal_path(),
                        mount.path,
                        profile.name
                    )
                } else {
                    format!(
                        "    /home/maki/{}  ←  {}  ({})  [{}]",
                        mount.dir_name(),
                        mount.path,
                        mount.usage.label(),
                        profile.name
                    )
                };
                lines.push(Line::from(Span::styled(label, Style::default())));
            }
        }

        // Home directory mounts
        if !info.home_mounts.is_empty() {
            lines.push(Line::default());
            lines.push(Line::from(Span::styled(
                "  Home directory mounts (rw)",
                t.keybind_section,
            )));
            for (host, name) in &info.home_mounts {
                let label = format!("    /home/maki/{name}  ←  {host}");
                lines.push(Line::from(Span::styled(label, Style::default())));
            }
        }

        // Extra workspace directories
        if !info.extra_workspace_dirs.is_empty() {
            lines.push(Line::default());
            lines.push(Line::from(Span::styled(
                "  Extra workspace directories (rw)",
                t.keybind_section,
            )));
            for (host, name) in &info.extra_workspace_dirs {
                let ws = &info.workspace_name;
                let label = format!("    /home/maki/workspace/{ws}/{name}  ←  {host}");
                lines.push(Line::from(Span::styled(label, Style::default())));
            }
        }

        // Environment variables
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            "  Environment variables (allow list)",
            t.keybind_section,
        )));
        for entry in &info.env_entries {
            let display_val = if entry.value.is_empty() {
                String::new()
            } else {
                format!(" = \"{}\"", entry.value)
            };
            let label = if entry.description.is_empty() {
                format!("    {}{display_val}", entry.key)
            } else {
                format!("    {}{display_val}  —  {}", entry.key, entry.description)
            };
            lines.push(Line::from(Span::styled(label, Style::default())));
        }

        // Show PATH directories contributed by enabled profiles
        let path_dirs: Vec<String> = info
            .profiles
            .iter()
            .filter(|(_, enabled)| *enabled)
            .flat_map(|(profile, _)| &profile.mounts)
            .filter(|m| m.usage == MountUsage::OnlyPath)
            .map(|m| m.sandbox_internal_path())
            .collect();
        if !path_dirs.is_empty() {
            lines.push(Line::from(Span::styled(
                format!(
                    "    (extended by active profiles: {})",
                    path_dirs.join(", ")
                ),
                t.tool_dim,
            )));
        }

        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            "  Not passed to sandbox (private)",
            t.keybind_section,
        )));
        lines.push(Line::from(Span::styled(
            "    API keys, SSH tokens, GITHUB_TOKEN, AWS_* — everything not on the allow list above",
            t.tool_dim,
        )));
    }

    fn render_browse(&mut self, lines: &mut Vec<Line>, viewport_h: u16) {
        let t = theme::current();
        let Some(browser) = &mut self.browser else {
            let msg = self
                .spawn_error
                .as_deref()
                .unwrap_or("sandbox not available");
            lines.push(Line::from(Span::styled(format!("  ({msg})"), t.tool_error)));
            return;
        };

        let chrome: u16 = 2;
        browser.viewport_entries = viewport_h.saturating_sub(chrome) as usize;

        if let Some(ref err) = browser.error {
            lines.push(Line::from(Span::styled(
                format!("  error: {err}"),
                t.tool_error,
            )));
        }

        for (i, entry) in browser.visible_entries().iter().enumerate() {
            let idx = browser.scroll + i;
            let selected = idx == browser.cursor;
            let prefix = if selected { "  ▸ " } else { "    " };
            let name = if entry.name == ".." {
                "../".to_string()
            } else if entry.is_dir {
                format!("{}/", entry.name)
            } else {
                entry.name.clone()
            };
            let style = if selected {
                t.item_selected
            } else if entry.is_dir {
                Style::default()
            } else {
                t.tool_dim
            };
            lines.push(Line::from(Span::styled(format!("{prefix}{name}"), style)));
        }
    }

    fn render_shell(&mut self, lines: &mut Vec<Line>) {
        let t = theme::current();
        let Some(shell) = &self.shell else {
            let msg = self
                .spawn_error
                .as_deref()
                .unwrap_or("sandbox not available");
            lines.push(Line::from(Span::styled(format!("  ({msg})"), t.tool_error)));
            return;
        };
        for entry in &shell.entries {
            let prompt = format!("  {}:$ ", shell.cwd);
            lines.push(Line::from(vec![
                Span::styled(prompt, t.keybind_section),
                Span::styled(entry.command.clone(), Style::default()),
            ]));
            if !entry.output.is_empty() {
                let style = if entry.is_error {
                    t.tool_error
                } else {
                    Style::default()
                };
                for line in entry.output.lines() {
                    lines.push(Line::from(Span::styled(format!("    {line}"), style)));
                }
            }
            lines.push(Line::default());
        }
        if let Some(ref err) = shell.error {
            lines.push(Line::from(Span::styled(
                format!("  error: {err}"),
                t.tool_error,
            )));
            lines.push(Line::default());
        }
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        if !self.open {
            return Rect::default();
        }

        let t = theme::current();

        let max_h = (area.height as u32 * MAX_HEIGHT_PERCENT as u32 / 100)
            .max(CHROME_LINES as u32 + 1) as u16;
        let viewport_h = max_h.saturating_sub(CHROME_LINES);

        let mut lines: Vec<Line> = Vec::new();

        match self.mode {
            Mode::Info => {
                self.render_info(&mut lines);
                lines.push(Line::default());
                let tab_hint = if self.info.enabled {
                    "Tab: Browse files"
                } else {
                    "Tab: Browse (enable sandbox_enabled in config first)"
                };
                lines.push(Line::from(Span::styled(tab_hint, t.keybind_desc)));
            }
            Mode::Browse => {
                self.render_browse(&mut lines, viewport_h.saturating_sub(1));
                lines.insert(0, Line::default());
                lines.push(Line::default());
                lines.push(Line::from(Span::styled(
                    "  Tab: Info  |  Esc: Close  |  Enter: Open dir  |  Backspace: Up",
                    t.keybind_desc,
                )));
            }
            Mode::Shell => {
                self.render_shell(&mut lines);
            }
        }

        // For Shell mode we need the inner area to know input dimensions,
        // so render modal early.
        let modal = Modal {
            title: TITLE,
            width_percent: WIDTH_PERCENT,
            max_height_percent: MAX_HEIGHT_PERCENT,
        };
        let modal_lines = if self.mode == Mode::Shell {
            // Enforce minimum so the content area (split from inner) has ~10 visible lines.
            // The inner area is split into content + hints + input, so modal_lines >= 13
            // gives content >= 10 (assuming max_h isn't too constrained).
            lines.len().saturating_sub(1).max(13) as u16
        } else {
            lines.len() as u16
        };
        let (popup, inner) = modal.render(frame, area, modal_lines);

        match self.mode {
            Mode::Browse => {
                let [header_area, content_area] =
                    Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]).areas(inner);

                let cwd = self.browser.as_ref().map(|b| b.cwd.as_str()).unwrap_or("/");
                let header = format!("  sandbox:{cwd}");
                frame.render_widget(
                    Paragraph::new(Line::from(Span::styled(&header, t.keybind_section))),
                    header_area,
                );

                self.scroll
                    .update_dimensions(lines.len() as u16, content_area.height);
                let scroll = self.scroll.offset();
                frame.render_widget(Paragraph::new(lines).scroll((scroll, 0)), content_area);

                let scroll_total = self
                    .browser
                    .as_ref()
                    .map(|b| b.total_entries())
                    .unwrap_or(0);
                let scroll_pos = self
                    .browser
                    .as_ref()
                    .map(|b| b.scroll_offset())
                    .unwrap_or(0);
                if scroll_total > content_area.height as usize {
                    render_vertical_scrollbar(
                        frame,
                        content_area,
                        scroll_total as u32,
                        scroll_pos as u32,
                    );
                }
            }
            Mode::Info => {
                let total = lines.len() as u16;
                self.scroll.update_dimensions(total, viewport_h);
                let scroll = self.scroll.offset();
                frame.render_widget(Paragraph::new(lines).scroll((scroll, 0)), inner);
                let scroll_pos = scroll as usize;
                if total as usize > viewport_h as usize {
                    render_vertical_scrollbar(frame, inner, total as u32, scroll_pos as u32);
                }
            }
            Mode::Shell => {
                let [content_area, input_area, hints_area] = Layout::vertical([
                    Constraint::Fill(1),
                    Constraint::Length(1),
                    Constraint::Length(1),
                ])
                .areas(inner);

                let total = lines.len() as u16;
                self.scroll.update_dimensions(total, content_area.height);
                // Auto-scroll to bottom when new entries appear
                if let Some(s) = &self.shell
                    && s.entries.len() > self.shell_entry_count
                {
                    self.scroll.scroll_to_bottom();
                    self.shell_entry_count = s.entries.len();
                }
                let scroll = self.scroll.offset();
                frame.render_widget(
                    Paragraph::new(lines).scroll((scroll, self.h_scroll as u16)),
                    content_area,
                );

                let scroll_pos = scroll as usize;
                if total as usize > content_area.height as usize {
                    render_vertical_scrollbar(frame, content_area, total as u32, scroll_pos as u32);
                }

                // Fixed key hints line between content and input
                frame.render_widget(
                    Paragraph::new(Line::from(Span::styled(
                        "  Tab: Browse  |  Esc: Close  |  Enter: Run  |  Up/Down: History  |  Left/Right: Scroll",
                        t.keybind_desc,
                    ))),
                    hints_area,
                );

                // Input line at bottom
                let prompt = match &self.shell {
                    Some(s) => format!("  {}:$ ", s.cwd),
                    None => "  sandbox:$ ".into(),
                };
                let input_content = match &self.shell {
                    Some(s) => s.input.clone(),
                    None => String::new(),
                };
                let input_line = Line::from(vec![
                    Span::styled(prompt, t.keybind_section),
                    Span::styled(input_content, Style::default()),
                ]);
                frame.render_widget(Paragraph::new(input_line), input_area);
            }
        }

        popup
    }
}

impl Overlay for SandboxModal {
    fn is_open(&self) -> bool {
        self.is_open()
    }

    fn close(&mut self) {
        SandboxModal::close(self);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::key as key_ev;
    use crossterm::event::KeyCode;
    use maki_sandbox::profiles::ProfileMount;
    use test_case::test_case;

    fn test_info() -> SandboxInfo {
        SandboxInfo {
            enabled: false,
            env_entries: vec![],
            allowed_env: vec![],
            workspace_dir: "/tmp".into(),
            workspace_name: "tmp".into(),
            home_mounts: vec![],
            profiles: vec![],
            extra_workspace_dirs: vec![],
        }
    }

    #[test_case(key_ev(KeyCode::Esc) ; "esc_closes")]
    fn handle_key_closes(k: KeyEvent) {
        let mut modal = SandboxModal::new(test_info(), None);
        modal.toggle();
        assert!(modal.handle_key(k));
        assert!(!modal.is_open());
    }

    #[test]
    fn handle_key_consumes_all() {
        let mut modal = SandboxModal::new(test_info(), None);
        modal.toggle();
        assert!(modal.handle_key(key_ev(KeyCode::Char('a'))));
        assert!(modal.is_open());
    }

    #[test]
    fn tab_toggles_mode() {
        let mut modal = SandboxModal::new(test_info(), None);
        modal.toggle();
        assert_eq!(modal.mode, Mode::Info);
        modal.handle_key(key_ev(KeyCode::Tab));
        // Sandbox disabled, so should stay in Info mode
        assert_eq!(
            modal.mode,
            Mode::Info,
            "should stay in info when sandbox disabled"
        );
    }

    #[test]
    fn tab_switches_to_browse_when_enabled() {
        let mut modal = SandboxModal::new(
            SandboxInfo {
                enabled: true,
                env_entries: vec![],
                allowed_env: vec![],
                workspace_dir: "/tmp".into(),
                workspace_name: "tmp".into(),
                home_mounts: vec![],
                profiles: vec![],
                extra_workspace_dirs: vec![],
            },
            None,
        );
        modal.toggle();
        modal.handle_key(key_ev(KeyCode::Tab));
        assert_eq!(modal.mode, Mode::Browse);
    }

    fn info_with(enabled: bool, profile_count: usize) -> SandboxInfo {
        let profiles = (0..profile_count)
            .map(|i| {
                (
                    SandboxProfile {
                        name: format!("p{i}"),
                        mounts: vec![],
                    },
                    false,
                )
            })
            .collect();
        SandboxInfo {
            enabled,
            env_entries: vec![],
            allowed_env: vec![],
            workspace_dir: "/tmp".into(),
            workspace_name: "tmp".into(),
            home_mounts: vec![],
            profiles,
            extra_workspace_dirs: vec![],
        }
    }

    #[test]
    fn updown_navigates_yolo_and_profiles() {
        let mut modal = SandboxModal::new(info_with(true, 2), None);
        modal.toggle();
        assert_eq!(modal.info_focus, Some(InfoFocus::Yolo));
        modal.handle_key(key_ev(KeyCode::Down));
        assert_eq!(modal.info_focus, Some(InfoFocus::Profile(0)));
        modal.handle_key(key_ev(KeyCode::Down));
        assert_eq!(modal.info_focus, Some(InfoFocus::Profile(1)));
        modal.handle_key(key_ev(KeyCode::Down));
        assert_eq!(
            modal.info_focus,
            Some(InfoFocus::Profile(1)),
            "should not wrap past the last option"
        );
        modal.handle_key(key_ev(KeyCode::Up));
        modal.handle_key(key_ev(KeyCode::Up));
        assert_eq!(modal.info_focus, Some(InfoFocus::Yolo));
        modal.handle_key(key_ev(KeyCode::Up));
        assert_eq!(
            modal.info_focus,
            Some(InfoFocus::Yolo),
            "should not wrap past the first option"
        );
    }

    #[test]
    fn updown_navigates_profiles_only_when_disabled() {
        let mut modal = SandboxModal::new(info_with(false, 2), None);
        modal.toggle();
        assert_eq!(modal.info_focus, Some(InfoFocus::Profile(0)));
        modal.handle_key(key_ev(KeyCode::Down));
        assert_eq!(modal.info_focus, Some(InfoFocus::Profile(1)));
    }

    #[test]
    fn info_renders_symlink_mounts_at_absolute_sandbox_path() {
        let profile = SandboxProfile {
            name: "rust".into(),
            mounts: vec![
                ProfileMount::rw("~/.cargo"),
                ProfileMount::sym_link("/etc/alternatives/cc"),
            ],
        };
        let mut modal = SandboxModal::new(
            SandboxInfo {
                enabled: true,
                env_entries: vec![],
                allowed_env: vec![],
                workspace_dir: "/tmp".into(),
                workspace_name: "tmp".into(),
                home_mounts: vec![],
                profiles: vec![(profile, true)],
                extra_workspace_dirs: vec![],
            },
            None,
        );
        let mut lines = Vec::new();
        modal.render_info(&mut lines);
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.contains("/etc/alternatives/cc  →  /etc/alternatives/cc"),
            "symlink should render as absolute sandbox path, got:\n{text}"
        );
        assert!(
            !text.contains("/home/maki//etc"),
            "symlink must not be treated as a home-relative path"
        );
        assert!(text.contains("/home/maki/.cargo  ←  "));
    }

    #[test]
    fn enter_toggles_focused_option() {
        let mut modal = SandboxModal::new(info_with(true, 1), None);
        modal.toggle();
        modal.handle_key(key_ev(KeyCode::Enter));
        assert!(modal.yolo, "Enter should toggle the YOLO checkbox");
        assert!(modal.take_yolo_changed());
        modal.handle_key(key_ev(KeyCode::Down));
        modal.handle_key(key_ev(KeyCode::Char(' ')));
        assert!(modal.info.profiles[0].1, "Space should toggle the profile");
    }

    #[test]
    fn s_toggling_sandbox_off_rehomes_yolo_focus() {
        let mut modal = SandboxModal::new(info_with(true, 0), None);
        modal.toggle();
        assert_eq!(modal.info_focus, Some(InfoFocus::Yolo));
        modal.handle_key(key_ev(KeyCode::Char('s')));
        assert_eq!(modal.info_focus, None);
    }

    #[test]
    fn tab_cycles_through_modes() {
        let mut modal = SandboxModal::new(
            SandboxInfo {
                enabled: true,
                env_entries: vec![],
                allowed_env: vec![],
                workspace_dir: "/tmp".into(),
                workspace_name: "tmp".into(),
                home_mounts: vec![],
                profiles: vec![],
                extra_workspace_dirs: vec![],
            },
            None,
        );
        modal.toggle();
        assert_eq!(modal.mode, Mode::Info);
        // Info -> Browse
        modal.handle_key(key_ev(KeyCode::Tab));
        assert_eq!(modal.mode, Mode::Browse);
        // Browse -> Shell
        modal.handle_key(key_ev(KeyCode::Tab));
        assert_eq!(modal.mode, Mode::Shell);
        // Shell -> Info
        modal.handle_key(key_ev(KeyCode::Tab));
        assert_eq!(modal.mode, Mode::Info);
    }

    fn dir(name: &str, is_dir: bool) -> DirEntry {
        DirEntry {
            name: name.into(),
            is_dir,
        }
    }

    #[test]
    fn browser_ready_applies_only_with_pending_job() {
        let mut modal = SandboxModal::new(info_with(true, 0), None);
        modal.toggle();
        modal.mode = Mode::Browse;
        let event = |entries| {
            SandboxModalEvent::BrowserReady(DirSnapshot {
                pwd: "/".into(),
                entries: Ok(entries),
            })
        };
        modal.apply(event(vec![dir("tmp", true)]));
        assert!(
            modal.browser.is_none(),
            "event without a pending job must be dropped"
        );
        modal.pending = Some(PendingJob::Browser);
        modal.apply(event(vec![]));
        let browser = modal.browser.expect("pending spawn should apply");
        assert_eq!(browser.cwd, "/");
        assert!(browser.entries.is_empty());
    }

    #[test]
    fn navigated_event_replaces_listing_and_resets_cursor() {
        let mut modal = SandboxModal::new(info_with(true, 0), None);
        modal.toggle();
        modal.pending = Some(PendingJob::Navigate);
        modal.browser = Some(SandboxFileBrowser::new("/".into(), vec![dir("tmp", true)]));
        modal.browser.as_mut().unwrap().cursor = 5;

        modal.apply(SandboxModalEvent::Navigated(DirSnapshot {
            pwd: "/tmp".into(),
            entries: Ok(vec![dir("a", false), dir("b", true)]),
        }));
        let browser = modal.browser.expect("browser stays open");
        assert_eq!(browser.cwd, "/tmp");
        assert_eq!(browser.cursor, 0, "navigation resets the cursor");
        let names: Vec<&str> = browser.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["..", "b", "a"], "dirs first, then .. prepended");
    }

    #[test]
    fn navigated_error_keeps_old_listing() {
        let mut modal = SandboxModal::new(info_with(true, 0), None);
        modal.toggle();
        modal.pending = Some(PendingJob::Navigate);
        modal.browser = Some(SandboxFileBrowser::new(
            "/tmp".into(),
            vec![dir("keep", false)],
        ));

        modal.apply(SandboxModalEvent::Navigated(DirSnapshot {
            pwd: "/tmp".into(),
            entries: Err("cd failed".into()),
        }));
        let browser = modal.browser.expect("browser stays open");
        assert_eq!(browser.cwd, "/tmp", "failed cd keeps the directory");
        assert_eq!(
            browser.error.as_deref(),
            Some("cd failed"),
            "the error is surfaced"
        );
        assert_eq!(browser.total_entries(), 2, ".. plus original entry kept");
    }

    #[test]
    fn shell_executed_event_appends_entry_and_clears_error() {
        let mut modal = SandboxModal::new(info_with(true, 0), None);
        modal.toggle();
        modal.mode = Mode::Shell;
        modal.shell = Some(SandboxShellState::new("/".into()));
        modal.shell.as_mut().unwrap().input.push_str("echo hi");
        modal.handle_key(key_ev(KeyCode::Enter));
        let shell = modal.shell.as_ref().unwrap();
        assert_eq!(shell.history, ["echo hi"]);
        assert_eq!(shell.input, "", "input is cleared on submit");

        modal.apply(SandboxModalEvent::Executed {
            command: "echo hi".into(),
            result: Ok(("hi".into(), false)),
        });
        modal.apply(SandboxModalEvent::Executed {
            command: "false".into(),
            result: Ok((String::new(), true)),
        });
        let shell = modal.shell.as_ref().unwrap();
        assert_eq!(shell.entries.len(), 2);
        assert!(shell.entries[1].is_error);
        assert!(shell.error.is_none());
    }

    #[test]
    fn failed_event_surfaces_spawn_error() {
        let mut modal = SandboxModal::new(info_with(true, 0), None);
        modal.toggle();
        modal.pending = Some(PendingJob::Shell);
        modal.apply(SandboxModalEvent::Failed("spawn failed".into()));
        assert_eq!(modal.spawn_error.as_deref(), Some("spawn failed"));
        assert_eq!(modal.pending, None);
    }
}
