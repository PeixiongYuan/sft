use std::io::{self, stdout};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use crossterm::{ExecutableCommand, cursor};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use crate::app::{App, AppEvent};

pub fn run(mut app: App) -> Result<()> {
    let (tx, rx) = mpsc::channel::<AppEvent>();
    app.start(tx.clone());

    enable_raw_mode()?;
    stdout().execute(EnterAlternateScreen)?;
    stdout().execute(cursor::Hide)?;
    let _guard = TerminalGuard;

    let backend = CrosstermBackend::new(stdout());
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    while !app.should_quit {
        while let Ok(event) = rx.try_recv() {
            app.handle_event(event, tx.clone())?;
        }

        terminal.draw(|frame| crate::ui::render(frame, &mut app))?;

        if event::poll(Duration::from_millis(50))?
            && let Event::Key(key) = event::read()?
        {
            app.handle_key(key, tx.clone())?;
        }
    }

    terminal.show_cursor()?;
    Ok(())
}

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = io::stdout().execute(cursor::Show);
        let _ = io::stdout().execute(LeaveAlternateScreen);
    }
}
