pub mod app;
pub mod backend;
pub mod client;
pub mod session;
pub mod sftp;
pub mod sync;
pub mod system;
pub mod terminal;

rust_i18n::i18n!("locales", fallback = "en");

gpui::actions!(ashell_terminal, [TerminalTabKey, TerminalBacktabKey]);

pub use app::keybinding_recorder::{
    ClosePane, Copy, FocusPaneDown, FocusPaneLeft, FocusPaneRight, FocusPaneUp, NewSsh,
    OpenCommandHistory, OpenQuickCommands, OpenSearch, OpenSession, OpenSettings, OpenTransfers,
    Paste, SplitPaneDown, SplitPaneLeft, SplitPaneRight, SplitPaneUp, ToggleSftpZoom,
    ToggleSidebar,
};

pub use app::{Ashell, PaneLayout, SelectorEntry, SftpContextMenuState, TabGroup};
