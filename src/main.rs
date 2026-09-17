mod app;
mod latex;
mod editor;
mod kernel;
mod notebook;
mod ui;

use anyhow::Result;
use app::{Action, App};
use clap::Parser;
use crossterm::cursor::SetCursorStyle;
use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event, EventStream, KeyEventKind,
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
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
}

fn enter_extras(enhanced: bool) {
    if enhanced {
        let _ = crossterm::execute!(
            std::io::stdout(),
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        );
    }
    let _ = crossterm::execute!(std::io::stdout(), EnableMouseCapture);
}

fn leave_extras(enhanced: bool) {
    let _ = crossterm::execute!(std::io::stdout(), DisableMouseCapture);
    if enhanced {
        let _ = crossterm::execute!(std::io::stdout(), PopKeyboardEnhancementFlags);
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let (events_tx, mut events_rx) = mpsc::unbounded_channel();
    let mut app = App::open(args.notebook, args.kernel, events_tx)?; // parse errors print before the alt screen
    app.spawn_kernel();

    let mut terminal = ratatui::init(); // installs a panic hook that restores the terminal
    // Query graphics protocol + font size (before EventStream claims stdin).
    // Falls back to unicode halfblocks; timeout-guarded for dumb terminals.
    let picker = if args.no_images {
        None
    } else {
        ratatui_image::picker::Picker::from_query_stdio().ok()
    };
    let mut rendered = Rendered::build(&app.notebook, picker);

    // kitty keyboard protocol: makes Shift+Enter / Ctrl+Enter distinct keys
    let enhanced = crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false);
    enter_extras(enhanced);
    let result = run(&mut terminal, &mut app, &mut rendered, &mut events_rx, enhanced).await;
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
    let mut cursor_mode: Option<editor::Mode> = None;
    while !app.should_quit {
        // modal cursor shape: block in normal, bar in insert
        let mode = app.editor.as_ref().map(|e| e.mode);
        if mode != cursor_mode {
            let style = match mode {
                Some(editor::Mode::Insert) => SetCursorStyle::SteadyBar,
                Some(editor::Mode::Normal) => SetCursorStyle::SteadyBlock,
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
                Some(Ok(_)) => {} // resize etc. -> redraw on next loop
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
    let Some(cell) = app.notebook.cells.get(app.selected) else { return };
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
