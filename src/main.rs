//! vmac: a modal (evil-style) editor whose behavior is written in Steel
//! Scheme and can be redefined while it runs. See README.md.

mod agent;
mod buffer;
mod clipboard;
mod editor;
mod lisp;
mod lsp;
mod markdown;
mod picker;
mod review;
mod plugins;
mod projects;
mod shell;
mod syntax;
mod terminal;
mod themes;

use buffer::Buffer;
use editor::Editor;
use gpui_kit::{AppContext as _, Bounds, Size, WindowBounds, WindowOptions, px};
use lisp::Lisp;
use shell::Shell;

fn main() {
    let start_file = std::env::args().nth(1);
    let dashboard = start_file.is_none();
    let buffer = match start_file {
        Some(path) => Buffer::from_file(path.into()).unwrap_or_else(|e| {
            eprintln!("vmac: {e}");
            std::process::exit(1);
        }),
        None => Buffer::new("*scratch*"),
    };

    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx| {
            gpui_kit::init(cx);
            shell::bind_keys(cx);
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    Size { width: px(1320.), height: px(840.) },
                    cx,
                ))),
                ..Default::default()
            };
            gpui_kit::open_window(options, cx, move |window, cx| {
                let editor = Editor::new(buffer);
                editor.lock().set_state_dir(projects::History::default_dir());
                editor.lock().clipboard = Some(editor::SystemClipboard(Box::new(clipboard::System)));
                let mut lisp = Lisp::with_config(
                    editor.clone(),
                    Lisp::default_sources(),
                    Lisp::default_lua_sources(),
                    Lisp::default_plugin_dir(),
                );
                // Started without a file: the dashboard (unless turned off).
                if dashboard {
                    let _ = lisp.eval("(when dashboard-on-start (call-binding 'dashboard))");
                }
                cx.new(|cx| Shell::new(editor, lisp, window, cx))
            })
            .expect("failed to open window");
            cx.activate(true);
        });
}
