use anyhow::Result;
use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as BASE64_URL_SAFE_NO_PAD;
use chrono::{DateTime, SecondsFormat, Utc};
use microvibe_config::Config;
use microvibe_core::{ApprovalRequest, RunLimits, Session};
use microvibe_protocol::{
    AgentEvent, ApprovalDecision, ImageAttachment, ImageSource, Role, ToolCall, ToolResult,
    ToolSpec, Usage,
};
use microvibe_tools::Tool;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};

const VIBE_ACP_VERSION: &str = "2.17.1";
const MAX_IMAGE_BYTES: usize = 10 * 1024 * 1024;
const MAX_IMAGES_PER_MESSAGE: usize = 8;
const PROXY_VARS: [(&str, &str); 6] = [
    ("HTTP_PROXY", "Proxy URL for HTTP requests"),
    ("HTTPS_PROXY", "Proxy URL for HTTPS requests"),
    ("ALL_PROXY", "Proxy URL for all requests (fallback)"),
    ("NO_PROXY", "Comma-separated list of hosts to bypass proxy"),
    ("SSL_CERT_FILE", "Path to custom SSL certificate file"),
    (
        "SSL_CERT_DIR",
        "Path to directory containing SSL certificates",
    ),
];
const DATA_RETENTION_MESSAGE: &str = "## Your Data Helps Improve Mistral AI\n\nAt Mistral AI, we're committed to delivering the best possible experience. When you use Mistral models on our API, your interactions may be collected to improve our models, ensuring they stay cutting-edge, accurate, and helpful.\n\nManage your data settings [here](https://admin.mistral.ai/plateforme/privacy)";
const VIBE_ACP_USAGE: &str = r#"usage: vibe-acp [-h] [-v] [--setup]
"#;
const VIBE_ACP_HELP: &str = r#"usage: vibe-acp [-h] [-v] [--setup]

Run Mistral Vibe in ACP mode

options:
  -h, --help     show this help message and exit
  -v, --version  show program's version number and exit
  --setup        Setup API key and exit
"#;

fn main() -> Result<()> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.iter().any(|arg| arg == "-h" || arg == "--help") {
        print!("{VIBE_ACP_HELP}");
        return Ok(());
    }
    if args.iter().any(|arg| arg == "-v" || arg == "--version") {
        println!("vibe-acp {VIBE_ACP_VERSION}");
        return Ok(());
    }
    if args.iter().any(|arg| arg == "--setup") {
        println!("Mistral Vibe setup is available through `vibe --setup`.");
        return Ok(());
    }
    if let Some(arg) = args.first() {
        eprintln!("{VIBE_ACP_USAGE}");
        eprintln!("vibe-acp: error: unrecognized arguments: {arg}");
        std::process::exit(2);
    }
    run_stdio_server()
}

fn run_stdio_server() -> Result<()> {
    let stdin = io::stdin();
    let mut lines = stdin.lock().lines();
    let mut stdout = io::stdout().lock();
    let mut server = AcpServer::default();
    while let Some(line) = lines.next() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let request = serde_json::from_str::<Value>(&line).unwrap_or(Value::Null);
        if request.get("id").is_none() {
            server.handle_json_rpc_notification(&request);
            continue;
        }
        if request
            .get("method")
            .and_then(Value::as_str)
            .is_some_and(|method| method == "session/prompt")
        {
            server.handle_prompt_json_rpc(request, &mut stdout, &mut lines)?;
            continue;
        }
        for response in server.handle_json_rpc(request) {
            write_json_rpc(&mut stdout, response)?;
        }
    }
    Ok(())
}

#[derive(Default)]
struct AcpServer {
    live_sessions: HashMap<String, Session>,
    session_modes: HashMap<String, String>,
    session_models: HashMap<String, String>,
    session_thinking: HashMap<String, String>,
    session_max_turns: HashMap<String, u32>,
    session_visible_usage: HashMap<String, Usage>,
    client_fs_read_text_file: bool,
    client_fs_write_text_file: bool,
    client_terminal: bool,
    pending_browser_sign_in_attempts: HashMap<String, PendingBrowserSignInAttempt>,
}

#[derive(Debug, Clone)]
struct PendingBrowserSignInAttempt {
    process_id: String,
    poll_url: String,
    code_verifier: String,
    provider: microvibe_config::ProviderConfig,
}

#[derive(Debug, Clone)]
struct BrowserAuthProvider {
    provider: microvibe_config::ProviderConfig,
    browser_base_url: String,
    api_base_url: String,
}

#[derive(Debug, Clone)]
struct BrowserSignInProcess {
    process_id: String,
    sign_in_url: String,
    poll_url: String,
    expires_at: String,
    code_verifier: String,
}

impl AcpServer {
    fn handle_prompt_json_rpc(
        &mut self,
        request: Value,
        stdout: &mut dyn Write,
        lines: &mut std::io::Lines<std::io::StdinLock<'_>>,
    ) -> Result<()> {
        let id = request.get("id").cloned().unwrap_or(Value::Null);
        let params = request.get("params").unwrap_or(&Value::Null);
        self.prompt_interactive(id, params, stdout, lines)
    }

    fn handle_json_rpc(&mut self, request: Value) -> Vec<Value> {
        let id = request.get("id").cloned().unwrap_or(Value::Null);
        let method = request
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let params = request.get("params").unwrap_or(&Value::Null);
        let response = match method {
            "initialize" => {
                self.client_fs_read_text_file = client_supports_fs_read(params);
                self.client_fs_write_text_file = client_supports_fs_write(params);
                self.client_terminal = client_supports_terminal(params);
                ok(id, initialize_result(params))
            }
            "session/new" => ok(id, self.new_session(params)),
            "authenticate" => self.authenticate(id, params),
            "session/list" => match list_sessions(params) {
                Ok(result) => ok(id, result),
                Err(error) => invalid_request(id, &error.to_string()),
            },
            "session/load" => return self.load_session(id, params),
            "session/fork" => self.fork_session(id, params),
            "session/prompt" => return self.prompt(id, params),
            "session/close" => self.close_session(id, params),
            "session/set_mode" => self.set_session_mode(id, params),
            "session/set_model" => self.set_session_model(id, params),
            "session/set_config_option" => self.set_config_option(id, params),
            "_session/set_title" => return self.set_title(id, params),
            "_session/delete" => self.delete_session(id, params),
            "_auth/status" => self.auth_status(id),
            "_auth/signOut" => self.auth_sign_out(id),
            "_trust/status" => self.workspace_trust_status(id, params),
            "_trust/decision" => self.workspace_trust_decision(id, params),
            _ => method_not_found(id, method),
        };
        vec![response]
    }

    fn handle_json_rpc_notification(&mut self, request: &Value) {
        let _method = request
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let _params = request.get("params").unwrap_or(&Value::Null);
    }

    fn new_session(&mut self, params: &Value) -> Value {
        let cwd = params
            .get("cwd")
            .and_then(Value::as_str)
            .map(resolve_path)
            .unwrap_or_else(current_dir_string);
        let _ = std::env::set_current_dir(&cwd);
        bootstrap_vibe_home();

        let config = Config::load().unwrap_or_else(|_| default_test_safe_config());
        let session = Session::new(config.clone());
        let session_id = session.id.0.clone();
        self.session_modes
            .insert(session_id.clone(), "default".to_string());
        self.session_models
            .insert(session_id.clone(), current_model_alias(&config));
        self.session_thinking
            .insert(session_id.clone(), current_thinking(&config));
        self.live_sessions.insert(session_id.clone(), session);
        json!({
            "_meta": {
                "workspace_trust": {
                    "status": workspace_trust_status_string(Path::new(&cwd)),
                    "details": workspace_trust_details(Path::new(&cwd)),
                },
            },
            "configOptions": config_options_for_state(&config, "default", &current_model_alias(&config), &current_thinking(&config)),
            "models": models_state_for_current(&config, &current_model_alias(&config)),
            "modes": modes_state_for("default"),
            "sessionId": session_id,
        })
    }

    fn authenticate(&mut self, id: Value, params: &Value) -> Value {
        let method_id = params
            .get("methodId")
            .or_else(|| params.get("method_id"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        match method_id {
            "browser-auth" => self.authenticate_browser_auth(id, params),
            "browser-auth-delegated" => self.authenticate_delegated_browser_auth(id, params),
            _ => {
                invalid_request_with_null_data(id, &format!("Unsupported auth method: {method_id}"))
            }
        }
    }

    fn authenticate_browser_auth(&mut self, id: Value, params: &Value) -> Value {
        let action = auth_param(params, "action");
        if !matches!(action, None | Some("start")) {
            return invalid_request_with_null_data(
                id,
                &format!(
                    "Unsupported browser auth action: {}",
                    action.unwrap_or_default()
                ),
            );
        }
        let Some(provider) = current_browser_auth_provider() else {
            return invalid_request_with_null_data(
                id,
                "Browser sign-in is not available for the configured provider.",
            );
        };
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(error) => return internal_error(id, &error.to_string()),
        };
        let result = runtime.block_on(async {
            let attempt = start_browser_sign_in_attempt(&provider).await?;
            open_browser_for_sign_in(&attempt.sign_in_url)?;
            let api_key = complete_started_browser_sign_in_attempt(&provider, &attempt).await?;
            persist_browser_auth_api_key(&provider.provider, &api_key)
        });
        match result {
            Ok(persist_result) => ok(
                id,
                json!({
                    "_meta": {
                        "browser-auth": {
                            "persistResult": persist_result,
                            "status": "completed",
                        },
                    },
                }),
            ),
            Err(error) => internal_error(id, &error),
        }
    }

    fn authenticate_delegated_browser_auth(&mut self, id: Value, params: &Value) -> Value {
        let action = auth_param(params, "action").unwrap_or("start");
        match action {
            "start" => self.start_delegated_browser_auth(id),
            "complete" => self.complete_delegated_browser_auth(id, params),
            _ => invalid_request_with_null_data(
                id,
                &format!("Unsupported delegated browser auth action: {action}"),
            ),
        }
    }

    fn start_delegated_browser_auth(&mut self, id: Value) -> Value {
        let Some(provider) = current_browser_auth_provider() else {
            return invalid_request_with_null_data(
                id,
                "Browser sign-in is not available for the configured provider.",
            );
        };
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(error) => return internal_error(id, &error.to_string()),
        };
        let result = runtime.block_on(start_browser_sign_in_attempt(&provider));
        let attempt = match result {
            Ok(attempt) => attempt,
            Err(error) => return internal_error(id, &error),
        };
        self.pending_browser_sign_in_attempts.insert(
            attempt.process_id.clone(),
            PendingBrowserSignInAttempt {
                process_id: attempt.process_id.clone(),
                poll_url: attempt.poll_url.clone(),
                code_verifier: attempt.code_verifier,
                provider: provider.provider,
            },
        );
        ok(
            id,
            json!({
                "_meta": {
                    "browser-auth-delegated": {
                        "attemptId": attempt.process_id,
                        "expiresAt": attempt.expires_at,
                        "signInUrl": attempt.sign_in_url,
                    },
                },
            }),
        )
    }

    fn complete_delegated_browser_auth(&mut self, id: Value, params: &Value) -> Value {
        let attempt_id = params
            .get("attemptId")
            .or_else(|| params.get("attempt_id"))
            .or_else(|| params.pointer("/_meta/attemptId"))
            .or_else(|| params.pointer("/_meta/attempt_id"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        if attempt_id.is_empty() {
            return invalid_request_with_null_data(id, "Missing browser sign-in attempt ID.");
        }
        let Some(pending) = self
            .pending_browser_sign_in_attempts
            .get(attempt_id)
            .cloned()
        else {
            return invalid_request_with_null_data(
                id,
                &format!("Unknown browser sign-in attempt: {attempt_id}"),
            );
        };
        let provider = browser_auth_provider_from_provider(pending.provider.clone());
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(error) => return internal_error(id, &error.to_string()),
        };
        let result = runtime.block_on(async {
            let api_key = complete_pending_browser_sign_in_attempt(&provider, &pending).await?;
            persist_browser_auth_api_key(&provider.provider, &api_key)
        });
        match result {
            Ok(persist_result) => {
                self.pending_browser_sign_in_attempts.remove(attempt_id);
                ok(
                    id,
                    json!({
                        "_meta": {
                            "browser-auth-delegated": {
                                "attemptId": attempt_id,
                                "persistResult": persist_result,
                                "status": "completed",
                            },
                        },
                    }),
                )
            }
            Err(error) => invalid_request_with_null_data(id, &error),
        }
    }

    fn close_session(&mut self, id: Value, params: &Value) -> Value {
        let session_id = params
            .get("sessionId")
            .or_else(|| params.get("session_id"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        if self.live_sessions.remove(session_id).is_none() {
            return json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {
                    "code": -32602,
                    "message": format!("Session not found: {session_id}"),
                    "data": { "session_id": session_id },
                },
            });
        }
        self.session_modes.remove(session_id);
        self.session_models.remove(session_id);
        self.session_thinking.remove(session_id);
        ok(id, json!({}))
    }

    fn fork_session(&mut self, id: Value, params: &Value) -> Value {
        let source_session_id = params
            .get("sessionId")
            .or_else(|| params.get("session_id"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let message_id = params
            .get("messageId")
            .or_else(|| params.get("message_id"))
            .and_then(Value::as_str)
            .map(ToString::to_string);
        let cwd = params
            .get("cwd")
            .and_then(Value::as_str)
            .map(resolve_path)
            .unwrap_or_else(current_dir_string);
        let _ = std::env::set_current_dir(&cwd);
        bootstrap_vibe_home();

        let Some(source_session) = self.live_sessions.get(&source_session_id) else {
            return session_not_found(id, &source_session_id);
        };
        let mode_id = self
            .session_modes
            .get(&source_session_id)
            .cloned()
            .unwrap_or_else(|| "default".to_string());
        if mode_id == "chat" {
            return invalid_request_with_null_data(id, "Agent 'chat' not found.");
        }
        let source_config = source_session.agent_config();
        let model_id = self
            .session_models
            .get(&source_session_id)
            .cloned()
            .unwrap_or_else(|| current_model_alias(&source_config));
        let thinking = self
            .session_thinking
            .get(&source_session_id)
            .cloned()
            .unwrap_or_else(|| current_thinking(&source_config));

        let mut config = Config::load().unwrap_or_else(|_| default_test_safe_config());
        config.default_agent = mode_id.clone();
        config.bypass_tool_permissions = matches!(mode_id.as_str(), "auto-approve" | "chat");
        config.active_model = Some(model_id.clone());
        if let Some(model) = config.models.iter().find(|model| model.alias == model_id) {
            config.model.name = model.name.clone();
            config.model.provider = model.provider.clone();
        }
        let fork_result = match message_id.as_deref() {
            Some(message_id) => {
                Session::fork_from_message_id(config.clone(), source_session, message_id)
            }
            None => Session::fork_from(config.clone(), source_session),
        };
        let mut forked = match fork_result {
            Ok(session) => session,
            Err(error) => return invalid_request(id, &error.to_string()),
        };
        let forked_session_id = forked.id.0.clone();
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(error) => return invalid_request(id, &error.to_string()),
        };
        if let Err(error) = runtime.block_on(forked.save()) {
            return invalid_request(id, &error.to_string());
        }
        self.session_modes
            .insert(forked_session_id.clone(), mode_id.clone());
        self.session_models
            .insert(forked_session_id.clone(), model_id.clone());
        self.session_thinking
            .insert(forked_session_id.clone(), thinking.clone());
        self.live_sessions.insert(forked_session_id.clone(), forked);
        ok(
            id,
            json!({
                "configOptions": config_options_for_state(&config, &mode_id, &model_id, &thinking),
                "models": models_state_for_current(&config, &model_id),
                "modes": modes_state_for(&mode_id),
                "sessionId": forked_session_id,
            }),
        )
    }

    fn set_session_mode(&mut self, id: Value, params: &Value) -> Value {
        let session_id = params
            .get("sessionId")
            .or_else(|| params.get("session_id"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let mode_id = params
            .get("modeId")
            .or_else(|| params.get("mode_id"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if !self.live_sessions.contains_key(&session_id) {
            return session_not_found(id, &session_id);
        }
        if !is_valid_mode(&mode_id) {
            return ok(id, json!({}));
        }
        if let Err(error) = self.apply_session_mode(&session_id, &mode_id) {
            return invalid_request(id, &error.to_string());
        }
        ok(id, json!({}))
    }

    fn set_session_model(&mut self, id: Value, params: &Value) -> Value {
        let session_id = params
            .get("sessionId")
            .or_else(|| params.get("session_id"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let model_id = params
            .get("modelId")
            .or_else(|| params.get("model_id"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if !self.live_sessions.contains_key(&session_id) {
            return session_not_found(id, &session_id);
        }
        let config = Config::load().unwrap_or_else(|_| default_test_safe_config());
        if !is_valid_model(&config, &model_id) {
            return ok(id, json!({}));
        }
        if let Err(error) = self.apply_session_model(&session_id, &model_id) {
            return invalid_request(id, &error.to_string());
        }
        ok(id, json!({}))
    }

    fn set_config_option(&mut self, id: Value, params: &Value) -> Value {
        let session_id = params
            .get("sessionId")
            .or_else(|| params.get("session_id"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let config_id = params
            .get("configId")
            .or_else(|| params.get("config_id"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        let value = params.get("value").unwrap_or(&Value::Null);
        if !value.is_string() {
            return invalid_params_string_type(id, "value", value.clone());
        }
        if !self.live_sessions.contains_key(&session_id) {
            return session_not_found(id, &session_id);
        }
        let config = Config::load().unwrap_or_else(|_| default_test_safe_config());
        let success = match config_id {
            "mode" => value
                .as_str()
                .filter(|mode| is_valid_mode(mode))
                .map(|mode| self.apply_session_mode(&session_id, mode).is_ok())
                .unwrap_or(false),
            "model" => value
                .as_str()
                .filter(|model| is_valid_model(&config, model))
                .map(|model| self.apply_session_model(&session_id, model).is_ok())
                .unwrap_or(false),
            "thinking" => match value
                .as_str()
                .filter(|thinking| is_valid_thinking(thinking))
            {
                Some(thinking) => {
                    if let Err(error) = Config::save_thinking(thinking) {
                        return invalid_request(id, &error.to_string());
                    }
                    self.session_thinking
                        .insert(session_id.clone(), thinking.to_string());
                    true
                }
                None => false,
            },
            "max_turns" => value
                .as_str()
                .and_then(|raw| raw.parse::<u32>().ok())
                .map(|max_turns| {
                    self.session_max_turns.insert(session_id.clone(), max_turns);
                    true
                })
                .unwrap_or(false),
            _ => false,
        };
        if !success {
            return ok(id, json!({}));
        }
        let mode = self.session_mode(&session_id, &config);
        let model = self.session_model(&session_id, &config);
        let thinking = self.session_thinking(&session_id, &config);
        ok(
            id,
            json!({ "configOptions": config_options_for_state(&config, &mode, &model, &thinking) }),
        )
    }

    fn set_title(&mut self, id: Value, params: &Value) -> Vec<Value> {
        let session_id = match required_non_empty_param(params, "sessionId") {
            Ok(session_id) => session_id,
            Err(error) => return vec![invalid_request(id, &error)],
        };
        let title = match required_non_empty_param(params, "title") {
            Ok(title) => title,
            Err(error) => return vec![invalid_request(id, &error)],
        };

        let mut update_session_id = session_id.clone();
        let mut updated_at = None;
        if let Some(live_key) = self.find_live_session_key(&session_id) {
            let Some(session) = self.live_sessions.get_mut(&live_key) else {
                return vec![session_not_found(id, &session_id)];
            };
            update_session_id = live_key;
            if let Err(error) = session.store.rename(&title) {
                return vec![invalid_request(id, &error.to_string())];
            }
            if let Ok(metadata) = read_saved_metadata(&session.store.session_dir) {
                updated_at = metadata
                    .get("end_time")
                    .and_then(Value::as_str)
                    .map(ToString::to_string);
            }
        } else {
            let Some(session_dir) = find_saved_session_dir_exact(&session_id) else {
                return vec![session_not_found(id, &session_id)];
            };
            let metadata = match update_saved_session_title(&session_dir, &title) {
                Ok(metadata) => metadata,
                Err(error) => return vec![invalid_request(id, &error.to_string())],
            };
            updated_at = metadata
                .get("end_time")
                .and_then(Value::as_str)
                .map(ToString::to_string);
        }

        let mut info_update = json!({
            "sessionUpdate": "session_info_update",
            "title": title,
        });
        if let Some(updated_at) = updated_at {
            info_update["updatedAt"] = Value::String(updated_at);
        }

        vec![
            session_update(&update_session_id, info_update),
            ok(id, json!({})),
        ]
    }

    fn delete_session(&mut self, id: Value, params: &Value) -> Value {
        let session_id = match required_delete_session_id_param(params) {
            Ok(session_id) => session_id,
            Err(error) => return invalid_request_with_null_data(id, &error),
        };
        if let Some(live_key) = self.find_live_session_key(&session_id) {
            let saved_dir = self
                .live_sessions
                .get(&live_key)
                .map(|session| session.store.session_dir.clone());
            self.live_sessions.remove(&live_key);
            self.session_modes.remove(&live_key);
            self.session_models.remove(&live_key);
            self.session_thinking.remove(&live_key);
            if let Some(saved_dir) = saved_dir.filter(|path| path.join("meta.json").is_file())
                && let Err(error) = std::fs::remove_dir_all(&saved_dir)
            {
                return invalid_request(id, &error.to_string());
            }
            clear_last_session_pointers(&session_id);
            return ok(id, json!({}));
        }

        if let Some(session_dir) = find_saved_session_dir_exact(&session_id) {
            if let Err(error) = std::fs::remove_dir_all(&session_dir) {
                return invalid_request(id, &error.to_string());
            }
            clear_last_session_pointers(&session_id);
        }
        ok(id, json!({}))
    }

    fn auth_status(&self, id: Value) -> Value {
        ok(id, auth_status_response(&assess_auth_state()))
    }

    fn auth_sign_out(&self, id: Value) -> Value {
        let state = assess_auth_state();
        if !state.sign_out_available {
            return invalid_request_with_null_data(
                id,
                &format!("Sign out is not available for auth state: {}", state.kind),
            );
        }
        let Some(env_key) = state.env_key else {
            return invalid_request_with_null_data(
                id,
                "Sign out is not available for auth state: auth_not_required",
            );
        };
        if let Err(error) = remove_dotenv_key(&env_key) {
            return invalid_request(id, &format!("Failed to sign out: {error}"));
        }
        ok(id, json!({}))
    }

    fn workspace_trust_status(&self, id: Value, params: &Value) -> Value {
        let cwd = params
            .get("cwd")
            .and_then(Value::as_str)
            .map(resolve_path)
            .unwrap_or_else(current_dir_string);
        ok(id, workspace_trust_response(Path::new(&cwd)))
    }

    fn workspace_trust_decision(&mut self, id: Value, params: &Value) -> Value {
        let cwd = params
            .get("cwd")
            .and_then(Value::as_str)
            .map(resolve_path)
            .unwrap_or_else(current_dir_string);
        let decision = params
            .get("decision")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let session_id = params
            .get("sessionId")
            .or_else(|| params.get("session_id"))
            .and_then(Value::as_str);

        if let Some(session_id) = session_id
            && !self.live_sessions.contains_key(session_id)
        {
            return session_not_found(id, session_id);
        }

        let prompt = workspace_trust_prompt(Path::new(&cwd), true);
        let Some(prompt) = prompt else {
            return invalid_request_with_null_data(id, "No workspace trust decision is available.");
        };
        let available = workspace_trust_available_decisions(&prompt);
        if !available.contains(&decision) {
            return invalid_request_with_null_data(
                id,
                &format!("Unsupported trust decision: {decision}"),
            );
        }

        match decision {
            "trust_repo" => {
                if let Some(repo_root) = &prompt.repo_root
                    && let Err(error) = save_workspace_trust_path(repo_root, true)
                {
                    return invalid_request(id, &error.to_string());
                }
            }
            "trust_cwd" => {
                if let Err(error) = save_workspace_trust_path(&prompt.cwd, true) {
                    return invalid_request(id, &error.to_string());
                }
            }
            "decline" => {
                if let Err(error) = save_workspace_trust_path(&prompt.cwd, false) {
                    return invalid_request(id, &error.to_string());
                }
            }
            _ => {
                return invalid_request_with_null_data(
                    id,
                    &format!("Unsupported trust decision: {decision}"),
                );
            }
        }

        if let Some(session_id) = session_id
            && matches!(decision, "trust_repo" | "trust_cwd")
            && let Some(session) = self.live_sessions.get_mut(session_id)
        {
            let config = Config::load().unwrap_or_else(|_| default_test_safe_config());
            *session = Session::new(config);
        }

        ok(id, workspace_trust_response(Path::new(&cwd)))
    }

    fn load_session(&mut self, id: Value, params: &Value) -> Vec<Value> {
        let requested_session_id = params
            .get("sessionId")
            .or_else(|| params.get("session_id"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        let cwd = params
            .get("cwd")
            .and_then(Value::as_str)
            .map(resolve_path)
            .unwrap_or_else(current_dir_string);
        let _ = std::env::set_current_dir(&cwd);
        bootstrap_vibe_home();

        let Some(session_dir) = find_session_dir(requested_session_id) else {
            return vec![session_not_found(id, requested_session_id)];
        };
        let config = Config::load().unwrap_or_else(|_| default_test_safe_config());
        let session = match Session::resume(config.clone(), session_dir.clone()) {
            Ok(session) => session,
            Err(error) => return vec![invalid_request(id, &error.to_string())],
        };
        let (session_id, messages) = match load_session_messages(&session_dir) {
            Ok(loaded) => loaded,
            Err(error) => return vec![invalid_request(id, &error.to_string())],
        };
        self.session_modes
            .insert(session_id.clone(), "default".to_string());
        self.session_models
            .insert(session_id.clone(), current_model_alias(&config));
        self.session_thinking
            .insert(session_id.clone(), current_thinking(&config));
        self.live_sessions.insert(session_id.clone(), session);

        let mut responses = Vec::new();
        let mut replayed_tool_calls = HashSet::new();
        for message in messages {
            responses.extend(replay_updates(
                &session_id,
                &message,
                &mut replayed_tool_calls,
            ));
        }
        responses.push(ok(
            id,
            json!({
                "_meta": {
                    "workspace_trust": {
                        "status": "untrusted",
                        "details": workspace_trust_details(Path::new(&cwd)),
                    },
                },
                "configOptions": config_options_for_state(&config, "default", &current_model_alias(&config), &current_thinking(&config)),
                "models": models_state_for_current(&config, &current_model_alias(&config)),
                "modes": modes_state_for("default"),
            }),
        ));
        responses.push(session_update(&session_id, load_usage_update()));
        responses
    }

    fn prompt(&mut self, id: Value, params: &Value) -> Vec<Value> {
        let session_id = params
            .get("sessionId")
            .or_else(|| params.get("session_id"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let Some(session) = self.live_sessions.get_mut(&session_id) else {
            return vec![session_not_found(id, &session_id)];
        };
        let raw_prompt = params.get("prompt").unwrap_or(&Value::Null);
        let prompt = prompt_text(raw_prompt);
        let user_message_id = user_message_id_from_params(params);
        let prior_usage = self
            .session_visible_usage
            .get(&session_id)
            .cloned()
            .unwrap_or_else(|| session.agent.cumulative_usage().clone());
        let prior_cumulative_usage = session.agent.cumulative_usage().clone();
        let pricing = session_usage_pricing(session);
        let has_prior_turns = session
            .agent
            .messages()
            .iter()
            .any(|message| message.role != Role::System);
        if let Some(mut responses) =
            handle_builtin_command(id.clone(), &session_id, session, &prompt, &user_message_id)
        {
            if has_prior_turns {
                responses.insert(
                    0,
                    session_update(&session_id, usage_update(&prior_usage, 200_000, pricing)),
                );
            }
            return responses;
        }
        let images = match extract_image_attachments(raw_prompt) {
            Ok(images) => images,
            Err(error) => return vec![invalid_image_attachment(id, &error.message, error.reason)],
        };
        let display_content = match user_display_content_metadata(params) {
            Ok(display_content) => display_content,
            Err(error) => return vec![invalid_request_with_null_data(id, &error)],
        };
        let assistant_message_id = uuid::Uuid::new_v4().to_string();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(error) => return vec![invalid_request(id, &error.to_string())],
        };
        let limits = run_limits_for(self.session_max_turns.get(&session_id).copied());
        let run_result = runtime.block_on(
            session
                .agent
                .run_turn_with_display_content_images_and_limits(
                    prompt.clone(),
                    display_content,
                    images,
                    tx,
                    limits,
                ),
        );
        let _ = runtime.block_on(session.save());

        let mut responses = Vec::new();
        if has_prior_turns {
            responses.push(session_update(
                &session_id,
                usage_update(&prior_usage, 200_000, pricing),
            ));
        } else {
            responses.push(session_update(
                &session_id,
                json!({
                    "title": title_from_prompt(&prompt),
                    "sessionUpdate": "session_info_update",
                }),
            ));
        }
        let mut usage = Usage::default();
        let mut pending_usage_update: Option<Usage> = None;
        let mut last_usage_update = prior_cumulative_usage;
        let mut last_visible_usage = Usage::default();
        while let Ok(event) = rx.try_recv() {
            match event {
                AgentEvent::AssistantDelta { text } => {
                    responses.push(session_update(
                        &session_id,
                        json!({
                            "content": { "text": text, "type": "text" },
                            "messageId": assistant_message_id,
                            "sessionUpdate": "agent_message_chunk",
                        }),
                    ));
                }
                AgentEvent::ThoughtDelta { text, message_id } => {
                    responses.push(session_update(
                        &session_id,
                        json!({
                            "content": { "text": text, "type": "text" },
                            "messageId": message_id,
                            "sessionUpdate": "agent_thought_chunk",
                        }),
                    ));
                }
                AgentEvent::ToolCallStarted { call } => {
                    for update in live_tool_call_updates(&call) {
                        responses.push(session_update(&session_id, update));
                    }
                }
                AgentEvent::ToolCallCompleted { result } => {
                    if result.name == "web_search" {
                        responses.push(session_update(
                            &session_id,
                            available_commands_update_payload(),
                        ));
                    }
                    responses.push(session_update(
                        &session_id,
                        live_tool_result_update(&result),
                    ));
                    if let Some(tool_usage) = pending_usage_update.take() {
                        responses.push(session_update(
                            &session_id,
                            usage_update(&tool_usage, 200_000, pricing),
                        ));
                    }
                }
                AgentEvent::UsageUpdated { usage: event_usage } => {
                    let visible_usage = subtract_usage(&event_usage, &last_usage_update);
                    pending_usage_update = Some(visible_usage.clone());
                    last_visible_usage = visible_usage;
                    last_usage_update = event_usage;
                }
                AgentEvent::TurnCompleted { usage: event_usage } => {
                    usage = event_usage;
                }
                _ => {}
            }
        }
        match run_result {
            Ok(()) => {
                if last_visible_usage.input_tokens + last_visible_usage.output_tokens > 0 {
                    self.session_visible_usage
                        .insert(session_id.clone(), last_visible_usage);
                }
                responses.push(ok(
                    id,
                    json!({
                        "stopReason": "end_turn",
                        "usage": {
                            "inputTokens": usage.input_tokens,
                            "outputTokens": usage.output_tokens,
                            "totalTokens": usage.input_tokens + usage.output_tokens,
                        },
                        "userMessageId": user_message_id,
                    }),
                ));
            }
            Err(error) => responses.push(invalid_request(id, &error.to_string())),
        }
        responses
    }

    fn prompt_interactive(
        &mut self,
        id: Value,
        params: &Value,
        stdout: &mut dyn Write,
        lines: &mut std::io::Lines<std::io::StdinLock<'_>>,
    ) -> Result<()> {
        let session_id = params
            .get("sessionId")
            .or_else(|| params.get("session_id"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let Some(session) = self.live_sessions.get_mut(&session_id) else {
            write_json_rpc(stdout, session_not_found(id, &session_id))?;
            return Ok(());
        };
        let raw_prompt = params.get("prompt").unwrap_or(&Value::Null);
        let prompt = prompt_text(raw_prompt);
        let user_message_id = user_message_id_from_params(params);
        let prior_usage = self
            .session_visible_usage
            .get(&session_id)
            .cloned()
            .unwrap_or_else(|| session.agent.cumulative_usage().clone());
        let prior_cumulative_usage = session.agent.cumulative_usage().clone();
        let pricing = session_usage_pricing(session);
        let has_prior_turns = session
            .agent
            .messages()
            .iter()
            .any(|message| message.role != Role::System);
        if has_prior_turns && parse_builtin_command(&prompt).is_some() {
            write_json_rpc(
                stdout,
                session_update(&session_id, usage_update(&prior_usage, 200_000, pricing)),
            )?;
        }
        if handle_builtin_command_interactive(
            id.clone(),
            &session_id,
            session,
            &prompt,
            &user_message_id,
            stdout,
        )? {
            return Ok(());
        }
        let images = match extract_image_attachments(raw_prompt) {
            Ok(images) => images,
            Err(error) => {
                write_json_rpc(
                    stdout,
                    invalid_image_attachment(id, &error.message, error.reason),
                )?;
                return Ok(());
            }
        };
        let display_content = match user_display_content_metadata(params) {
            Ok(display_content) => display_content,
            Err(error) => {
                write_json_rpc(stdout, invalid_request_with_null_data(id, &error))?;
                return Ok(());
            }
        };
        let assistant_message_id = uuid::Uuid::new_v4().to_string();
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let (approval_tx, mut approval_rx) = mpsc::unbounded_channel();
        let (client_request_tx, mut client_request_rx) = mpsc::unbounded_channel();
        if self.client_fs_read_text_file {
            session.agent.replace_tool(AcpReadTool {
                session_id: session_id.clone(),
                tx: client_request_tx.clone(),
            });
        }
        if self.client_fs_write_text_file {
            session.agent.replace_tool(AcpWriteFileTool {
                session_id: session_id.clone(),
                tx: client_request_tx.clone(),
            });
            session.agent.replace_tool(AcpEditTool {
                session_id: session_id.clone(),
                tx: client_request_tx.clone(),
            });
        }
        if self.client_terminal {
            session.agent.replace_tool(AcpBashTool {
                session_id: session_id.clone(),
                tx: client_request_tx.clone(),
            });
        }
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(error) => {
                write_json_rpc(stdout, invalid_request(id, &error.to_string()))?;
                return Ok(());
            }
        };

        if has_prior_turns {
            write_json_rpc(
                stdout,
                session_update(&session_id, usage_update(&prior_usage, 200_000, pricing)),
            )?;
        } else {
            write_json_rpc(
                stdout,
                session_update(
                    &session_id,
                    json!({
                        "title": title_from_prompt(&prompt),
                        "sessionUpdate": "session_info_update",
                    }),
                ),
            )?;
        }

        let mut usage = Usage::default();
        let mut pending_usage_update: Option<Usage> = None;
        let mut last_usage_update = prior_cumulative_usage;
        let mut last_visible_usage = Usage::default();
        let run_result = runtime.block_on(async {
            let limits = run_limits_for(self.session_max_turns.get(&session_id).copied());
            let mut turn = Box::pin(session.agent.run_turn_with_approval_display_images_user_message_id_and_limits(
                prompt.clone(),
                event_tx,
                approval_tx,
                display_content,
                images,
                user_message_id.clone(),
                limits,
            ));
            let run_result = loop {
                tokio::select! {
                    biased;
                    Some(event) = event_rx.recv() => {
                        write_live_agent_event(stdout, &session_id, &assistant_message_id, event, &mut usage, &mut pending_usage_update, &mut last_usage_update, &mut last_visible_usage, pricing)?;
                    }
                    Some(approval) = approval_rx.recv() => {
                        request_acp_permission(stdout, lines, &session_id, approval)?;
                    }
                    Some(client_request) = client_request_rx.recv() => {
                        handle_acp_client_request(stdout, lines, client_request)?;
                    }
                    result = &mut turn => break result,
                }
            };
            while let Ok(event) = event_rx.try_recv() {
                write_live_agent_event(stdout, &session_id, &assistant_message_id, event, &mut usage, &mut pending_usage_update, &mut last_usage_update, &mut last_visible_usage, pricing)?;
            }
            Ok::<_, anyhow::Error>(run_result)
        });

        let save_result = runtime.block_on(session.save());
        if let Err(error) = save_result {
            write_json_rpc(stdout, invalid_request(id, &error.to_string()))?;
            return Ok(());
        }

        match run_result {
            Ok(Ok(())) => {
                if last_visible_usage.input_tokens + last_visible_usage.output_tokens > 0 {
                    self.session_visible_usage
                        .insert(session_id.clone(), last_visible_usage);
                }
                write_json_rpc(
                    stdout,
                    ok(
                        id,
                        json!({
                            "stopReason": "end_turn",
                            "usage": {
                                "inputTokens": usage.input_tokens,
                                "outputTokens": usage.output_tokens,
                                "totalTokens": usage.input_tokens + usage.output_tokens,
                            },
                            "userMessageId": user_message_id,
                        }),
                    ),
                )?
            }
            Ok(Err(error)) | Err(error) => {
                write_json_rpc(stdout, invalid_request(id, &error.to_string()))?;
            }
        }
        Ok(())
    }

    fn find_live_session_key(&self, requested_session_id: &str) -> Option<String> {
        if self.live_sessions.contains_key(requested_session_id) {
            return Some(requested_session_id.to_string());
        }
        self.live_sessions
            .iter()
            .find_map(|(key, session)| (session.id.0 == requested_session_id).then(|| key.clone()))
    }

    fn apply_session_mode(&mut self, session_id: &str, mode_id: &str) -> Result<()> {
        let Some(session) = self.live_sessions.get_mut(session_id) else {
            return Ok(());
        };
        let mut config = Config::load().unwrap_or_else(|_| default_test_safe_config());
        config.default_agent = mode_id.to_string();
        config.bypass_tool_permissions = matches!(mode_id, "auto-approve" | "chat");
        session.switch_agent(config);
        self.session_modes
            .insert(session_id.to_string(), mode_id.to_string());
        Ok(())
    }

    fn apply_session_model(&mut self, session_id: &str, model_id: &str) -> Result<()> {
        let Some(session) = self.live_sessions.get_mut(session_id) else {
            return Ok(());
        };
        let mut config = Config::load().unwrap_or_else(|_| default_test_safe_config());
        config.active_model = Some(model_id.to_string());
        if let Some(model) = config.models.iter().find(|model| model.alias == model_id) {
            config.model.name = model.name.clone();
            config.model.provider = model.provider.clone();
        }
        Config::save_active_model(model_id)?;
        session.switch_agent(config);
        self.session_models
            .insert(session_id.to_string(), model_id.to_string());
        Ok(())
    }

    fn session_mode(&self, session_id: &str, _config: &Config) -> String {
        self.session_modes
            .get(session_id)
            .cloned()
            .unwrap_or_else(|| "default".to_string())
    }

    fn session_model(&self, session_id: &str, config: &Config) -> String {
        self.session_models
            .get(session_id)
            .cloned()
            .unwrap_or_else(|| current_model_alias(config))
    }

    fn session_thinking(&self, session_id: &str, config: &Config) -> String {
        self.session_thinking
            .get(session_id)
            .cloned()
            .unwrap_or_else(|| current_thinking(config))
    }
}

fn ok(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn run_limits_for(max_turns: Option<u32>) -> RunLimits {
    RunLimits {
        max_turns,
        ..RunLimits::default()
    }
}

fn method_not_found(id: Value, method: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": -32601,
            "message": "Method not found",
            "data": { "method": method },
        },
    })
}

fn invalid_request(id: Value, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": -32602,
            "message": message,
        },
    })
}

fn invalid_request_with_null_data(id: Value, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": -32602,
            "message": message,
            "data": Value::Null,
        },
    })
}

fn invalid_params_string_type(id: Value, field: &str, input: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": -32602,
            "message": "Invalid params",
            "data": {
                "errors": [
                    {
                        "type": "string_type",
                        "loc": [field],
                        "msg": "Input should be a valid string",
                        "input": input,
                        "url": "https://errors.pydantic.dev/2.13/v/string_type",
                    }
                ]
            },
        },
    })
}

fn internal_error(id: Value, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": -32603,
            "message": message,
        },
    })
}

fn auth_param<'a>(params: &'a Value, key: &str) -> Option<&'a str> {
    params
        .get(key)
        .or_else(|| params.get("_meta").and_then(|meta| meta.get(key)))
        .and_then(Value::as_str)
}

fn invalid_image_attachment(id: Value, message: &str, reason: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": -31007,
            "message": message,
            "data": { "reason": reason },
        },
    })
}

fn session_not_found(id: Value, session_id: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": -32602,
            "message": format!("Session not found: {session_id}"),
            "data": { "session_id": session_id },
        },
    })
}

fn write_json_rpc(stdout: &mut dyn Write, response: Value) -> Result<()> {
    writeln!(stdout, "{}", response)?;
    stdout.flush()?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn write_live_agent_event(
    stdout: &mut dyn Write,
    session_id: &str,
    assistant_message_id: &str,
    event: AgentEvent,
    usage: &mut Usage,
    pending_usage_update: &mut Option<Usage>,
    last_usage_update: &mut Usage,
    last_visible_usage: &mut Usage,
    pricing: AcpUsagePricing,
) -> Result<()> {
    match event {
        AgentEvent::AssistantDelta { text } => {
            write_json_rpc(
                stdout,
                session_update(
                    session_id,
                    json!({
                        "content": { "text": text, "type": "text" },
                        "messageId": assistant_message_id,
                        "sessionUpdate": "agent_message_chunk",
                    }),
                ),
            )?;
        }
        AgentEvent::ThoughtDelta { text, message_id } => {
            write_json_rpc(
                stdout,
                session_update(
                    session_id,
                    json!({
                        "content": { "text": text, "type": "text" },
                        "messageId": message_id,
                        "sessionUpdate": "agent_thought_chunk",
                    }),
                ),
            )?;
        }
        AgentEvent::ToolCallStarted { call } => {
            for update in live_tool_call_updates(&call) {
                write_json_rpc(stdout, session_update(session_id, update))?;
            }
        }
        AgentEvent::ToolCallCompleted { result } => {
            if result.name == "web_search" {
                write_json_rpc(
                    stdout,
                    session_update(session_id, available_commands_update_payload()),
                )?;
            }
            write_json_rpc(
                stdout,
                session_update(session_id, live_tool_result_update(&result)),
            )?;
            if let Some(tool_usage) = pending_usage_update.take() {
                write_json_rpc(
                    stdout,
                    session_update(session_id, usage_update(&tool_usage, 200_000, pricing)),
                )?;
            }
        }
        AgentEvent::UsageUpdated { usage: event_usage } => {
            let visible_usage = subtract_usage(&event_usage, last_usage_update);
            *pending_usage_update = Some(visible_usage.clone());
            *last_visible_usage = visible_usage;
            *last_usage_update = event_usage;
        }
        AgentEvent::TurnCompleted { usage: event_usage } => {
            *usage = event_usage;
        }
        _ => {}
    }
    Ok(())
}

fn handle_builtin_command(
    id: Value,
    session_id: &str,
    session: &mut Session,
    prompt: &str,
    user_message_id: &str,
) -> Option<Vec<Value>> {
    let (command, args) = parse_builtin_command(prompt)?;
    let responses = match command.as_str() {
        "help" => command_reply(id, session_id, &acp_help_text(), user_message_id, None),
        "data-retention" => command_reply(
            id,
            session_id,
            DATA_RETENTION_MESSAGE,
            user_message_id,
            None,
        ),
        "proxy-setup" => command_reply(
            id,
            session_id,
            &proxy_setup_reply(&args),
            user_message_id,
            None,
        ),
        "reload" => vec![
            command_reply_update(
                session_id,
                "Configuration reloaded (includes agent instructions and skills).",
            ),
            session_update(session_id, available_commands_update_payload()),
            ok(id, command_result(user_message_id, None)),
        ],
        "compact" => {
            if session.agent.messages().len() <= 1 {
                command_reply(
                    id,
                    session_id,
                    "No conversation history to compact yet.",
                    user_message_id,
                    None,
                )
            } else {
                match compact_session_blocking(session, args.trim()) {
                    Ok((old_id, new_id)) => {
                        command_compact_response(id, session_id, user_message_id, &old_id, &new_id)
                    }
                    Err(error) => vec![invalid_request(id, &error.to_string())],
                }
            }
        }
        "teleport" => {
            if !active_model_is_mistral(session) {
                teleport_command_reply(
                    id,
                    session_id,
                    "Teleport requires an active Mistral model. Switch to a Mistral model, then try again.",
                    user_message_id,
                    "unavailable",
                )
            } else if session.agent.messages().len() <= 1 {
                teleport_command_reply(
                    id,
                    session_id,
                    "No conversation history to teleport.",
                    user_message_id,
                    "no_history",
                )
            } else {
                teleport_command_reply(
                    id,
                    session_id,
                    "Teleport to Vibe Code Web is unavailable in this environment.",
                    user_message_id,
                    "unavailable",
                )
            }
        }
        _ => return None,
    };
    Some(responses)
}

fn handle_builtin_command_interactive(
    id: Value,
    session_id: &str,
    session: &mut Session,
    prompt: &str,
    user_message_id: &str,
    stdout: &mut dyn Write,
) -> Result<bool> {
    let Some(responses) = handle_builtin_command(id, session_id, session, prompt, user_message_id)
    else {
        return Ok(false);
    };
    for response in responses {
        write_json_rpc(stdout, response)?;
    }
    Ok(true)
}

fn parse_builtin_command(prompt: &str) -> Option<(String, String)> {
    let normalized = prompt.trim();
    let (head, tail) = normalized
        .split_once(char::is_whitespace)
        .unwrap_or((normalized, ""));
    let command = head.strip_prefix('/')?.to_ascii_lowercase();
    Some((command, tail.trim_start().to_string()))
}

fn command_reply(
    id: Value,
    session_id: &str,
    text: &str,
    user_message_id: &str,
    meta: Option<Value>,
) -> Vec<Value> {
    vec![
        command_reply_update(session_id, text),
        ok(id, command_result(user_message_id, meta)),
    ]
}

fn command_reply_update(session_id: &str, text: &str) -> Value {
    command_reply_update_with_meta(session_id, text, None)
}

fn command_reply_update_with_meta(session_id: &str, text: &str, meta: Option<Value>) -> Value {
    let mut update = json!({
        "content": { "text": text, "type": "text" },
        "messageId": uuid::Uuid::new_v4().to_string(),
        "sessionUpdate": "agent_message_chunk",
    });
    if let Some(meta) = meta {
        update["_meta"] = meta;
    }
    session_update(session_id, update)
}

fn command_result(user_message_id: &str, meta: Option<Value>) -> Value {
    let mut result = json!({
        "stopReason": "end_turn",
        "userMessageId": user_message_id,
    });
    if let Some(meta) = meta {
        result["_meta"] = meta;
    }
    result
}

fn user_message_id_from_params(params: &Value) -> String {
    params
        .get("messageId")
        .or_else(|| params.get("message_id"))
        .and_then(Value::as_str)
        .filter(|message_id| !message_id.is_empty())
        .map(ToString::to_string)
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string())
}

fn compact_session_blocking(session: &mut Session, extra: &str) -> Result<(String, String)> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let (old_id, new_id) = runtime.block_on(session.compact(extra))?;
    Ok((old_id.0, new_id.0))
}

fn command_compact_response(
    id: Value,
    session_id: &str,
    user_message_id: &str,
    old_session_id: &str,
    new_session_id: &str,
) -> Vec<Value> {
    let tool_call_id = uuid::Uuid::new_v4().to_string();
    vec![
        session_update(
            session_id,
            json!({
                "content": [{
                    "content": {
                        "text": "Automatic context management, no approval required. This may take some time...",
                        "type": "text",
                    },
                    "type": "content",
                }],
                "kind": "other",
                "sessionUpdate": "tool_call",
                "status": "in_progress",
                "title": "Compacting conversation history...",
                "toolCallId": tool_call_id,
            }),
        ),
        session_update(
            session_id,
            json!({
                "content": [{
                    "content": {
                        "text": compact_complete_display(old_session_id, new_session_id),
                        "type": "text",
                    },
                    "type": "content",
                }],
                "sessionUpdate": "tool_call_update",
                "status": "completed",
                "title": "Compacted conversation history",
                "toolCallId": tool_call_id,
            }),
        ),
        ok(id, command_result(user_message_id, None)),
    ]
}

fn teleport_command_reply(
    id: Value,
    session_id: &str,
    text: &str,
    user_message_id: &str,
    status: &str,
) -> Vec<Value> {
    let meta = json!({
        "tool_name": "teleport",
        "teleport": { "status": status },
    });
    vec![
        command_reply_update_with_meta(session_id, text, Some(meta.clone())),
        ok(id, command_result(user_message_id, Some(meta))),
    ]
}

fn active_model_is_mistral(session: &Session) -> bool {
    let config = session.agent.config();
    config.model.provider.eq_ignore_ascii_case("mistral")
        || config.model.name.to_ascii_lowercase().contains("mistral")
}

fn proxy_setup_reply(args: &str) -> String {
    let args = args.trim();
    if args.is_empty() {
        return proxy_help_text();
    }

    let (raw_key, value) = args.split_once(char::is_whitespace).unwrap_or((args, ""));
    let key = raw_key.to_ascii_uppercase();
    if !is_supported_proxy_key(&key) {
        return format!("Error: {}", unknown_proxy_key_error(&key));
    }

    if value.trim().is_empty() {
        match unset_proxy_var(&key) {
            Ok(()) => format!(
                "Removed `{key}` from ~/.vibe/.env\n\nPlease start a new chat for changes to take effect."
            ),
            Err(error) => format!("Error: {error}"),
        }
    } else {
        let value = value.trim();
        match set_proxy_var(&key, value) {
            Ok(()) => format!(
                "Set `{key}={value}` in ~/.vibe/.env\n\nPlease start a new chat for changes to take effect."
            ),
            Err(error) => format!("Error: {error}"),
        }
    }
}

fn proxy_help_text() -> String {
    let mut lines = vec![
        "## Proxy Configuration".to_string(),
        String::new(),
        "Configure proxy and SSL settings for HTTP requests.".to_string(),
        String::new(),
        "### Usage:".to_string(),
        "- `/proxy-setup` - Show this help and current settings".to_string(),
        "- `/proxy-setup KEY value` - Set an environment variable".to_string(),
        "- `/proxy-setup KEY` - Remove an environment variable".to_string(),
        String::new(),
        "### Supported Variables:".to_string(),
    ];

    for (key, description) in PROXY_VARS {
        lines.push(format!("- `{key}`: {description}"));
    }

    lines.extend([String::new(), "### Current Settings:".to_string()]);
    let mut any_set = false;
    for (key, _) in PROXY_VARS {
        if let Some(value) = dotenv_value(key)
            && !value.is_empty()
        {
            lines.push(format!("- `{key}={value}`"));
            any_set = true;
        }
    }
    if !any_set {
        lines.push("- (none configured)".to_string());
    }

    lines.join("\n")
}

fn is_supported_proxy_key(key: &str) -> bool {
    PROXY_VARS
        .iter()
        .any(|(supported_key, _)| *supported_key == key)
}

fn unknown_proxy_key_error(key: &str) -> String {
    let supported = PROXY_VARS
        .iter()
        .map(|(supported_key, _)| *supported_key)
        .collect::<Vec<_>>()
        .join(", ");
    format!("Unknown key '{key}'. Supported: {supported}")
}

fn set_dotenv_var(key: &str, value: &str) -> Result<()> {
    let Some(path) = env_file_path() else {
        return Ok(());
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let raw = std::fs::read_to_string(&path).unwrap_or_default();
    let mut replaced = false;
    let mut lines = Vec::new();
    for line in raw.lines() {
        let trimmed = line.trim_start();
        let replace = trimmed
            .split_once('=')
            .map(|(candidate, _)| candidate.trim() == key)
            .unwrap_or(false);
        if replace {
            lines.push(format!("{key}='{}'", dotenv_single_quote(value)));
            replaced = true;
        } else {
            lines.push(line.to_string());
        }
    }
    if !replaced {
        lines.push(format!("{key}='{}'", dotenv_single_quote(value)));
    }
    let mut output = lines.join("\n");
    if !output.is_empty() {
        output.push('\n');
    }
    std::fs::write(path, output)?;
    Ok(())
}

fn set_proxy_var(key: &str, value: &str) -> Result<()> {
    set_dotenv_var(key, value)
}

fn unset_proxy_var(key: &str) -> Result<()> {
    remove_dotenv_key(key)
}

fn dotenv_single_quote(value: &str) -> String {
    value.replace('\n', "").replace('\'', "\\'")
}

fn acp_help_text() -> String {
    let mut lines = vec!["### Available Commands".to_string(), String::new()];
    for command in builtin_command_specs() {
        let hint = command
            .input_hint
            .map(|hint| format!(" `<{hint}>`"))
            .unwrap_or_default();
        lines.push(format!(
            "- `/{}`{}: {}",
            command.name, hint, command.description
        ));
    }

    let builtin_names = builtin_command_specs()
        .into_iter()
        .map(|command| command.name)
        .collect::<HashSet<_>>();
    let mut skills = acp_user_invocable_skills()
        .into_iter()
        .filter(|skill| !builtin_names.contains(skill.name.as_str()))
        .collect::<Vec<_>>();
    skills.sort_by(|left, right| left.name.cmp(&right.name));
    if !skills.is_empty() {
        lines.extend([
            String::new(),
            "### Available Skills".to_string(),
            String::new(),
        ]);
        for skill in skills {
            lines.push(format!("- `/{}`: {}", skill.name, skill.description));
        }
    }

    lines.join("\n")
}

#[derive(Debug, Clone)]
struct AcpSkillInfo {
    name: String,
    description: String,
}

fn acp_user_invocable_skills() -> Vec<AcpSkillInfo> {
    let skills_dir = vibe_home()
        .unwrap_or_else(|| PathBuf::from(".vibe"))
        .join("skills");
    let Ok(entries) = std::fs::read_dir(skills_dir) else {
        return Vec::new();
    };
    let mut skills = entries
        .flatten()
        .filter_map(|entry| read_acp_skill_info(&entry.path()))
        .collect::<Vec<_>>();
    skills.sort_by(|left, right| left.name.cmp(&right.name));
    skills
}

fn read_acp_skill_info(path: &Path) -> Option<AcpSkillInfo> {
    let content = std::fs::read_to_string(path.join("SKILL.md")).ok()?;
    let mut in_frontmatter = false;
    let mut seen_frontmatter = false;
    let mut name = None;
    let mut description = None;
    let mut user_invocable = true;
    for line in content.lines() {
        if line.trim() == "---" {
            if !seen_frontmatter {
                seen_frontmatter = true;
                in_frontmatter = true;
                continue;
            }
            break;
        }
        if !in_frontmatter {
            continue;
        }
        if let Some(value) = line.strip_prefix("name:") {
            name = Some(value.trim().trim_matches('"').to_string());
        } else if let Some(value) = line.strip_prefix("description:") {
            description = Some(value.trim().trim_matches('"').to_string());
        } else if let Some(value) = line.strip_prefix("user_invocable:") {
            user_invocable = value.trim().eq_ignore_ascii_case("true");
        }
    }
    user_invocable.then_some(AcpSkillInfo {
        name: name?,
        description: description.unwrap_or_default(),
    })
}

fn compact_complete_display(old_session_id: &str, new_session_id: &str) -> String {
    format!(
        "Compaction completed.\nsession: {} (before compaction) \u{2192} {} (after compaction)",
        shorten_session_id(old_session_id),
        shorten_session_id(new_session_id)
    )
}

fn shorten_session_id(session_id: &str) -> String {
    session_id.chars().take(8).collect()
}

fn request_acp_permission(
    stdout: &mut dyn Write,
    lines: &mut std::io::Lines<std::io::StdinLock<'_>>,
    session_id: &str,
    approval: ApprovalRequest,
) -> Result<ApprovalDecision> {
    let request_id = uuid::Uuid::new_v4().to_string();
    write_json_rpc(
        stdout,
        json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "method": "session/request_permission",
                "params": {
                "options": permission_options(required_permissions_for_call(&approval.call)),
                "sessionId": session_id,
                "toolCall": { "toolCallId": approval.call.id },
            },
        }),
    )?;

    let decision = read_permission_decision(lines, &request_id);
    let _ = approval.respond_to.send(decision);
    Ok(decision)
}

fn read_permission_decision(
    lines: &mut std::io::Lines<std::io::StdinLock<'_>>,
    request_id: &str,
) -> ApprovalDecision {
    for line in lines.by_ref() {
        let Ok(line) = line else {
            return ApprovalDecision::Deny;
        };
        let Ok(response) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if response.get("id").and_then(Value::as_str) != Some(request_id) {
            continue;
        }
        let outcome = response
            .pointer("/result/outcome/outcome")
            .and_then(Value::as_str);
        if outcome != Some("selected") {
            if outcome == Some("cancelled") {
                return ApprovalDecision::Cancelled;
            }
            return ApprovalDecision::Deny;
        }
        let option_id = response
            .pointer("/result/outcome/optionId")
            .or_else(|| response.pointer("/result/outcome/option_id"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        return match option_id {
            "allow_once" => ApprovalDecision::AllowOnce,
            "allow_always" => ApprovalDecision::AllowSession,
            "allow_always_permanent" => ApprovalDecision::AllowAlways,
            "reject_once" => ApprovalDecision::DenyWithFeedback,
            _ => ApprovalDecision::Deny,
        };
    }
    ApprovalDecision::Deny
}

fn permission_options(required_permissions: Option<Value>) -> Vec<Value> {
    let required_permissions_meta = required_permissions.map(|permissions| {
        json!({
            "required_permissions": permissions,
        })
    });
    let mut allow_always = json!({
        "kind": "allow_always",
        "name": "Allow for remainder of this session",
        "optionId": "allow_always",
    });
    let mut allow_always_permanent = json!({
        "kind": "allow_always",
        "name": "Always allow",
        "optionId": "allow_always_permanent",
    });
    if let Some(meta) = required_permissions_meta {
        allow_always["_meta"] = meta.clone();
        allow_always_permanent["_meta"] = meta;
    }
    vec![
        json!({
            "kind": "allow_once",
            "name": "Allow once",
            "optionId": "allow_once",
        }),
        allow_always,
        allow_always_permanent,
        json!({
            "kind": "reject_once",
            "name": "Deny",
            "optionId": "reject_once",
        }),
    ]
}

fn required_permissions_for_call(call: &ToolCall) -> Option<Value> {
    if call.name == "web_fetch" {
        let url = call.arguments.get("url").and_then(Value::as_str)?;
        let normalized_url = normalize_fetch_url_for_acp(url);
        let host = url_host_for_display(&normalized_url);
        return Some(json!([{
            "invocation_pattern": host,
            "label": format!("fetching from {host}"),
            "scope": "url_pattern",
            "session_pattern": host,
        }]));
    }
    if call.name == "bash" {
        let command = call.arguments.get("command").and_then(Value::as_str)?;
        let tokens: Vec<&str> = command.split_whitespace().collect();
        if tokens.is_empty() {
            return None;
        }
        let session_pattern = bash_session_permission_pattern(&tokens);
        return Some(json!([{
            "invocation_pattern": command,
            "label": session_pattern,
            "scope": "command_pattern",
            "session_pattern": session_pattern,
        }]));
    }
    None
}

fn bash_session_permission_pattern(tokens: &[&str]) -> String {
    for length in (1..=tokens.len()).rev() {
        let prefix = tokens[..length].join(" ");
        if let Some(arity) = bash_permission_arity(&prefix) {
            return format!("{} *", tokens[..arity.min(tokens.len())].join(" "));
        }
    }
    format!("{} *", tokens[0])
}

fn bash_permission_arity(prefix: &str) -> Option<usize> {
    match prefix {
        "cat" | "cd" | "chmod" | "chown" | "cp" | "echo" | "env" | "export" | "grep" | "kill"
        | "killall" | "ln" | "ls" | "mkdir" | "mv" | "ps" | "pwd" | "rm" | "rmdir" | "sleep"
        | "source" | "tail" | "touch" | "unset" | "which" => Some(1),
        "bazel" | "brew" | "bun" | "cargo" | "cdk" | "cf" | "cmake" | "composer" | "consul"
        | "crictl" | "deno" | "docker" | "firebase" | "flyctl" | "git" | "go" | "gradle"
        | "helm" | "heroku" | "hugo" | "ip" | "kind" | "kubectl" | "kustomize" | "make" | "mc"
        | "minikube" | "mongosh" | "mysql" | "mvn" | "ng" | "npm" | "nvm" | "nx" | "openssl"
        | "pip" | "pipenv" | "pnpm" | "poetry" | "podman" | "psql" | "pulumi" | "pyenv"
        | "python" | "rake" | "rbenv" | "redis-cli" | "rustup" | "serverless" | "skaffold"
        | "sls" | "sst" | "swift" | "systemctl" | "terraform" | "tmux" | "turbo" | "ufw" | "uv"
        | "vercel" | "volta" | "wp" | "yarn" => Some(2),
        "aws" | "az" | "doctl" | "eksctl" | "gcloud" | "gh" | "sfdx" | "vault" => Some(3),
        "bun run"
        | "bun x"
        | "cargo add"
        | "cargo run"
        | "consul kv"
        | "deno task"
        | "docker builder"
        | "docker compose"
        | "docker container"
        | "docker image"
        | "docker network"
        | "docker volume"
        | "eksctl create"
        | "git config"
        | "git remote"
        | "git stash"
        | "ip addr"
        | "ip link"
        | "ip netns"
        | "ip route"
        | "kubectl kustomize"
        | "kubectl rollout"
        | "mc admin"
        | "npm exec"
        | "npm init"
        | "npm run"
        | "npm view"
        | "openssl req"
        | "openssl x509"
        | "pnpm dlx"
        | "pnpm exec"
        | "pnpm run"
        | "podman container"
        | "podman image"
        | "pulumi stack"
        | "terraform workspace"
        | "uv run"
        | "vault auth"
        | "vault kv"
        | "yarn dlx"
        | "yarn run" => Some(3),
        _ => None,
    }
}

enum AcpClientRequest {
    CreateTerminal {
        session_id: String,
        command: String,
        cwd: String,
        output_byte_limit: u64,
        respond_to: oneshot::Sender<std::result::Result<String, String>>,
    },
    WaitForTerminalExit {
        session_id: String,
        terminal_id: String,
        tool_name: String,
        tool_call_id: String,
        timeout_secs: u64,
        respond_to: oneshot::Sender<std::result::Result<Option<i64>, String>>,
    },
    TerminalOutput {
        session_id: String,
        terminal_id: String,
        respond_to: oneshot::Sender<std::result::Result<String, String>>,
    },
    KillTerminal {
        session_id: String,
        terminal_id: String,
        respond_to: oneshot::Sender<std::result::Result<(), String>>,
    },
    ReleaseTerminal {
        session_id: String,
        terminal_id: String,
        respond_to: oneshot::Sender<std::result::Result<(), String>>,
    },
    ReadTextFile {
        session_id: String,
        path: String,
        line: Option<u64>,
        limit: Option<u64>,
        respond_to: oneshot::Sender<std::result::Result<String, String>>,
    },
    WriteTextFile {
        session_id: String,
        path: String,
        content: String,
        respond_to: oneshot::Sender<std::result::Result<(), String>>,
    },
}

struct AcpReadTool {
    session_id: String,
    tx: mpsc::UnboundedSender<AcpClientRequest>,
}

struct AcpWriteFileTool {
    session_id: String,
    tx: mpsc::UnboundedSender<AcpClientRequest>,
}

struct AcpEditTool {
    session_id: String,
    tx: mpsc::UnboundedSender<AcpClientRequest>,
}

struct AcpBashTool {
    session_id: String,
    tx: mpsc::UnboundedSender<AcpClientRequest>,
}

#[async_trait]
impl Tool for AcpBashTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "bash".to_string(),
            description: "Run a one-off bash command and capture its output.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "The bash command to run." },
                    "timeout": { "type": ["integer", "null"], "description": "Override the default command timeout." }
                },
                "required": ["command"]
            }),
        }
    }

    async fn run(&self, call: &ToolCall) -> ToolResult {
        match self.run_bash(call).await {
            Ok(output) => ToolResult {
                call_id: call.id.clone(),
                name: call.name.clone(),
                output,
                success: true,
            },
            Err(error) => ToolResult {
                call_id: call.id.clone(),
                name: call.name.clone(),
                output: error,
                success: false,
            },
        }
    }
}

impl AcpBashTool {
    async fn run_bash(&self, call: &ToolCall) -> std::result::Result<String, String> {
        let command = call
            .arguments
            .get("command")
            .and_then(Value::as_str)
            .ok_or_else(|| "command is required".to_string())?
            .to_string();
        let timeout = call
            .arguments
            .get("timeout")
            .and_then(Value::as_u64)
            .unwrap_or(300);
        let cwd = std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .display()
            .to_string();

        let (create_respond_to, create_response) = oneshot::channel();
        self.tx
            .send(AcpClientRequest::CreateTerminal {
                session_id: self.session_id.clone(),
                command: command.clone(),
                cwd,
                output_byte_limit: 16_000,
                respond_to: create_respond_to,
            })
            .map_err(|_| "Client not available in tool state. This tool can only be used within an ACP session.".to_string())?;
        let terminal_id = create_response
            .await
            .map_err(|_| "Failed to create terminal: client response channel closed".to_string())?
            .map_err(|error| format!("Failed to create terminal: {error}"))?;

        let result = self
            .run_terminal_command(call, &command, timeout, &terminal_id)
            .await;
        let (release_respond_to, release_response) = oneshot::channel();
        let _ = self.tx.send(AcpClientRequest::ReleaseTerminal {
            session_id: self.session_id.clone(),
            terminal_id,
            respond_to: release_respond_to,
        });
        let _ = release_response.await;
        result
    }

    async fn run_terminal_command(
        &self,
        call: &ToolCall,
        command: &str,
        timeout: u64,
        terminal_id: &str,
    ) -> std::result::Result<String, String> {
        let (wait_respond_to, wait_response) = oneshot::channel();
        self.tx
            .send(AcpClientRequest::WaitForTerminalExit {
                session_id: self.session_id.clone(),
                terminal_id: terminal_id.to_string(),
                tool_name: call.name.clone(),
                tool_call_id: call.id.clone(),
                timeout_secs: timeout,
                respond_to: wait_respond_to,
            })
            .map_err(|_| "Client not available in tool state. This tool can only be used within an ACP session.".to_string())?;
        let returncode = match wait_response.await {
            Ok(Ok(Some(returncode))) => returncode,
            Ok(Ok(None)) => {
                let (kill_respond_to, kill_response) = oneshot::channel();
                let _ = self.tx.send(AcpClientRequest::KillTerminal {
                    session_id: self.session_id.clone(),
                    terminal_id: terminal_id.to_string(),
                    respond_to: kill_respond_to,
                });
                let _ = kill_response.await;
                return Err(format!(
                    "Command timed out after {timeout}s: {}",
                    py_string_repr(command)
                ));
            }
            Ok(Err(error)) => return Err(error),
            Err(_) => return Err("terminal/wait_for_exit response channel closed".to_string()),
        };
        let (output_respond_to, output_response) = oneshot::channel();
        self.tx
            .send(AcpClientRequest::TerminalOutput {
                session_id: self.session_id.clone(),
                terminal_id: terminal_id.to_string(),
                respond_to: output_respond_to,
            })
            .map_err(|_| "Client not available in tool state. This tool can only be used within an ACP session.".to_string())?;
        let stdout = output_response
            .await
            .map_err(|_| "terminal/output response channel closed".to_string())??;
        if returncode != 0 {
            let mut message = format!(
                "Command failed: {}\nReturn code: {returncode}",
                py_string_repr(command)
            );
            if !stdout.is_empty() {
                message.push_str(&format!("\nStdout: {stdout}"));
            }
            return Err(message);
        }
        Ok(format!(
            "command: {command}\nstdout: {stdout}\nstderr: \nreturncode: {returncode}"
        ))
    }
}

fn py_string_repr(value: &str) -> String {
    format!("'{}'", value.replace('\\', "\\\\").replace('\'', "\\'"))
}

#[async_trait]
impl Tool for AcpReadTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "read".to_string(),
            description: "Read a text file with line numbers. Results are formatted with line number prefixes for easy reference.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "file_path": { "type": "string", "description": "The absolute path to the file to read." },
                    "offset": { "type": ["integer", "null"], "description": "The line number to start reading from (1-indexed). Only provide if the file is too large to read at once." },
                    "limit": { "type": "integer", "default": 2000, "description": "The number of lines to read. Lower it to read a smaller portion of a large file." }
                },
                "required": ["file_path"]
            }),
        }
    }

    async fn run(&self, call: &ToolCall) -> ToolResult {
        match self.run_read(call).await {
            Ok(output) => ToolResult {
                call_id: call.id.clone(),
                name: call.name.clone(),
                output,
                success: true,
            },
            Err(error) => ToolResult {
                call_id: call.id.clone(),
                name: call.name.clone(),
                output: error,
                success: false,
            },
        }
    }
}

impl AcpReadTool {
    async fn run_read(&self, call: &ToolCall) -> std::result::Result<String, String> {
        let raw_path = call
            .arguments
            .get("file_path")
            .and_then(Value::as_str)
            .ok_or_else(|| "file_path is required".to_string())?;
        let path = resolve_read_file_path(raw_path)?;
        let offset = call.arguments.get("offset").and_then(Value::as_u64);
        let start_line = offset.unwrap_or(1);
        let limit = call
            .arguments
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(2_000);
        if limit == 0 {
            return Err("limit must be greater than 0".to_string());
        }

        let (respond_to, response) = oneshot::channel();
        self.tx
            .send(AcpClientRequest::ReadTextFile {
                session_id: self.session_id.clone(),
                path: path.display().to_string(),
                line: offset,
                limit: Some(limit + 1),
                respond_to,
            })
            .map_err(|_| "Client not available in tool state. This tool can only be used within an ACP session.".to_string())?;
        let content = response
            .await
            .map_err(|_| "Error reading file: client response channel closed".to_string())??;
        Ok(format_acp_read_output(
            &path, &content, start_line, offset, limit,
        ))
    }
}

#[async_trait]
impl Tool for AcpWriteFileTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "write_file".to_string(),
            description:
                "Create a UTF-8 file. Fails if the file already exists; use edit to modify."
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "content": { "type": "string" }
                },
                "required": ["path", "content"]
            }),
        }
    }

    async fn run(&self, call: &ToolCall) -> ToolResult {
        match self.run_write(call).await {
            Ok(output) => ToolResult {
                call_id: call.id.clone(),
                name: call.name.clone(),
                output,
                success: true,
            },
            Err(error) => ToolResult {
                call_id: call.id.clone(),
                name: call.name.clone(),
                output: error,
                success: false,
            },
        }
    }
}

impl AcpWriteFileTool {
    async fn run_write(&self, call: &ToolCall) -> std::result::Result<String, String> {
        let raw_path = call
            .arguments
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| "path is required".to_string())?;
        let content = call
            .arguments
            .get("content")
            .and_then(Value::as_str)
            .ok_or_else(|| "content is required".to_string())?;
        let path = resolve_write_file_path(raw_path)?;
        if path.exists() {
            return Err(format!(
                "File '{}' already exists. Use edit to modify it.",
                path.display()
            ));
        }
        let normalized_content = normalize_newlines_to_os(content);
        let (respond_to, response) = oneshot::channel();
        self.tx
            .send(AcpClientRequest::WriteTextFile {
                session_id: self.session_id.clone(),
                path: path.display().to_string(),
                content: normalized_content.clone(),
                respond_to,
            })
            .map_err(|_| "Client not available in tool state. This tool can only be used within an ACP session.".to_string())?;
        response
            .await
            .map_err(|_| "Error writing file: client response channel closed".to_string())??;
        Ok(format!(
            "path: {}\nbytes_written: {}\ncontent: {}",
            path.display(),
            normalized_content.len(),
            content,
        ))
    }
}

#[async_trait]
impl Tool for AcpEditTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "edit".to_string(),
            description: "Perform exact string replacements in files. Supports single or bulk (replace_all) substitutions with atomic, concurrent-safe writes.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "file_path": { "type": "string", "description": "The absolute path to the file to modify" },
                    "old_string": { "type": "string", "description": "The text to replace" },
                    "new_string": { "type": "string", "description": "The text to replace it with (must be different from old_string)" },
                    "replace_all": { "type": "boolean", "default": false, "description": "Replace all occurrences of old_string (default false)" }
                },
                "required": ["file_path", "old_string", "new_string"]
            }),
        }
    }

    async fn run(&self, call: &ToolCall) -> ToolResult {
        match self.run_edit(call).await {
            Ok(output) => ToolResult {
                call_id: call.id.clone(),
                name: call.name.clone(),
                output,
                success: true,
            },
            Err(error) => ToolResult {
                call_id: call.id.clone(),
                name: call.name.clone(),
                output: error,
                success: false,
            },
        }
    }
}

impl AcpEditTool {
    async fn run_edit(&self, call: &ToolCall) -> std::result::Result<String, String> {
        let raw_path = call
            .arguments
            .get("file_path")
            .and_then(Value::as_str)
            .ok_or_else(|| "file_path is required".to_string())?;
        let old = call
            .arguments
            .get("old_string")
            .and_then(Value::as_str)
            .ok_or_else(|| "old_string is required".to_string())?;
        let new = call
            .arguments
            .get("new_string")
            .and_then(Value::as_str)
            .ok_or_else(|| "new_string is required".to_string())?;
        if old.is_empty() {
            return Err(
                "old_string cannot be empty. Use write_file to create new files.".to_string(),
            );
        }
        if old == new {
            return Err("No changes to make — old_string and new_string are identical".to_string());
        }
        let replace_all = call
            .arguments
            .get("replace_all")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let path = resolve_edit_file_path(raw_path)?;
        let (read_respond_to, read_response) = oneshot::channel();
        self.tx
            .send(AcpClientRequest::ReadTextFile {
                session_id: self.session_id.clone(),
                path: path.display().to_string(),
                line: None,
                limit: None,
                respond_to: read_respond_to,
            })
            .map_err(|_| "Client not available in tool state. This tool can only be used within an ACP session.".to_string())?;
        let raw_content = read_response
            .await
            .map_err(|_| "Error reading file: client response channel closed".to_string())?
            .map_err(|error| format!("Error reading {}: {error}", path.display()))?;
        let (content, newline) = normalize_newlines_with_style(&raw_content);
        let count = content.matches(old).count();
        if count == 0 {
            return Err(format!(
                "String to replace not found in file.\nString: {old}"
            ));
        }
        if count > 1 && !replace_all {
            return Err(format!(
                "Found {count} matches of the string to replace, but replace_all is false. To replace all occurrences, set replace_all to true. To replace only one occurrence, please provide more context to uniquely identify the instance.\nString: {old}"
            ));
        }
        let updated = if replace_all {
            content.replace(old, new)
        } else {
            content.replacen(old, new, 1)
        };
        if updated != content {
            let write_content = updated.replace('\n', newline);
            let (write_respond_to, write_response) = oneshot::channel();
            self.tx
                .send(AcpClientRequest::WriteTextFile {
                    session_id: self.session_id.clone(),
                    path: path.display().to_string(),
                    content: write_content,
                    respond_to: write_respond_to,
                })
                .map_err(|_| "Client not available in tool state. This tool can only be used within an ACP session.".to_string())?;
            write_response
                .await
                .map_err(|_| "Error writing file: client response channel closed".to_string())?
                .map_err(|error| format!("Error writing {}: {error}", path.display()))?;
        }
        let message = if replace_all {
            "The file has been updated. All occurrences were successfully replaced"
        } else {
            "The file has been updated successfully."
        };
        Ok(format!(
            "file: {}\nmessage: {}\nold_string: {}\nnew_string: {}",
            path.display(),
            message,
            old,
            new,
        ))
    }
}

fn handle_acp_client_request(
    stdout: &mut dyn Write,
    lines: &mut std::io::Lines<std::io::StdinLock<'_>>,
    request: AcpClientRequest,
) -> Result<()> {
    match request {
        AcpClientRequest::CreateTerminal {
            session_id,
            command,
            cwd,
            output_byte_limit,
            respond_to,
        } => {
            let result = request_acp_create_terminal(
                stdout,
                lines,
                &session_id,
                &command,
                &cwd,
                output_byte_limit,
            );
            let _ = respond_to.send(result);
        }
        AcpClientRequest::WaitForTerminalExit {
            session_id,
            terminal_id,
            tool_name,
            tool_call_id,
            timeout_secs,
            respond_to,
        } => {
            let result = request_acp_wait_for_terminal_exit(
                stdout,
                lines,
                &session_id,
                &terminal_id,
                &tool_name,
                &tool_call_id,
                timeout_secs,
            );
            let _ = respond_to.send(result);
        }
        AcpClientRequest::TerminalOutput {
            session_id,
            terminal_id,
            respond_to,
        } => {
            let result = request_acp_terminal_output(stdout, lines, &session_id, &terminal_id);
            let _ = respond_to.send(result);
        }
        AcpClientRequest::KillTerminal {
            session_id,
            terminal_id,
            respond_to,
        } => {
            let result = request_acp_empty_terminal_method(
                stdout,
                lines,
                "terminal/kill",
                &session_id,
                &terminal_id,
            );
            let _ = respond_to.send(result);
        }
        AcpClientRequest::ReleaseTerminal {
            session_id,
            terminal_id,
            respond_to,
        } => {
            let result = request_acp_empty_terminal_method(
                stdout,
                lines,
                "terminal/release",
                &session_id,
                &terminal_id,
            );
            let _ = respond_to.send(result);
        }
        AcpClientRequest::ReadTextFile {
            session_id,
            path,
            line,
            limit,
            respond_to,
        } => {
            let result = request_acp_read_text_file(stdout, lines, &session_id, &path, line, limit);
            let _ = respond_to.send(result);
        }
        AcpClientRequest::WriteTextFile {
            session_id,
            path,
            content,
            respond_to,
        } => {
            let result = request_acp_write_text_file(stdout, lines, &session_id, &path, &content);
            let _ = respond_to.send(result);
        }
    }
    Ok(())
}

fn request_acp_create_terminal(
    stdout: &mut dyn Write,
    lines: &mut std::io::Lines<std::io::StdinLock<'_>>,
    session_id: &str,
    command: &str,
    cwd: &str,
    output_byte_limit: u64,
) -> std::result::Result<String, String> {
    let request_id = uuid::Uuid::new_v4().to_string();
    write_json_rpc(
        stdout,
        json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "method": "terminal/create",
            "params": {
                "command": command,
                "cwd": cwd,
                "outputByteLimit": output_byte_limit,
                "sessionId": session_id,
            },
        }),
    )
    .map_err(|error| error.to_string())?;
    for line in lines.by_ref() {
        let line = line.map_err(|error| error.to_string())?;
        let Ok(response) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if response.get("id").and_then(Value::as_str) != Some(&request_id) {
            continue;
        }
        if let Some(error) = response.pointer("/error/message").and_then(Value::as_str) {
            return Err(error.to_string());
        }
        return response
            .pointer("/result/terminalId")
            .or_else(|| response.pointer("/result/terminal_id"))
            .and_then(Value::as_str)
            .map(ToString::to_string)
            .ok_or_else(|| "invalid terminal/create response".to_string());
    }
    Err("terminal/create response not received".to_string())
}

fn request_acp_wait_for_terminal_exit(
    stdout: &mut dyn Write,
    lines: &mut std::io::Lines<std::io::StdinLock<'_>>,
    session_id: &str,
    terminal_id: &str,
    tool_name: &str,
    tool_call_id: &str,
    timeout_secs: u64,
) -> std::result::Result<Option<i64>, String> {
    let request_id = uuid::Uuid::new_v4().to_string();
    write_json_rpc(
        stdout,
        json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "method": "terminal/wait_for_exit",
            "params": {
                "sessionId": session_id,
                "terminalId": terminal_id,
            },
        }),
    )
    .map_err(|error| error.to_string())?;
    write_json_rpc(
        stdout,
        session_update(
            session_id,
            terminal_opened_update(tool_name, tool_call_id, terminal_id),
        ),
    )
    .map_err(|error| error.to_string())?;
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    loop {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            write_available_commands_update(stdout, session_id)?;
            return Ok(None);
        };
        if !stdin_ready_within(remaining)? {
            write_available_commands_update(stdout, session_id)?;
            return Ok(None);
        }
        let Some(line) = lines.next() else {
            return Err("terminal/wait_for_exit response not received".to_string());
        };
        let line = line.map_err(|error| error.to_string())?;
        let Ok(response) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if response.get("id").and_then(Value::as_str) != Some(&request_id) {
            continue;
        }
        if let Some(error) = response.pointer("/error/message").and_then(Value::as_str) {
            return Err(error.to_string());
        }
        let exit_code = response
            .pointer("/result/exitCode")
            .or_else(|| response.pointer("/result/exit_code"));
        return match exit_code {
            Some(Value::Null) | None => Ok(Some(0)),
            Some(value) => value
                .as_i64()
                .map(Some)
                .ok_or_else(|| "invalid terminal/wait_for_exit response".to_string()),
        };
    }
}

fn write_available_commands_update(
    stdout: &mut dyn Write,
    session_id: &str,
) -> std::result::Result<(), String> {
    write_json_rpc(
        stdout,
        session_update(session_id, available_commands_update_payload()),
    )
    .map_err(|error| error.to_string())
}

fn available_commands_update_payload() -> Value {
    json!({
        "availableCommands": advertised_available_commands(),
        "sessionUpdate": "available_commands_update",
    })
}

fn advertised_available_commands() -> Vec<Value> {
    let mut commands = builtin_command_specs()
        .into_iter()
        .map(command_spec_value)
        .collect::<Vec<_>>();
    commands.extend(acp_user_invocable_skills().into_iter().map(|skill| {
        json!({
            "description": skill.description,
            "input": { "hint": "instructions for the skill" },
            "name": skill.name,
        })
    }));
    commands.sort_by(|left, right| {
        left.get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .cmp(
                right
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            )
    });
    commands
}

#[derive(Clone)]
struct AcpCommandSpec {
    name: &'static str,
    description: &'static str,
    input_hint: Option<&'static str>,
}

fn builtin_command_specs() -> Vec<AcpCommandSpec> {
    vec![
        AcpCommandSpec {
            name: "compact",
            description: "Compact conversation history by summarizing. Optionally pass instructions to guide the summary",
            input_hint: Some("Optional instructions to guide the compaction summary"),
        },
        AcpCommandSpec {
            name: "data-retention",
            description: "Show data retention information",
            input_hint: None,
        },
        AcpCommandSpec {
            name: "help",
            description: "Show available commands and keyboard shortcuts",
            input_hint: None,
        },
        AcpCommandSpec {
            name: "leanstall",
            description: "Install the Lean 4 agent (leanstral)",
            input_hint: None,
        },
        AcpCommandSpec {
            name: "log",
            description: "Show path to current session log directory",
            input_hint: None,
        },
        AcpCommandSpec {
            name: "mcp",
            description: "Show MCP OAuth status, login guidance, or log out an OAuth MCP server",
            input_hint: Some("status | login <alias> | logout <alias>"),
        },
        AcpCommandSpec {
            name: "proxy-setup",
            description: "Configure proxy and SSL certificate settings",
            input_hint: Some("KEY value to set, KEY to unset, or empty for help"),
        },
        AcpCommandSpec {
            name: "reload",
            description: "Reload configuration, agent instructions, and skills from disk",
            input_hint: None,
        },
        AcpCommandSpec {
            name: "teleport",
            description: "Teleport session to Vibe Code Web",
            input_hint: None,
        },
        AcpCommandSpec {
            name: "unleanstall",
            description: "Uninstall the Lean 4 agent",
            input_hint: None,
        },
    ]
}

fn command_spec_value(command: AcpCommandSpec) -> Value {
    let mut value = json!({
        "description": command.description,
        "name": command.name,
    });
    if let Some(hint) = command.input_hint {
        value["input"] = json!({ "hint": hint });
    }
    value
}

fn stdin_ready_within(timeout: Duration) -> std::result::Result<bool, String> {
    let millis = timeout.as_millis().min(i32::MAX as u128) as i32;
    let mut pollfd = libc::pollfd {
        fd: 0,
        events: libc::POLLIN,
        revents: 0,
    };
    let result = unsafe { libc::poll(&mut pollfd, 1, millis) };
    if result < 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    Ok(result > 0)
}

fn request_acp_terminal_output(
    stdout: &mut dyn Write,
    lines: &mut std::io::Lines<std::io::StdinLock<'_>>,
    session_id: &str,
    terminal_id: &str,
) -> std::result::Result<String, String> {
    let request_id = uuid::Uuid::new_v4().to_string();
    write_json_rpc(
        stdout,
        json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "method": "terminal/output",
            "params": {
                "sessionId": session_id,
                "terminalId": terminal_id,
            },
        }),
    )
    .map_err(|error| error.to_string())?;
    for line in lines.by_ref() {
        let line = line.map_err(|error| error.to_string())?;
        let Ok(response) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if response.get("id").and_then(Value::as_str) != Some(&request_id) {
            continue;
        }
        if let Some(error) = response.pointer("/error/message").and_then(Value::as_str) {
            return Err(error.to_string());
        }
        return response
            .pointer("/result/output")
            .and_then(Value::as_str)
            .map(ToString::to_string)
            .ok_or_else(|| "invalid terminal/output response".to_string());
    }
    Err("terminal/output response not received".to_string())
}

fn request_acp_empty_terminal_method(
    stdout: &mut dyn Write,
    lines: &mut std::io::Lines<std::io::StdinLock<'_>>,
    method: &str,
    session_id: &str,
    terminal_id: &str,
) -> std::result::Result<(), String> {
    let request_id = uuid::Uuid::new_v4().to_string();
    write_json_rpc(
        stdout,
        json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "method": method,
            "params": {
                "sessionId": session_id,
                "terminalId": terminal_id,
            },
        }),
    )
    .map_err(|error| error.to_string())?;
    for line in lines.by_ref() {
        let line = line.map_err(|error| error.to_string())?;
        let Ok(response) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if response.get("id").and_then(Value::as_str) != Some(&request_id) {
            continue;
        }
        if let Some(error) = response.pointer("/error/message").and_then(Value::as_str) {
            return Err(error.to_string());
        }
        return Ok(());
    }
    Err(format!("{method} response not received"))
}

fn request_acp_read_text_file(
    stdout: &mut dyn Write,
    lines: &mut std::io::Lines<std::io::StdinLock<'_>>,
    session_id: &str,
    path: &str,
    line: Option<u64>,
    limit: Option<u64>,
) -> std::result::Result<String, String> {
    let request_id = uuid::Uuid::new_v4().to_string();
    let mut params = serde_json::Map::new();
    if let Some(limit) = limit {
        params.insert("limit".to_string(), json!(limit));
    }
    if let Some(line) = line {
        params.insert("line".to_string(), json!(line));
    }
    params.insert("path".to_string(), json!(path));
    params.insert("sessionId".to_string(), json!(session_id));
    let request = json!({
        "jsonrpc": "2.0",
        "id": request_id,
        "method": "fs/read_text_file",
        "params": Value::Object(params),
    });
    write_json_rpc(stdout, request).map_err(|error| error.to_string())?;
    read_text_file_response(lines, &request_id)
}

fn read_text_file_response(
    lines: &mut std::io::Lines<std::io::StdinLock<'_>>,
    request_id: &str,
) -> std::result::Result<String, String> {
    for line in lines.by_ref() {
        let line = line.map_err(|error| error.to_string())?;
        let Ok(response) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if response.get("id").and_then(Value::as_str) != Some(request_id) {
            continue;
        }
        if let Some(error) = response.pointer("/error/message").and_then(Value::as_str) {
            return Err(error.to_string());
        }
        return response
            .pointer("/result/content")
            .and_then(Value::as_str)
            .map(ToString::to_string)
            .ok_or_else(|| "Error reading file: invalid fs/read_text_file response".to_string());
    }
    Err("Error reading file: client response not received".to_string())
}

fn request_acp_write_text_file(
    stdout: &mut dyn Write,
    lines: &mut std::io::Lines<std::io::StdinLock<'_>>,
    session_id: &str,
    path: &str,
    content: &str,
) -> std::result::Result<(), String> {
    let request_id = uuid::Uuid::new_v4().to_string();
    write_json_rpc(
        stdout,
        json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "method": "fs/write_text_file",
            "params": {
                "content": content,
                "path": path,
                "sessionId": session_id,
            },
        }),
    )
    .map_err(|error| error.to_string())?;
    write_text_file_response(lines, &request_id)
}

fn write_text_file_response(
    lines: &mut std::io::Lines<std::io::StdinLock<'_>>,
    request_id: &str,
) -> std::result::Result<(), String> {
    for line in lines.by_ref() {
        let line = line.map_err(|error| error.to_string())?;
        let Ok(response) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if response.get("id").and_then(Value::as_str) != Some(request_id) {
            continue;
        }
        if let Some(error) = response.pointer("/error/message").and_then(Value::as_str) {
            return Err(error.to_string());
        }
        return Ok(());
    }
    Err("Error writing file: client response not received".to_string())
}

fn resolve_read_file_path(raw_path: &str) -> std::result::Result<PathBuf, String> {
    if raw_path.trim().is_empty() {
        return Err("file_path cannot be empty".to_string());
    }
    let expanded = if raw_path == "~" {
        dirs::home_dir().unwrap_or_else(|| PathBuf::from(raw_path))
    } else if let Some(rest) = raw_path.strip_prefix("~/") {
        dirs::home_dir()
            .map(|home| home.join(rest))
            .unwrap_or_else(|| PathBuf::from(raw_path))
    } else {
        PathBuf::from(raw_path)
    };
    let path = if expanded.is_absolute() {
        expanded
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(expanded)
    };
    let path = path.canonicalize().unwrap_or(path);
    if !path.exists() {
        return Err(format!("File not found at: {}", path.display()));
    }
    if path.is_dir() {
        return Err(format!(
            "Path is a directory, not a file: {}",
            path.display()
        ));
    }
    Ok(path)
}

fn resolve_write_file_path(raw_path: &str) -> std::result::Result<PathBuf, String> {
    if raw_path.trim().is_empty() {
        return Err("path cannot be empty".to_string());
    }
    let expanded = if raw_path == "~" {
        dirs::home_dir().unwrap_or_else(|| PathBuf::from(raw_path))
    } else if let Some(rest) = raw_path.strip_prefix("~/") {
        dirs::home_dir()
            .map(|home| home.join(rest))
            .unwrap_or_else(|| PathBuf::from(raw_path))
    } else {
        PathBuf::from(raw_path)
    };
    let path = if expanded.is_absolute() {
        expanded
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(expanded)
    };
    Ok(resolve_missing_path(&path))
}

fn resolve_edit_file_path(raw_path: &str) -> std::result::Result<PathBuf, String> {
    if raw_path.trim().is_empty() {
        return Err("File path cannot be empty".to_string());
    }
    let path = resolve_read_file_path(raw_path)?;
    if !path.is_file() {
        return Err(format!("Path is not a file: {}", path.display()));
    }
    Ok(path)
}

fn resolve_missing_path(path: &Path) -> PathBuf {
    if let Ok(canonical) = path.canonicalize() {
        return canonical;
    }
    let Some(parent) = path.parent() else {
        return path.to_path_buf();
    };
    let Ok(canonical_parent) = parent.canonicalize() else {
        return path.to_path_buf();
    };
    path.file_name()
        .map(|name| canonical_parent.join(name))
        .unwrap_or(canonical_parent)
}

fn normalize_newlines_to_os(content: &str) -> String {
    let normalized = content.replace("\r\n", "\n").replace('\r', "\n");
    if cfg!(windows) {
        normalized.replace('\n', "\r\n")
    } else {
        normalized
    }
}

fn normalize_newlines_with_style(content: &str) -> (String, &str) {
    let newline = if content.contains("\r\n") {
        "\r\n"
    } else if content.contains('\r') {
        "\r"
    } else {
        "\n"
    };
    (content.replace("\r\n", "\n").replace('\r', "\n"), newline)
}

fn format_acp_read_output(
    path: &Path,
    response_content: &str,
    start_line: u64,
    requested_offset: Option<u64>,
    requested_limit: u64,
) -> String {
    let mut lines = response_content.lines().collect::<Vec<_>>();
    let was_truncated = lines.len() as u64 > requested_limit;
    lines.truncate(requested_limit as usize);
    let content = if !lines.is_empty() {
        lines
            .iter()
            .enumerate()
            .map(|(idx, line)| format!("{:>9}→{}", start_line + idx as u64, line))
            .collect::<Vec<_>>()
            .join("\n")
    } else if response_content.is_empty() {
        "<vibe_warning>Warning: the file exists but the contents are empty.</vibe_warning>"
            .to_string()
    } else {
        format!(
            "<vibe_warning>Warning: no content returned for offset {start_line}.</vibe_warning>"
        )
    };
    let total_lines = if response_content.is_empty() {
        "0".to_string()
    } else {
        "None".to_string()
    };
    OkReadOutput {
        file_path: path.display().to_string(),
        content,
        num_lines: lines.len() as u64,
        start_line,
        requested_offset,
        requested_limit,
        total_lines,
        was_truncated,
    }
    .to_tool_output()
}

struct OkReadOutput {
    file_path: String,
    content: String,
    num_lines: u64,
    start_line: u64,
    requested_offset: Option<u64>,
    requested_limit: u64,
    total_lines: String,
    was_truncated: bool,
}

impl OkReadOutput {
    fn to_tool_output(&self) -> String {
        format!(
            "file_path: {}\ncontent: {}\nnum_lines: {}\nstart_line: {}\nrequested_offset: {}\nrequested_limit: {}\ntotal_lines: {}\nwas_truncated: {}",
            self.file_path,
            self.content,
            self.num_lines,
            self.start_line,
            self.requested_offset
                .map(|value| value.to_string())
                .unwrap_or_else(|| "None".to_string()),
            self.requested_limit,
            self.total_lines,
            py_bool(self.was_truncated),
        )
    }
}

fn required_non_empty_param(params: &Value, key: &str) -> std::result::Result<String, String> {
    params
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .ok_or_else(|| format!("Invalid ACP session request: missing or empty {key}"))
}

fn required_delete_session_id_param(params: &Value) -> std::result::Result<String, String> {
    let Some(raw_value) = params.get("sessionId") else {
        return Err(format!(
            "Invalid ACP session delete request: 1 validation error for SessionDeleteRequest\nsession_id\n  Field required [type=missing, input_value={}, input_type=dict]\n    For further information visit https://errors.pydantic.dev/2.13/v/missing",
            python_value_repr(params)
        ));
    };
    let Some(raw_session_id) = raw_value.as_str() else {
        return Err(format!(
            "Invalid ACP session delete request: 1 validation error for SessionDeleteRequest\nsessionId\n  Input should be a valid string [type=string_type, input_value={}, input_type={}]\n    For further information visit https://errors.pydantic.dev/2.13/v/string_type",
            python_value_repr(raw_value),
            python_type_name(raw_value),
        ));
    };
    let session_id = raw_session_id.trim();
    if session_id.is_empty() {
        return Err(format!(
            "Invalid ACP session delete request: 1 validation error for SessionDeleteRequest\nsessionId\n  String should have at least 1 character [type=string_too_short, input_value='{}', input_type=str]\n    For further information visit https://errors.pydantic.dev/2.13/v/string_too_short",
            raw_session_id.replace('\'', "\\'"),
        ));
    }
    Ok(session_id.to_string())
}

fn python_value_repr(value: &Value) -> String {
    match value {
        Value::Null => "None".to_string(),
        Value::Bool(value) => {
            if *value {
                "True".to_string()
            } else {
                "False".to_string()
            }
        }
        Value::Number(value) => value.to_string(),
        Value::String(value) => format!("'{}'", value.replace('\'', "\\'")),
        Value::Array(values) => {
            let inner = values
                .iter()
                .map(python_value_repr)
                .collect::<Vec<_>>()
                .join(", ");
            format!("[{inner}]")
        }
        Value::Object(values) => {
            let inner = values
                .iter()
                .map(|(key, value)| format!("'{key}': {}", python_value_repr(value)))
                .collect::<Vec<_>>()
                .join(", ");
            format!("{{{inner}}}")
        }
    }
}

fn python_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(_) => "int",
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}

fn initialize_result(params: &Value) -> Value {
    let protocol_version = params
        .get("protocolVersion")
        .and_then(Value::as_u64)
        .unwrap_or(1);
    let client_info = params.get("clientInfo").unwrap_or(&Value::Null);
    let auth_methods = auth_methods(params, client_info);
    json!({
        "agentCapabilities": {
            "loadSession": true,
            "promptCapabilities": {
                "audio": false,
                "embeddedContext": true,
                "image": true,
            },
            "sessionCapabilities": {
                "close": {},
                "fork": {},
                "list": {},
            },
        },
        "agentInfo": {
            "name": "@mistralai/mistral-vibe",
            "title": "Mistral Vibe",
            "version": VIBE_ACP_VERSION,
        },
        "authMethods": auth_methods,
        "protocolVersion": protocol_version,
    })
}

fn client_supports_fs_read(params: &Value) -> bool {
    params
        .pointer("/clientCapabilities/fs/readTextFile")
        .or_else(|| params.pointer("/client_capabilities/fs/read_text_file"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn client_supports_fs_write(params: &Value) -> bool {
    params
        .pointer("/clientCapabilities/fs/writeTextFile")
        .or_else(|| params.pointer("/client_capabilities/fs/write_text_file"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn client_supports_terminal(params: &Value) -> bool {
    params
        .pointer("/clientCapabilities/terminal")
        .or_else(|| params.pointer("/client_capabilities/terminal"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn auth_methods(params: &Value, client_info: &Value) -> Vec<Value> {
    if is_authenticated_jetbrains(client_info) {
        return Vec::new();
    }
    if !active_provider_supports_browser_auth() {
        return Vec::new();
    }
    let mut methods = vec![browser_auth_method()];
    let meta = params
        .get("clientCapabilities")
        .and_then(|capabilities| capabilities.get("_meta"));
    if meta
        .and_then(|meta| meta.get("terminal-auth"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        methods.push(terminal_auth_method());
    }
    if meta
        .and_then(|meta| meta.get("browser-auth-delegated"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        methods.push(delegated_browser_auth_method());
    }
    methods
}

fn active_provider_supports_browser_auth() -> bool {
    let config = Config::load().unwrap_or_else(|_| default_test_safe_config());
    let provider_name = config.model.provider.as_str();
    let Ok(provider) = config.active_provider() else {
        return false;
    };
    provider_supports_browser_auth(provider_name, provider)
}

fn current_browser_auth_provider() -> Option<BrowserAuthProvider> {
    let config = Config::load().unwrap_or_else(|_| default_test_safe_config());
    let provider_name = config.model.provider.as_str();
    let provider = config.active_provider().ok()?.clone();
    if !provider_supports_browser_auth(provider_name, &provider) {
        return None;
    }
    Some(browser_auth_provider_from_provider(provider))
}

fn browser_auth_provider_from_provider(
    provider: microvibe_config::ProviderConfig,
) -> BrowserAuthProvider {
    let browser_base_url = provider
        .browser_auth_base_url
        .clone()
        .unwrap_or_else(|| "https://console.mistral.ai".to_string())
        .trim_end_matches('/')
        .to_string();
    let api_base_url = provider
        .browser_auth_api_base_url
        .clone()
        .unwrap_or_else(|| format!("{browser_base_url}/api"))
        .trim_end_matches('/')
        .to_string();
    BrowserAuthProvider {
        provider,
        browser_base_url,
        api_base_url,
    }
}

async fn start_browser_sign_in_attempt(
    provider: &BrowserAuthProvider,
) -> std::result::Result<BrowserSignInProcess, String> {
    let code_verifier = generate_code_verifier();
    let code_challenge = generate_code_challenge(&code_verifier);
    let client = reqwest::Client::new();
    let response = client
        .post(format!("{}/vibe/sign-in", provider.api_base_url))
        .json(&json!({
            "code_challenge": code_challenge,
            "code_challenge_method": "S256",
        }))
        .send()
        .await
        .map_err(|_| "Failed to start browser sign-in.".to_string())?;
    if !response.status().is_success() {
        return Err("Failed to start browser sign-in.".to_string());
    }
    let payload = response
        .json::<Value>()
        .await
        .map_err(|_| "Failed to start browser sign-in.".to_string())?;
    let process_id = payload
        .get("process_id")
        .and_then(Value::as_str)
        .ok_or_else(|| "Failed to start browser sign-in.".to_string())?
        .to_string();
    let sign_in_url = payload
        .get("sign_in_url")
        .and_then(Value::as_str)
        .ok_or_else(|| "Failed to start browser sign-in.".to_string())?;
    let poll_url = payload
        .get("poll_url")
        .and_then(Value::as_str)
        .ok_or_else(|| "Failed to start browser sign-in.".to_string())?;
    if !url_is_under_base(sign_in_url, &provider.browser_base_url)
        || !url_is_under_base(poll_url, &provider.api_base_url)
    {
        return Err("Failed to start browser sign-in.".to_string());
    }
    let expires_at = payload
        .get("expires_at")
        .and_then(Value::as_str)
        .ok_or_else(|| "Failed to start browser sign-in.".to_string())
        .map(normalize_browser_auth_expires_at)?;
    Ok(BrowserSignInProcess {
        process_id,
        sign_in_url: sign_in_url.to_string(),
        poll_url: poll_url.to_string(),
        expires_at,
        code_verifier,
    })
}

async fn complete_pending_browser_sign_in_attempt(
    provider: &BrowserAuthProvider,
    attempt: &PendingBrowserSignInAttempt,
) -> std::result::Result<String, String> {
    let client = reqwest::Client::new();
    let response = client
        .get(&attempt.poll_url)
        .send()
        .await
        .map_err(|_| "Browser sign-in status could not be retrieved.".to_string())?;
    if response.status().as_u16() == 410 {
        return Err("Browser sign-in expired.".to_string());
    }
    if !response.status().is_success() {
        return Err("Browser sign-in status could not be retrieved.".to_string());
    }
    let payload = response
        .json::<Value>()
        .await
        .map_err(|_| "Browser sign-in status could not be retrieved.".to_string())?;
    let status = payload
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let exchange_token = match status {
        "completed" => payload
            .get("exchange_token")
            .and_then(Value::as_str)
            .ok_or_else(|| "Sign-in worked, but setup couldn't finish.".to_string())?,
        "expired" => return Err("Browser sign-in expired.".to_string()),
        "denied" => return Err("Browser sign-in was denied.".to_string()),
        "error" => {
            return Err(payload
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("Browser sign-in failed.")
                .to_string());
        }
        "pending" => return Err("Browser sign-in status could not be retrieved.".to_string()),
        _ => return Err("Browser sign-in returned an unknown state.".to_string()),
    };
    let response = client
        .post(format!(
            "{}/vibe/sign-in/{}/exchange",
            provider.api_base_url, attempt.process_id
        ))
        .json(&json!({
            "exchange_token": exchange_token,
            "code_verifier": attempt.code_verifier,
        }))
        .send()
        .await
        .map_err(|_| "Failed to exchange browser sign-in for an API key.".to_string())?;
    if !response.status().is_success() {
        return Err("Failed to exchange browser sign-in for an API key.".to_string());
    }
    let payload = response
        .json::<Value>()
        .await
        .map_err(|_| "Failed to exchange browser sign-in for an API key.".to_string())?;
    payload
        .get("api_key")
        .and_then(Value::as_str)
        .filter(|api_key| !api_key.is_empty())
        .map(ToString::to_string)
        .ok_or_else(|| "Browser sign-in exchange did not return an API key.".to_string())
}

async fn complete_started_browser_sign_in_attempt(
    provider: &BrowserAuthProvider,
    attempt: &BrowserSignInProcess,
) -> std::result::Result<String, String> {
    let pending = PendingBrowserSignInAttempt {
        process_id: attempt.process_id.clone(),
        poll_url: attempt.poll_url.clone(),
        code_verifier: attempt.code_verifier.clone(),
        provider: provider.provider.clone(),
    };
    complete_pending_browser_sign_in_attempt(provider, &pending).await
}

fn persist_browser_auth_api_key(
    provider: &microvibe_config::ProviderConfig,
    api_key: &str,
) -> std::result::Result<&'static str, String> {
    let key = provider.api_key_env.trim();
    if key.is_empty() {
        return Ok("skipped");
    }
    set_dotenv_var(key, api_key).map_err(|error| error.to_string())?;
    Ok("completed")
}

fn open_browser_for_sign_in(url: &str) -> std::result::Result<(), String> {
    if let Ok(browser) = std::env::var("BROWSER")
        && !browser.trim().is_empty()
    {
        return open_browser_with_browser_env(&browser, url);
    }
    let status = if cfg!(target_os = "macos") {
        std::process::Command::new("open").arg(url).status()
    } else if cfg!(target_os = "windows") {
        std::process::Command::new("rundll32")
            .args(["url.dll,FileProtocolHandler", url])
            .status()
    } else {
        std::process::Command::new("xdg-open").arg(url).status()
    };
    status
        .ok()
        .filter(|status| status.success())
        .map(|_| ())
        .ok_or_else(|| "Failed to open browser for sign-in.".to_string())
}

fn open_browser_with_browser_env(browser: &str, url: &str) -> std::result::Result<(), String> {
    let parts = shlex::split(browser).unwrap_or_else(|| vec![browser.to_string()]);
    let Some((program, raw_args)) = parts.split_first() else {
        return Err("Failed to open browser for sign-in.".to_string());
    };
    let mut saw_placeholder = false;
    let mut args = raw_args
        .iter()
        .map(|arg| {
            if arg.contains("%s") {
                saw_placeholder = true;
                arg.replace("%s", url)
            } else {
                arg.clone()
            }
        })
        .collect::<Vec<_>>();
    if !saw_placeholder {
        args.push(url.to_string());
    }
    std::process::Command::new(program)
        .args(args)
        .status()
        .ok()
        .filter(|status| status.success())
        .map(|_| ())
        .ok_or_else(|| "Failed to open browser for sign-in.".to_string())
}

fn generate_code_verifier() -> String {
    BASE64_URL_SAFE_NO_PAD.encode(uuid::Uuid::new_v4().as_bytes())
}

fn generate_code_challenge(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    BASE64_URL_SAFE_NO_PAD.encode(digest)
}

fn normalize_browser_auth_expires_at(value: &str) -> String {
    DateTime::parse_from_rfc3339(value)
        .map(|timestamp| {
            timestamp
                .with_timezone(&Utc)
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        })
        .unwrap_or_else(|_| value.to_string())
}

fn url_is_under_base(value: &str, base_url: &str) -> bool {
    reqwest::Url::parse(value)
        .ok()
        .zip(reqwest::Url::parse(base_url).ok())
        .is_some_and(|(url, base)| {
            url.scheme() == base.scheme()
                && url.host_str() == base.host_str()
                && url.port_or_known_default() == base.port_or_known_default()
                && url.path().starts_with(base.path())
        })
}

fn provider_supports_browser_auth(
    provider_name: &str,
    provider: &microvibe_config::ProviderConfig,
) -> bool {
    provider.backend == "mistral"
        || (provider_name == "mistral"
            && (provider.browser_auth_base_url.is_some()
                || provider.browser_auth_api_base_url.is_some()
                || provider.base_url.contains("mistral.ai")))
}

fn py_bool(value: bool) -> &'static str {
    if value { "True" } else { "False" }
}

fn is_authenticated_jetbrains(client_info: &Value) -> bool {
    std::env::var("MISTRAL_API_KEY").is_ok()
        && client_info
            .get("name")
            .and_then(Value::as_str)
            .is_some_and(|name| name.starts_with("JetBrains."))
}

fn browser_auth_method() -> Value {
    json!({
        "description": "Sign into Mistral Vibe through your Mistral AI Studio account.",
        "id": "browser-auth",
        "name": "Sign in through Mistral AI Studio",
    })
}

fn delegated_browser_auth_method() -> Value {
    json!({
        "description": "Sign into Mistral Vibe through your Mistral AI Studio account.",
        "id": "browser-auth-delegated",
        "name": "Sign in through Mistral AI Studio",
    })
}

fn terminal_auth_method() -> Value {
    json!({
        "_meta": {
            "terminal-auth": {
                "command": "vibe-acp",
                "args": ["--setup"],
                "label": "Mistral Vibe Setup",
            },
        },
        "args": ["--setup"],
        "description": "Register your API Key inside Mistral Vibe",
        "id": "vibe-setup",
        "name": "Register your API Key",
        "type": "terminal",
    })
}

struct AuthState {
    kind: &'static str,
    authenticated: bool,
    sign_out_available: bool,
    env_key: Option<String>,
}

fn assess_auth_state() -> AuthState {
    let config = Config::load().unwrap_or_else(|_| default_test_safe_config());
    let provider = config.active_provider().ok();
    let env_key = provider
        .map(|provider| provider.api_key_env.trim())
        .unwrap_or("");
    if env_key.is_empty() {
        return AuthState {
            kind: "auth_not_required",
            authenticated: true,
            sign_out_available: false,
            env_key: None,
        };
    }

    let process_has_value = std::env::var(env_key).is_ok_and(|value| !value.is_empty());
    let dotenv_has_value = dotenv_value(env_key).is_some_and(|value| !value.is_empty());
    if !process_has_value && !dotenv_has_value {
        return AuthState {
            kind: "signed_out",
            authenticated: false,
            sign_out_available: false,
            env_key: Some(env_key.to_string()),
        };
    }

    if env_key != "MISTRAL_API_KEY" {
        return AuthState {
            kind: "unsupported_provider",
            authenticated: true,
            sign_out_available: false,
            env_key: Some(env_key.to_string()),
        };
    }

    if process_has_value {
        return AuthState {
            kind: "process_env",
            authenticated: true,
            sign_out_available: false,
            env_key: Some(env_key.to_string()),
        };
    }

    if dotenv_has_value {
        return AuthState {
            kind: "vibe_home_env_file",
            authenticated: true,
            sign_out_available: true,
            env_key: Some(env_key.to_string()),
        };
    }

    unreachable!("signed out state handled before auth source classification")
}

fn auth_status_response(state: &AuthState) -> Value {
    json!({
        "authenticated": state.authenticated,
        "authState": state.kind,
        "signOutAvailable": state.sign_out_available,
    })
}

fn env_file_path() -> Option<PathBuf> {
    vibe_home().map(|home| home.join(".env"))
}

fn dotenv_value(key: &str) -> Option<String> {
    let path = env_file_path()?;
    let raw = std::fs::read_to_string(path).ok()?;
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || !line.starts_with(key) {
            continue;
        }
        let (candidate_key, raw_value) = line.split_once('=')?;
        if candidate_key.trim() != key {
            continue;
        }
        return Some(unquote_dotenv_value(raw_value.trim()).to_string());
    }
    None
}

fn unquote_dotenv_value(value: &str) -> &str {
    if value.len() >= 2 {
        let bytes = value.as_bytes();
        if (bytes[0] == b'\'' && bytes[value.len() - 1] == b'\'')
            || (bytes[0] == b'"' && bytes[value.len() - 1] == b'"')
        {
            return &value[1..value.len() - 1];
        }
    }
    value
}

fn remove_dotenv_key(key: &str) -> Result<()> {
    let Some(path) = env_file_path() else {
        return Ok(());
    };
    let raw = std::fs::read_to_string(&path).unwrap_or_default();
    let mut kept = Vec::new();
    for line in raw.lines() {
        let trimmed = line.trim_start();
        let remove = trimmed
            .split_once('=')
            .map(|(candidate, _)| candidate.trim() == key)
            .unwrap_or(false);
        if !remove {
            kept.push(line);
        }
    }
    let mut output = kept.join("\n");
    if !output.is_empty() {
        output.push('\n');
    }
    std::fs::write(path, output)?;
    Ok(())
}

fn bootstrap_vibe_home() {
    let Some(vibe_home) = vibe_home() else {
        return;
    };
    let _ = std::fs::create_dir_all(&vibe_home);
    let config_path = vibe_home.join("config.toml");
    if !config_path.exists() {
        let _ = std::fs::write(&config_path, "enable_telemetry = false\n");
    }
    let history_path = vibe_home.join("vibehistory");
    if !history_path.exists() {
        let _ = std::fs::write(&history_path, "");
    }
}

fn vibe_home() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("VIBE_HOME") {
        return Some(PathBuf::from(path));
    }
    dirs::home_dir().map(|home| home.join(".vibe"))
}

fn session_root() -> Option<PathBuf> {
    vibe_home().map(|home| home.join("logs").join("session"))
}

fn list_sessions(params: &Value) -> Result<Value> {
    let cwd_filter = params
        .get("cwd")
        .and_then(Value::as_str)
        .map(str::to_string);
    let Some(root) = session_root() else {
        return Ok(json!({ "sessions": [] }));
    };
    if !root.exists() {
        return Ok(json!({ "sessions": [] }));
    }

    let mut sessions = Vec::new();
    for entry in std::fs::read_dir(root)? {
        let path = entry?.path();
        if !path.is_dir()
            || !path.join("meta.json").is_file()
            || !path.join("messages.jsonl").is_file()
        {
            continue;
        }
        if !saved_messages_are_valid_for_acp_list(&path.join("messages.jsonl")) {
            continue;
        }
        let raw = match std::fs::read_to_string(path.join("meta.json")) {
            Ok(raw) => raw,
            Err(_) => continue,
        };
        let metadata = match serde_json::from_str::<Value>(&raw) {
            Ok(metadata) => metadata,
            Err(_) => continue,
        };
        let Some(session_id) = metadata.get("session_id").and_then(Value::as_str) else {
            continue;
        };
        let cwd = metadata
            .pointer("/environment/working_directory")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if cwd_filter.as_ref().is_some_and(|filter| filter != &cwd) {
            continue;
        }
        let updated_at = metadata
            .get("end_time")
            .and_then(Value::as_str)
            .and_then(normalize_timestamp);
        let sort_key = updated_at.clone().unwrap_or_default();
        let mut session = json!({
            "cwd": cwd,
            "sessionId": session_id,
            "title": metadata.get("title").and_then(Value::as_str),
        });
        if let Some(updated_at) = updated_at {
            session["updatedAt"] = Value::String(updated_at);
        }
        sessions.push((sort_key, session));
    }
    sessions.sort_by(|a, b| b.0.cmp(&a.0));
    Ok(json!({ "sessions": sessions.into_iter().map(|(_, value)| value).collect::<Vec<_>>() }))
}

fn saved_messages_are_valid_for_acp_list(path: &Path) -> bool {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return false;
    };
    let mut saw_message = false;
    for line in raw.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            return false;
        };
        if !value.is_object() {
            return false;
        }
        saw_message = true;
    }
    saw_message
}

fn find_session_dir(session_id: &str) -> Option<PathBuf> {
    let root = session_root()?;
    if !root.exists() {
        return None;
    }
    let short = session_id.chars().take(8).collect::<String>();
    let mut matches = Vec::new();
    for entry in std::fs::read_dir(root).ok()? {
        let path = entry.ok()?.path();
        if !path.is_dir() || !path.join("meta.json").is_file() {
            continue;
        }
        let name_matches = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("session_") && name.ends_with(&short));
        let metadata_matches = std::fs::read_to_string(path.join("meta.json"))
            .ok()
            .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
            .and_then(|metadata| {
                metadata
                    .get("session_id")
                    .and_then(Value::as_str)
                    .map(|stored| stored == session_id)
            })
            .unwrap_or(false);
        if name_matches || metadata_matches {
            matches.push(path);
        }
    }
    matches.sort();
    matches.pop()
}

fn find_saved_session_dir_exact(session_id: &str) -> Option<PathBuf> {
    let root = session_root()?;
    if !root.exists() {
        return None;
    }
    for entry in std::fs::read_dir(root).ok()? {
        let path = entry.ok()?.path();
        if !path.is_dir() || !path.join("meta.json").is_file() {
            continue;
        }
        let metadata = read_saved_metadata(&path).ok()?;
        if metadata.get("session_id").and_then(Value::as_str) == Some(session_id) {
            return Some(path);
        }
    }
    None
}

fn read_saved_metadata(session_dir: &Path) -> Result<Value> {
    Ok(serde_json::from_str::<Value>(&std::fs::read_to_string(
        session_dir.join("meta.json"),
    )?)?)
}

fn update_saved_session_title(session_dir: &Path, title: &str) -> Result<Value> {
    let mut metadata = read_saved_metadata(session_dir)?;
    metadata["title"] = Value::String(title.to_string());
    metadata["title_source"] = Value::String("manual".to_string());
    std::fs::write(
        session_dir.join("meta.json"),
        serde_json::to_string_pretty(&metadata)?,
    )?;
    Ok(metadata)
}

fn clear_last_session_pointers(session_id: &str) {
    let Some(root) = session_root() else {
        return;
    };
    let pointer_dir = root.join(".last_session");
    let Ok(entries) = std::fs::read_dir(pointer_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Ok(raw) = std::fs::read_to_string(&path) else {
            continue;
        };
        if raw.trim() == session_id {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn load_session_messages(session_dir: &Path) -> Result<(String, Vec<Value>)> {
    let metadata = read_saved_metadata(session_dir)?;
    let session_id = metadata
        .get("session_id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let raw = std::fs::read_to_string(session_dir.join("messages.jsonl"))?;
    let mut messages = Vec::new();
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        messages.push(serde_json::from_str::<Value>(line)?);
    }
    Ok((session_id, messages))
}

fn replay_updates(
    session_id: &str,
    message: &Value,
    replayed_tool_calls: &mut HashSet<String>,
) -> Vec<Value> {
    let Some(role) = message.get("role").and_then(Value::as_str) else {
        return Vec::new();
    };
    match role {
        "user" => {
            let mut update = json!({
                "content": {
                    "text": message.get("content").and_then(Value::as_str).unwrap_or_default(),
                    "type": "text",
                },
                "messageId": message_id_from(message),
                "sessionUpdate": "user_message_chunk",
            });
            if let Some(display_content) = message.get("user_display_content") {
                update["_meta"] = json!({ "user_display_content": display_content });
            }
            vec![session_update(session_id, update)]
        }
        "assistant" => {
            let mut updates = Vec::new();
            for tool_call in message
                .get("tool_calls")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let Some(update) = replay_tool_call(tool_call) {
                    if let Some(tool_call_id) = update.get("toolCallId").and_then(Value::as_str) {
                        replayed_tool_calls.insert(tool_call_id.to_string());
                    }
                    updates.push(session_update(session_id, update));
                }
            }
            if let Some(reasoning) = message
                .get("reasoning_content")
                .and_then(Value::as_str)
                .filter(|reasoning| !reasoning.is_empty())
            {
                updates.push(session_update(
                    session_id,
                    json!({
                        "content": {
                            "text": reasoning,
                            "type": "text",
                        },
                        "messageId": message
                            .get("reasoning_message_id")
                            .or_else(|| message.get("reasoningMessageId"))
                            .and_then(Value::as_str)
                            .map(ToString::to_string)
                            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
                        "sessionUpdate": "agent_thought_chunk",
                    }),
                ));
            }
            if let Some(update) = message
                .get("content")
                .and_then(Value::as_str)
                .filter(|content| !content.is_empty())
                .map(|content| {
                    session_update(
                        session_id,
                        json!({
                        "content": {
                            "text": content,
                            "type": "text",
                        },
                        "messageId": message_id_from(message),
                        "sessionUpdate": "agent_message_chunk",
                        }),
                    )
                })
            {
                updates.push(update);
            }
            updates
        }
        "tool" => {
            let Some(tool_call_id) = message
                .get("tool_call_id")
                .or_else(|| message.get("toolCallId"))
                .and_then(Value::as_str)
            else {
                return Vec::new();
            };
            if !replayed_tool_calls.contains(tool_call_id) {
                return Vec::new();
            }
            let tool_name = message
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let content = message
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or_default();
            vec![session_update(
                session_id,
                json!({
                    "_meta": { "tool_name": tool_name },
                    "content": [{
                        "content": {
                            "text": content,
                            "type": "text",
                        },
                        "type": "content",
                    }],
                    "kind": tool_kind(tool_name),
                    "rawOutput": content,
                    "status": "completed",
                    "toolCallId": tool_call_id,
                    "sessionUpdate": "tool_call_update",
                }),
            )]
        }
        _ => Vec::new(),
    }
}

fn replay_tool_call(tool_call: &Value) -> Option<Value> {
    let tool_call_id = tool_call.get("id").and_then(Value::as_str)?;
    let function = tool_call.get("function")?;
    let tool_name = function
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let arguments = function
        .get("arguments")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let mut update = json!({
        "_meta": { "tool_name": tool_name },
        "kind": tool_kind(tool_name),
        "rawInput": arguments,
        "status": "completed",
        "title": tool_name,
        "toolCallId": tool_call_id,
        "sessionUpdate": "tool_call",
    });
    if tool_name == "read"
        && let Ok(args) = serde_json::from_str::<Value>(arguments)
        && let Some(path) = args.get("file_path").and_then(Value::as_str)
    {
        let has_explicit_range = args.get("offset").is_some() || args.get("limit").is_some();
        let offset = args.get("offset").cloned().unwrap_or(Value::Null);
        let limit = args.get("limit").cloned().unwrap_or_else(|| json!(2000));
        update["rawInput"] = Value::String(format!(
            "{{\"file_path\":{},\"offset\":{},\"limit\":{}}}",
            serde_json::to_string(path).unwrap_or_else(|_| "\"\"".to_string()),
            offset,
            limit
        ));
        if !has_explicit_range {
            update["title"] = Value::String(format!("Reading {path}"));
        }
    }
    Some(update)
}

fn session_update(session_id: &str, update: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "sessionId": session_id,
            "update": update,
        },
    })
}

fn prompt_text(prompt: &Value) -> String {
    prompt
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|block| match block.get("type").and_then(Value::as_str) {
            Some("text") => block
                .get("text")
                .and_then(Value::as_str)
                .map(str::to_string),
            Some("resource") => block.get("resource").map(resource_prompt_text),
            Some("resource_link") => Some(resource_link_prompt_text(block)),
            _ => None,
        })
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

struct InvalidImageAttachment {
    message: String,
    reason: &'static str,
}

fn extract_image_attachments(
    prompt: &Value,
) -> std::result::Result<Vec<ImageAttachment>, InvalidImageAttachment> {
    let image_blocks = prompt
        .as_array()
        .into_iter()
        .flatten()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("image"))
        .collect::<Vec<_>>();

    if image_blocks.len() > MAX_IMAGES_PER_MESSAGE {
        return Err(InvalidImageAttachment {
            message: format!(
                "Too many images: {} > {MAX_IMAGES_PER_MESSAGE}",
                image_blocks.len()
            ),
            reason: "too_many",
        });
    }

    image_blocks
        .into_iter()
        .map(image_block_to_attachment)
        .collect()
}

fn image_block_to_attachment(
    block: &Value,
) -> std::result::Result<ImageAttachment, InvalidImageAttachment> {
    let mime_type = block
        .get("mime_type")
        .or_else(|| block.get("mimeType"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let Some(ext) = extension_for_mime(mime_type) else {
        return Err(InvalidImageAttachment {
            message: format!("Unsupported image mime type: {mime_type}"),
            reason: "wrong_type",
        });
    };

    let encoded = block
        .get("data")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let data = BASE64
        .decode(encoded)
        .map_err(|error| InvalidImageAttachment {
            message: format!("Invalid base64 image data: {}", python_base64_error(&error)),
            reason: "invalid_base64",
        })?;
    if data.len() > MAX_IMAGE_BYTES {
        return Err(InvalidImageAttachment {
            message: format!("Image is too large: {} > {MAX_IMAGE_BYTES}", data.len()),
            reason: "too_large",
        });
    }

    let alias = block
        .get("uri")
        .and_then(Value::as_str)
        .and_then(|uri| Path::new(uri).file_name().and_then(|name| name.to_str()))
        .map(str::to_string)
        .unwrap_or_else(|| format!("pasted-image{ext}"));

    Ok(ImageAttachment {
        source: ImageSource::Inline {
            data: BASE64.encode(data),
        },
        alias,
        mime_type: mime_type.to_string(),
    })
}

fn python_base64_error(error: &base64::DecodeError) -> &'static str {
    match error {
        base64::DecodeError::InvalidByte(_, _) => "Only base64 data is allowed",
        base64::DecodeError::InvalidLength(_) | base64::DecodeError::InvalidLastSymbol(_, _) => {
            "Invalid base64-encoded string"
        }
        base64::DecodeError::InvalidPadding => "Incorrect padding",
    }
}

fn extension_for_mime(mime_type: &str) -> Option<&'static str> {
    match mime_type {
        "image/png" => Some(".png"),
        "image/jpeg" => Some(".jpg"),
        "image/gif" => Some(".gif"),
        "image/webp" => Some(".webp"),
        _ => None,
    }
}

fn user_display_content_metadata(params: &Value) -> std::result::Result<Option<Value>, String> {
    let value = params
        .get("_meta")
        .and_then(|meta| meta.get("user_display_content"))
        .or_else(|| params.get("user_display_content"));
    let Some(value) = value else {
        return Ok(None);
    };
    let valid = value.get("version").and_then(Value::as_str).is_some()
        && value.get("host").and_then(Value::as_str).is_some()
        && value.get("content").and_then(Value::as_array).is_some();
    if valid {
        Ok(Some(value.clone()))
    } else {
        Err("Invalid user display content metadata".to_string())
    }
}

fn resource_prompt_text(resource: &Value) -> String {
    let mut parts = Vec::new();
    if let Some(uri) = resource.get("uri").and_then(Value::as_str) {
        parts.push(format!("path: {uri}"));
    }
    if let Some(text) = resource.get("text").and_then(Value::as_str) {
        parts.push(format!("content: {text}"));
    } else if let Some(blob) = resource.get("blob").and_then(Value::as_str) {
        parts.push(format!("content: {blob}"));
    }
    parts.join("\n")
}

fn resource_link_prompt_text(block: &Value) -> String {
    let mut parts = Vec::new();
    for key in ["uri", "name", "title", "description", "mime_type"] {
        if let Some(value) = block.get(key).and_then(Value::as_str) {
            parts.push(format!("{key}: {value}"));
        }
    }
    if let Some(size) = block.get("size").and_then(Value::as_u64) {
        parts.push(format!("size: {size}"));
    }
    parts.join("\n")
}

fn title_from_prompt(prompt: &str) -> String {
    let title = prompt.lines().next().unwrap_or_default().trim();
    if title.is_empty() {
        "New chat".to_string()
    } else {
        title.chars().take(50).collect()
    }
}

fn live_tool_call_updates(call: &ToolCall) -> Vec<Value> {
    if call.name == "todo" {
        return Vec::new();
    }
    if call.name == "bash" {
        return vec![
            json!({
                "_meta": { "tool_name": call.name },
                "kind": "execute",
                "sessionUpdate": "tool_call",
                "status": "pending",
                "title": "",
                "toolCallId": call.id,
            }),
            bash_tool_call_update(call),
        ];
    }
    if call.name == "grep" {
        let mut updates = vec![json!({
            "_meta": { "tool_name": call.name },
            "kind": "search",
            "status": "pending",
            "title": "grep",
            "toolCallId": call.id,
            "sessionUpdate": "tool_call",
        })];
        if let Some(detailed) = grep_tool_call_update(call) {
            updates.push(detailed);
        }
        return updates;
    }
    if call.name == "read" {
        let mut updates = vec![json!({
            "_meta": { "tool_name": call.name },
            "kind": "read",
            "status": "pending",
            "title": "read",
            "toolCallId": call.id,
            "sessionUpdate": "tool_call",
        })];
        if let Some(detailed) = read_tool_call_update(call) {
            updates.push(detailed);
        }
        return updates;
    }
    if call.name == "web_fetch" {
        return vec![
            json!({
                "_meta": { "tool_name": call.name },
                "kind": "fetch",
                "status": "pending",
                "title": "web_fetch",
                "toolCallId": call.id,
                "sessionUpdate": "tool_call",
            }),
            web_fetch_tool_call_update(call),
        ];
    }
    if call.name == "web_search" {
        return vec![
            json!({
                "_meta": { "tool_name": call.name },
                "kind": "search",
                "status": "pending",
                "title": "web_search",
                "toolCallId": call.id,
                "sessionUpdate": "tool_call",
            }),
            web_search_tool_call_update(call),
        ];
    }
    if call.name == "skill" {
        return vec![
            json!({
                "_meta": { "tool_name": call.name },
                "kind": "read",
                "status": "pending",
                "title": "skill",
                "toolCallId": call.id,
                "sessionUpdate": "tool_call",
            }),
            skill_tool_call_update(call),
        ];
    }
    if call.name == "task" {
        return vec![
            json!({
                "_meta": { "tool_name": call.name },
                "kind": "other",
                "status": "pending",
                "title": "task",
                "toolCallId": call.id,
                "sessionUpdate": "tool_call",
            }),
            task_tool_call_update(call),
        ];
    }
    if call.name == "write_file"
        && let Some(update) = write_file_tool_call_update(call)
    {
        return vec![
            json!({
                "_meta": { "tool_name": call.name },
                "kind": "edit",
                "status": "pending",
                "title": "write_file",
                "toolCallId": call.id,
                "sessionUpdate": "tool_call",
            }),
            update,
        ];
    }
    if call.name == "edit"
        && let Some(update) = edit_tool_call_update(call)
    {
        return vec![
            json!({
                "_meta": { "tool_name": call.name },
                "kind": "edit",
                "status": "pending",
                "title": "edit",
                "toolCallId": call.id,
                "sessionUpdate": "tool_call",
            }),
            update,
        ];
    }
    let tool_call = json!({
        "id": call.id,
        "function": {
            "name": call.name,
            "arguments": call.arguments.to_string(),
        },
    });
    let Some(mut update) = replay_tool_call(&tool_call) else {
        return Vec::new();
    };
    update["status"] = Value::String("pending".to_string());
    vec![update]
}

fn bash_tool_call_update(call: &ToolCall) -> Value {
    json!({
        "_meta": { "tool_name": call.name },
        "kind": "execute",
        "rawInput": bash_raw_input(&call.arguments),
        "status": "pending",
        "title": bash_tool_title(&call.arguments),
        "toolCallId": call.id,
        "sessionUpdate": "tool_call",
    })
}

fn bash_raw_input(arguments: &Value) -> String {
    json!({
        "command": arguments.get("command").cloned().unwrap_or(Value::Null),
        "timeout": arguments.get("timeout").cloned().unwrap_or(Value::Null),
    })
    .to_string()
}

fn bash_tool_title(arguments: &Value) -> String {
    let command = arguments
        .get("command")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if let Some(timeout) = arguments.get("timeout").and_then(Value::as_u64) {
        format!("{command} (timeout {timeout}s)")
    } else {
        command.to_string()
    }
}

fn terminal_opened_update(tool_name: &str, tool_call_id: &str, terminal_id: &str) -> Value {
    json!({
        "_meta": { "tool_name": tool_name },
        "content": [{
            "terminalId": terminal_id,
            "type": "terminal",
        }],
        "kind": "execute",
        "status": "in_progress",
        "toolCallId": tool_call_id,
        "sessionUpdate": "tool_call_update",
    })
}

fn grep_tool_call_update(call: &ToolCall) -> Option<Value> {
    let pattern = call.arguments.get("pattern").and_then(Value::as_str)?;
    let path = call
        .arguments
        .get("path")
        .and_then(Value::as_str)
        .unwrap_or(".");
    let search_path = Path::new(path)
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(path))
        .display()
        .to_string();
    Some(json!({
        "_meta": {
            "tool_name": call.name,
            "query": pattern,
            "search_path": search_path,
        },
        "kind": "search",
        "rawInput": grep_raw_input(&call.arguments),
        "status": "pending",
        "title": format!("Grepping '{pattern}'"),
        "toolCallId": call.id,
        "sessionUpdate": "tool_call",
    }))
}

fn read_tool_call_update(call: &ToolCall) -> Option<Value> {
    let path = call.arguments.get("file_path").and_then(Value::as_str)?;
    let resolved = Path::new(path)
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(path))
        .display()
        .to_string();
    let limit = call
        .arguments
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(2_000);
    let offset = call.arguments.get("offset").and_then(Value::as_u64);
    let location = if limit != 2_000 {
        json!({
            "_meta": {
                "type": "file_range",
                "offset": offset.unwrap_or(1),
                "limit": limit,
            },
            "path": resolved,
        })
    } else {
        let mut location = json!({
            "_meta": { "type": "file" },
            "path": resolved,
        });
        if let Some(offset) = offset {
            location["line"] = json!(offset);
        }
        location
    };
    Some(json!({
        "_meta": { "tool_name": call.name },
        "kind": "read",
        "locations": [location],
        "rawInput": read_raw_input(&call.arguments),
        "status": "pending",
        "title": read_tool_title(path, offset, limit),
        "toolCallId": call.id,
        "sessionUpdate": "tool_call",
    }))
}

fn read_raw_input(arguments: &Value) -> String {
    json!({
        "file_path": arguments.get("file_path").cloned().unwrap_or(Value::Null),
        "offset": arguments.get("offset").cloned().unwrap_or(Value::Null),
        "limit": arguments.get("limit").cloned().unwrap_or_else(|| json!(2000)),
    })
    .to_string()
}

fn read_tool_title(path: &str, offset: Option<u64>, limit: u64) -> String {
    let mut title = format!("Reading {path}");
    let mut extras = Vec::new();
    if let Some(offset) = offset {
        extras.push(format!("from line {offset}"));
    }
    if limit != 2_000 {
        extras.push(format!("limit {limit} lines"));
    }
    if !extras.is_empty() {
        title.push_str(&format!(" ({})", extras.join(", ")));
    }
    title
}

fn web_fetch_tool_call_update(call: &ToolCall) -> Value {
    let url = call
        .arguments
        .get("url")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let normalized_url = normalize_fetch_url_for_acp(url);
    json!({
        "_meta": { "tool_name": call.name },
        "kind": "fetch",
        "locations": [{
            "_meta": { "type": "url" },
            "path": normalized_url,
        }],
        "rawInput": web_fetch_raw_input(&call.arguments),
        "status": "pending",
        "title": format!("Fetching: {}", url_host_for_display(&normalized_url)),
        "toolCallId": call.id,
        "sessionUpdate": "tool_call",
    })
}

fn web_fetch_raw_input(arguments: &Value) -> String {
    format!(
        "{{\"url\":{},\"timeout\":{}}}",
        serde_json::to_string(arguments.get("url").unwrap_or(&Value::Null))
            .unwrap_or_else(|_| "null".to_string()),
        arguments.get("timeout").cloned().unwrap_or(Value::Null),
    )
}

fn normalize_fetch_url_for_acp(url: &str) -> String {
    if let Some(stripped) = url.strip_prefix("//") {
        format!("https://{stripped}")
    } else if url.starts_with("http://") || url.starts_with("https://") {
        url.to_string()
    } else {
        format!("https://{url}")
    }
}

fn url_host_for_display(url: &str) -> String {
    let without_scheme = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
        .unwrap_or(url);
    without_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(without_scheme)
        .to_string()
}

fn skill_tool_call_update(call: &ToolCall) -> Value {
    let name = call
        .arguments
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    json!({
        "_meta": {
            "skill_name": name,
            "tool_name": call.name,
        },
        "kind": "read",
        "rawInput": skill_raw_input(&call.arguments),
        "status": "pending",
        "title": format!("Loading skill: {name}"),
        "toolCallId": call.id,
        "sessionUpdate": "tool_call",
    })
}

fn skill_raw_input(arguments: &Value) -> String {
    format!(
        "{{\"name\":{}}}",
        serde_json::to_string(arguments.get("name").unwrap_or(&Value::Null))
            .unwrap_or_else(|_| "null".to_string()),
    )
}

fn web_search_tool_call_update(call: &ToolCall) -> Value {
    let query = call
        .arguments
        .get("query")
        .and_then(Value::as_str)
        .unwrap_or_default();
    json!({
        "_meta": {
            "query": query,
            "tool_name": call.name,
        },
        "kind": "search",
        "rawInput": web_search_raw_input(&call.arguments),
        "status": "pending",
        "title": format!("Searching the web: '{query}'"),
        "toolCallId": call.id,
        "sessionUpdate": "tool_call",
    })
}

fn web_search_raw_input(arguments: &Value) -> String {
    format!(
        "{{\"query\":{}}}",
        serde_json::to_string(arguments.get("query").unwrap_or(&Value::Null))
            .unwrap_or_else(|_| "null".to_string()),
    )
}

fn task_tool_call_update(call: &ToolCall) -> Value {
    let task = call
        .arguments
        .get("task")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let agent = call
        .arguments
        .get("agent")
        .and_then(Value::as_str)
        .unwrap_or("explore");
    json!({
        "_meta": {
            "agent": agent,
            "task": task,
            "tool_name": call.name,
        },
        "kind": "other",
        "rawInput": task_raw_input(&call.arguments),
        "status": "pending",
        "title": format!("Running {agent} agent: {task}"),
        "toolCallId": call.id,
        "sessionUpdate": "tool_call",
    })
}

fn task_raw_input(arguments: &Value) -> String {
    let agent = arguments
        .get("agent")
        .cloned()
        .unwrap_or_else(|| json!("explore"));
    format!(
        "{{\"task\":{},\"agent\":{}}}",
        serde_json::to_string(arguments.get("task").unwrap_or(&Value::Null))
            .unwrap_or_else(|_| "null".to_string()),
        serde_json::to_string(&agent).unwrap_or_else(|_| "\"explore\"".to_string()),
    )
}

fn write_file_tool_call_update(call: &ToolCall) -> Option<Value> {
    let path = call.arguments.get("path").and_then(Value::as_str)?;
    let content = call.arguments.get("content").and_then(Value::as_str)?;
    let resolved = resolve_missing_path(Path::new(path)).display().to_string();
    Some(json!({
        "_meta": { "tool_name": call.name },
        "content": [{
            "type": "diff",
            "path": path,
            "newText": content,
        }],
        "kind": "edit",
        "locations": [{ "path": resolved }],
        "rawInput": write_file_raw_input(&call.arguments),
        "status": "pending",
        "title": format!("Writing {}", path),
        "toolCallId": call.id,
        "sessionUpdate": "tool_call",
    }))
}

fn write_file_raw_input(arguments: &Value) -> String {
    json!({
        "path": arguments.get("path").cloned().unwrap_or(Value::Null),
        "content": arguments.get("content").cloned().unwrap_or(Value::Null),
    })
    .to_string()
}

fn edit_tool_call_update(call: &ToolCall) -> Option<Value> {
    let path = call.arguments.get("file_path").and_then(Value::as_str)?;
    let old = call.arguments.get("old_string").and_then(Value::as_str)?;
    let new = call.arguments.get("new_string").and_then(Value::as_str)?;
    let resolved = Path::new(path)
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(path))
        .display()
        .to_string();
    Some(json!({
        "_meta": { "tool_name": call.name },
        "content": [{
            "type": "diff",
            "path": path,
            "oldText": old,
            "newText": new,
        }],
        "kind": "edit",
        "locations": [{ "path": resolved }],
        "rawInput": edit_raw_input(&call.arguments),
        "status": "pending",
        "title": edit_tool_title(path),
        "toolCallId": call.id,
        "sessionUpdate": "tool_call",
    }))
}

fn edit_raw_input(arguments: &Value) -> String {
    json!({
        "file_path": arguments.get("file_path").cloned().unwrap_or(Value::Null),
        "old_string": arguments.get("old_string").cloned().unwrap_or(Value::Null),
        "new_string": arguments.get("new_string").cloned().unwrap_or(Value::Null),
        "replace_all": arguments.get("replace_all").cloned().unwrap_or_else(|| json!(false)),
    })
    .to_string()
}

fn edit_tool_title(path: &str) -> String {
    let name = Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(path);
    format!("Editing {name}")
}

fn grep_raw_input(arguments: &Value) -> String {
    json!({
        "pattern": arguments.get("pattern").cloned().unwrap_or(Value::Null),
        "path": arguments.get("path").cloned().unwrap_or_else(|| json!(".")),
        "max_matches": arguments.get("max_matches").cloned().unwrap_or(Value::Null),
        "use_default_ignore": arguments.get("use_default_ignore").cloned().unwrap_or_else(|| json!(true)),
    })
    .to_string()
}

fn live_tool_result_update(result: &ToolResult) -> Value {
    if result.name == "bash" {
        return bash_tool_result_update(result);
    }
    if result.name == "grep" {
        return grep_tool_result_update(result);
    }
    if result.name == "read" {
        return read_tool_result_update(result);
    }
    if result.name == "web_fetch" {
        return web_fetch_tool_result_update(result);
    }
    if result.name == "web_search" {
        return web_search_tool_result_update(result);
    }
    if result.name == "skill" {
        return skill_tool_result_update(result);
    }
    if result.name == "task" {
        return task_tool_result_update(result);
    }
    if result.name == "write_file" {
        return write_file_tool_result_update(result);
    }
    if result.name == "edit" {
        return edit_tool_result_update(result);
    }
    if result.name == "todo" {
        return todo_tool_result_update(result);
    }
    json!({
        "_meta": { "tool_name": result.name },
        "content": [{
            "content": {
                "text": result.output,
                "type": "text",
            },
            "type": "content",
        }],
        "kind": tool_kind(&result.name),
        "rawOutput": result.output,
        "status": if result.success { "completed" } else { "failed" },
        "toolCallId": result.call_id,
        "sessionUpdate": "tool_call_update",
    })
}

fn bash_tool_result_update(result: &ToolResult) -> Value {
    if !result.success {
        let raw_output = if result.output.starts_with("<tool_error>") {
            result.output.clone()
        } else {
            format!("<tool_error>bash failed: {}</tool_error>", result.output)
        };
        return json!({
            "_meta": { "tool_name": result.name },
            "kind": "execute",
            "rawOutput": raw_output,
            "status": "failed",
            "toolCallId": result.call_id,
            "sessionUpdate": "tool_call_update",
        });
    }
    let command = parse_bash_output_command(&result.output);
    json!({
        "_meta": { "tool_name": result.name },
        "content": [{
            "content": {
                "text": format!("Ran {command}"),
                "type": "text",
            },
            "type": "content",
        }],
        "kind": "execute",
        "status": "completed",
        "toolCallId": result.call_id,
        "sessionUpdate": "tool_call_update",
    })
}

fn todo_tool_result_update(result: &ToolResult) -> Value {
    if !result.success {
        let raw_output = if result.output.starts_with("<tool_error>") {
            result.output.clone()
        } else {
            format!("<tool_error>todo failed: {}</tool_error>", result.output)
        };
        return json!({
            "_meta": { "tool_name": result.name },
            "kind": "other",
            "rawOutput": raw_output,
            "status": "failed",
            "toolCallId": result.call_id,
            "sessionUpdate": "tool_call_update",
        });
    }
    json!({
        "entries": todo_plan_entries(&result.output),
        "sessionUpdate": "plan",
    })
}

fn todo_plan_entries(output: &str) -> Vec<Value> {
    let Some(raw_todos) = output
        .lines()
        .find_map(|line| line.strip_prefix("todos: "))
        .map(str::trim)
    else {
        return Vec::new();
    };
    if raw_todos == "[]" {
        return Vec::new();
    }
    raw_todos
        .trim_start_matches('[')
        .trim_end_matches(']')
        .split("}, {")
        .filter_map(todo_plan_entry)
        .collect()
}

fn todo_plan_entry(raw: &str) -> Option<Value> {
    let item = raw.trim().trim_start_matches('{').trim_end_matches('}');
    let content = todo_quoted_field(item, "content")?;
    let status = todo_enum_field(item, "status").unwrap_or_else(|| "pending".to_string());
    let priority = todo_enum_field(item, "priority").unwrap_or_else(|| "medium".to_string());
    Some(json!({
        "content": content,
        "priority": priority,
        "status": status,
    }))
}

fn todo_quoted_field(item: &str, field: &str) -> Option<String> {
    let marker = format!("'{field}': '");
    let rest = item.split_once(&marker)?.1;
    let value = rest.split_once('\'')?.0;
    Some(value.to_string())
}

fn todo_enum_field(item: &str, field: &str) -> Option<String> {
    let marker = format!("'{field}': <Todo");
    let rest = item.split_once(&marker)?.1;
    let (_, value_part) = rest.split_once(": '")?;
    let value = value_part.split_once('\'')?.0;
    Some(value.to_string())
}

fn parse_bash_output_command(output: &str) -> String {
    output
        .strip_prefix("command: ")
        .and_then(|rest| rest.split_once("\nstdout: "))
        .map(|(command, _)| command.to_string())
        .unwrap_or_default()
}

fn web_fetch_tool_result_update(result: &ToolResult) -> Value {
    if !result.success {
        return json!({
            "_meta": { "tool_name": result.name },
            "kind": "fetch",
            "rawOutput": result.output,
            "status": "failed",
            "toolCallId": result.call_id,
            "sessionUpdate": "tool_call_update",
        });
    }
    let parsed = parse_web_fetch_output(&result.output);
    let display_content_type = parsed
        .content_type
        .split(';')
        .next()
        .unwrap_or(&parsed.content_type)
        .trim();
    let char_count = parsed.content.chars().count();
    json!({
        "_meta": { "tool_name": result.name },
        "content": [{
            "content": {
                "text": format!("Fetched {} ({} chars, {})", parsed.url, char_count, display_content_type),
                "type": "text",
            },
            "type": "content",
        }],
        "kind": "fetch",
        "locations": [{
            "_meta": {
                "char_count": char_count,
                "truncated": parsed.was_truncated,
                "type": "url",
            },
            "path": parsed.url,
        }],
        "rawOutput": parsed.to_raw_output(),
        "status": "completed",
        "toolCallId": result.call_id,
        "sessionUpdate": "tool_call_update",
    })
}

struct WebFetchOutput {
    url: String,
    content: String,
    content_type: String,
    was_truncated: bool,
}

impl WebFetchOutput {
    fn to_raw_output(&self) -> String {
        format!(
            "{{\"url\":{},\"content\":{},\"content_type\":{},\"was_truncated\":{}}}",
            serde_json::to_string(&self.url).unwrap_or_else(|_| "\"\"".to_string()),
            serde_json::to_string(&self.content).unwrap_or_else(|_| "\"\"".to_string()),
            serde_json::to_string(&self.content_type).unwrap_or_else(|_| "\"\"".to_string()),
            self.was_truncated,
        )
    }
}

fn parse_web_fetch_output(output: &str) -> WebFetchOutput {
    let url = output
        .strip_prefix("url: ")
        .and_then(|rest| rest.split_once("\ncontent: "))
        .map(|(url, _)| url.to_string())
        .unwrap_or_default();
    let content = output
        .split_once("\ncontent: ")
        .and_then(|(_, rest)| rest.split_once("\ncontent_type: "))
        .map(|(content, _)| content.to_string())
        .unwrap_or_default();
    let content_type = output
        .split_once("\ncontent_type: ")
        .and_then(|(_, rest)| rest.split_once("\nwas_truncated: "))
        .map(|(content_type, _)| content_type.to_string())
        .unwrap_or_default();
    let was_truncated = output
        .lines()
        .find_map(|line| line.strip_prefix("was_truncated: "))
        .is_some_and(|value| value == "True");
    WebFetchOutput {
        url,
        content,
        content_type,
        was_truncated,
    }
}

fn skill_tool_result_update(result: &ToolResult) -> Value {
    let parsed = parse_skill_output(&result.output);
    if !result.success {
        return json!({
            "_meta": {
                "skill_name": parsed.name,
                "tool_name": result.name,
            },
            "kind": "read",
            "rawOutput": result.output,
            "status": "failed",
            "toolCallId": result.call_id,
            "sessionUpdate": "tool_call_update",
        });
    }
    json!({
        "_meta": {
            "skill_name": parsed.name,
            "tool_name": result.name,
        },
        "content": [{
            "content": {
                "text": format!("Loaded skill: {}", parsed.name),
                "type": "text",
            },
            "type": "content",
        }],
        "kind": "read",
        "locations": [{ "path": parsed.skill_dir }],
        "rawOutput": parsed.to_raw_output(),
        "status": "completed",
        "toolCallId": result.call_id,
        "sessionUpdate": "tool_call_update",
    })
}

struct SkillOutput {
    name: String,
    content: String,
    skill_dir: String,
}

impl SkillOutput {
    fn to_raw_output(&self) -> String {
        format!(
            "{{\"name\":{},\"content\":{},\"skill_dir\":{}}}",
            serde_json::to_string(&self.name).unwrap_or_else(|_| "\"\"".to_string()),
            serde_json::to_string(&self.content).unwrap_or_else(|_| "\"\"".to_string()),
            serde_json::to_string(&self.skill_dir).unwrap_or_else(|_| "\"\"".to_string()),
        )
    }
}

fn parse_skill_output(output: &str) -> SkillOutput {
    let name = output
        .strip_prefix("name: ")
        .and_then(|rest| rest.split_once("\ncontent: "))
        .map(|(name, _)| name.to_string())
        .unwrap_or_default();
    let content = output
        .split_once("\ncontent: ")
        .and_then(|(_, rest)| rest.split_once("\nskill_dir: "))
        .map(|(content, _)| content.to_string())
        .unwrap_or_default();
    let skill_dir = output
        .split_once("\nskill_dir: ")
        .map(|(_, skill_dir)| skill_dir.to_string())
        .unwrap_or_default();
    SkillOutput {
        name,
        content,
        skill_dir,
    }
}

fn web_search_tool_result_update(result: &ToolResult) -> Value {
    if !result.success {
        return json!({
            "_meta": { "tool_name": result.name },
            "kind": "search",
            "rawOutput": result.output,
            "status": "failed",
            "toolCallId": result.call_id,
            "sessionUpdate": "tool_call_update",
        });
    }
    let parsed = parse_web_search_output(&result.output);
    let source_count = parsed.sources.len();
    let plural = if source_count == 1 { "" } else { "s" };
    let locations = parsed
        .sources
        .iter()
        .map(|source| {
            json!({
                "_meta": {
                    "title": source.title,
                    "type": "url",
                },
                "path": source.url,
            })
        })
        .collect::<Vec<_>>();
    let mut update = json!({
        "_meta": { "tool_name": result.name },
        "content": [{
            "content": {
                "text": format!("Searched '{}' ({} source{})", parsed.query, source_count, plural),
                "type": "text",
            },
            "type": "content",
        }],
        "kind": "search",
        "rawOutput": parsed.to_raw_output(),
        "status": "completed",
        "toolCallId": result.call_id,
        "sessionUpdate": "tool_call_update",
    });
    if !locations.is_empty() {
        update["locations"] = Value::Array(locations);
    }
    update
}

struct WebSearchSource {
    title: String,
    url: String,
}

struct WebSearchOutput {
    query: String,
    answer: String,
    sources: Vec<WebSearchSource>,
}

impl WebSearchOutput {
    fn to_raw_output(&self) -> String {
        let sources = self
            .sources
            .iter()
            .map(|source| {
                format!(
                    "{{\"title\":{},\"url\":{}}}",
                    serde_json::to_string(&source.title).unwrap_or_else(|_| "\"\"".to_string()),
                    serde_json::to_string(&source.url).unwrap_or_else(|_| "\"\"".to_string()),
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        format!(
            "{{\"query\":{},\"answer\":{},\"sources\":[{}]}}",
            serde_json::to_string(&self.query).unwrap_or_else(|_| "\"\"".to_string()),
            serde_json::to_string(&self.answer).unwrap_or_else(|_| "\"\"".to_string()),
            sources,
        )
    }
}

fn parse_web_search_output(output: &str) -> WebSearchOutput {
    let query = output
        .strip_prefix("query: ")
        .and_then(|rest| rest.split_once("\nanswer: "))
        .map(|(query, _)| query.to_string())
        .unwrap_or_default();
    let answer = output
        .split_once("\nanswer: ")
        .and_then(|(_, rest)| rest.split_once("\nsources: "))
        .map(|(answer, _)| answer.to_string())
        .unwrap_or_default();
    let sources_raw = output
        .split_once("\nsources: ")
        .map(|(_, sources)| sources)
        .unwrap_or_default();
    WebSearchOutput {
        query,
        answer,
        sources: parse_web_search_sources(sources_raw),
    }
}

fn parse_web_search_sources(raw: &str) -> Vec<WebSearchSource> {
    raw.split("{'title': ")
        .skip(1)
        .filter_map(|rest| {
            let (title_raw, rest) = rest.split_once(", 'url': ")?;
            let (url_raw, _) = rest.split_once('}')?;
            Some(WebSearchSource {
                title: trim_python_string_repr(title_raw).to_string(),
                url: trim_python_string_repr(url_raw).to_string(),
            })
        })
        .collect()
}

fn trim_python_string_repr(value: &str) -> &str {
    value.trim().trim_start_matches('\'').trim_end_matches('\'')
}

fn task_tool_result_update(result: &ToolResult) -> Value {
    let parsed = parse_task_output(&result.output);
    if !result.success {
        return json!({
            "_meta": { "tool_name": result.name },
            "kind": "other",
            "rawOutput": result.output,
            "status": "failed",
            "toolCallId": result.call_id,
            "sessionUpdate": "tool_call_update",
        });
    }
    let turn_word = if parsed.turns_used == 1 {
        "turn"
    } else {
        "turns"
    };
    let message = if parsed.completed {
        format!("Agent completed in {} {}", parsed.turns_used, turn_word)
    } else {
        format!(
            "Agent interrupted after {} {}",
            parsed.turns_used, turn_word
        )
    };
    json!({
        "_meta": {
            "response": parsed.response,
            "tool_name": result.name,
            "turn_count": parsed.turns_used,
        },
        "content": [{
            "content": {
                "text": message,
                "type": "text",
            },
            "type": "content",
        }],
        "kind": "other",
        "rawOutput": parsed.to_raw_output(),
        "status": if parsed.completed { "completed" } else { "failed" },
        "toolCallId": result.call_id,
        "sessionUpdate": "tool_call_update",
    })
}

struct TaskOutput {
    response: String,
    turns_used: u64,
    completed: bool,
}

impl TaskOutput {
    fn to_raw_output(&self) -> String {
        format!(
            "{{\"response\":{},\"turns_used\":{},\"completed\":{}}}",
            serde_json::to_string(&self.response).unwrap_or_else(|_| "\"\"".to_string()),
            self.turns_used,
            self.completed,
        )
    }
}

fn parse_task_output(output: &str) -> TaskOutput {
    let response = output
        .strip_prefix("response: ")
        .and_then(|rest| rest.split_once("\nturns_used: "))
        .map(|(response, _)| response.to_string())
        .unwrap_or_default();
    let turns_used = output
        .split_once("\nturns_used: ")
        .and_then(|(_, rest)| rest.split_once("\ncompleted: "))
        .and_then(|(turns, _)| turns.parse().ok())
        .unwrap_or(0);
    let completed = output
        .lines()
        .find_map(|line| line.strip_prefix("completed: "))
        .is_some_and(|value| value == "True");
    TaskOutput {
        response,
        turns_used,
        completed,
    }
}

fn write_file_tool_result_update(result: &ToolResult) -> Value {
    if !result.success {
        return json!({
            "_meta": { "tool_name": result.name },
            "kind": "edit",
            "rawOutput": result.output,
            "status": "failed",
            "toolCallId": result.call_id,
            "sessionUpdate": "tool_call_update",
        });
    }
    let parsed = parse_write_file_output(&result.output);
    json!({
        "_meta": { "tool_name": result.name },
        "content": [{
            "type": "diff",
            "path": parsed.path,
            "newText": parsed.content,
        }],
        "kind": "edit",
        "locations": [{ "path": parsed.path }],
        "rawOutput": parsed.to_raw_output(),
        "status": "completed",
        "toolCallId": result.call_id,
        "sessionUpdate": "tool_call_update",
    })
}

fn edit_tool_result_update(result: &ToolResult) -> Value {
    if !result.success {
        return json!({
            "_meta": { "tool_name": result.name },
            "kind": "edit",
            "rawOutput": result.output,
            "status": "failed",
            "toolCallId": result.call_id,
            "sessionUpdate": "tool_call_update",
        });
    }
    let parsed = parse_edit_output(&result.output);
    json!({
        "_meta": { "tool_name": result.name },
        "content": [{
            "type": "diff",
            "path": parsed.file,
            "oldText": parsed.old_string,
            "newText": parsed.new_string,
        }],
        "kind": "edit",
        "locations": [{ "path": parsed.file }],
        "rawOutput": parsed.to_raw_output(),
        "status": "completed",
        "toolCallId": result.call_id,
        "sessionUpdate": "tool_call_update",
    })
}

struct EditOutput {
    file: String,
    message: String,
    old_string: String,
    new_string: String,
}

impl EditOutput {
    fn to_raw_output(&self) -> String {
        format!(
            "{{\"file\":{},\"message\":{},\"old_string\":{},\"new_string\":{}}}",
            serde_json::to_string(&self.file).unwrap_or_else(|_| "\"\"".to_string()),
            serde_json::to_string(&self.message).unwrap_or_else(|_| "\"\"".to_string()),
            serde_json::to_string(&self.old_string).unwrap_or_else(|_| "\"\"".to_string()),
            serde_json::to_string(&self.new_string).unwrap_or_else(|_| "\"\"".to_string()),
        )
    }
}

fn parse_edit_output(output: &str) -> EditOutput {
    let file = output
        .strip_prefix("file: ")
        .and_then(|rest| rest.split_once("\nmessage: "))
        .map(|(file, _)| file.to_string())
        .unwrap_or_default();
    let message = output
        .split_once("\nmessage: ")
        .and_then(|(_, rest)| rest.split_once("\nold_string: "))
        .map(|(message, _)| message.to_string())
        .unwrap_or_default();
    let old_string = output
        .split_once("\nold_string: ")
        .and_then(|(_, rest)| rest.split_once("\nnew_string: "))
        .map(|(old, _)| old.to_string())
        .unwrap_or_default();
    let new_string = output
        .split_once("\nnew_string: ")
        .map(|(_, new)| new.to_string())
        .unwrap_or_default();
    EditOutput {
        file,
        message,
        old_string,
        new_string,
    }
}

struct WriteFileOutput {
    path: String,
    bytes_written: u64,
    content: String,
}

impl WriteFileOutput {
    fn to_raw_output(&self) -> String {
        format!(
            "{{\"path\":{},\"bytes_written\":{},\"content\":{}}}",
            serde_json::to_string(&self.path).unwrap_or_else(|_| "\"\"".to_string()),
            self.bytes_written,
            serde_json::to_string(&self.content).unwrap_or_else(|_| "\"\"".to_string()),
        )
    }
}

fn parse_write_file_output(output: &str) -> WriteFileOutput {
    let path = output
        .strip_prefix("path: ")
        .and_then(|rest| rest.split_once("\nbytes_written: "))
        .map(|(path, _)| path.to_string())
        .unwrap_or_default();
    let bytes_written = output
        .split_once("\nbytes_written: ")
        .and_then(|(_, rest)| rest.split_once("\ncontent: "))
        .and_then(|(bytes, _)| bytes.parse().ok())
        .unwrap_or(0);
    let content = output
        .split_once("\ncontent: ")
        .map(|(_, content)| content.to_string())
        .unwrap_or_default();
    WriteFileOutput {
        path,
        bytes_written,
        content,
    }
}

fn read_tool_result_update(result: &ToolResult) -> Value {
    if !result.success {
        return json!({
            "_meta": { "tool_name": result.name },
            "kind": "read",
            "rawOutput": result.output,
            "status": "failed",
            "toolCallId": result.call_id,
            "sessionUpdate": "tool_call_update",
        });
    }
    let parsed = parse_read_output(&result.output);
    let path = Path::new(&parsed.file_path);
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(&parsed.file_path);
    let location = read_result_location(&parsed);
    json!({
        "_meta": { "tool_name": result.name },
        "content": [{
            "content": {
                "text": format!("Read from {file_name}"),
                "type": "text",
            },
            "type": "content",
        }],
        "kind": "read",
        "locations": [location],
        "rawOutput": parsed.to_raw_output(),
        "status": "completed",
        "toolCallId": result.call_id,
        "sessionUpdate": "tool_call_update",
    })
}

#[derive(Debug, Clone)]
struct ReadOutput {
    file_path: String,
    content: String,
    num_lines: u64,
    start_line: u64,
    requested_offset: Option<u64>,
    requested_limit: u64,
    total_lines: Option<u64>,
    was_truncated: bool,
}

impl ReadOutput {
    fn to_raw_output(&self) -> String {
        json!({
            "file_path": self.file_path,
            "content": self.content,
            "num_lines": self.num_lines,
            "start_line": self.start_line,
            "requested_offset": self.requested_offset,
            "requested_limit": self.requested_limit,
            "total_lines": self.total_lines,
            "was_truncated": self.was_truncated,
        })
        .to_string()
    }
}

fn parse_read_output(output: &str) -> ReadOutput {
    let file_path = output
        .strip_prefix("file_path: ")
        .and_then(|rest| rest.split_once("\ncontent: "))
        .map(|(path, _)| path.to_string())
        .unwrap_or_default();
    let content = output
        .split_once("\ncontent: ")
        .and_then(|(_, rest)| rest.split_once("\nnum_lines: "))
        .map(|(content, _)| content.to_string())
        .unwrap_or_default();
    ReadOutput {
        file_path,
        content,
        num_lines: read_output_u64(output, "num_lines").unwrap_or(0),
        start_line: read_output_u64(output, "start_line").unwrap_or(1),
        requested_offset: read_output_optional_u64(output, "requested_offset"),
        requested_limit: read_output_u64(output, "requested_limit").unwrap_or(2_000),
        total_lines: read_output_optional_u64(output, "total_lines"),
        was_truncated: read_output_bool(output, "was_truncated"),
    }
}

fn read_output_u64(output: &str, key: &str) -> Option<u64> {
    output
        .lines()
        .find_map(|line| line.strip_prefix(&format!("{key}: ")))
        .and_then(|value| value.parse().ok())
}

fn read_output_optional_u64(output: &str, key: &str) -> Option<u64> {
    output
        .lines()
        .find_map(|line| line.strip_prefix(&format!("{key}: ")))
        .and_then(|value| (value != "None").then_some(value))
        .and_then(|value| value.parse().ok())
}

fn read_output_bool(output: &str, key: &str) -> bool {
    output
        .lines()
        .find_map(|line| line.strip_prefix(&format!("{key}: ")))
        .is_some_and(|value| value == "True")
}

fn read_result_location(parsed: &ReadOutput) -> Value {
    let bounded = parsed.requested_limit != 2_000 || parsed.was_truncated;
    if bounded {
        return json!({
            "_meta": {
                "type": "file_range",
                "offset": parsed.start_line,
                "limit": parsed.num_lines,
            },
            "path": parsed.file_path,
        });
    }
    let mut location = json!({
        "_meta": { "type": "file" },
        "path": parsed.file_path,
    });
    if let Some(offset) = parsed.requested_offset {
        location["line"] = json!(offset);
    }
    location
}

fn grep_tool_result_update(result: &ToolResult) -> Value {
    if !result.success && !looks_like_grep_output(&result.output) {
        return json!({
            "_meta": { "tool_name": result.name },
            "kind": "search",
            "rawOutput": result.output,
            "status": "failed",
            "toolCallId": result.call_id,
            "sessionUpdate": "tool_call_update",
        });
    }
    let parsed = parse_grep_output(&result.output);
    let content = if parsed.match_count == 1 {
        "Found 1 matches".to_string()
    } else {
        format!("Found {} matches", parsed.match_count)
    };
    let locations = parsed
        .locations
        .iter()
        .map(|(path, line)| json!({ "line": line, "path": path }))
        .collect::<Vec<_>>();
    let mut update = json!({
        "_meta": { "tool_name": result.name },
        "content": [{
            "content": {
                "text": content,
                "type": "text",
            },
            "type": "content",
        }],
        "kind": "search",
        "rawOutput": json!({
            "matches": parsed.matches,
            "match_count": parsed.match_count,
            "was_truncated": parsed.was_truncated,
        }).to_string(),
        "status": if result.success { "completed" } else { "failed" },
        "toolCallId": result.call_id,
        "sessionUpdate": "tool_call_update",
    });
    if !locations.is_empty() {
        update["locations"] = Value::Array(locations);
    }
    update
}

fn looks_like_grep_output(output: &str) -> bool {
    output.lines().any(|line| {
        line.starts_with("matches: ")
            || line.starts_with("match_count: ")
            || line.starts_with("was_truncated: ")
    })
}

struct GrepOutput {
    matches: String,
    match_count: u64,
    was_truncated: bool,
    locations: Vec<(String, u64)>,
}

fn parse_grep_output(output: &str) -> GrepOutput {
    let mut matches = String::new();
    let mut match_count = 0;
    let mut was_truncated = false;
    for line in output.lines() {
        if let Some(value) = line.strip_prefix("matches: ") {
            matches = normalize_grep_matches(value);
        } else if let Some(value) = line.strip_prefix("match_count: ") {
            match_count = value.trim().parse().unwrap_or(0);
        } else if let Some(value) = line.strip_prefix("was_truncated: ") {
            was_truncated = value.trim().eq_ignore_ascii_case("true");
        }
    }
    let locations = matches
        .lines()
        .filter_map(|line| {
            let (path, rest) = line.split_once(':')?;
            let (line_no, _) = rest.split_once(':')?;
            let line_no = line_no.parse().ok()?;
            let absolute = Path::new(path)
                .canonicalize()
                .unwrap_or_else(|_| PathBuf::from(path))
                .display()
                .to_string();
            Some((absolute, line_no))
        })
        .collect();
    GrepOutput {
        matches,
        match_count,
        was_truncated,
        locations,
    }
}

fn normalize_grep_matches(raw: &str) -> String {
    let cwd = std::env::current_dir().ok();
    raw.lines()
        .map(|line| {
            let Some((path, rest)) = line.split_once(':') else {
                return line.to_string();
            };
            let normalized = if let Some(cwd) = cwd.as_ref() {
                Path::new(path)
                    .strip_prefix(cwd)
                    .ok()
                    .map(|relative| format!("./{}", relative.display()))
                    .unwrap_or_else(|| path.to_string())
            } else {
                path.to_string()
            };
            normalized.replace("/./", "/") + ":" + rest
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[derive(Debug, Clone, Copy)]
struct AcpUsagePricing {
    input_price: f64,
    output_price: f64,
}

fn session_usage_pricing(session: &Session) -> AcpUsagePricing {
    let config = session.agent.config();
    AcpUsagePricing {
        input_price: config.model.input_price,
        output_price: config.model.output_price,
    }
}

fn usage_update(usage: &Usage, size: u64, pricing: AcpUsagePricing) -> Value {
    let mut update = json!({
        "sessionUpdate": "usage_update",
        "size": size,
        "used": usage.input_tokens + usage.output_tokens,
    });
    let cost = (usage.input_tokens as f64 / 1_000_000.0) * pricing.input_price
        + (usage.output_tokens as f64 / 1_000_000.0) * pricing.output_price;
    if cost > 0.0 {
        update["cost"] = json!({
            "amount": cost,
            "currency": "USD",
        });
    }
    update
}

fn subtract_usage(current: &Usage, previous: &Usage) -> Usage {
    Usage {
        input_tokens: current.input_tokens.saturating_sub(previous.input_tokens),
        output_tokens: current.output_tokens.saturating_sub(previous.output_tokens),
    }
}

fn load_usage_update() -> Value {
    json!({
        "cost": {
            "amount": 0.0,
            "currency": "USD",
        },
        "sessionUpdate": "usage_update",
        "size": 200_000,
        "used": 0,
    })
}

fn message_id_from(message: &Value) -> String {
    message
        .get("message_id")
        .or_else(|| message.get("messageId"))
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string())
}

fn tool_kind(tool_name: &str) -> &'static str {
    match tool_name {
        "read" => "read",
        "grep" | "web_search" => "search",
        "web_fetch" => "fetch",
        "write_file" | "edit" => "edit",
        "bash" => "execute",
        "skill" => "read",
        _ => "other",
    }
}

fn default_test_safe_config() -> Config {
    toml::from_str(
        r#"
[model]
provider = "mistral"
name = "mistral-medium-3.5[high]"
temperature = 0.1
max_context_tokens = 200000
input_price = 1.5
output_price = 7.5

[providers.mistral]
base_url = "https://api.mistral.ai/v1"
api_key_env = "MISTRAL_API_KEY"
wire_format = "openai_chat"

[permissions]
mode = "ask"
"#,
    )
    .expect("fallback ACP config must parse")
}

fn normalize_timestamp(timestamp: &str) -> Option<String> {
    DateTime::parse_from_rfc3339(timestamp)
        .ok()
        .map(|timestamp| {
            timestamp
                .with_timezone(&Utc)
                .to_rfc3339_opts(SecondsFormat::AutoSi, false)
        })
}

fn current_dir_string() -> String {
    std::env::current_dir()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|_| ".".to_string())
}

fn resolve_path(path: &str) -> String {
    let path = Path::new(path);
    path.canonicalize()
        .unwrap_or_else(|_| path.to_path_buf())
        .display()
        .to_string()
}

fn models_state_for_current(config: &Config, current: &str) -> Value {
    if !config.models.is_empty() {
        return json!({
            "availableModels": config.models.iter().map(|model| {
                json!({ "modelId": model.alias, "name": model.alias })
            }).collect::<Vec<_>>(),
            "currentModelId": current,
        });
    }
    json!({
        "availableModels": [
            { "modelId": "mistral-medium-3.5", "name": "mistral-medium-3.5" },
            { "modelId": "devstral-small", "name": "devstral-small" },
            { "modelId": "local", "name": "local" },
        ],
        "currentModelId": current,
    })
}

fn modes_state_for(current_mode: &str) -> Value {
    json!({
        "availableModes": mode_values().into_iter().map(|(id, name, description)| {
            json!({ "description": description, "id": id, "name": name })
        }).collect::<Vec<_>>(),
        "currentModeId": current_mode,
    })
}

fn config_options_for_state(
    config: &Config,
    current_mode: &str,
    current_model: &str,
    thinking: &str,
) -> Vec<Value> {
    let model_options = if config.models.is_empty() {
        vec![
            json!({ "description": "mistral-vibe-cli-latest", "name": "mistral-medium-3.5", "value": "mistral-medium-3.5" }),
            json!({ "description": "devstral-small-latest", "name": "devstral-small", "value": "devstral-small" }),
            json!({ "description": "devstral", "name": "local", "value": "local" }),
        ]
    } else {
        config
            .models
            .iter()
            .map(|model| json!({ "description": model.name, "name": model.alias, "value": model.alias }))
            .collect::<Vec<_>>()
    };
    vec![
        json!({
            "currentValue": current_mode,
            "options": mode_values().into_iter().map(|(id, name, description)| {
                json!({ "description": description, "name": name, "value": id })
            }).collect::<Vec<_>>(),
            "category": "mode",
            "id": "mode",
            "name": "Session Mode",
            "type": "select",
        }),
        json!({
            "currentValue": current_model,
            "options": model_options,
            "category": "model",
            "id": "model",
            "name": "Model",
            "type": "select",
        }),
        json!({
            "currentValue": thinking,
            "options": [
                { "name": "Off", "value": "off" },
                { "name": "Low", "value": "low" },
                { "name": "Medium", "value": "medium" },
                { "name": "High", "value": "high" },
                { "name": "Max", "value": "max" },
            ],
            "category": "thinking",
            "id": "thinking",
            "name": "Thinking",
            "type": "select",
        }),
    ]
}

fn current_model_alias(config: &Config) -> String {
    config.active_model.clone().unwrap_or_else(|| {
        if config.model.name.starts_with("mistral-medium-3.5") {
            "mistral-medium-3.5".to_string()
        } else {
            config
                .model
                .name
                .split('[')
                .next()
                .unwrap_or(&config.model.name)
                .to_string()
        }
    })
}

fn current_thinking(config: &Config) -> String {
    let current = current_model_alias(config);
    config
        .models
        .iter()
        .find(|model| model.alias == current)
        .map(|model| model.thinking.clone())
        .unwrap_or_else(|| {
            config
                .model
                .name
                .split_once('[')
                .and_then(|(_, rest)| rest.strip_suffix(']'))
                .unwrap_or("high")
                .to_string()
        })
}

fn is_valid_mode(mode_id: &str) -> bool {
    mode_values().iter().any(|(id, _, _)| *id == mode_id)
}

fn is_valid_model(config: &Config, model_id: &str) -> bool {
    if model_id.trim().is_empty() {
        return false;
    }
    if config.models.is_empty() {
        matches!(model_id, "mistral-medium-3.5" | "devstral-small" | "local")
    } else {
        config.models.iter().any(|model| model.alias == model_id)
    }
}

fn is_valid_thinking(thinking: &str) -> bool {
    matches!(thinking, "off" | "low" | "medium" | "high" | "max")
}

fn mode_values() -> Vec<(&'static str, &'static str, &'static str)> {
    vec![
        (
            "default",
            "Default",
            "Requires approval for tool executions",
        ),
        (
            "plan",
            "Plan",
            "Read-only agent for exploration and planning",
        ),
        (
            "accept-edits",
            "Accept Edits",
            "Auto-approves file edits only",
        ),
        (
            "auto-approve",
            "Auto Approve",
            "Auto-approves all tool executions",
        ),
        (
            "chat",
            "Chat",
            "Read-only conversational mode for questions and discussions",
        ),
    ]
}

struct WorkspaceTrustPrompt {
    cwd: PathBuf,
    repo_root: Option<PathBuf>,
    detected_files: Vec<String>,
    repo_detected_files: Vec<String>,
    offer_repo_trust: bool,
}

fn workspace_trust_response(cwd: &Path) -> Value {
    json!({
        "trust_status": workspace_trust_status_string(cwd),
        "details": workspace_trust_details(cwd),
    })
}

fn workspace_trust_status_string(cwd: &Path) -> &'static str {
    let mut current = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let raw = std::fs::read_to_string(trusted_folders_file()).unwrap_or_default();
    loop {
        let normalized = current.display().to_string();
        if trust_section_contains(&raw, "trusted", &normalized) {
            return "trusted";
        }
        if trust_section_contains(&raw, "untrusted", &normalized) {
            return "untrusted";
        }
        let Some(parent) = current.parent() else {
            return "untrusted";
        };
        if parent == current {
            return "untrusted";
        }
        current = parent.to_path_buf();
    }
}

fn workspace_trust_details(cwd: &Path) -> Value {
    if workspace_trust_status_string(cwd) != "untrusted" {
        return Value::Null;
    }
    let Some(prompt) = workspace_trust_prompt(cwd, true) else {
        return Value::Null;
    };

    let mut ignored = BTreeMap::new();
    for file in &prompt.repo_detected_files {
        ignored.insert(file.clone(), ());
    }
    match &prompt.repo_root {
        Some(repo_root) => {
            let cwd_relative = prompt
                .cwd
                .strip_prefix(repo_root)
                .ok()
                .map(Path::to_path_buf)
                .unwrap_or_default();
            let cwd_prefix = if cwd_relative.as_os_str().is_empty() {
                String::new()
            } else {
                cwd_relative.display().to_string()
            };
            for file in &prompt.detected_files {
                if cwd_prefix.is_empty() {
                    ignored.insert(file.clone(), ());
                } else {
                    ignored.insert(format!("{cwd_prefix}/{file}"), ());
                }
            }
        }
        None => {
            for file in &prompt.detected_files {
                ignored.insert(file.clone(), ());
            }
        }
    }

    json!({
        "cwd": prompt.cwd.display().to_string(),
        "repoRoot": prompt.repo_root.as_ref().map(|path| path.display().to_string()),
        "ignoredFiles": ignored.into_keys().collect::<Vec<_>>(),
        "availableDecisions": workspace_trust_available_decisions(&prompt),
    })
}

fn workspace_trust_prompt(
    cwd: &Path,
    include_explicitly_untrusted: bool,
) -> Option<WorkspaceTrustPrompt> {
    let cwd = cwd.canonicalize().ok()?;
    if dirs::home_dir()
        .and_then(|home| home.canonicalize().ok())
        .is_some_and(|home| home == cwd)
    {
        return None;
    }
    if workspace_trust_status_string(&cwd) == "trusted" {
        return None;
    }
    if !include_explicitly_untrusted && workspace_explicitly_untrusted(&cwd) {
        return None;
    }

    let repo_root = find_git_repo_ancestor(&cwd);
    let detected_files = find_trustable_files(&cwd);
    let repo_detected_files = find_repo_trustable_files_for_cwd(&cwd, repo_root.as_deref());
    if detected_files.is_empty() && repo_detected_files.is_empty() {
        return None;
    }

    let offer_repo_trust = repo_root.as_ref().is_some_and(|repo_root| {
        repo_root != &cwd
            && cwd.starts_with(repo_root)
            && workspace_trust_status_string(repo_root) != "trusted"
            && (include_explicitly_untrusted || !workspace_explicitly_untrusted(repo_root))
    });

    Some(WorkspaceTrustPrompt {
        cwd,
        repo_root,
        detected_files,
        repo_detected_files,
        offer_repo_trust,
    })
}

fn workspace_trust_available_decisions(prompt: &WorkspaceTrustPrompt) -> Vec<&'static str> {
    let mut decisions = vec!["trust_cwd", "decline"];
    if prompt.offer_repo_trust {
        decisions.insert(0, "trust_repo");
    }
    decisions
}

fn find_trustable_files(path: &Path) -> Vec<String> {
    let mut found = BTreeMap::new();
    if path.join("AGENTS.md").is_file() {
        found.insert("AGENTS.md".to_string(), ());
    }
    let local_vibe = path.join(".vibe");
    if local_vibe.is_dir() {
        found.insert(".vibe/".to_string(), ());
    }
    found.into_keys().collect()
}

fn find_repo_trustable_files_for_cwd(cwd: &Path, repo_root: Option<&Path>) -> Vec<String> {
    let Some(repo_root) = repo_root else {
        return Vec::new();
    };
    if !cwd.starts_with(repo_root) || cwd == repo_root {
        return Vec::new();
    }
    let mut found = BTreeMap::new();
    for file in find_trustable_files(repo_root) {
        found.insert(file, ());
    }
    let mut current = cwd.parent();
    while let Some(path) = current {
        if path == repo_root {
            break;
        }
        if path.join("AGENTS.md").is_file()
            && let Ok(relative) = path.join("AGENTS.md").strip_prefix(repo_root)
        {
            found.insert(relative.display().to_string(), ());
        }
        current = path.parent();
    }
    found.into_keys().collect()
}

fn find_git_repo_ancestor(path: &Path) -> Option<PathBuf> {
    let home = dirs::home_dir().and_then(|home| home.canonicalize().ok());
    let mut current = path.canonicalize().ok()?;
    loop {
        if home.as_ref().is_some_and(|home| home == &current) {
            return None;
        }
        if current.join(".git").join("HEAD").is_file() {
            return Some(current);
        }
        let parent = current.parent()?;
        if parent == current {
            return None;
        }
        current = parent.to_path_buf();
    }
}

fn workspace_explicitly_untrusted(cwd: &Path) -> bool {
    let raw = std::fs::read_to_string(trusted_folders_file()).unwrap_or_default();
    normalize_trust_path(cwd)
        .ok()
        .is_some_and(|path| trust_section_contains(&raw, "untrusted", &path))
}

fn trust_section_contains(raw: &str, section: &str, path: &str) -> bool {
    raw.lines()
        .find(|line| line.trim_start().starts_with(&format!("{section} = [")))
        .is_some_and(|line| line.contains(&format!("\"{}\"", escape_toml_string(path))))
}

fn save_workspace_trust_path(path: &Path, trusted: bool) -> Result<()> {
    let path = normalize_trust_path(path)?;
    let file = trusted_folders_file();
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let raw = std::fs::read_to_string(&file).unwrap_or_default();
    let mut trusted_entries = parse_trust_entries(&raw, "trusted");
    let mut untrusted_entries = parse_trust_entries(&raw, "untrusted");
    if trusted {
        if !trusted_entries.contains(&path) {
            trusted_entries.push(path.clone());
        }
        untrusted_entries.retain(|entry| entry != &path);
    } else {
        if !untrusted_entries.contains(&path) {
            untrusted_entries.push(path.clone());
        }
        trusted_entries.retain(|entry| entry != &path);
    }
    std::fs::write(
        file,
        format!(
            "trusted = [{}]\nuntrusted = [{}]\n",
            toml_array(&trusted_entries),
            toml_array(&untrusted_entries)
        ),
    )?;
    Ok(())
}

fn parse_trust_entries(raw: &str, section: &str) -> Vec<String> {
    let Some(line) = raw
        .lines()
        .find(|line| line.trim_start().starts_with(&format!("{section} = [")))
    else {
        return Vec::new();
    };
    let Some((_, rest)) = line.split_once('[') else {
        return Vec::new();
    };
    let Some((body, _)) = rest.rsplit_once(']') else {
        return Vec::new();
    };
    body.split(',')
        .filter_map(|entry| {
            let entry = entry.trim();
            if entry.len() >= 2 && entry.starts_with('"') && entry.ends_with('"') {
                Some(
                    entry[1..entry.len() - 1]
                        .replace("\\\"", "\"")
                        .replace("\\\\", "\\"),
                )
            } else {
                None
            }
        })
        .collect()
}

fn trusted_folders_file() -> PathBuf {
    vibe_home()
        .unwrap_or_else(|| PathBuf::from(".vibe"))
        .join("trusted_folders.toml")
}

fn normalize_trust_path(path: &Path) -> Result<String> {
    Ok(path.canonicalize()?.display().to_string())
}

fn toml_array(entries: &[String]) -> String {
    entries
        .iter()
        .map(|entry| format!("\"{}\"", escape_toml_string(entry)))
        .collect::<Vec<_>>()
        .join(", ")
}

fn escape_toml_string(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(
        base_url: &str,
        backend: &str,
        browser_auth_base_url: Option<&str>,
        browser_auth_api_base_url: Option<&str>,
    ) -> microvibe_config::ProviderConfig {
        microvibe_config::ProviderConfig {
            base_url: base_url.to_string(),
            api_key_env: "TEST_API_KEY".to_string(),
            backend: backend.to_string(),
            browser_auth_base_url: browser_auth_base_url.map(str::to_string),
            browser_auth_api_base_url: browser_auth_api_base_url.map(str::to_string),
            wire_format: "openai_chat".to_string(),
        }
    }

    #[test]
    fn browser_auth_support_matches_mistral_provider_rules() {
        assert!(provider_supports_browser_auth(
            "mistral",
            &provider("https://api.mistral.ai/v1", "generic", None, None)
        ));
        assert!(provider_supports_browser_auth(
            "custom-mistral",
            &provider("https://proxy.example/v1", "mistral", None, None)
        ));
        assert!(provider_supports_browser_auth(
            "mistral",
            &provider(
                "https://proxy.example/v1",
                "generic",
                Some("https://console.mistral.ai"),
                Some("https://console.mistral.ai/api"),
            )
        ));
        assert!(!provider_supports_browser_auth(
            "llamacpp",
            &provider("http://127.0.0.1:8080/v1", "generic", None, None)
        ));
    }
}
