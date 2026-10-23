mod app;
mod config;
mod editor;
mod gendocs;
mod kernel;
mod latex;
mod log;
mod notebook;
mod nvim;
mod recolor;
mod ui;

use anyhow::Result;
use app::{Action, App};
use clap::{CommandFactory, Parser};
use crossterm::cursor::SetCursorStyle;
use crossterm::event::{
    DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
    EnableFocusChange, EnableMouseCapture, Event, EventStream, KeyEventKind,
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate};
use futures::{FutureExt, StreamExt};
use std::path::PathBuf;
use tokio::sync::mpsc;
use ui::Rendered;

/// A fast Jupyter notebook TUI
///
/// Open, edit and run .ipynb notebooks in the terminal, with vim keys, inline
/// plots and rendered LaTeX. Press ? inside for the keys.
///
/// Documentation: https://samox73.github.io/jotter/
#[derive(Parser)]
#[command(name = "jotter", version)]
struct Args {
    /// Path to the .ipynb file to open (none: a scratchpad; `:w name.ipynb` saves it)
    notebook: Option<PathBuf>,
    /// Kernelspec name (default: the notebook's kernelspec, then python3)
    #[arg(long)]
    kernel: Option<String>,
    /// Disable inline graphics (kitty/sixel)
    #[arg(long)]
    no_images: bool,
    /// Append a debug log (jotter + kernel wire) to this file
    #[arg(long)]
    log: Option<PathBuf>,
    /// Write docs reference pages, man page and completions into DIR
    #[arg(long, hide = true, value_name = "DIR")]
    generate: Option<PathBuf>,
}

/// Physically clear the screen. (`Terminal::clear` would also query the
/// cursor position, which fails on terminals that don't answer DSR.)
fn clear_screen() {
    use crossterm::terminal::{Clear, ClearType};
    let _ = crossterm::execute!(std::io::stdout(), Clear(ClearType::All));
}

fn enter_extras(enhanced: bool) {
    if enhanced {
        let _ = crossterm::execute!(
            std::io::stdout(),
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        );
    }
    let _ = crossterm::execute!(
        std::io::stdout(),
        EnableMouseCapture,
        EnableFocusChange,
        EnableBracketedPaste
    );
}

fn leave_extras(enhanced: bool) {
    let _ = crossterm::execute!(
        std::io::stdout(),
        DisableMouseCapture,
        DisableFocusChange,
        DisableBracketedPaste
    );
    if enhanced {
        let _ = crossterm::execute!(std::io::stdout(), PopKeyboardEnhancementFlags);
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    if let Some(dir) = args.generate {
        return gendocs::generate(Args::command(), &dir);
    }
    let notebook = args.notebook.unwrap_or_default(); // empty: scratchpad
    let config_warning = config::init();
    log::init(args.log.as_deref())?; // bad log path: fail loudly before the alt screen
    let (events_tx, mut events_rx) = mpsc::unbounded_channel();
    let mut app = App::open(notebook, args.kernel, events_tx)?; // parse errors print before the alt screen
    if let Some(warning) = config_warning {
        app.message = Some(warning);
    }
    app.spawn_kernel();

    let mut terminal = ratatui::init(); // installs a panic hook that restores the terminal
    // The alternate screen may still hold a killed session's frame (kill -9
    // can't restore the terminal): ratatui's first frame only writes cells
    // that differ from a *blank* screen, so blank it for real first.
    clear_screen();
    // Two round trips before EventStream claims stdin, each ending at a
    // reply every terminal sends (DSR, DA1) so the timeouts only matter for
    // a terminal that answers nothing: the graphics protocol and cell size
    // (falls back to unicode halfblocks), then the colours for recolouring
    // plots together with the keyboard protocol (Shift+Enter, Ctrl+Enter as
    // distinct keys).
    let picker = if args.no_images {
        None
    } else {
        ratatui_image::picker::Picker::from_query_stdio_with_options(
            ratatui_image::picker::cap_parser::QueryStdioOptions {
                timeout: std::time::Duration::from_secs(1),
                ..Default::default()
            },
        )
        .ok()
    };
    let (palette, enhanced) = recolor::query();
    recolor::init(palette.filter(|_| picker.is_some() && config::get().recolor_plots));
    // kitty keeps images across a crashed session: drop them all
    if picker
        .as_ref()
        .is_some_and(|p| p.protocol_type() == ratatui_image::picker::ProtocolType::Kitty)
    {
        use std::io::Write;
        let _ = write!(std::io::stdout(), "\x1b_Ga=d,d=A\x1b\\");
        let _ = std::io::stdout().flush();
    }
    let dir = app.path.parent().map(PathBuf::from).unwrap_or_default();
    let mut rendered = Rendered::build(&app.notebook, picker, dir);

    app.keyboard_protocol = enhanced;
    ::log::info!(
        "terminal: graphics {}, keyboard protocol {}",
        rendered.graphics_summary(),
        if enhanced { "kitty" } else { "legacy" }
    );
    enter_extras(enhanced);
    // A panic unwinds through here: ratatui's hook has already left the
    // alternate screen and printed the message, and this path switches off
    // the modes it doesn't know about (mouse, paste, keyboard protocol) and
    // keeps the unsaved work as the recovery copy.
    let result = std::panic::AssertUnwindSafe(run(
        &mut terminal,
        &mut app,
        &mut rendered,
        &mut events_rx,
        enhanced,
    ))
    .catch_unwind()
    .await;
    leave_extras(enhanced);
    let _ = crossterm::execute!(std::io::stdout(), SetCursorStyle::DefaultUserShape);
    // The terminal may be gone (closed under us). ratatui::restore() and
    // Terminal's Drop report failures with eprintln!, which panics on a dead
    // tty and then aborts from ratatui's panic hook; so try, and skip Drop.
    let _ = ratatui::try_restore();
    let _ = terminal.show_cursor();
    std::mem::forget(terminal);
    match result {
        Ok(result) => result,
        Err(payload) => {
            app.maybe_autosave();
            std::panic::resume_unwind(payload)
        }
    }
}

async fn run(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    rendered: &mut Rendered,
    kernel_events: &mut mpsc::UnboundedReceiver<kernel::Event>,
    enhanced: bool,
) -> Result<()> {
    let mut term_events = EventStream::new();
    use tokio::signal::unix::{SignalKind, signal};
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sighup = signal(SignalKind::hangup())?;
    let mut sigint = signal(SignalKind::interrupt())?;
    let mut cursor_mode: Option<nvim::ModeKind> = None;
    // Autosave sidecar tick; writes only when something changed since last time.
    let mut autosave = tokio::time::interval(std::time::Duration::from_secs(
        config::get().autosave_secs.max(1),
    ));
    while !app.should_quit {
        // modal cursor shape: block in normal, bar in insert, underline in replace
        let mode = app.editor.as_ref().map(|e| e.mode());
        if mode != cursor_mode {
            let style = match mode {
                Some(nvim::ModeKind::Insert) => SetCursorStyle::SteadyBar,
                Some(nvim::ModeKind::Replace) => SetCursorStyle::SteadyUnderScore,
                Some(_) => SetCursorStyle::SteadyBlock,
                None => SetCursorStyle::DefaultUserShape,
            };
            let _ = crossterm::execute!(std::io::stdout(), style);
            cursor_mode = mode;
        }
        // One synchronized update per frame (terminals without it ignore
        // the mode), so the terminal never shows a half-drawn frame: a
        // replaced image's new copy and the old one's delete land together.
        let _ = crossterm::execute!(std::io::stdout(), BeginSynchronizedUpdate);
        terminal.draw(|frame| ui::draw(frame, app, rendered))?;
        ui::flush_kitty_deletes(&mut std::io::stdout())?;
        let _ = crossterm::execute!(std::io::stdout(), EndSynchronizedUpdate);
        tokio::select! {
            maybe_event = term_events.next() => {
                let Some(event) = maybe_event else { break };
                let mut event = event?;
                // Keys that arrived together (a paste, a macro, repeat
                // outrunning the frame rate) are all handled before the one
                // redraw; a frame per key would cost a full redraw each.
                // Mouse and other events still draw at once: mouse hits
                // resolve against the last frame.
                loop {
                    let is_key = matches!(event, Event::Key(_) | Event::Paste(_));
                    if let Action::ExternalEdit = on_term_event(app, rendered, event) {
                        external_edit(terminal, app, rendered, enhanced);
                        break;
                    }
                    if !is_key || app.should_quit || !crossterm::event::poll(std::time::Duration::ZERO)? {
                        break;
                    }
                    event = crossterm::event::read()?;
                }
            },
            Some(event) = kernel_events.recv() => {
                app.apply_kernel_event(event, rendered);
                // drain whatever else queued up before redrawing once
                while let Ok(event) = kernel_events.try_recv() {
                    app.apply_kernel_event(event, rendered);
                }
            }
            // terminal closed / killed: keep the work, restore the terminal,
            // and let Drop take the kernel down with us
            _ = sigterm.recv() => app.on_signal("SIGTERM"),
            _ = sighup.recv() => app.on_signal("SIGHUP"),
            _ = sigint.recv() => app.on_signal("SIGINT"),
            // fallback for terminals without focus events, then the sidecar
            _ = autosave.tick() => {
                app.check_disk(rendered);
                app.maybe_autosave();
            }
        }
    }
    Ok(())
}

fn on_term_event(app: &mut App, rendered: &mut Rendered, event: Event) -> Action {
    match event {
        Event::Key(key) if key.kind == KeyEventKind::Press => return app.on_key(key, rendered),
        Event::Paste(text) => app.on_paste(text, rendered),
        Event::Mouse(mouse) => app.on_mouse(mouse, rendered),
        // reflow happens on redraw; images need a rebuild if the font changed
        Event::Resize(..) => rendered.on_resize(&app.notebook),
        // nvim-autoread-style external change detection
        Event::FocusGained => app.check_disk(rendered),
        _ => {} // focus etc. -> redraw on next loop
    }
    Action::None
}

/// Suspend the TUI, open the selected cell in $VISUAL/$EDITOR, read it back.
fn external_edit(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    rendered: &mut Rendered,
    enhanced: bool,
) {
    let Some(cell) = app.notebook.cells.get(app.selected) else {
        return;
    };
    let suffix = rendered.edit_suffix(cell);
    let tmp = std::env::temp_dir().join(format!("jotter-cell-{}.{suffix}", std::process::id()));
    let editor_cmd = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".into());

    let result = std::fs::write(&tmp, &cell.source).and_then(|()| {
        leave_extras(enhanced);
        ratatui::restore();
        let status = std::process::Command::new(&editor_cmd).arg(&tmp).status();
        *terminal = ratatui::init();
        clear_screen(); // the editor's screen is not ours to diff against
        enter_extras(enhanced);
        status
    });
    match result {
        Ok(status) if status.success() => match std::fs::read_to_string(&tmp) {
            Ok(source) => app.apply_external_edit(source, rendered),
            Err(e) => app.message = Some(format!("read-back failed: {e}")),
        },
        Ok(status) => app.message = Some(format!("{editor_cmd} exited with {status}")),
        Err(e) => app.message = Some(format!("{editor_cmd} failed: {e}")),
    }
    let _ = std::fs::remove_file(&tmp);
}
