use std::io::{self, stdout, Stdout};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossterm::{
    event::{self, Event, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};

use crate::ui::state::{DashboardAction, DashboardState};
use crate::ui::render;

pub fn spawn_dashboard_thread(
    state: Arc<Mutex<DashboardState>>,
    shutdown: Arc<AtomicBool>,
) {
    std::thread::spawn(move || {
        if let Err(e) = run_dashboard(state, shutdown) {
            let _ = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open("bot.log")
                .and_then(|mut f| {
                    use std::io::Write;
                    writeln!(f, "TUI exited: {e}")
                });
        }
    });
}

fn run_dashboard(
    state: Arc<Mutex<DashboardState>>,
    shutdown: Arc<AtomicBool>,
) -> io::Result<()> {
    enable_raw_mode()?;
    let mut stdout: Stdout = stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let tick_rate = Duration::from_millis(200);
    let mut last_tick = Instant::now();

    loop {
        if shutdown.load(Ordering::Relaxed) {
            break;
        }

        {
            let app = state.lock().expect("dashboard lock");
            terminal.draw(|frame| render::draw(frame, &app))?;
        }

        let timeout = tick_rate.saturating_sub(last_tick.elapsed());
        if event::poll(timeout)? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    let action = {
                        let mut app = state.lock().expect("dashboard lock");
                        app.on_key(key.code)
                    };
                    if matches!(action, DashboardAction::Quit) {
                        shutdown.store(true, Ordering::Relaxed);
                        break;
                    }
                }
            }
        }

        if last_tick.elapsed() >= tick_rate {
            {
                let mut app = state.lock().expect("dashboard lock");
                app.on_render_tick();
            }
            last_tick = Instant::now();
        }
    }

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}
