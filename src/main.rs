//! gmacs: a modal (evil-style) editor whose behavior is written in Steel
//! Scheme and can be redefined while it runs. See README.md.

mod buffer;
mod editor;
mod lisp;
mod lsp;
mod markdown;
mod picker;
mod plugins;
mod shell;
mod syntax;
mod terminal;

use buffer::Buffer;
use editor::Editor;
use gpui_kit::{AppContext as _, Bounds, Size, WindowBounds, WindowOptions, px};
use lisp::Lisp;
use shell::Shell;

fn main() {
    let buffer = match std::env::args().nth(1) {
        Some(path) => Buffer::from_file(path.into()).unwrap_or_else(|e| {
            eprintln!("gmacs: {e}");
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
                    Size { width: px(900.), height: px(640.) },
                    cx,
                ))),
                ..Default::default()
            };
            gpui_kit::open_window(options, cx, move |window, cx| {
                let editor = Editor::new(buffer);
                let lisp = Lisp::with_config(
                    editor.clone(),
                    Lisp::default_sources(),
                    Lisp::default_lua_sources(),
                    Lisp::default_plugin_dir(),
                );
                cx.new(|cx| Shell::new(editor, lisp, window, cx))
            })
            .expect("failed to open window");
            cx.activate(true);
        });
}
