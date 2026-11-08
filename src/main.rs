//! vmac: a modal (evil-style) editor whose behavior is written in Lua and
//! can be redefined while it runs. See README.md.

mod agent;
mod buffer;
mod clipboard;
mod daemon;
mod editor;
mod lisp;
mod lsp;
mod markdown;
mod packages;
mod picker;
mod frame;
mod review;
mod server;
mod session;
mod sessions;
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

const USAGE: &str = "\
usage: vmac [FILE]              edit FILE (or show the dashboard) in this window
       vmac attach [TARGET]     open a window on the vmac server, starting it if
                                needed; TARGET is a session's name or a project's
                                folder (its session is made the first time)
       vmac ls                  list the server's sessions
       vmac kill-server         stop the server and everything in it
       vmac --server            run the server here, without a window

Closing a window attached to the server, or :q, only detaches it: the
sessions, their terminals, language servers and agents keep running.
The server listens on $VMAC_SOCKET (default: a socket of your own in
$XDG_RUNTIME_DIR or the temporary folder).";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let socket = daemon::socket_path();
    let fail = |e: &dyn std::fmt::Display| -> ! {
        eprintln!("vmac: {e}");
        std::process::exit(1);
    };
    match args.first().map(String::as_str) {
        Some("-h" | "--help") => println!("{USAGE}"),
        Some("--server") => {
            packages::set_auto_install(true);
            if let Err(e) = daemon::serve(|| new_server(None), &socket) {
                fail(&e);
            }
        }
        Some("attach" | "a") => {
            let target = args.get(1).cloned();
            let client = daemon::ensure_server(&socket)
                .and_then(|()| daemon::Client::attach(&socket, target))
                .unwrap_or_else(|e| fail(&e));
            if let Some(why) = client.gone() {
                fail(&why);
            }
            open_window(Box::new(client));
        }
        Some("ls" | "list") => match daemon::list(&socket) {
            Ok((active, all)) => {
                for name in all {
                    println!("{name}{}", if name == active { " (active)" } else { "" });
                }
            }
            Err(_) => fail(&"no server running"),
        },
        Some("kill-server") => daemon::kill(&socket).unwrap_or_else(|_| fail(&"no server running")),
        Some(flag) if flag.starts_with('-') => fail(&format!("unknown option {flag}\n{USAGE}")),
        file => {
            let buffer = match file {
                Some(path) => Buffer::from_file(path.into()).unwrap_or_else(|e| fail(&e)),
                None => Buffer::new("*scratch*"),
            };
            let dashboard = file.is_none();
            // GPUI wants the window made on its thread: the sessions are made there.
            open_window_with(move || {
                packages::set_auto_install(true);
                Box::new(new_server(Some((buffer, dashboard))))
            });
        }
    }
}

/// The sessions: the first showing `start` (a buffer, and whether to show
/// the dashboard), or the dashboard; each one after on the dashboard.
fn new_server(start: Option<(Buffer, bool)>) -> server::Server {
    let (buffer, dashboard) = start.unwrap_or_else(|| (Buffer::new("*scratch*"), true));
    let (editor, mut lisp) = new_session(buffer);
    // Started without a file: the dashboard (unless turned off).
    if dashboard {
        let _ = lisp.eval("if core.dashboard_on_start then core.call_binding('dashboard') end");
    }
    let first = session::Session::new(editor, lisp);
    let make = Box::new(|| {
        let (editor, lisp) = new_session(Buffer::new("*scratch*"));
        session::Session::new(editor, lisp)
    });
    server::Server::new(first, make).follow_cwd()
}

fn open_window(host: Box<dyn server::Host>) {
    let mut host = Some(host);
    open_window_with(move || host.take().expect("one window"));
}

/// Opens the window, drawing from what `host` makes.
fn open_window_with(host: impl FnOnce() -> Box<dyn server::Host> + 'static) {
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
                let host = host();
                cx.new(|cx| Shell::new(host, window, cx))
            })
            .expect("failed to open window");
            cx.activate(true);
        });
}

/// An editor showing `buffer`, and its scripting engine with the config loaded.
fn new_session(buffer: Buffer) -> (Editor, Lisp) {
    let editor = Editor::new(buffer);
    editor.lock().set_state_dir(projects::History::default_dir());
    editor.lock().clipboard = Some(editor::SystemClipboard(Box::new(clipboard::System)));
    let lisp = Lisp::with_config(editor.clone(), Lisp::default_lua_sources(), Lisp::default_plugin_dir());
    (editor, lisp)
}
