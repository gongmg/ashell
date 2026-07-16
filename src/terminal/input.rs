use std::ops::Range;

use alacritty_terminal::index::Side;
use alacritty_terminal::selection::SelectionType;
use gpui::{
    ClipboardItem, Context, Focusable as _, KeyDownEvent, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, PathPromptOptions, Pixels, Point, ScrollDelta, ScrollWheelEvent,
    Window, px,
};

use crate::{
    Ashell, TerminalBacktabKey, TerminalTabKey,
    terminal::{BackendCommand, encode_key},
};

thread_local! {
    static LAST_DRAG_SCROLL: std::cell::Cell<Option<std::time::Instant>> = std::cell::Cell::new(None);
}

impl Ashell {
    fn remember_terminal_input(&mut self, tab_id: &str, tab_ix: usize, bytes: &[u8]) {
        let cursor_before_input = self.tabs[tab_ix].render_snapshot().cursor;
        let mut completed_commands = Vec::new();
        let mut pending_completed_commands = Vec::new();
        let state = self.command_buffers.entry(tab_id.to_string()).or_default();

        for &byte in bytes {
            match byte {
                b'\r' | b'\n' => {
                    pending_completed_commands.push((
                        state.start_row,
                        state.start_col,
                        state.typed.clone(),
                    ));
                    *state = Default::default();
                }
                0x08 | 0x7f => {
                    state.typed.pop();
                }
                byte if byte.is_ascii_graphic() || byte == b' ' => {
                    if state.start_row.is_none() || state.start_col.is_none() {
                        if let Some(cursor) = cursor_before_input {
                            state.start_row = Some(cursor.row);
                            state.start_col = Some(cursor.col);
                        }
                    }
                    state.typed.push(byte as char);
                }
                _ => {}
            }
        }

        for (row, col, typed) in pending_completed_commands {
            let visible_command = row
                .zip(col)
                .and_then(|(row, col)| self.tabs[tab_ix].visible_line_text_from(row, col));
            let command = choose_completed_command(visible_command, typed);
            if !command.is_empty() {
                completed_commands.push(command);
            }
        }

        let had_completed_commands = !completed_commands.is_empty();
        for command in completed_commands {
            self.config.record_command(command);
        }
        if !bytes.is_empty() && had_completed_commands {
            let _ = self.config.save();
        }
    }

    pub(crate) fn run_terminal_command(
        &mut self,
        command: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let command = command.trim();
        if command.is_empty() {
            return;
        }
        let mut bytes = command.as_bytes().to_vec();
        bytes.push(b'\r');
        self.send_terminal_input(bytes, window, cx);
    }

    pub(crate) fn clear_terminal_buffer(&mut self, cx: &mut Context<Self>) {
        let Some(active_id) = self.active_tab.clone() else {
            return;
        };
        let Some(tab_ix) = self.tabs.iter().position(|tab| tab.id == active_id) else {
            return;
        };

        self.tabs[tab_ix].clear_selection();
        self.tabs[tab_ix].scroll_to_bottom();
        let preserved_line = self.tabs[tab_ix].current_or_last_visible_line_text();
        let bottom_row = self.tabs[tab_ix].rows.max(1);
        self.tabs[tab_ix].feed(b"\x1b[2J\x1b[3J");
        if let Some(preserved_line) = preserved_line {
            let restore = format!("\x1b[{bottom_row};1H{preserved_line}");
            self.tabs[tab_ix].feed(restore.as_bytes());
        }
        self.command_buffers.remove(&active_id);
        self.status = "terminal buffer cleared".into();
        cx.notify();
    }

    pub(crate) fn download_terminal_buffer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(active_id) = self.active_tab.clone() else {
            return;
        };
        let Some(tab) = self.tabs.iter().find(|tab| tab.id == active_id) else {
            return;
        };

        let content = terminal_buffer_text(tab);
        let title = sanitize_filename(&tab.title);
        let timestamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
        let suggested_name = format!("ashell-{title}-{timestamp}.txt");
        let path_prompt = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(suggested_name.into()),
        });

        cx.spawn_in(window, async move |this, cx| {
            match path_prompt.await {
                Ok(Ok(Some(mut paths))) => {
                    if let Some(mut path) = paths.pop() {
                        if path.extension().is_none() {
                            path.set_extension("txt");
                        }
                        match std::fs::write(&path, content.as_bytes()) {
                            Ok(()) => {
                                this.update(cx, |this, cx| {
                                    this.status =
                                        format!("terminal buffer saved: {}", path.display()).into();
                                    cx.notify();
                                })?;
                            }
                            Err(err) => {
                                this.update(cx, |this, cx| {
                                    this.status =
                                        format!("save terminal buffer failed: {err}").into();
                                    cx.notify();
                                })?;
                            }
                        }
                    }
                }
                Ok(Err(err)) => {
                    this.update(cx, |this, cx| {
                        this.status = format!("save picker failed: {err}").into();
                        cx.notify();
                    })?;
                }
                _ => {}
            }
            Ok::<(), anyhow::Error>(())
        })
        .detach();
    }

    pub(crate) fn on_terminal_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // If the search input is focused, skip terminal key processing
        // so the input can handle text entry, paste, etc. normally.
        if self
            .search_input
            .read(cx)
            .focus_handle(cx)
            .is_focused(window)
        {
            return;
        }

        // Pane navigation: Alt + h/j/k/l
        if event.keystroke.modifiers.alt
            && !event.keystroke.modifiers.shift
            && !event.keystroke.modifiers.control
            && !event.keystroke.modifiers.platform
        {
            match event.keystroke.key.to_ascii_lowercase().as_str() {
                "h" => self.focus_adjacent_pane("left"),
                "j" => self.focus_adjacent_pane("down"),
                "k" => self.focus_adjacent_pane("up"),
                "l" => self.focus_adjacent_pane("right"),
                "q" => {
                    if let Some(active_id) = self.active_tab.clone() {
                        self.close_tab(active_id, cx);
                    }
                }
                _ => return,
            }
            window.prevent_default();
            cx.stop_propagation();
            cx.notify();
            return;
        }

        // Pane split: Shift+Alt + h/j/k/l
        if event.keystroke.modifiers.shift
            && event.keystroke.modifiers.alt
            && !event.keystroke.modifiers.control
            && !event.keystroke.modifiers.platform
        {
            let direction = match event.keystroke.key.to_ascii_lowercase().as_str() {
                "h" => Some("left"),
                "j" => Some("down"),
                "k" => Some("up"),
                "l" => Some("right"),
                _ => None,
            };
            if let Some(dir) = direction {
                self.split_current_pane(dir, cx);
                window.prevent_default();
                cx.stop_propagation();
                cx.notify();
                return;
            }
        }

        if event.keystroke.modifiers.secondary() && event.keystroke.key == "," {
            self.show_settings_dialog(window, cx);
            window.prevent_default();
            cx.stop_propagation();
            return;
        }
        if event.keystroke.modifiers.shift
            && event.keystroke.modifiers.secondary()
            && event.keystroke.key == "o"
        {
            self.show_selector_dialog(window, cx);
            window.prevent_default();
            cx.stop_propagation();
            return;
        }
        if event.keystroke.modifiers.secondary() && event.keystroke.key.eq_ignore_ascii_case("c") {
            if let Some(text) = self.active_terminal_selection_text() {
                cx.write_to_clipboard(ClipboardItem::new_string(text));
                window.prevent_default();
                cx.stop_propagation();
                return;
            }
        }
        if event.keystroke.modifiers.shift
            && !event.keystroke.modifiers.control
            && !event.keystroke.modifiers.alt
            && !event.keystroke.modifiers.platform
            && event.keystroke.key.eq_ignore_ascii_case("insert")
        {
            if let Some(clipboard) = cx.read_from_clipboard() {
                if let Some(text) = clipboard.text() {
                    self.paste_into_terminal(&text, window, cx);
                    return;
                }
            }
        }

        // If the active tab is disconnected and user presses Enter, reconnect
        if event.keystroke.key == "enter"
            && !event.keystroke.modifiers.shift
            && !event.keystroke.modifiers.control
            && !event.keystroke.modifiers.alt
            && !event.keystroke.modifiers.platform
        {
            if let Some(progress) = &self.connection_progress {
                if progress.failed {
                    self.retry_connection_progress(cx);
                    window.prevent_default();
                    cx.stop_propagation();
                    return;
                }
            }

            let active_id = self.active_tab.clone();
            if let Some(active_id) = active_id {
                let is_disconnected = self
                    .tabs
                    .iter()
                    .find(|t| t.id == active_id)
                    .is_some_and(|tab| tab.disconnected_reason.is_some());
                if is_disconnected {
                    self.retry_disconnected_tab(&active_id, cx);
                    window.prevent_default();
                    cx.stop_propagation();
                    return;
                }
            }
        }

        if event.prefer_character_input {
            if let Some(text) = event.keystroke.key_char.as_deref()
                && !text.is_empty()
                && !event.keystroke.modifiers.function
                && !event.keystroke.modifiers.platform
                && !text.is_ascii()
            {
                self.send_terminal_input(text.as_bytes().to_vec(), window, cx);
                return;
            }

            window.prevent_default();
            cx.stop_propagation();
            return;
        }

        let Some(active_id) = self.active_tab.clone() else {
            return;
        };
        let Some(tab_ix) = self.tabs.iter().position(|t| t.id == active_id) else {
            return;
        };

        if self.handle_terminal_history_key(tab_ix, &event.keystroke.key, event, window, cx) {
            return;
        }

        if self.tabs[tab_ix].render_snapshot().display_offset > 0 {
            self.tabs[tab_ix].scroll_to_bottom();
        }
        self.tabs[tab_ix].clear_selection();

        if let Some(bytes) =
            encode_key(&event.keystroke, self.tabs[tab_ix].app_cursor_mode(), false)
        {
            self.remember_terminal_input(&active_id, tab_ix, &bytes);
            self.tabs[tab_ix].send_backend(BackendCommand::Input(bytes));
            window.prevent_default();
            cx.stop_propagation();
            cx.notify();
        }
    }

    fn handle_terminal_history_key(
        &mut self,
        tab_ix: usize,
        key: &str,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !event.keystroke.modifiers.shift
            || event.keystroke.modifiers.control
            || event.keystroke.modifiers.alt
            || event.keystroke.modifiers.platform
        {
            return false;
        }

        match key.to_ascii_lowercase().as_str() {
            "pageup" => self.tabs[tab_ix].scroll_page_up(),
            "pagedown" => self.tabs[tab_ix].scroll_page_down(),
            "home" => self.tabs[tab_ix].scroll_to_top(),
            "end" => self.tabs[tab_ix].scroll_to_bottom(),
            _ => return false,
        }

        window.prevent_default();
        cx.stop_propagation();
        cx.notify();
        true
    }

    pub(crate) fn on_terminal_tab_action(
        &mut self,
        _: &TerminalTabKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.send_terminal_input(vec![b'\t'], window, cx);
    }

    pub(crate) fn on_terminal_backtab_action(
        &mut self,
        _: &TerminalBacktabKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.send_terminal_input(b"\x1b[Z".to_vec(), window, cx);
    }

    fn send_terminal_input(&mut self, bytes: Vec<u8>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(active_id) = self.active_tab.clone() else {
            return;
        };
        let Some(tab_ix) = self.tabs.iter().position(|t| t.id == active_id) else {
            return;
        };

        if self.tabs[tab_ix].render_snapshot().display_offset > 0 {
            self.tabs[tab_ix].scroll_to_bottom();
        }

        self.tabs[tab_ix].clear_selection();
        self.remember_terminal_input(&active_id, tab_ix, &bytes);
        self.tabs[tab_ix].send_backend(BackendCommand::Input(bytes));
        window.prevent_default();
        cx.stop_propagation();
        cx.notify();
    }

    pub(crate) fn active_terminal_selection_text(&self) -> Option<String> {
        let active_id = self.active_tab.as_ref()?;
        self.tabs
            .iter()
            .find(|tab| &tab.id == active_id)
            .and_then(|tab| tab.selection_text())
    }

    pub(crate) fn paste_into_terminal(
        &mut self,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(active_id) = self.active_tab.clone() else {
            return;
        };
        let Some(tab_ix) = self.tabs.iter().position(|tab| tab.id == active_id) else {
            return;
        };

        if self.tabs[tab_ix].render_snapshot().display_offset > 0 {
            self.tabs[tab_ix].scroll_to_bottom();
        }
        self.tabs[tab_ix].clear_selection();
        self.remember_terminal_input(&active_id, tab_ix, text.as_bytes());
        self.tabs[tab_ix].paste_text(text);
        window.prevent_default();
        cx.stop_propagation();
        cx.notify();
    }

    pub(crate) fn terminal_accepts_text_input(&self) -> bool {
        self.active_tab.is_some()
    }

    pub(crate) fn terminal_marked_text_range(&self) -> Option<Range<usize>> {
        self.terminal_marked_text
            .as_ref()
            .map(|text| 0..text.encode_utf16().count())
    }

    pub(crate) fn set_terminal_marked_text(
        &mut self,
        text: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.terminal_marked_text = if text.is_empty() { None } else { Some(text) };
        window.invalidate_character_coordinates();
        cx.notify();
    }

    pub(crate) fn clear_terminal_marked_text(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.terminal_marked_text.take().is_some() {
            window.invalidate_character_coordinates();
            cx.notify();
        }
    }

    pub(crate) fn commit_terminal_ime_text(
        &mut self,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(active_id) = self.active_tab.clone() else {
            return;
        };
        let Some(tab_ix) = self.tabs.iter().position(|tab| tab.id == active_id) else {
            return;
        };

        if self.tabs[tab_ix].render_snapshot().display_offset > 0 {
            self.tabs[tab_ix].scroll_to_bottom();
        }
        self.tabs[tab_ix].clear_selection();
        self.terminal_marked_text = None;
        self.remember_terminal_input(&active_id, tab_ix, text.as_bytes());
        self.tabs[tab_ix].send_backend(BackendCommand::Input(text.as_bytes().to_vec()));
        window.invalidate_character_coordinates();
        cx.notify();
    }

    pub(crate) fn on_terminal_right_click(
        &mut self,
        _event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.config.right_click_copy_paste() {
            return;
        }

        let mut handled = false;
        if let Some(text) = self.active_terminal_selection_text() {
            if !text.is_empty() {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));

                let active_id = self.active_tab.clone();
                if let Some(active_id) = active_id {
                    if let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == active_id) {
                        tab.clear_selection();
                    }
                }
                cx.notify();
                handled = true;
            }
        }

        if !handled {
            if let Some(clipboard_item) = cx.read_from_clipboard() {
                if let Some(text) = clipboard_item.text() {
                    if !text.is_empty() {
                        self.paste_into_terminal(&text, window, cx);
                    }
                }
            }
        }
    }

    pub(crate) fn begin_terminal_selection(
        &mut self,
        event: &MouseDownEvent,
        cx: &mut Context<Self>,
    ) {
        let click_count = event.click_count.max(1);
        let selection_type = match click_count {
            1 => SelectionType::Simple,
            2 => SelectionType::Semantic,
            3 => SelectionType::Lines,
            _ => SelectionType::Simple,
        };
        let Some((row, col, side)) = self.terminal_grid_point_and_side(event.position) else {
            return;
        };
        let Some(active_id) = self.active_tab.clone() else {
            return;
        };
        if let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == active_id) {
            tab.begin_selection(row, col, side, selection_type);
            self.terminal_selecting = true;
            cx.notify();
        }
    }

    pub(crate) fn on_terminal_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Handle split drag
        if self.dragging_splitter.is_some() {
            if event.pressed_button == Some(MouseButton::Left) {
                self.on_split_drag_move(event, window, cx);
                cx.notify();
            } else {
                self.end_drag_split();
                cx.notify();
            }
            return;
        }
        if !self.terminal_selecting || event.pressed_button != Some(MouseButton::Left) {
            return;
        }
        let Some((row, col, side)) = self.terminal_grid_point_and_side(event.position) else {
            return;
        };
        let Some(active_id) = self.active_tab.clone() else {
            return;
        };
        let snapshot = match self.active_snapshot() {
            Some(s) => s,
            None => return,
        };
        let max_row = snapshot.rows.saturating_sub(1);

        let mut scroll_delta = 0i32;
        if max_row >= 6 {
            if row <= 2 || row >= max_row.saturating_sub(2) {
                let now = std::time::Instant::now();
                let should_scroll = LAST_DRAG_SCROLL.with(|last| {
                    if let Some(last_time) = last.get() {
                        if now.duration_since(last_time) >= std::time::Duration::from_millis(80) {
                            last.set(Some(now));
                            true
                        } else {
                            false
                        }
                    } else {
                        last.set(Some(now));
                        true
                    }
                });

                if should_scroll {
                    if row == 0 {
                        scroll_delta = 2;
                    } else if row == 1 {
                        scroll_delta = 1;
                    } else if row == 2 {
                        scroll_delta = 1;
                    } else if row == max_row {
                        scroll_delta = -2;
                    } else if row == max_row.saturating_sub(1) {
                        scroll_delta = -1;
                    } else if row == max_row.saturating_sub(2) {
                        scroll_delta = -1;
                    }
                }
            } else {
                LAST_DRAG_SCROLL.with(|last| last.set(None));
            }
        }

        if let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == active_id) {
            if scroll_delta != 0 {
                tab.scroll_history(scroll_delta);
            }
            tab.update_selection(row, col, side);
            cx.notify();
        }
    }

    pub(crate) fn on_terminal_mouse_up(
        &mut self,
        _event: &MouseUpEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.dragging_splitter.is_some() {
            self.end_drag_split();
        }
        self.terminal_selecting = false;
        cx.notify();
    }

    fn terminal_grid_point_and_side(
        &self,
        position: Point<Pixels>,
    ) -> Option<(usize, usize, Side)> {
        let active_id = self.active_tab.as_ref()?;
        let bounds = self.terminal_bounds.get(active_id)?;
        if !bounds.contains(&position) {
            // Try other pane bounds
            for (_, b) in &self.terminal_bounds {
                if b.contains(&position) {
                    // Found a different pane - focus it
                    // (this path is for click-to-focus; handled via focus_terminal)
                    return None;
                }
            }
            return None;
        }
        let local_x = (position.x - bounds.origin.x).max(px(0.));
        let local_y = (position.y - bounds.origin.y).max(px(0.));
        let cell_width = px(self.terminal_cell_width());
        let line_height = px(self.terminal_line_height());
        let snapshot = self.active_snapshot()?;
        let max_col = snapshot.cols.saturating_sub(1);
        let max_row = snapshot.rows.saturating_sub(1);
        let col = ((local_x / cell_width).floor() as usize).min(max_col);
        let row = ((local_y / line_height).floor() as usize).min(max_row);
        let cell_offset_x = px(local_x.as_f32() % cell_width.as_f32());
        let side = if cell_offset_x >= (cell_width / 2.) {
            Side::Right
        } else {
            Side::Left
        };
        Some((row, col, side))
    }

    pub(crate) fn on_terminal_scroll(
        &mut self,
        event: &ScrollWheelEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Platform modifier (Cmd on macOS, Ctrl on Windows/Linux) + scroll → zoom terminal font size
        if event.modifiers.platform {
            let delta = match event.delta {
                ScrollDelta::Lines(point) => point.y as f32 * 20.0,
                ScrollDelta::Pixels(point) => point.y.as_f32(),
            };
            self.terminal_zoom_accumulator += delta;
            let step = 20.0;
            if self.terminal_zoom_accumulator.abs() >= step {
                let zoom_steps = (self.terminal_zoom_accumulator / step).trunc();
                self.terminal_zoom_accumulator -= zoom_steps * step;
                self.change_terminal_font_size(zoom_steps * 0.5, cx);
            }
            window.prevent_default();
            cx.stop_propagation();
            return;
        }

        let Some(active_id) = self.active_tab.clone() else {
            return;
        };

        // Get coordinates before mutably borrowing tabs
        let grid_point = self.terminal_grid_point_and_side(event.position);

        let line_height = self.terminal_line_height();

        if let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == active_id) {
            let delta_lines = match event.delta {
                ScrollDelta::Lines(point) => point.y.round() as i32,
                ScrollDelta::Pixels(point) => {
                    tab.scroll_pixel_y += point.y.as_f32();
                    let lines = (tab.scroll_pixel_y / line_height).trunc() as i32;
                    tab.scroll_pixel_y -= (lines as f32) * line_height;
                    lines
                }
            };

            if delta_lines == 0 {
                return;
            }

            let mode = tab.term.mode();

            let is_mouse_tracking = mode.intersects(
                alacritty_terminal::term::TermMode::MOUSE_REPORT_CLICK
                    | alacritty_terminal::term::TermMode::MOUSE_MOTION
                    | alacritty_terminal::term::TermMode::MOUSE_DRAG,
            );

            let is_alternate_scroll = mode.contains(
                alacritty_terminal::term::TermMode::ALT_SCREEN
                    | alacritty_terminal::term::TermMode::ALTERNATE_SCROLL,
            );

            if is_mouse_tracking {
                if let Some((row, col, _)) = grid_point {
                    let sgr = mode.contains(alacritty_terminal::term::TermMode::SGR_MOUSE);
                    let button = if delta_lines > 0 { 64 } else { 65 };
                    let times = delta_lines.abs();
                    let mut bytes = Vec::new();
                    for _ in 0..times {
                        if sgr {
                            bytes.extend_from_slice(
                                format!("\x1b[<{};{};{}M", button, col + 1, row + 1).as_bytes(),
                            );
                        } else {
                            if col < 223 && row < 223 {
                                bytes.extend_from_slice(b"\x1b[M");
                                bytes.push(button as u8 + 32);
                                bytes.push(col as u8 + 33);
                                bytes.push(row as u8 + 33);
                            }
                        }
                    }
                    if !bytes.is_empty() {
                        tab.send_backend(crate::terminal::BackendCommand::Input(bytes));
                    }
                }
                window.prevent_default();
                cx.stop_propagation();
                return;
            } else if is_alternate_scroll {
                let times = delta_lines.abs();
                let code = if delta_lines > 0 { b'A' } else { b'B' };
                let mut bytes = Vec::with_capacity((times * 3) as usize);
                for _ in 0..times {
                    bytes.extend_from_slice(&[b'\x1b', b'O', code]);
                }
                if !bytes.is_empty() {
                    tab.send_backend(crate::terminal::BackendCommand::Input(bytes));
                }
                window.prevent_default();
                cx.stop_propagation();
                return;
            }

            tab.scroll_history(delta_lines);
            window.prevent_default();
            cx.stop_propagation();
            cx.notify();
        }
    }
}

fn terminal_buffer_text(tab: &crate::terminal::TerminalTab) -> String {
    let (_grid_start, rows) = tab.full_grid_rows();
    let mut output = String::new();

    for row in rows {
        let Some(max_col) = row.cells.iter().map(|(col, _)| *col).max() else {
            output.push('\n');
            continue;
        };
        let mut chars = vec![' '; max_col.saturating_add(1) as usize];
        for (col, ch) in row.cells {
            if col >= 0 {
                let col = col as usize;
                if col < chars.len() {
                    chars[col] = ch;
                }
            }
        }
        let line = chars.into_iter().collect::<String>();
        output.push_str(&line);
        if !row.wrapped {
            output.push('\n');
        }
    }
    output
}

fn sanitize_filename(value: &str) -> String {
    let mut sanitized = value
        .chars()
        .map(|ch| match ch {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '-',
            ch if ch.is_control() => '-',
            ch => ch,
        })
        .collect::<String>();
    sanitized = sanitized.trim_matches([' ', '.']).to_string();
    if sanitized.is_empty() {
        "terminal".into()
    } else {
        sanitized.chars().take(80).collect()
    }
}

fn choose_completed_command(visible_command: Option<String>, typed: String) -> String {
    let typed = typed.trim().to_string();
    let Some(visible) = visible_command.map(|command| command.trim().to_string()) else {
        return typed;
    };

    if visible.is_empty() {
        return typed;
    }
    if typed.is_empty() {
        return visible;
    }

    if visible.len() >= typed.len() {
        visible
    } else {
        typed
    }
}
