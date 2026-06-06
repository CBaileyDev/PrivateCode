use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use futures_util::{SinkExt, StreamExt};
use private_code_core::db::{self};
use private_code_core::permissions::PermissionPrompt;
use private_code_protocol::event::{DeltaPayload, ProtocolEvent, UsageStats};
use private_code_protocol::message::{ChatMessage, ContentBlock, Role, ToolResultContent};
use ratatui::{
    Terminal,
    backend::Backend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use tokio::sync::mpsc;
use uuid::Uuid;

pub struct AppState {
    pub session_id: String,
    pub workspace_path: PathBuf,
    pub messages: Vec<ChatMessage>,
    pub input_text: String,
    pub show_sidebar: bool,
    pub usage: UsageStats,
    pub current_model: String,
    pub current_agent: String,

    // Scrolling
    pub scroll: u16,

    // Streaming assistant message state
    pub streaming_text: String,
    pub streaming_reasoning: String,
    pub streaming_tool_uses: Vec<(String, String, String)>, // (id, name, input_delta)

    // Active permission prompt state
    pub active_prompt: Option<PermissionPrompt>,

    // Connection state
    pub connected: bool,
    pub latest_seq: Arc<AtomicI64>,
}

impl AppState {
    pub fn new(session_id: &str, workspace_path: &Path) -> Self {
        Self {
            session_id: session_id.to_string(),
            workspace_path: workspace_path.to_path_buf(),
            messages: Vec::new(),
            input_text: String::new(),
            show_sidebar: true,
            usage: UsageStats::default(),
            current_model: "claude-opus-4-8".to_string(),
            current_agent: "build".to_string(),
            scroll: 0,
            streaming_text: String::new(),
            streaming_reasoning: String::new(),
            streaming_tool_uses: Vec::new(),
            active_prompt: None,
            connected: false,
            latest_seq: Arc::new(AtomicI64::new(0)),
        }
    }
}

pub enum TuiControlEvent {
    WsConnected,
    WsDisconnected,
    ProtocolEvent(ProtocolEvent),
}

async fn ws_manager(
    daemon_url: String,
    token: String,
    session_id: String,
    mut outgoing_rx: mpsc::Receiver<serde_json::Value>,
    control_tx: mpsc::Sender<TuiControlEvent>,
    latest_seq: Arc<AtomicI64>,
) {
    let ws_base = daemon_url
        .replace("http://", "ws://")
        .replace("https://", "wss://");
    let mut backoff = std::time::Duration::from_millis(100);

    loop {
        let after_seq = latest_seq.load(Ordering::Relaxed);
        let url_str = format!(
            "{}/ws?session_id={}&after_seq={}",
            ws_base, session_id, after_seq
        );

        let req = match http::Request::builder()
            .uri(&url_str)
            .header("Authorization", format!("Bearer {}", token))
            .body(())
        {
            Ok(r) => r,
            Err(_) => {
                tokio::time::sleep(backoff).await;
                continue;
            }
        };

        let ws_conn = tokio_tungstenite::connect_async(req).await;

        let (ws_stream, _) = match ws_conn {
            Ok(conn) => conn,
            Err(_) => {
                let _ = control_tx.send(TuiControlEvent::WsDisconnected).await;
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(std::time::Duration::from_secs(5));
                continue;
            }
        };

        backoff = std::time::Duration::from_millis(100);
        let _ = control_tx.send(TuiControlEvent::WsConnected).await;

        let (mut write, mut read) = ws_stream.split();
        let mut ping_interval = tokio::time::interval(std::time::Duration::from_secs(30));

        loop {
            tokio::select! {
                Some(rpc) = outgoing_rx.recv() => {
                    if let Ok(text) = serde_json::to_string(&rpc)
                        && write.send(tokio_tungstenite::tungstenite::Message::Text(text)).await.is_err() {
                            break;
                        }
                }
                res = read.next() => {
                    let msg_res = match res {
                        Some(m) => m,
                        None => break,
                    };
                    let msg = match msg_res {
                        Ok(m) => m,
                        Err(_) => break,
                    };

                    match msg {
                        tokio_tungstenite::tungstenite::Message::Text(text) => {
                            if let Ok(event) = serde_json::from_str::<ProtocolEvent>(&text) {
                                let seq = match &event {
                                    ProtocolEvent::MessageCompleted { seq, .. } => *seq,
                                    ProtocolEvent::ToolRequested { seq, .. } => *seq,
                                    ProtocolEvent::ToolPermissionRequired { seq, .. } => *seq,
                                    ProtocolEvent::ToolOutput { seq, .. } => *seq,
                                    ProtocolEvent::CheckpointCreated { seq, .. } => *seq,
                                    ProtocolEvent::UsageUpdated { seq, .. } => *seq,
                                    ProtocolEvent::Error { seq, .. } => *seq,
                                    _ => 0,
                                };
                                if seq > 0 {
                                    latest_seq.store(seq, Ordering::Relaxed);
                                }
                                let _ = control_tx.send(TuiControlEvent::ProtocolEvent(event)).await;
                            }
                        }
                        tokio_tungstenite::tungstenite::Message::Ping(payload) => {
                            let _ = write.send(tokio_tungstenite::tungstenite::Message::Pong(payload)).await;
                        }
                        _ => {}
                    }
                }
                _ = ping_interval.tick() => {
                    if write.send(tokio_tungstenite::tungstenite::Message::Ping(vec![])).await.is_err() {
                        break;
                    }
                }
            }
        }

        let _ = control_tx.send(TuiControlEvent::WsDisconnected).await;
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
}

/// The conversation list is reconciled from the shared SQLite DB (the daemon
/// persists every user/assistant/tool message there). WS events drive live
/// streaming + tell us *when* to reload; the DB is the source of truth. This is
/// what makes attach/reconnect show real history and the user's own prompt visible.
async fn reload_messages(pool: &sqlx::SqlitePool, session_id: &str, state: &mut AppState) {
    if let Ok(msgs) = db::get_messages(pool, session_id).await {
        state.messages.clear();
        for m in msgs {
            if let Ok(cm) = serde_json::from_str::<ChatMessage>(&m.data) {
                state.messages.push(cm);
            }
        }
        state.scroll = 1000;
    }
}

pub async fn run_tui<B: Backend>(
    terminal: &mut Terminal<B>,
    pool: sqlx::SqlitePool,
    daemon_url: &str,
    token: &str,
    session_id: &str,
) -> io::Result<()> {
    // 1. Initial Session Check from local SQLite database
    let session = match db::get_session(&pool, session_id).await {
        Ok(Some(s)) => s,
        _ => return Err(io::Error::new(io::ErrorKind::NotFound, "Session not found")),
    };

    let mut state = AppState::new(session_id, Path::new(&session.workspace_path));

    state.current_agent = session.agent_id.clone();
    state.usage = UsageStats {
        input_tokens: session.tokens_input,
        output_tokens: session.tokens_output,
        reasoning_tokens: session.tokens_reasoning,
        cache_read_tokens: session.tokens_cache_read,
        cache_write_tokens: session.tokens_cache_write,
        cost: session.cost,
    };

    if let Ok(model_val) = serde_json::from_str::<serde_json::Value>(&session.model_config)
        && let Some(m) = model_val["model_id"].as_str()
    {
        state.current_model = m.to_string();
    }

    // Load existing conversation from the shared DB (real history on first paint).
    reload_messages(&pool, session_id, &mut state).await;

    // 2. Establish channels and WS client
    let (outgoing_tx, outgoing_rx) = mpsc::channel::<serde_json::Value>(100);
    let (control_tx, mut control_rx) = mpsc::channel::<TuiControlEvent>(1000);

    let latest_seq = state.latest_seq.clone();
    let d_url = daemon_url.to_string();
    let d_token = token.to_string();
    let s_id = session_id.to_string();

    tokio::spawn(async move {
        ws_manager(d_url, d_token, s_id, outgoing_rx, control_tx, latest_seq).await;
    });

    loop {
        // Render
        terminal.draw(|f| draw_ui(f, &state))?;

        // Poll for inputs and events
        tokio::select! {
            res = tokio::task::spawn_blocking(|| event::poll(std::time::Duration::from_millis(50))) => {
                if let Ok(Ok(true)) = res
                    && let Event::Key(key) = event::read()? {
                        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
                            break;
                        }

                        if let Some(ref prompt) = state.active_prompt {
                            match key.code {
                                KeyCode::Char('y') | KeyCode::Char('Y') => {
                                    let req = serde_json::json!({
                                        "jsonrpc": "2.0",
                                        "id": Uuid::new_v4().to_string(),
                                        "method": "permission.reply",
                                        "params": {
                                            "session_id": session_id.to_string(),
                                            "permission_id": prompt.permission_id.clone(),
                                            "reply": "once"
                                        }
                                    });
                                    let _ = outgoing_tx.send(req).await;
                                    state.active_prompt = None;
                                }
                                KeyCode::Char('n') | KeyCode::Char('N') => {
                                    let req = serde_json::json!({
                                        "jsonrpc": "2.0",
                                        "id": Uuid::new_v4().to_string(),
                                        "method": "permission.reply",
                                        "params": {
                                            "session_id": session_id.to_string(),
                                            "permission_id": prompt.permission_id.clone(),
                                            "reply": "reject"
                                        }
                                    });
                                    let _ = outgoing_tx.send(req).await;
                                    state.active_prompt = None;
                                }
                                KeyCode::Char('a') | KeyCode::Char('A') => {
                                    let req = serde_json::json!({
                                        "jsonrpc": "2.0",
                                        "id": Uuid::new_v4().to_string(),
                                        "method": "permission.reply",
                                        "params": {
                                            "session_id": session_id.to_string(),
                                            "permission_id": prompt.permission_id.clone(),
                                            "reply": "always"
                                        }
                                    });
                                    let _ = outgoing_tx.send(req).await;
                                    state.active_prompt = None;
                                }
                                _ => {}
                            }
                        } else {
                            match key.code {
                                KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                                    state.show_sidebar = !state.show_sidebar;
                                }
                                KeyCode::Enter if !state.input_text.trim().is_empty() => {
                                    let cmd = state.input_text.trim().to_string();
                                    state.input_text.clear();

                                    if cmd.starts_with('/') {
                                        handle_slash_command(&mut state, &cmd, &pool, daemon_url, token, session_id, &session.project_id).await;
                                    } else {
                                        // Optimistically show the user's prompt immediately (the DB
                                        // reload on message.completed reconciles it).
                                        state.messages.push(ChatMessage {
                                            id: Uuid::new_v4().to_string(),
                                            role: Role::User,
                                            content: vec![ContentBlock::Text { text: cmd.clone() }],
                                            created_at: chrono::Utc::now().timestamp(),
                                        });
                                        state.scroll = 1000;
                                        let req = serde_json::json!({
                                            "jsonrpc": "2.0",
                                            "id": Uuid::new_v4().to_string(),
                                            "method": "session.prompt",
                                            "params": {
                                                "session_id": session_id.to_string(),
                                                "prompt": cmd,
                                                "delivery": "steer"
                                            }
                                        });
                                        let _ = outgoing_tx.send(req).await;
                                    }
                                }
                                KeyCode::Esc => {
                                    let req = serde_json::json!({
                                        "jsonrpc": "2.0",
                                        "id": Uuid::new_v4().to_string(),
                                        "method": "session.abort",
                                        "params": {
                                            "session_id": session_id.to_string()
                                        }
                                    });
                                    let _ = outgoing_tx.send(req).await;
                                    state.streaming_text.clear();
                                    state.streaming_reasoning.clear();
                                    state.streaming_tool_uses.clear();
                                }
                                KeyCode::Char(c) => {
                                    state.input_text.push(c);
                                }
                                KeyCode::Backspace => {
                                    state.input_text.pop();
                                }
                                KeyCode::Up if state.scroll > 0 => {
                                    state.scroll -= 1;
                                }
                                KeyCode::Down => {
                                    state.scroll += 1;
                                }
                                _ => {}
                            }
                        }
                    }
            }

            // Read events from WS control channel
            Some(control_event) = control_rx.recv() => {
                match control_event {
                    TuiControlEvent::WsConnected => {
                        state.connected = true;
                        // Catch up on anything persisted while we were disconnected.
                        reload_messages(&pool, session_id, &mut state).await;
                    }
                    TuiControlEvent::WsDisconnected => {
                        state.connected = false;
                    }
                    TuiControlEvent::ProtocolEvent(event) => {
                        match event {
                            ProtocolEvent::MessageDelta { delta, .. } => {
                                match delta {
                                    DeltaPayload::Text { text } => {
                                        state.streaming_text.push_str(&text);
                                    }
                                    DeltaPayload::Reasoning { reasoning } => {
                                        state.streaming_reasoning.push_str(&reasoning);
                                    }
                                    DeltaPayload::ToolUse { id, name, input_delta } => {
                                        if let Some(t) = state.streaming_tool_uses.iter_mut().find(|(tid, _, _)| tid == &id) {
                                            t.2.push_str(&input_delta);
                                        } else {
                                            state.streaming_tool_uses.push((id, name, input_delta));
                                        }
                                    }
                                }
                            }
                            ProtocolEvent::MessageCompleted { usage, .. } => {
                                // The assistant message (and the user prompt) are now durably
                                // persisted; clear the live stream buffers and reload the
                                // authoritative list from the DB.
                                state.streaming_text.clear();
                                state.streaming_reasoning.clear();
                                state.streaming_tool_uses.clear();
                                state.usage = usage;
                                reload_messages(&pool, session_id, &mut state).await;
                            }
                            ProtocolEvent::ToolOutput { .. } => {
                                // Tool result persisted — reconcile from the DB.
                                reload_messages(&pool, session_id, &mut state).await;
                            }
                            ProtocolEvent::ToolPermissionRequired { permission_id, tool_name, action, resources, preview, .. } => {
                                state.active_prompt = Some(PermissionPrompt {
                                    permission_id,
                                    tool_name,
                                    action,
                                    resources,
                                    preview,
                                });
                            }
                            ProtocolEvent::UsageUpdated { usage, .. } => {
                                state.usage = usage;
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
    }

    Ok(())
}

async fn handle_slash_command(
    state: &mut AppState,
    cmd: &str,
    pool: &sqlx::SqlitePool,
    daemon_url: &str,
    token: &str,
    session_id: &str,
    project_id: &str,
) {
    let parts: Vec<&str> = cmd.split_whitespace().collect();
    if parts.is_empty() {
        return;
    }

    match parts[0] {
        "/model" if parts.len() > 1 => {
            let model_id = parts[1];
            let model_cfg = serde_json::json!({
                "provider_id": "anthropic",
                "model_id": model_id
            })
            .to_string();

            let _ = sqlx::query("UPDATE session SET model_config = ?1 WHERE id = ?2")
                .bind(&model_cfg)
                .bind(session_id)
                .execute(pool)
                .await;

            state.current_model = model_id.to_string();
        }
        "/agent" if parts.len() > 1 => {
            let agent_id = parts[1];
            let _ = sqlx::query("UPDATE session SET agent_id = ?1 WHERE id = ?2")
                .bind(agent_id)
                .bind(session_id)
                .execute(pool)
                .await;

            state.current_agent = agent_id.to_string();
        }
        "/revert" => {
            let client = reqwest::Client::new();
            let url = format!(
                "{}/project/{}/session/{}/revert",
                daemon_url, project_id, session_id
            );
            if let Ok(res) = client.post(&url).bearer_auth(token).send().await
                && res.status().is_success()
            {
                // Re-trigger history load
                state.messages.clear();
                state.latest_seq.store(0, Ordering::Relaxed);
            }
        }
        "/compact" => {
            let client = reqwest::Client::new();
            let url = format!(
                "{}/project/{}/session/{}/compact",
                daemon_url, project_id, session_id
            );
            if let Ok(res) = client.post(&url).bearer_auth(token).send().await
                && res.status().is_success()
            {
                // Re-trigger history load
                state.messages.clear();
                state.latest_seq.store(0, Ordering::Relaxed);
            }
        }
        _ => {}
    }
}

fn parse_inline_markdown(text: &str) -> Line<'static> {
    let mut spans = Vec::new();
    let mut current = String::new();
    let mut chars = text.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '`' {
            if !current.is_empty() {
                spans.push(Span::raw(std::mem::take(&mut current)));
            }
            let mut code = String::new();
            while let Some(&nc) = chars.peek() {
                if nc == '`' {
                    chars.next();
                    break;
                }
                code.push(chars.next().unwrap());
            }
            spans.push(Span::styled(code, Style::default().fg(Color::Yellow)));
        } else if c == '*' {
            if chars.peek() == Some(&'*') {
                chars.next();
                if !current.is_empty() {
                    spans.push(Span::raw(std::mem::take(&mut current)));
                }
                let mut bold_text = String::new();
                while let Some(&nc) = chars.peek() {
                    if nc == '*' {
                        chars.next();
                        if chars.peek() == Some(&'*') {
                            chars.next();
                            break;
                        }
                        bold_text.push('*');
                    } else {
                        bold_text.push(chars.next().unwrap());
                    }
                }
                spans.push(Span::styled(
                    bold_text,
                    Style::default().add_modifier(Modifier::BOLD),
                ));
            } else {
                if !current.is_empty() {
                    spans.push(Span::raw(std::mem::take(&mut current)));
                }
                let mut italic_text = String::new();
                while let Some(&nc) = chars.peek() {
                    if nc == '*' {
                        chars.next();
                        break;
                    }
                    italic_text.push(chars.next().unwrap());
                }
                spans.push(Span::styled(
                    italic_text,
                    Style::default().add_modifier(Modifier::ITALIC),
                ));
            }
        } else {
            current.push(c);
        }
    }
    if !current.is_empty() {
        spans.push(Span::raw(current));
    }
    Line::from(spans)
}

fn render_markdown(text: &str) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut in_code_block = false;

    for raw_line in text.lines() {
        let trimmed = raw_line.trim();
        if let Some(rest) = trimmed.strip_prefix("```") {
            in_code_block = !in_code_block;
            let label = if in_code_block {
                format!("── {} ──", rest.trim())
            } else {
                "─────────────────".to_string()
            };
            lines.push(Line::from(vec![Span::styled(
                label,
                Style::default().fg(Color::Cyan).add_modifier(Modifier::DIM),
            )]));
            continue;
        }

        if in_code_block {
            lines.push(Line::from(vec![Span::styled(
                raw_line.to_string(),
                Style::default().fg(Color::LightGreen),
            )]));
        } else {
            lines.push(parse_inline_markdown(raw_line));
        }
    }
    lines
}

fn draw_ui(f: &mut ratatui::Frame, state: &AppState) {
    let size = f.size();

    // 1. Overall Layout
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(3)])
        .split(size);

    let main_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(if state.show_sidebar {
            [Constraint::Percentage(75), Constraint::Percentage(25)]
        } else {
            [Constraint::Percentage(100), Constraint::Min(0)]
        })
        .split(chunks[0]);

    // 2. Render Left Panel (Chat History)
    let mut chat_lines = Vec::new();
    for msg in &state.messages {
        let prefix = match msg.role {
            Role::User => Span::styled(
                "[User]: ",
                Style::default()
                    .fg(Color::LightBlue)
                    .add_modifier(Modifier::BOLD),
            ),
            Role::Assistant => Span::styled(
                "[Agent]: ",
                Style::default()
                    .fg(Color::LightGreen)
                    .add_modifier(Modifier::BOLD),
            ),
            Role::System => Span::styled(
                "[System]: ",
                Style::default()
                    .fg(Color::LightRed)
                    .add_modifier(Modifier::DIM),
            ),
        };

        for block in &msg.content {
            match block {
                ContentBlock::Text { text } => {
                    chat_lines.push(Line::from(vec![prefix.clone()]));
                    chat_lines.extend(render_markdown(text));
                    chat_lines.push(Line::from(""));
                }
                ContentBlock::Reasoning { reasoning } => {
                    chat_lines.push(Line::from(vec![Span::styled(
                        "Thought:",
                        Style::default()
                            .fg(Color::Magenta)
                            .add_modifier(Modifier::ITALIC),
                    )]));
                    chat_lines.extend(render_markdown(reasoning));
                    chat_lines.push(Line::from(""));
                }
                ContentBlock::ToolUse { id: _, name, input } => {
                    chat_lines.push(Line::from(vec![
                        Span::styled("Tool Request: ", Style::default().fg(Color::Cyan)),
                        Span::styled(
                            name,
                            Style::default()
                                .fg(Color::Cyan)
                                .add_modifier(Modifier::BOLD),
                        ),
                    ]));
                    chat_lines.push(Line::from(
                        serde_json::to_string_pretty(input).unwrap_or_default(),
                    ));
                    chat_lines.push(Line::from(""));
                }
                ContentBlock::ToolResult {
                    tool_use_id: _,
                    content,
                    is_error,
                } => {
                    let style = if *is_error {
                        Style::default().fg(Color::LightRed)
                    } else {
                        Style::default().fg(Color::Cyan)
                    };
                    chat_lines.push(Line::from(vec![Span::styled("Tool Output:", style)]));
                    let content_str = match content {
                        ToolResultContent::Text(t) => t.clone(),
                        ToolResultContent::Json(val) => {
                            serde_json::to_string_pretty(val).unwrap_or_default()
                        }
                    };
                    chat_lines.extend(render_markdown(&content_str));
                    chat_lines.push(Line::from(""));
                }
            }
        }
    }

    // Render streaming assistant response if active
    if !state.streaming_text.is_empty()
        || !state.streaming_reasoning.is_empty()
        || !state.streaming_tool_uses.is_empty()
    {
        chat_lines.push(Line::from(vec![Span::styled(
            "[Agent is thinking/streaming...]",
            Style::default()
                .fg(Color::LightGreen)
                .add_modifier(Modifier::BOLD),
        )]));
        if !state.streaming_reasoning.is_empty() {
            chat_lines.push(Line::from(vec![Span::styled(
                "Thought:",
                Style::default()
                    .fg(Color::Magenta)
                    .add_modifier(Modifier::ITALIC),
            )]));
            chat_lines.extend(render_markdown(&state.streaming_reasoning));
        }
        if !state.streaming_text.is_empty() {
            chat_lines.extend(render_markdown(&state.streaming_text));
        }
        for (_, name, input) in &state.streaming_tool_uses {
            chat_lines.push(Line::from(vec![
                Span::styled("Tool Request: ", Style::default().fg(Color::Cyan)),
                Span::styled(
                    name,
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
            ]));
            chat_lines.push(Line::from(input.clone()));
        }
        chat_lines.push(Line::from(""));
    }

    let conn_status_title = if state.connected {
        " Private Code Chat (Connected) "
    } else {
        " Private Code Chat (DISCONNECTED - RECONNECTING...) "
    };

    let chat_paragraph = Paragraph::new(Text::from(chat_lines))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(conn_status_title),
        )
        .wrap(Wrap { trim: true })
        .scroll((state.scroll, 0));

    f.render_widget(chat_paragraph, main_chunks[0]);

    // 3. Render Right Panel (Sidebar)
    if state.show_sidebar {
        let mut sidebar_lines = Vec::new();
        sidebar_lines.push(Line::from(vec![
            Span::styled(
                "Session ID: ",
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw(&state.session_id[..8.min(state.session_id.len())]),
        ]));
        sidebar_lines.push(Line::from(vec![
            Span::styled(
                "Active Agent: ",
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled(&state.current_agent, Style::default().fg(Color::LightGreen)),
        ]));
        sidebar_lines.push(Line::from(vec![
            Span::styled(
                "Active Model: ",
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled(&state.current_model, Style::default().fg(Color::Yellow)),
        ]));
        sidebar_lines.push(Line::from(""));
        sidebar_lines.push(Line::from(Span::styled(
            "Token Usage:",
            Style::default().add_modifier(Modifier::BOLD),
        )));
        sidebar_lines.push(Line::from(format!(
            "  Input:  {}",
            state.usage.input_tokens
        )));
        sidebar_lines.push(Line::from(format!(
            "  Output: {}",
            state.usage.output_tokens
        )));
        sidebar_lines.push(Line::from(format!(
            "  Reason: {}",
            state.usage.reasoning_tokens
        )));
        sidebar_lines.push(Line::from(format!(
            "  C-Read: {}",
            state.usage.cache_read_tokens
        )));
        sidebar_lines.push(Line::from(format!(
            "  C-Write: {}",
            state.usage.cache_write_tokens
        )));
        sidebar_lines.push(Line::from(""));
        sidebar_lines.push(Line::from(vec![
            Span::styled(
                "Total Cost: ",
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("${:.4}", state.usage.cost),
                Style::default().fg(Color::LightGreen),
            ),
        ]));
        sidebar_lines.push(Line::from(""));
        sidebar_lines.push(Line::from(Span::styled(
            "Shortcuts:",
            Style::default().add_modifier(Modifier::BOLD),
        )));
        sidebar_lines.push(Line::from("  Ctrl-D : Toggle Sidebar"));
        sidebar_lines.push(Line::from("  Ctrl-C : Quit"));
        sidebar_lines.push(Line::from("  Esc    : Cancel turn"));
        sidebar_lines.push(Line::from("  Up/Dn  : Scroll chat"));
        sidebar_lines.push(Line::from(""));
        sidebar_lines.push(Line::from(Span::styled(
            "Commands:",
            Style::default().add_modifier(Modifier::BOLD),
        )));
        sidebar_lines.push(Line::from("  /model <name>"));
        sidebar_lines.push(Line::from("  /agent <name>"));
        sidebar_lines.push(Line::from("  /revert"));
        sidebar_lines.push(Line::from("  /compact"));

        let sidebar_paragraph = Paragraph::new(Text::from(sidebar_lines))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Session Info "),
            )
            .wrap(Wrap { trim: true });

        f.render_widget(sidebar_paragraph, main_chunks[1]);
    }

    // 4. Render Bottom Input Bar
    let input_block = Paragraph::new(format!("> {}", state.input_text)).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Input prompt or command "),
    );
    f.render_widget(input_block, chunks[1]);

    // 5. Render Permission Modal (if active)
    if let Some(ref prompt) = state.active_prompt {
        let modal_area = centered_rect(60, 40, size);
        f.render_widget(Clear, modal_area);

        let mut modal_lines = vec![Line::from(vec![Span::styled(
            "Security Check Required",
            Style::default()
                .fg(Color::LightRed)
                .add_modifier(Modifier::BOLD),
        )])];
        modal_lines.push(Line::from(""));
        modal_lines.push(Line::from(vec![
            Span::styled("Tool Name: ", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(&prompt.tool_name),
        ]));
        modal_lines.push(Line::from(vec![
            Span::styled(
                "Permission: ",
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw(&prompt.action),
        ]));
        modal_lines.push(Line::from(vec![
            Span::styled("Resource: ", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(
                prompt
                    .resources
                    .first()
                    .cloned()
                    .unwrap_or_else(|| "*".to_string()),
            ),
        ]));
        modal_lines.push(Line::from(""));
        modal_lines.push(Line::from(Span::styled(
            "Arguments:",
            Style::default().add_modifier(Modifier::BOLD),
        )));
        modal_lines.extend(render_markdown(&prompt.preview));
        modal_lines.push(Line::from(""));
        modal_lines.push(Line::from(vec![Span::styled(
            "Decision: [y]es / [n]o / [a]lways allow for this project",
            Style::default().fg(Color::Yellow),
        )]));

        let modal_block = Paragraph::new(Text::from(modal_lines))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Gated Tool Authorization "),
            )
            .wrap(Wrap { trim: true });

        f.render_widget(modal_block, modal_area);
    }
}

fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}
