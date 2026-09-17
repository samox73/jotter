mod app;
mod kernel;
mod notebook;
mod ui;

use anyhow::Result;
use app::App;
use clap::Parser;
use crossterm::event::{
    Event, EventStream, KeyEventKind, KeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
};
use futures::StreamExt;
use std::path::PathBuf;
use tokio::sync::mpsc;
use ui::Rendered;

/// A fast Jupyter notebook TUI.
#[derive(Parser)]
#[command(version)]
struct Args {
    /// Path to the .ipynb file to open
    notebook: PathBuf,
    /// Kernelspec name (default: the notebook's kernelspec, then python3)
    #[arg(long)]
    kernel: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let (events_tx, mut events_rx) = mpsc::unbounded_channel();
    let mut app = App::open(args.notebook, args.kernel, events_tx)?; // parse errors print before the alt screen
    let mut rendered = Rendered::build(&app.notebook);
    app.spawn_kernel();

    let mut terminal = ratatui::init(); // installs a panic hook that restores the terminal
    // kitty keyboard protocol: makes Shift+Enter / Ctrl+Enter distinct keys
    let enhanced = crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false);
    if enhanced {
        let _ = crossterm::execute!(
            std::io::stdout(),
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        );
    }
    let result = run(&mut terminal, &mut app, &mut rendered, &mut events_rx).await;
    if enhanced {
        let _ = crossterm::execute!(std::io::stdout(), PopKeyboardEnhancementFlags);
    }
    ratatui::restore();
    result
}

async fn run(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    rendered: &mut Rendered,
    kernel_events: &mut mpsc::UnboundedReceiver<kernel::Event>,
) -> Result<()> {
    let mut term_events = EventStream::new();
    while !app.should_quit {
        terminal.draw(|frame| ui::draw(frame, app, rendered))?;
        tokio::select! {
            maybe_event = term_events.next() => match maybe_event {
                Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                    app.on_key(key, rendered);
                }
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
