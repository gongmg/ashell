#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

use gpui::KeyBinding;
use gpui_component_assets::Assets;

use ashell::{TerminalBacktabKey, TerminalTabKey, app, client};

fn main() {
    app::startup::sync_macos_launch_environment();
    app::startup::init_logging();
    app::startup::init_panic_logging();

    if client::is_client_mode() {
        client::attach_parent_console();
        std::process::exit(client::run_blocking());
    }

    #[cfg(target_os = "macos")]
    let app = gpui_platform::application()
        .with_assets(Assets)
        .with_quit_mode(gpui::QuitMode::LastWindowClosed);

    #[cfg(not(target_os = "macos"))]
    let app = gpui_platform::application().with_assets(Assets);

    app.on_reopen(|cx| {
        if cx.windows().is_empty() {
            app::startup::open_main_window(cx);
        }
    });
    app.run(move |cx| {
        gpui_component::init(cx);
        cx.bind_keys([
            KeyBinding::new(
                "tab",
                TerminalTabKey,
                Some(app::constants::TERMINAL_KEY_CONTEXT),
            ),
            KeyBinding::new(
                "shift-tab",
                TerminalBacktabKey,
                Some(app::constants::TERMINAL_KEY_CONTEXT),
            ),
        ]);
        app::startup::bind_workspace_keys(cx);
        app::theme::load_embedded_themes(cx);
        if let Err(err) = app::theme::load_fonts(cx) {
            tracing::warn!("failed to load embedded fonts: {err:#}");
        }
        app::startup::open_main_window(cx);
    });
}
