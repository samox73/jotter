mod app;
mod config;
mod editor;
mod kernel;
mod latex;
mod log;
mod notebook;
mod nvim;
mod ui;

use anyhow::Result;
use app::{Action, App};
use clap::Parser;
use crossterm::cursor::SetCursorStyle;
use crossterm::event::{
    DisableFocusChange, DisableMouseCapture, EnableFocusChange, EnableMouseCapture, Event,
    EventStream, KeyEventKind, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use futures::StreamExt;
use std::path::PathBuf;
use tokio::sync::mpsc;
use ui::Rendered;

/// jOtter — a fast Jupyter notebook TUI. The Jupyter otter. 🦦
#[derive(Parser)]
#[command(name = "jotter", version)]
struct Args {
    /// Path to the .ipynb file to open
    notebook: PathBuf,
    /// Kernelspec name (default: the notebook's kernelspec, then python3)
    #[arg(long)]
    kernel: Option<String>,
    /// Disable inline graphics (kitty/sixel)
    #[arg(long)]
    no_images: bool,
    /// Append a debug log (jotter + kernel wire) to this file
    #[arg(long)]
    log: Option<PathBuf>,
}

fn enter_extras(enhanced: bool) {
    if enhanced {
        let _ = crossterm::execute!(
            std::io::stdout(),
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        );
    }
    let _ = crossterm::execute!(std::io::stdout(), EnableMouseCapture, EnableFocusChange);
}

fn leave_extras(enhanced: bool) {
    let _ = crossterm::execute!(std::io::stdout(), DisableMouseCapture, DisableFocusChange);
    if enhanced {
        let _ = crossterm::execute!(std::io::stdout(), PopKeyboardEnhancementFlags);
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let config_warning = config::init();
    if let Some(path) = &args.log {
        log::init(path)?; // bad log path: fail loudly before the alt screen
    }
    let (events_tx, mut events_rx) = mpsc::unbounded_channel();
    let mut app = App::open(args.notebook, args.kernel, events_tx)?; // parse errors print before the alt screen
    if let Some(warning) = config_warning {
        app.message = Some(warning);
    }
    app.spawn_kernel();

    let mut terminal = ratatui::init(); // installs a panic hook that restores the terminal
    // Query graphics protocol + font size (before EventStream claims stdin).
    // Falls back to unicode halfblocks; timeout-guarded for dumb terminals.
    let picker = if args.no_images {
        None
    } else {
        ratatui_image::picker::Picker::from_query_stdio().ok()
    };
    let dir = app.path.parent().map(PathBuf::from).unwrap_or_default();
    let mut rendered = Rendered::build(&app.notebook, picker, dir);

    // kitty keyboard protocol: makes Shift+Enter / Ctrl+Enter distinct keys
    let enhanced = crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false);
    enter_extras(enhanced);
    let result = run(
        &mut terminal,
        &mut app,
        &mut rendered,
        &mut events_rx,
        enhanced,
    )
    .await;
    leave_extras(enhanced);
    let _ = crossterm::execute!(std::io::stdout(), SetCursorStyle::DefaultUserShape);
    ratatui::restore();
    result
}

async fn run(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    rendered: &mut Rendered,
    kernel_events: &mut mpsc::UnboundedReceiver<kernel::Event>,
    enhanced: bool,
) -> Result<()> {
    let mut term_events = EventStream::new();
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
        terminal.draw(|frame| ui::draw(frame, app, rendered))?;
        tokio::select! {
            maybe_event = term_events.next() => match maybe_event {
                Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                    if let Action::ExternalEdit = app.on_key(key, rendered) {
                        external_edit(terminal, app, rendered, enhanced);
                    }
                }
                Some(Ok(Event::Mouse(mouse))) => app.on_mouse(mouse, rendered),
                // reflow happens on redraw; images need a rebuild if the font changed
                Some(Ok(Event::Resize(..))) => rendered.on_resize(&app.notebook),
                // nvim-autoread-style external change detection
                Some(Ok(Event::FocusGained)) => app.check_disk(rendered),
                Some(Ok(_)) => {} // focus etc. -> redraw on next loop
                Some(Err(e)) => return Err(e.into()),
                None => break,
            },
            Some(event) = kernel_events.recv() => {
                app.apply_kernel_event(event, rendered);
                // drain whatever else queued up before redrawing once
                while let Ok(event) = kernel_events.try_recv() {
                    app.apply_kernel_event(event, rendered);
                }
            }
            // fallback for terminals without focus events, then the sidecar
            _ = autosave.tick() => {
                app.check_disk(rendered);
                app.maybe_autosave();
            }
        }
    }
    Ok(())
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
    let suffix = if cell.cell_type == "code" { "py" } else { "md" };
    let tmp = std::env::temp_dir().join(format!("jotter-cell-{}.{suffix}", std::process::id()));
    let editor_cmd = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".into());

    let result = std::fs::write(&tmp, &cell.source).and_then(|()| {
        leave_extras(enhanced);
        ratatui::restore();
        let status = std::process::Command::new(&editor_cmd).arg(&tmp).status();
        *terminal = ratatui::init();
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
