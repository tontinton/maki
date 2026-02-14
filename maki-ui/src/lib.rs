pub mod app;

use std::env;
use std::io::stdout;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use color_eyre::Result;
use crossterm::ExecutableCommand;
use crossterm::event::{self, Event};
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use maki_agent::Model;
use maki_agent::agent;
use maki_agent::{AgentEvent, AgentInput};
use tracing::error;

use app::{Action, App, Msg};

const EVENT_POLL_INTERVAL_MS: u64 = 16;

pub fn run(model: Model) -> Result<()> {
    let mut terminal = ratatui::init();
    stdout().execute(EnterAlternateScreen)?;
    terminal::enable_raw_mode()?;

    let result = run_event_loop(&mut terminal, model);

    terminal::disable_raw_mode()?;
    stdout().execute(LeaveAlternateScreen)?;
    ratatui::restore();

    result
}

fn run_event_loop(terminal: &mut ratatui::DefaultTerminal, model: Model) -> Result<()> {
    let mut app = App::new(model.pricing.clone());
    let (agent_tx, agent_rx) = mpsc::channel::<AgentEvent>();
    let (input_tx, input_rx) = mpsc::channel::<AgentInput>();

    let cwd = env::current_dir()?.to_string_lossy().to_string();
    spawn_agent_thread(input_rx, agent_tx, cwd, model);

    loop {
        terminal.draw(|f| app.view(f))?;

        while let Ok(event) = agent_rx.try_recv() {
            for action in app.update(Msg::Agent(event)) {
                handle_action(action, &input_tx);
            }
        }

        if event::poll(Duration::from_millis(EVENT_POLL_INTERVAL_MS))?
            && let Event::Key(key) = event::read()?
        {
            for action in app.update(Msg::Key(key)) {
                handle_action(action, &input_tx);
            }
        }

        if app.should_quit {
            break;
        }
    }

    Ok(())
}

fn spawn_agent_thread(
    input_rx: mpsc::Receiver<AgentInput>,
    event_tx: mpsc::Sender<AgentEvent>,
    cwd: String,
    model: Model,
) {
    thread::spawn(move || {
        let provider = match maki_agent::provider::from_model(&model) {
            Ok(p) => p,
            Err(e) => {
                error!(error = %e, "provider error");
                let _ = event_tx.send(AgentEvent::Error {
                    message: e.to_string(),
                });
                return;
            }
        };
        let mut history = Vec::new();
        while let Ok(input) = input_rx.recv() {
            let system = agent::build_system_prompt(&cwd, &input.mode);
            if let Err(e) = agent::run(&*provider, &model, input, &mut history, &system, &event_tx)
            {
                error!(error = %e, "agent error");
                let _ = event_tx.send(AgentEvent::Error {
                    message: e.to_string(),
                });
            }
        }
    });
}

fn handle_action(action: Action, input_tx: &mpsc::Sender<AgentInput>) {
    match action {
        Action::SendMessage(input) => {
            let _ = input_tx.send(input);
        }
        Action::Quit => {}
    }
}
