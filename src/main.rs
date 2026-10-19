//! Audiogasm TUI entry point.
//!
//! Sets up the terminal, runs the event loop, and restores the terminal even
//! when the loop returns an error.

use audiogasm_tui::App;

fn main() -> anyhow::Result<()> {
    // Built before the alternate screen so a startup failure (no data dir, no
    // audio device) reads as a normal error rather than a corrupted terminal.
    let mut app = App::new()?;
    let mut terminal = ratatui::init();
    let result = app.run(&mut terminal);
    ratatui::restore();
    result?;
    Ok(())
}
