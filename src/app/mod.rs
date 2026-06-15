pub mod constants;
pub mod dialogs;
pub mod keybinding_recorder;
pub mod startup;
pub mod theme;
pub mod ui;

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    ops::Range,
    rc::Rc,
    sync::mpsc,
    time::{Duration, Instant},
};

use gpui::{
    AppContext as _, Bounds, Context, Entity, FocusHandle, Pixels, Point, SharedString, Size,
    UniformListScrollHandle, Window, point, px, size,
};
use gpui_component::{
    Theme, ThemeMode, ThemeRegistry,
    input::{InputEvent, InputState},
    resizable::ResizableState,
    scroll::ScrollbarHandle,
};
use rust_i18n::t;
use tokio::runtime::Runtime;

use crate::{
    session::config::{AuthMethod, ConfigStore},
    system::{SystemSampler, SystemSnapshot},
    terminal::{self, BackendEvent, TabKind, TerminalTab},
};

#[derive(Clone, Debug)]
pub(crate) enum PaneLayout {
    Single(String),
    Horizontal(Vec<PaneLayout>, f32), // children, split_ratio (0.0-1.0)
    Vertical(Vec<PaneLayout>, f32),   // children, split_ratio (0.0-1.0)
}

#[derive(Clone)]
pub(crate) struct TabGroup {
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) pane_root: PaneLayout,
    pub(crate) sftp: Option<crate::terminal::SftpUiState>,
}

impl PaneLayout {
    pub fn tab_ids(&self) -> Vec<&str> {
        match self {
            PaneLayout::Single(id) => vec![id.as_str()],
            PaneLayout::Horizontal(children, _) | PaneLayout::Vertical(children, _) => {
                children.iter().flat_map(|c| c.tab_ids()).collect()
            }
        }
    }

    pub fn contains(&self, tab_id: &str) -> bool {
        match self {
            PaneLayout::Single(id) => id == tab_id,
            PaneLayout::Horizontal(children, _) | PaneLayout::Vertical(children, _) => {
                children.iter().any(|c| c.contains(tab_id))
            }
        }
    }

    pub fn focused_tab_id(&self, path: &[usize]) -> Option<&str> {
        match self {
            PaneLayout::Single(id) if path.is_empty() => Some(id.as_str()),
            PaneLayout::Horizontal(children, _) | PaneLayout::Vertical(children, _) => {
                let (&first, rest) = path.split_first()?;
                children.get(first).and_then(|c| c.focused_tab_id(rest))
            }
            _ => None,
        }
    }

    pub fn replace_at(&mut self, path: &[usize], replacement: PaneLayout) {
        match (self, path) {
            (this @ PaneLayout::Single(_), []) => *this = replacement,
            (
                PaneLayout::Horizontal(children, _) | PaneLayout::Vertical(children, _),
                [first, rest @ ..],
            ) => {
                if let Some(child) = children.get_mut(*first) {
                    child.replace_at(rest, replacement);
                }
            }
            _ => {}
        }
    }

    pub fn remove_tab(&mut self, tab_id: &str) -> bool {
        match self {
            PaneLayout::Single(id) if id == tab_id => {
                *self = PaneLayout::Single(String::new());
                true
            }
            PaneLayout::Single(_) => false,
            PaneLayout::Horizontal(children, _) | PaneLayout::Vertical(children, _) => {
                for child in children.iter_mut() {
                    child.remove_tab(tab_id);
                }
                children.retain(|c| !matches!(c, PaneLayout::Single(id) if id.is_empty()));
                if children.is_empty() {
                    *self = PaneLayout::Single(String::new());
                } else if children.len() == 1 {
                    if let Some(replacement) = children.pop() {
                        *self = replacement;
                    }
                }
                true
            }
        }
    }

    #[allow(dead_code)]
    pub fn total_panes(&self) -> usize {
        match self {
            PaneLayout::Single(_) => 1,
            PaneLayout::Horizontal(children, _) | PaneLayout::Vertical(children, _) => {
                children.iter().map(|c| c.total_panes()).sum()
            }
        }
    }
}

pub(crate) struct TerminalScrollbarState {
    line_height: Pixels,
    total_lines: usize,
    viewport_lines: usize,
    display_offset: usize,
}

#[derive(Clone, Default)]
pub(crate) struct TerminalScrollbarHandle {
    state: Rc<RefCell<Option<TerminalScrollbarState>>>,
    pub(crate) future_display_offset: Rc<Cell<Option<usize>>>,
}

impl TerminalScrollbarHandle {
    pub(crate) fn update(&self, snapshot: &terminal::RenderSnapshot, line_height: Pixels) {
        self.state.replace(Some(TerminalScrollbarState {
            line_height,
            total_lines: snapshot.history_size + snapshot.rows,
            viewport_lines: snapshot.rows,
            display_offset: snapshot.display_offset,
        }));
    }
}

impl ScrollbarHandle for TerminalScrollbarHandle {
    fn offset(&self) -> Point<Pixels> {
        let state_ref = self.state.borrow();
        let Some(state) = state_ref.as_ref() else {
            return point(px(0.), px(0.));
        };
        let scroll_offset = state
            .total_lines
            .saturating_sub(state.viewport_lines)
            .saturating_sub(state.display_offset);
        point(px(0.), -(scroll_offset as f32 * state.line_height))
    }

    fn set_offset(&self, offset: Point<Pixels>) {
        let state_ref = self.state.borrow();
        let Some(state) = state_ref.as_ref() else {
            return;
        };
        let offset_delta = (offset.y / state.line_height).round() as i32;
        let max_offset = state.total_lines.saturating_sub(state.viewport_lines);
        let display_offset = (max_offset as i32 + offset_delta).clamp(0, max_offset as i32);
        self.future_display_offset
            .set(Some(display_offset as usize));
    }

    fn content_size(&self) -> Size<Pixels> {
        let state_ref = self.state.borrow();
        let Some(state) = state_ref.as_ref() else {
            return size(px(0.), px(0.));
        };
        size(
            px(0.),
            state.total_lines.max(state.viewport_lines) as f32 * state.line_height,
        )
    }
}

pub(crate) struct Ashell {
    pub(crate) focus_handle: FocusHandle,
    pub(crate) selector_focus_handle: FocusHandle,
    pub(crate) host_input: Entity<InputState>,
    pub(crate) session_name_input: Entity<InputState>,
    pub(crate) port_input: Entity<InputState>,
    pub(crate) user_input: Entity<InputState>,
    pub(crate) password_input: Entity<InputState>,
    pub(crate) key_path_input: Entity<InputState>,
    pub(crate) key_inline_input: Entity<InputState>,
    pub(crate) sftp_path_input: Entity<InputState>,
    pub(crate) ssh_auth_method: AuthMethod,
    pub(crate) editing_session_id: Option<String>,
    pub(crate) follow_system_theme: bool,
    pub(crate) theme_mode: ThemeMode,
    pub(crate) light_theme_name: SharedString,
    pub(crate) dark_theme_name: SharedString,
    pub(crate) ui_font_size: f32,
    pub(crate) terminal_font_size: f32,
    pub(crate) ui_font_family: SharedString,
    pub(crate) terminal_font_family: SharedString,
    pub(crate) tabs: Vec<TerminalTab>,
    pub(crate) active_tab: Option<String>,
    pub(crate) tab_groups: Vec<TabGroup>,
    pub(crate) active_group: Option<String>,
    pub(crate) selector_selection: usize,
    pub(crate) workspace_panels: Entity<ResizableState>,
    pub(crate) body_panels: Entity<ResizableState>,
    pub(crate) is_layout_reset: bool,
    pub(crate) terminal_scrollbars: HashMap<String, TerminalScrollbarHandle>,
    pub(crate) remote_files_scroll_handle: UniformListScrollHandle,
    pub(crate) disk_scroll_handle: gpui::ScrollHandle,
    pub(crate) tabs_scroll_handle: gpui::ScrollHandle,
    pub(crate) selector_scroll_handle: gpui::ScrollHandle,
    pub(crate) saved_scroll_handle: gpui::ScrollHandle,
    pub(crate) connection_scroll_handle: gpui::ScrollHandle,
    pub(crate) connection_progress: Option<ConnectionProgress>,
    pub(crate) pending_sftp_path_sync: Option<String>,
    pub(crate) sftp_context_menu: Option<SftpContextMenuState>,
    pub(crate) sftp_creating_folder: bool,
    pub(crate) sftp_new_folder_input: Entity<InputState>,
    pub(crate) sftp_delete_scroll_handle: gpui::ScrollHandle,
    pub(crate) show_hidden_files: bool,
    pub(crate) transfers: Vec<crate::terminal::Transfer>,
    pub(crate) show_transfers_dialog: bool,
    pub(crate) system_status: Option<SharedString>,
    pub(crate) pane_root: PaneLayout,
    pub(crate) focused_pane_path: Vec<usize>,
    pub(crate) terminal_bounds: HashMap<String, Bounds<Pixels>>,
    pub(crate) terminal_selecting: bool,
    pub(crate) dragging_splitter: Option<(Vec<usize>, usize)>, // (parent_path, child_index)
    pub(crate) drag_split_origin: Option<gpui::Point<Pixels>>,
    pub(crate) terminal_marked_text: Option<String>,
    pub(crate) sftp_panel_minimized: bool,
    pub(crate) prev_monitoring_size: Option<Pixels>,
    pub(crate) status: SharedString,
    pub(crate) config: ConfigStore,
    pub(crate) system_sampler: SystemSampler,
    pub(crate) recording_action: Option<String>,
    /// Error message when a recorded keybinding conflicts with another
    pub(crate) keybind_error: Option<(String, String)>, // (action_id, error_message)
    /// Whether workspace keybindings are currently suspended (during settings)
    pub(crate) keybinds_suspended: bool,
    pub(crate) system: SystemSnapshot,
    pub(crate) cpu_history: Vec<f32>,
    pub(crate) net_rx_history: Vec<f32>,
    pub(crate) net_tx_history: Vec<f32>,
    pub(crate) last_system_sample: Instant,
    pub(crate) last_theme_sync: Instant,

    pub(crate) system_tab_id: Option<String>,
    pub(crate) sftp_handles: std::collections::HashMap<String, crate::sftp::SftpHandle>,

    pub(crate) remote_sample_in_flight: bool,
    pub(crate) runtime: Runtime,
    pub(crate) events_rx: mpsc::Receiver<BackendEvent>,
    pub(crate) events_tx: mpsc::Sender<BackendEvent>,
    pub(crate) _subscriptions: Vec<gpui::Subscription>,
}

#[derive(Clone)]
pub(crate) enum SelectorEntry {
    Local,
    NewSsh,
    Saved(String),
}

#[derive(Clone)]
pub(crate) struct ConnectionProgress {
    pub(crate) tab_id: String,
    pub(crate) title: SharedString,
    pub(crate) lines: Vec<SharedString>,
    pub(crate) failed: bool,
}

#[derive(Clone)]
pub(crate) struct SftpContextMenuState {
    pub(crate) remote_path: String,
    pub(crate) is_dir: bool,
    pub(crate) position: Point<Pixels>,
}

impl Ashell {
    fn transfer_source_title(&self, tab_id: &str) -> String {
        self.tabs
            .iter()
            .find(|tab| tab.id == tab_id)
            .map(|tab| tab.title.clone())
            .or_else(|| {
                self.tab_groups
                    .iter()
                    .find(|group| group.id == tab_id)
                    .map(|group| group.title.clone())
            })
            .or_else(|| {
                self.tab_groups
                    .iter()
                    .find(|group| group.pane_root.contains(tab_id))
                    .map(|group| group.title.clone())
            })
            .unwrap_or_else(|| "Unknown".to_string())
    }

    pub(crate) fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let host_input = cx.new(|cx| InputState::new(window, cx).placeholder(t!("host")));
        let session_name_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("name (optional)"));
        let port_input = cx.new(|cx| InputState::new(window, cx).default_value("22"));
        let user_input = cx.new(|cx| InputState::new(window, cx).default_value("root"));
        let password_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t!("password"))
                .masked(true)
        });
        let key_path_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("~/.ssh/id_ed25519"));
        let key_inline_input = cx.new(|cx| {
            InputState::new(window, cx)
                .multi_line(true)
                .rows(5)
                .placeholder("-----BEGIN OPENSSH PRIVATE KEY-----")
        });
        let sftp_path_input = cx.new(|cx| InputState::new(window, cx).default_value("/"));
        let sftp_new_folder_input =
            cx.new(|cx| InputState::new(window, cx).placeholder(t!("new_folder").to_string()));

        let _subscriptions = vec![
            cx.subscribe_in(&host_input, window, Self::on_input_event),
            cx.subscribe_in(&session_name_input, window, Self::on_input_event),
            cx.subscribe_in(&port_input, window, Self::on_input_event),
            cx.subscribe_in(&user_input, window, Self::on_input_event),
            cx.subscribe_in(&password_input, window, Self::on_input_event),
            cx.subscribe_in(&key_path_input, window, Self::on_input_event),
            cx.subscribe_in(&key_inline_input, window, Self::on_input_event),
            cx.subscribe_in(&sftp_path_input, window, Self::on_input_event),
            cx.subscribe_in(&sftp_new_folder_input, window, Self::on_input_event),
        ];

        let (events_tx, events_rx) = mpsc::channel();
        let workspace_panels = cx.new(|_| ResizableState::default());
        let body_panels = cx.new(|_| ResizableState::default());
        let mut system_sampler = SystemSampler::new();
        let system = system_sampler.sample();
        let default_light_theme_name = ThemeRegistry::global(cx).default_light_theme().name.clone();
        let default_dark_theme_name = ThemeRegistry::global(cx).default_dark_theme().name.clone();
        let config = ConfigStore::load().unwrap_or_else(|err| {
            tracing::warn!("failed to load config: {err:#}");
            ConfigStore::in_memory()
        });
        let follow_system_theme =
            if config.light_theme_name().is_empty() && config.dark_theme_name().is_empty() {
                true
            } else {
                config.follow_system_theme()
            };

        let theme_mode = match config.theme_mode() {
            "light" => ThemeMode::Light,
            "dark" => ThemeMode::Dark,
            _ => ThemeMode::Light,
        };
        let light_theme_name = if config.light_theme_name().is_empty() {
            default_light_theme_name
        } else {
            config.light_theme_name().into()
        };
        let dark_theme_name = if config.dark_theme_name().is_empty() {
            default_dark_theme_name
        } else {
            config.dark_theme_name().into()
        };

        let configured_locale = config.locale();
        let mut active_locale = configured_locale.to_string();
        if active_locale == "system" {
            active_locale = sys_locale::get_locale().unwrap_or_else(|| "en".to_string());
            if active_locale.starts_with("zh") {
                active_locale = "zh-CN".to_string();
            } else {
                active_locale = "en".to_string();
            }
        }
        rust_i18n::set_locale(&active_locale);
        gpui_component::set_locale(&active_locale);
        let ui_font_family: SharedString = config.ui_font_family().into();
        let terminal_font_family: SharedString = config.terminal_font_family().into();
        let mut this = Self {
            focus_handle: cx.focus_handle(),
            selector_focus_handle: cx.focus_handle(),
            host_input,
            session_name_input,
            port_input,
            user_input,
            password_input,
            key_path_input,
            key_inline_input,
            sftp_path_input,
            ssh_auth_method: AuthMethod::Password,
            editing_session_id: None,
            follow_system_theme,
            theme_mode,
            light_theme_name,
            dark_theme_name,
            ui_font_size: config.ui_font_size(),
            terminal_font_size: config.terminal_font_size(),
            ui_font_family,
            terminal_font_family,
            tabs: Vec::new(),
            active_tab: None,
            tab_groups: Vec::new(),
            active_group: None,
            pane_root: PaneLayout::Single(String::new()),
            focused_pane_path: Vec::new(),
            selector_selection: 0,
            workspace_panels,
            body_panels,
            is_layout_reset: false,
            terminal_scrollbars: HashMap::new(),
            remote_files_scroll_handle: UniformListScrollHandle::new(),
            disk_scroll_handle: gpui::ScrollHandle::new(),
            tabs_scroll_handle: gpui::ScrollHandle::new(),
            selector_scroll_handle: gpui::ScrollHandle::new(),
            saved_scroll_handle: gpui::ScrollHandle::new(),
            connection_scroll_handle: gpui::ScrollHandle::new(),
            connection_progress: None,
            pending_sftp_path_sync: Some("/".into()),
            sftp_context_menu: None,
            sftp_creating_folder: false,
            sftp_new_folder_input,
            sftp_delete_scroll_handle: gpui::ScrollHandle::new(),
            show_hidden_files: config.show_hidden_files(),
            transfers: {
                let mut transfers = config.transfers();
                for t in transfers.iter_mut() {
                    if matches!(t.state, crate::terminal::TransferState::Running | crate::terminal::TransferState::Paused) {
                        t.state = crate::terminal::TransferState::Zombie(t!("zombie_reason").to_string());
                    }
                }
                transfers
            },
            show_transfers_dialog: false,
            system_status: None,
            terminal_bounds: HashMap::new(),
            terminal_selecting: false,
            terminal_marked_text: None,
            dragging_splitter: None,
            drag_split_origin: None,
            sftp_panel_minimized: false,
            prev_monitoring_size: None,
            status: "ready".into(),
            config,
            system_sampler,
            recording_action: None,
            keybind_error: None,
            keybinds_suspended: false,
            system,
            cpu_history: Vec::with_capacity(20),
            net_rx_history: Vec::with_capacity(20),
            net_tx_history: Vec::with_capacity(20),
            last_system_sample: Instant::now(),
            last_theme_sync: Instant::now(),

            system_tab_id: None,
            sftp_handles: std::collections::HashMap::new(),

            remote_sample_in_flight: false,
            runtime: Runtime::new().expect("create tokio runtime"),
            events_rx,
            events_tx,
            _subscriptions,
        };

        this.apply_theme_preferences(window, cx);
        // this.open_local(cx);
        this.start_event_pump(cx);
        this
    }

    pub(crate) fn on_input_event(
        &mut self,
        input: &Entity<InputState>,
        event: &InputEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if input == &self.sftp_path_input {
            if let InputEvent::PressEnter { .. } = event {
                let path = self
                    .sftp_path_input
                    .read(cx)
                    .text()
                    .to_string()
                    .trim()
                    .to_string();
                self.navigate_sftp(if path.is_empty() { "/".into() } else { path }, cx);
                window.prevent_default();
                cx.stop_propagation();
            }
        } else if input == &self.sftp_new_folder_input {
            match event {
                InputEvent::PressEnter { .. } => {
                    let name = self.sftp_new_folder_input.read(cx).text().to_string();
                    if !name.is_empty() {
                        let base_path = self.sftp_path_input.read(cx).text().to_string();
                        let path = crate::sftp::join_remote(&base_path, &name);
                        if let Some(handle) = self.active_sftp_handle() {
                            let _ = handle
                                .commands
                                .send(crate::sftp::SftpCommand::CreateDir(path));
                        }
                    }
                    self.sftp_creating_folder = false;
                    window.prevent_default();
                    cx.stop_propagation();
                }
                InputEvent::Blur => {
                    self.sftp_creating_folder = false;
                }
                _ => {}
            }
        }
        cx.notify();
    }

    pub(crate) fn start_event_pump(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let mut idle_frames = 0u32;
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(16))
                    .await;
                if this
                    .update(cx, |this, cx| {
                        let changed = this.drain_backend_events();
                        let system_sampled = this.sample_system_if_due();
                        this.sync_theme_if_due(cx);
                        if changed || system_sampled {
                            cx.notify();
                            idle_frames = 0;
                        } else {
                            idle_frames += 1;
                            if idle_frames >= 60 {
                                cx.notify();
                                idle_frames = 0;
                            }
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    }

    pub(crate) fn drain_backend_events(&mut self) -> bool {
        let mut changed = false;
        let mut transfers_changed = false;
        while let Ok(event) = self.events_rx.try_recv() {
            changed = true;
            match event {
                BackendEvent::Output { tab_id, bytes } => {
                    if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == tab_id) {
                        tab.feed(&bytes);
                    }
                }
                BackendEvent::Status { tab_id, text } => {
                    if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == tab_id) {
                        tab.status = text.clone();
                    }
                    if let Some(progress) = self.connection_progress.as_mut() {
                        if progress.tab_id == tab_id {
                            progress.lines.push(text.clone().into());
                            let _idx = progress.lines.len().saturating_sub(1);
                            self.connection_scroll_handle
                                .set_offset(point(px(0.), px(-99999.0)));
                        }
                    }
                    self.status = text.into();
                }
                BackendEvent::Connected { tab_id } => {
                    if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == tab_id) {
                        tab.connected = true;
                    }
                    self.sync_system_tab_to_active_group();
                    self.request_active_system_snapshot();
                    if self
                        .connection_progress
                        .as_ref()
                        .is_some_and(|progress| progress.tab_id == tab_id && !progress.failed)
                    {
                        self.connection_progress = None;
                    }
                }
                BackendEvent::SftpEntries {
                    tab_id,
                    path,
                    entries,
                } => {
                    if let Some(group) = self.tab_groups.iter_mut().find(|g| g.id == tab_id) {
                        if let Some(sftp) = group.sftp.as_mut() {
                            sftp.current_path = path;
                            sftp.entries = entries;
                            self.pending_sftp_path_sync = Some(sftp.current_path.clone());
                        }
                    }
                }
                BackendEvent::SftpPreview { tab_id, preview } => {
                    if let Some(group) = self.tab_groups.iter_mut().find(|g| g.id == tab_id) {
                        if let Some(sftp) = group.sftp.as_mut() {
                            sftp.selected_path = Some(preview.path.clone());
                            sftp.preview = Some(preview);
                        }
                    }
                }
                BackendEvent::SftpStatus { tab_id, text } => {
                    if let Some(group) = self.tab_groups.iter_mut().find(|g| g.id == tab_id) {
                        if let Some(sftp) = group.sftp.as_mut() {
                            sftp.status = text.clone();
                        }
                    }
                    if self.active_group.as_ref() == Some(&tab_id) {
                        self.status = text.into();
                    }
                }
                BackendEvent::RemoteSystem { tab_id, snapshot } => {
                    self.remote_sample_in_flight = false;
                    if self.system_tab_id.as_deref() == Some(tab_id.as_str()) {
                        self.system_status = None;
                        self.system = snapshot.clone();
                        self.cpu_history.push(snapshot.cpu_percent);
                        if self.cpu_history.len() > 20 {
                            self.cpu_history.remove(0);
                        }
                        self.net_rx_history.push(snapshot.net_rx_rate as f32);
                        if self.net_rx_history.len() > 20 {
                            self.net_rx_history.remove(0);
                        }
                        self.net_tx_history.push(snapshot.net_tx_rate as f32);
                        if self.net_tx_history.len() > 20 {
                            self.net_tx_history.remove(0);
                        }
                    }
                }
                BackendEvent::RemoteSystemUnavailable { tab_id, reason } => {
                    self.remote_sample_in_flight = false;
                    if self.system_tab_id.as_deref() == Some(tab_id.as_str()) {
                        self.system_status = Some(reason.clone().into());
                        self.status = reason.into();
                    }
                }
                BackendEvent::Closed { tab_id, reason } => {
                    self.remote_sample_in_flight = false;
                    let mut tab_title = None;
                    let mut session_label = None;
                    if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == tab_id) {
                        tab.connected = false;
                        tab.status = reason.clone();
                        tab_title = Some(tab.title.clone());
                        session_label = tab.session.as_ref().map(|session| {
                            format!("{}@{}:{}", session.user, session.host, session.port)
                        });
                    }
                    if self.system_tab_id.as_deref() == Some(tab_id.as_str()) {
                        self.system_status = Some(reason.clone().into());
                    }
                    let is_graceful_exit =
                        reason == "local shell closed" || reason == "ssh session closed";
                    // Auto-close the pane on graceful exit (e.g. user typed exit)
                    if is_graceful_exit {
                        self.handle_tab_close(tab_id.clone());
                        self.status = reason.into();
                        self.remote_sample_in_flight = false;
                        return changed;
                    }
                    let mut needs_new_progress = false;
                    if let Some(progress) = self.connection_progress.as_mut() {
                        if progress.tab_id == tab_id {
                            progress.lines.push(reason.clone().into());
                            let _idx = progress.lines.len().saturating_sub(1);
                            self.connection_scroll_handle
                                .set_offset(point(px(0.), px(-99999.0)));
                            let _ = session_label;
                            let _ = tab_title;
                            progress.title = t!("connection_failed").into();
                            progress.failed = true;
                        } else if !progress.failed {
                            // We were showing connecting progress, but another tab dropped!
                            // Switch to failed state so the user can see it and retry.
                            progress.tab_id = tab_id.clone();
                            let msg = format!("{}: {}", tab_title.unwrap_or_default(), reason);
                            progress.lines.push(msg.into());
                            self.connection_scroll_handle
                                .set_offset(point(px(0.), px(-99999.0)));
                            progress.title = t!("connection_failed").into();
                            progress.failed = true;
                        } else {
                            // Already showing a failure dialog, just append the new failure
                            let msg = format!("{}: {}", tab_title.unwrap_or_default(), reason);
                            progress.lines.push(msg.into());
                            self.connection_scroll_handle
                                .set_offset(point(px(0.), px(-99999.0)));
                        }
                    } else if let Some(_) = session_label {
                        needs_new_progress = true;
                    }

                    if needs_new_progress && !is_graceful_exit {
                        self.connection_progress = Some(ConnectionProgress {
                            tab_id: tab_id.clone(),
                            title: t!("connection_failed").into(),
                            lines: vec![reason.clone().into()],
                            failed: true,
                        });
                    }
                    self.status = reason.into();
                }
                BackendEvent::TransferProgress {
                    tab_id: _,
                    id,
                    transferred,
                    total,
                    state,
                } => {
                    if let Some(t) = self.transfers.iter_mut().find(|t| t.info.id == id) {
                        t.transferred = transferred;
                        if let Some(total) = total {
                            t.total = Some(total);
                        }
                        t.state = state;
                        transfers_changed = true;
                    }
                }
                BackendEvent::TransferStarted { tab_id, info } => {
                    let tab_title = self.transfer_source_title(&tab_id);
                    self.transfers.insert(
                        0,
                        crate::terminal::Transfer {
                            tab_id,
                            tab_title,
                            info,
                            transferred: 0,
                            total: None,
                            state: crate::terminal::TransferState::Running,
                        },
                    );
                    if self.transfers.len() > 100 {
                        self.transfers.truncate(100);
                    }
                    transfers_changed = true;
                }
                BackendEvent::SftpHome { tab_id, home } => {
                    if let Some(group) = self.tab_groups.iter_mut().find(|g| g.id == tab_id) {
                        if let Some(sftp) = group.sftp.as_mut() {
                            sftp.home_dir = home;
                        }
                    }
                }
                BackendEvent::TerminalTitleChanged { tab_id, title } => {
                    if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == tab_id) {
                        tab.title = title.clone();
                    }
                }
            }
        }
        if transfers_changed {
            self.config.set_transfers(self.transfers.clone());
        }
        changed
    }

    pub(crate) fn sample_system_if_due(&mut self) -> bool {
        if self.last_system_sample.elapsed() >= SystemSampler::interval() {
            self.last_system_sample = Instant::now();
            // Use system_tab_id (not active_tab) to decide remote vs local sampling
            if let Some(ref tab_id) = self.system_tab_id.clone() {
                if self
                    .tabs
                    .iter()
                    .any(|t| t.id == *tab_id && t.kind == TabKind::Ssh && t.connected)
                    && self.system_status.is_none()
                {
                    self.request_active_system_snapshot();
                    return false;
                }
            }
            let snapshot = self.system_sampler.sample();
            let cpu_usage = snapshot.cpu_percent;
            self.cpu_history.push(cpu_usage);
            if self.cpu_history.len() > 20 {
                self.cpu_history.remove(0);
            }
            self.net_rx_history.push(snapshot.net_rx_rate as f32);
            if self.net_rx_history.len() > 20 {
                self.net_rx_history.remove(0);
            }
            self.net_tx_history.push(snapshot.net_tx_rate as f32);
            if self.net_tx_history.len() > 20 {
                self.net_tx_history.remove(0);
            }
            self.system = snapshot;
            return true;
        }
        false
    }

    pub(crate) fn sync_theme_if_due(&mut self, cx: &mut Context<Self>) {
        if self.follow_system_theme && self.last_theme_sync.elapsed() >= Duration::from_secs(1) {
            self.last_theme_sync = Instant::now();
            Theme::sync_system_appearance(None, cx);
            cx.refresh_windows();
        }
    }

    pub(crate) fn request_active_system_snapshot(&mut self) {
        let Some(ref tab_id) = self.system_tab_id.clone() else {
            return;
        };
        let Some(backend) = (|| {
            let tab = self.tabs.iter().find(|t| t.id == *tab_id)?;
            if !tab.connected {
                return None;
            }
            Some(tab.backend.clone())
        })() else {
            return;
        };
        if self.remote_sample_in_flight {
            return;
        }
        self.remote_sample_in_flight = true;
        backend.send(crate::terminal::BackendCommand::SampleMetrics);
    }

    pub(crate) fn terminal_ime_bounds_for_range(
        &self,
        range_utf16: Range<usize>,
        element_bounds: Bounds<Pixels>,
        cell_width: f32,
        line_height: f32,
    ) -> Option<Bounds<Pixels>> {
        let snapshot = self.active_snapshot()?;
        let cursor = snapshot.cursor?;
        let x = element_bounds.origin.x
            + px(cell_width) * cursor.col as f32
            + px(cell_width) * range_utf16.start as f32;
        let y = element_bounds.origin.y + px(line_height) * cursor.row as f32;
        Some(Bounds::new(
            point(x, y),
            size(px(cell_width), px(line_height)),
        ))
    }

    pub(crate) fn remove_transfer(&mut self, transfer_id: &str, cx: &mut Context<Self>) {
        self.transfers.retain(|t| t.info.id != transfer_id);
        self.config.set_transfers(self.transfers.clone());
        cx.notify();
    }


}
