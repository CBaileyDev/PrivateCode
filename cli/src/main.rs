use clap::{Parser, Subcommand};
use crossterm::{
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use private_code_core::db;
use private_code_protocol::event::{DeltaPayload, ProtocolEvent};
use private_code_tui::run_tui;
use ratatui::{Terminal, backend::CrosstermBackend};
use std::path::{Path, PathBuf};
use uuid::Uuid;

use futures_util::{SinkExt, StreamExt};

#[derive(Parser, Debug)]
#[command(
    name = "private-code",
    version = "0.1.0",
    about = "Private Code Native AI Coding Agent"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// The workspace path to target
    #[arg(short, long, default_value = ".")]
    workspace: String,

    /// Override global database path
    #[arg(short, long)]
    database: Option<String>,

    /// Session ID to load/resume
    #[arg(short, long)]
    session: Option<String>,

    /// Agent mode: 'build' (read/write/run tools) or 'plan' (read-only default)
    #[arg(short, long, default_value = "build")]
    agent: String,

    /// Override model ID
    #[arg(short, long, default_value = "claude-opus-4-8")]
    model: String,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Start the daemon in the foreground
    Serve {
        /// Port to bind to
        #[arg(short, long, default_value_t = 48123)]
        port: u16,
    },
    /// Launch the interactive TUI (starts daemon if not running)
    Tui {
        /// Port the daemon is running on (or should run on)
        #[arg(short, long, default_value_t = 48123)]
        port: u16,
    },
    /// One-shot headless prompt execution
    Prompt {
        /// The prompt string
        prompt: String,
        /// Port the daemon is running on
        #[arg(short, long, default_value_t = 48123)]
        port: u16,
    },
}

async fn is_daemon_running(port: u16) -> bool {
    let client = reqwest::Client::new();
    let url = format!("http://127.0.0.1:{}/provider", port);
    match client.get(&url).send().await {
        Ok(_) => true,
        Err(e) => !e.is_connect(),
    }
}

async fn ensure_daemon(
    port: u16,
    database: &Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    if is_daemon_running(port).await {
        return Ok(());
    }

    let current_exe = std::env::current_exe()?;
    let mut cmd = std::process::Command::new(current_exe);
    cmd.arg("serve").arg("--port").arg(port.to_string());
    if let Some(db) = database {
        cmd.arg("--database").arg(db);
    }

    // Spawn daemon in the background
    let _child = cmd.spawn()?;

    // Wait for daemon to spin up and bind to port
    for _ in 0..20 {
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        if is_daemon_running(port).await {
            return Ok(());
        }
    }

    Err("Failed to start daemon in background".into())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Cli::parse();

    // 1. Establish workspace and global directories
    let workspace_path = std::fs::canonicalize(Path::new(&args.workspace))
        .unwrap_or_else(|_| PathBuf::from(&args.workspace));

    let home_dir = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    let global_data_dir = Path::new(&home_dir)
        .join(".local")
        .join("share")
        .join("private-code");
    std::fs::create_dir_all(&global_data_dir)?;

    // 2. Open DB & Run Migrations
    let db_path = match &args.database {
        Some(path) => PathBuf::from(path),
        None => global_data_dir.join("private-code.db"),
    };
    let db_url = format!("sqlite://{}", db_path.to_string_lossy());
    let pool = db::connect_db(&db_url).await?;
    db::run_migrations(&pool).await?;

    // 3. Resolve Project and Session
    let project_name = workspace_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("active_project");

    let project_id =
        match sqlx::query_scalar::<_, String>("SELECT id FROM project WHERE directory = ?1")
            .bind(workspace_path.to_string_lossy().to_string())
            .fetch_optional(&pool)
            .await?
        {
            Some(pid) => pid,
            None => {
                let pid = Uuid::new_v4().to_string();
                db::create_project(
                    &pool,
                    &pid,
                    project_name,
                    workspace_path.to_string_lossy().as_ref(),
                )
                .await?;
                pid
            }
        };

    let session_id = match &args.session {
        Some(sid) => sid.clone(),
        None => {
            let sid = Uuid::new_v4().to_string();
            let model_config = serde_json::json!({
                "provider_id": "anthropic",
                "model_id": args.model
            })
            .to_string();

            db::create_session(
                &pool,
                &sid,
                &project_id,
                workspace_path.to_string_lossy().as_ref(),
                workspace_path.to_string_lossy().as_ref(),
                "CLI Chat Session",
                &args.agent,
                &model_config,
            )
            .await?;
            sid
        }
    };

    let command = args.command.unwrap_or(Commands::Tui { port: 48123 });

    match command {
        Commands::Serve { port } => {
            // Setup simple tracing subscriber for the serve command
            tracing_subscriber::fmt::init();
            private_code_daemon::start_daemon(pool, global_data_dir, port).await?;
        }
        Commands::Tui { port } => {
            ensure_daemon(port, &args.database).await?;

            let token = std::fs::read_to_string(global_data_dir.join("daemon_token"))?
                .trim()
                .to_string();

            let daemon_url = format!("http://127.0.0.1:{}", port);

            // Setup crossterm terminal
            enable_raw_mode()?;
            let mut stdout = std::io::stdout();
            execute!(stdout, EnterAlternateScreen)?;
            let backend = CrosstermBackend::new(stdout);
            let mut terminal = Terminal::new(backend)?;

            // Run TUI
            let tui_res = run_tui(&mut terminal, pool, &daemon_url, &token, &session_id).await;

            // Restore terminal settings on exit
            disable_raw_mode()?;
            execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
            terminal.show_cursor()?;

            if let Err(e) = tui_res {
                eprintln!("TUI Error: {}", e);
            }
        }
        Commands::Prompt { prompt, port } => {
            ensure_daemon(port, &args.database).await?;

            let token = std::fs::read_to_string(global_data_dir.join("daemon_token"))?
                .trim()
                .to_string();

            let ws_url = format!("ws://127.0.0.1:{}/ws?session_id={}", port, session_id);
            let req = http::Request::builder()
                .uri(&ws_url)
                .header("Authorization", format!("Bearer {}", token))
                .body(())?;

            let (ws_stream, _) = tokio_tungstenite::connect_async(req).await?;
            let (mut write, mut read) = ws_stream.split();

            // Send prompt RPC
            let prompt_req = serde_json::json!({
                "jsonrpc": "2.0",
                "id": "prompt_oneshot",
                "method": "session.prompt",
                "params": {
                    "session_id": session_id,
                    "prompt": prompt,
                    "delivery": "steer"
                }
            });
            write
                .send(tokio_tungstenite::tungstenite::Message::Text(
                    prompt_req.to_string(),
                ))
                .await?;

            // Stream response
            use std::io::Write;
            while let Some(msg_res) = read.next().await {
                let msg = msg_res?;
                if let tokio_tungstenite::tungstenite::Message::Text(text) = msg
                    && let Ok(event) = serde_json::from_str::<ProtocolEvent>(&text)
                {
                    match event {
                        ProtocolEvent::MessageDelta {
                            delta: DeltaPayload::Text { text },
                            ..
                        } => {
                            print!("{}", text);
                            std::io::stdout().flush()?;
                        }
                        ProtocolEvent::MessageCompleted { .. } => {
                            println!();
                            break;
                        }
                        ProtocolEvent::Error { message, .. } => {
                            eprintln!("\nError: {}", message);
                            break;
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    Ok(())
}
