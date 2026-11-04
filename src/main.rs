//! Dawngasm TUI entry point.
//!
//! Sets up the terminal, runs the event loop, and restores the terminal even
//! when the loop returns an error.

use dawngasm_tui::services::webview::{self, CLEARANCE_FLAG};
use dawngasm_tui::App;

fn main() -> anyhow::Result<()> {
    // The TUI runs the webview in a child process with this hidden flag. That
    // keeps the GUI event loop out of the terminal loop and gives each capture
    // a fresh process.
    if std::env::args().any(|argument| argument == CLEARANCE_FLAG) {
        if let Err(error) = webview::capture_and_save() {
            eprintln!("{error}");
            std::process::exit(1);
        }
        return Ok(());
    }

    // Built before the alternate screen so a startup failure (no data dir, no
    // audio device) reads as a normal error rather than a corrupted terminal.
    let mut app = App::new()?;
    let mut terminal = ratatui::init();
    let result = app.run(&mut terminal);
    ratatui::restore();
    result?;
    Ok(())
}
