use clap::{Parser, Subcommand};
use crossterm::{
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use private_code_core::coordinator::SessionCoordinator;
use private_code_core::db;
use private_code_core::export;
use private_code_protocol::event::{DeltaPayload, ProtocolEvent, UsageStats};
use private_code_protocol::message::ChatMessage;
use private_code_providers::keyring_store::{delete_key, list_configured_providers, set_key};
use private_code_providers::{ModelProvider, ProviderError, ProviderEvent};
use private_code_tools::ToolRegistry;
use private_code_tui::run_tui;
use ratatui::{Terminal, backend::CrosstermBackend};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use uuid::Uuid;

use futures_util::stream::BoxStream;
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
    /// Run an in-process engine smoke test (no network, in-memory DB) over the
    /// shared coordinator and report per-turn loop latency. Exits non-zero on any
    /// failure or hang. A regression signal for the engine loop — NOT a benchmark
    /// (the provider is scripted with zero model latency).
    Selftest {
        /// Number of turns to drive through the coordinator
        #[arg(short, long, default_value_t = 3)]
        turns: u32,
    },
    /// Securely store a provider API key in the OS keychain
    Auth {
        #[command(subcommand)]
        action: AuthCommands,
    },
    /// Export a session to Markdown or JSON
    Export {
        session_id: String,
        #[arg(short, long, default_value = "markdown")]
        format: String,
        #[arg(short, long)]
        output: Option<String>,
    },
    /// Check for CLI updates (prints latest release tag from GitHub)
    Update,
}

#[derive(Subcommand, Debug)]
enum AuthCommands {
    /// Store an API key: `private-code auth set anthropic`
    Set { provider: String },
    /// List providers with configured keys (no values shown)
    List,
    /// Remove a stored key
    Remove { provider: String },
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

    // `selftest` is a self-contained engine smoke test: it must NOT touch on-disk
    // state (no global data dir, no on-disk DB, no project/session rows in the real
    // database), so intercept it before the normal workspace/DB bootstrap below.
    if let Some(Commands::Selftest { turns }) = &args.command {
        return run_selftest(*turns).await;
    }

    if let Some(Commands::Auth { action }) = &args.command {
        return run_auth(action).await;
    }

    if let Some(Commands::Update) = &args.command {
        return run_update_check().await;
    }

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

    if let Some(Commands::Export {
        session_id,
        format,
        output,
    }) = &args.command
    {
        let content = if format == "json" {
            export::export_session_json(&pool, session_id).await?
        } else {
            export::export_session_markdown(&pool, session_id).await?
        };
        if let Some(path) = output {
            std::fs::write(path, content)?;
            println!("Exported to {path}");
        } else {
            print!("{content}");
        }
        return Ok(());
    }

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
        // Intercepted before the DB bootstrap above (it must not touch on-disk
        // state), so the normal command dispatch never sees it.
        Commands::Selftest { .. } => unreachable!("selftest is handled before DB bootstrap"),
        Commands::Auth { .. } | Commands::Export { .. } | Commands::Update => {
            unreachable!("auth/export/update are handled before the command dispatch match")
        }
    }

    Ok(())
}

/// A scripted, zero-latency provider for `selftest`: every turn streams one short
/// text delta then stops with `end_turn`. It makes NO network call, so selftest
/// exercises `run_turn` → orchestrator → event routing → `MessageCompleted` end
/// to end without a live API key. Because model latency is stubbed to zero, the
/// reported timings measure the engine loop only — this is a smoke/regression
/// signal, NOT a latency benchmark.
struct SelftestProvider;

#[async_trait::async_trait]
impl ModelProvider for SelftestProvider {
    async fn stream_chat(
        &self,
        _model_id: &str,
        _system_prompt: Option<&str>,
        _max_tokens: u32,
        _messages: &[ChatMessage],
        _tools: &[serde_json::Value],
    ) -> Result<BoxStream<'static, Result<ProviderEvent, ProviderError>>, ProviderError> {
        let events = vec![
            Ok(ProviderEvent::TextDelta("selftest ok".into())),
            Ok(ProviderEvent::MessageStop {
                usage: UsageStats::default(),
                finish_reason: Some("end_turn".into()),
            }),
        ];
        Ok(futures_util::stream::iter(events).boxed())
    }

    fn count_tokens(&self, _model_id: &str, text: &str) -> usize {
        (text.chars().count() / 4).max(1)
    }
}

/// Await the next `MessageCompleted` on the broadcast, treating an `Error` event
/// or a closed channel as failure. A lagged receiver simply continues (a token
/// burst can outrun a slow reader; selftest reads promptly, so this is
/// belt-and-braces rather than a real risk).
async fn wait_for_completed(
    rx: &mut tokio::sync::broadcast::Receiver<ProtocolEvent>,
) -> Result<(), Box<dyn std::error::Error>> {
    use tokio::sync::broadcast::error::RecvError;
    loop {
        match rx.recv().await {
            Ok(ProtocolEvent::MessageCompleted { .. }) => return Ok(()),
            Ok(ProtocolEvent::Error { message, .. }) => {
                return Err(format!("turn emitted an error event: {message}").into());
            }
            Ok(_) => continue,
            Err(RecvError::Lagged(_)) => continue,
            Err(RecvError::Closed) => {
                return Err("event channel closed before the turn completed".into());
            }
        }
    }
}

/// Drive `turns` turns through an in-process [`SessionCoordinator`] backed by an
/// in-memory DB, throwaway workspace, and the scripted [`SelftestProvider`], then
/// report per-turn loop latency. Every turn must complete (hard failure on error
/// or hang); the timings are informational only — there is deliberately NO timing
/// ceiling, because a fixed millisecond budget is environment-dependent and would
/// make CI flaky. A hang is caught by a wrapping per-turn timeout instead.
async fn run_selftest(turns: u32) -> Result<(), Box<dyn std::error::Error>> {
    let turns = turns.max(1);
    eprintln!(
        "engine selftest: driving {turns} turn(s) through the in-process coordinator \
         (in-memory DB, scripted zero-latency provider)…"
    );

    // Isolated, auto-cleaned temp dirs so selftest leaves NO on-disk state behind:
    // an in-memory SQLite DB, a throwaway workspace, and a throwaway snapshot store
    // (the orchestrator git-checkpoints the workspace into the data dir each turn).
    let workspace = tempfile::tempdir()?;
    let data_dir = tempfile::tempdir()?;
    let ws_str = workspace.path().to_string_lossy().to_string();

    let pool = db::connect_db("sqlite::memory:").await?;
    db::run_migrations(&pool).await?;

    let project_id = Uuid::new_v4().to_string();
    db::create_project(&pool, &project_id, "selftest", &ws_str).await?;
    let session_id = Uuid::new_v4().to_string();
    let model_config =
        serde_json::json!({"provider_id": "anthropic", "model_id": "claude-opus-4-8"}).to_string();
    db::create_session(
        &pool,
        &session_id,
        &project_id,
        &ws_str,
        &ws_str,
        "selftest",
        "build",
        &model_config,
    )
    .await?;

    let coord = SessionCoordinator::new(
        pool,
        data_dir.path().to_path_buf(),
        Arc::new(SelftestProvider),
        Arc::new(ToolRegistry::new()),
    );

    // A turn that never completes is a hard failure (deadlock / lost wakeup),
    // caught by this wrapping timeout — NOT a latency budget.
    const TURN_TIMEOUT: Duration = Duration::from_secs(20);

    let mut durations: Vec<Duration> = Vec::with_capacity(turns as usize);
    for i in 0..turns {
        // Subscribe fresh before each turn so no buffered prior-turn event is
        // mistaken for this turn's completion.
        let mut rx = coord.get_or_create_session(&session_id).await?;
        let start = Instant::now();
        coord
            .run_turn(&session_id, &format!("selftest turn {i}"), "steer")
            .await?;
        match tokio::time::timeout(TURN_TIMEOUT, wait_for_completed(&mut rx)).await {
            Ok(Ok(())) => durations.push(start.elapsed()),
            Ok(Err(e)) => {
                coord.shutdown(Duration::from_secs(5)).await;
                return Err(format!("selftest turn {i} failed: {e}").into());
            }
            Err(_) => {
                coord.shutdown(Duration::from_secs(5)).await;
                return Err(format!(
                    "selftest turn {i} hung: no MessageCompleted within {TURN_TIMEOUT:?}"
                )
                .into());
            }
        }
    }

    coord.shutdown(Duration::from_secs(5)).await;

    // Correctness: exactly one completed turn per requested turn.
    if durations.len() != turns as usize {
        return Err(format!(
            "selftest expected {turns} completed turns, got {}",
            durations.len()
        )
        .into());
    }

    // Informational metrics ONLY (no pass/fail threshold — latency is stubbed).
    let total: Duration = durations.iter().sum();
    let avg = total / durations.len() as u32;
    let min = durations.iter().min().copied().unwrap_or_default();
    let max = durations.iter().max().copied().unwrap_or_default();
    println!(
        "selftest PASSED: {} turn(s) completed | avg {avg:?} | min {min:?} | max {max:?} \
         (engine-loop only; model latency stubbed to zero)",
        durations.len()
    );

    Ok(())
}

async fn run_auth(action: &AuthCommands) -> Result<(), Box<dyn std::error::Error>> {
    match action {
        AuthCommands::Set { provider } => {
            print!("Enter API key for {provider}: ");
            use std::io::{self, Write};
            io::stdout().flush()?;
            let mut key = String::new();
            io::stdin().read_line(&mut key)?;
            let key = key.trim();
            set_key(provider, key)?;
            println!("Stored key for {provider} in OS keychain.");
        }
        AuthCommands::List => {
            let candidates = [
                "anthropic",
                "openai",
                "google",
                "nvidia",
                "deepseek",
                "groq",
            ];
            let configured = list_configured_providers(&candidates);
            if configured.is_empty() {
                println!("No provider keys configured.");
            } else {
                for p in configured {
                    println!("{p}");
                }
            }
        }
        AuthCommands::Remove { provider } => {
            delete_key(provider)?;
            println!("Removed key for {provider}.");
        }
    }
    Ok(())
}

async fn run_update_check() -> Result<(), Box<dyn std::error::Error>> {
    let client = reqwest::Client::new();
    let resp = client
        .get("https://api.github.com/repos/CBaileyDev/PrivateCode/releases/latest")
        .header("User-Agent", "private-code-cli")
        .send()
        .await?;
    if resp.status().is_success() {
        let body: serde_json::Value = resp.json().await?;
        let tag = body["tag_name"].as_str().unwrap_or("unknown");
        println!("Latest release: {tag}");
        println!("Download: https://github.com/CBaileyDev/PrivateCode/releases/latest");
    } else {
        eprintln!("Update check failed: HTTP {}", resp.status());
    }
    Ok(())
}
