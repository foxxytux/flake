use crate::config::{self, CodexConfig};
use anyhow::{Context, Result, anyhow};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::random;
use reqwest::StatusCode;
use reqwest::blocking::{Client, Response};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::env;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::Command;

const CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const REDIRECT_URI: &str = "http://localhost:1455/auth/callback";

#[derive(Debug, Clone)]
pub struct CodexClient {
    pub model: String,
    pub base_url: String,
    credentials: OAuthCredentials,
    client: Client,
}

#[derive(Debug, Clone, Default)]
pub struct ConversationState {
    turns: Vec<ConversationTurn>,
    active: Option<ActiveTurn>,
}

#[derive(Debug, Clone)]
struct ActiveTurn {
    prompt: String,
    response: String,
}

#[derive(Debug, Clone)]
struct ConversationTurn {
    prompt: String,
    response: String,
}

#[derive(Debug, Clone)]
pub struct PredictionContext {
    pub file_path: String,
    pub language: String,
    pub prefix: String,
    pub suffix: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthCredentials {
    pub refresh: String,
    pub access: String,
    pub expires: i64,
    pub account_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct AuthFile {
    #[serde(default)]
    #[serde(rename = "openai-codex")]
    openai_codex: Option<StoredOAuthCredentials>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredOAuthCredentials {
    #[serde(rename = "type")]
    kind: String,
    #[serde(flatten)]
    creds: OAuthCredentials,
}

#[derive(Debug, Clone, Deserialize)]
struct ResponsesResponse {
    #[serde(default)]
    output_text: Option<String>,
    #[serde(default)]
    output: Vec<ResponseOutputItem>,
}

#[derive(Debug, Clone, Deserialize)]
struct ResponseOutputItem {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    content: Vec<ResponseOutputContent>,
}

#[derive(Debug, Clone, Deserialize)]
struct ResponseOutputContent {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: Option<String>,
}

#[derive(Debug, Serialize)]
struct ResponsesRequest {
    model: String,
    instructions: String,
    input: Value,
    store: bool,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    include: Option<Vec<String>>,
}

impl CodexClient {
    pub fn from_config(config: &CodexConfig) -> Result<Self> {
        let credentials = load_or_refresh_credentials()?;
        let client = Client::builder()
            .build()
            .context("failed to build HTTP client")?;
        Ok(Self {
            model: config.model.clone(),
            base_url: config.base_url.clone(),
            credentials,
            client,
        })
    }

    pub fn predict_completion(&self, ctx: PredictionContext) -> Result<String> {
        let prompt = format!(
            "File: {}\nLanguage: {}\nPrefix:\n<<<PREFIX\n{}\nPREFIX>>>\nSuffix:\n<<<SUFFIX\n{}\nSUFFIX>>>\n",
            ctx.file_path, ctx.language, ctx.prefix, ctx.suffix
        );
        self.post_responses(
            "Complete code surgically and return only the text to insert at the cursor.",
            &prompt,
        )
        .map(|text| text.trim().to_string())
    }

    #[allow(dead_code)]
    pub fn ask(&self, prompt: &str, workspace: &str) -> Result<String> {
        self.ask_stream(prompt, workspace, |_| {})
    }

    pub fn ask_stream<F>(&self, prompt: &str, workspace: &str, mut on_delta: F) -> Result<String>
    where
        F: FnMut(&str),
    {
        let input = format!(
            "Workspace context:\n{}\n\nRequest:\n{}\n\nTool protocol:\nYou must use tools proactively whenever the request depends on repository state, file contents, paths, symbols, or any fact that can be verified from the workspace. Do not guess if a tool can confirm the answer. Before responding to any code or workspace question, inspect the repo with tools first. Keep using tools until you have enough evidence.\n\nAllowed tool requests are lines that start with `TOOL ` followed by one of `/pwd`, `/ls PATH`, `/tree PATH`, or `/cat PATH`. Emit only the tool line; do not wrap it in markdown. After tool results are provided, continue with the answer and use tools again if more detail is still needed.",
            workspace, prompt
        );
        self.post_responses_stream(
            "You are Flake, a terminal-first AI coding agent. Be concise, concrete, and useful. Treat tool use as the default whenever workspace facts matter. If there is any uncertainty about the repository, inspect it with tools before answering. Never pretend you inspected files or paths you did not verify.",
            &input,
            &mut on_delta,
        )
    }

    fn post_responses(&self, instructions: &str, input: &str) -> Result<String> {
        let body = ResponsesRequest {
            model: self.model.clone(),
            instructions: instructions.to_string(),
            input: json!([
                {
                    "role": "user",
                    "content": [
                        { "type": "input_text", "text": input }
                    ]
                }
            ]),
            store: false,
            stream: false,
            include: Some(vec!["reasoning.encrypted_content".to_string()]),
        };

        let mut credentials = self.credentials.clone();
        let response = self.send_responses_request(&body, &credentials, false)?;
        if response.status().is_success() {
            return self.read_response(response);
        }

        let status = response.status();
        let text = response.text().unwrap_or_default();
        if should_refresh_credentials(status, &text) {
            credentials = refresh_openai_codex_token(&credentials.refresh)?;
            save_credentials(&credentials)?;
            let response = self.send_responses_request(&body, &credentials, false)?;
            if response.status().is_success() {
                return self.read_response(response);
            }
            let status = response.status();
            let text = response.text().unwrap_or_default();
            return Err(anyhow!("Codex request failed: {} {}", status, text));
        }

        Err(anyhow!("Codex request failed: {} {}", status, text))
    }

    fn post_responses_stream<F>(
        &self,
        instructions: &str,
        input: &str,
        on_delta: &mut F,
    ) -> Result<String>
    where
        F: FnMut(&str),
    {
        let body = ResponsesRequest {
            model: self.model.clone(),
            instructions: instructions.to_string(),
            input: json!([
                {
                    "role": "user",
                    "content": [
                        { "type": "input_text", "text": input }
                    ]
                }
            ]),
            store: false,
            stream: true,
            include: Some(vec!["reasoning.encrypted_content".to_string()]),
        };

        let mut credentials = self.credentials.clone();
        let response = self.send_responses_request(&body, &credentials, true)?;
        if response.status().is_success() {
            return self.read_stream(response, on_delta);
        }

        let status = response.status();
        let text = response.text().unwrap_or_default();
        if should_refresh_credentials(status, &text) {
            credentials = refresh_openai_codex_token(&credentials.refresh)?;
            save_credentials(&credentials)?;
            let response = self.send_responses_request(&body, &credentials, true)?;
            if response.status().is_success() {
                return self.read_stream(response, on_delta);
            }
            let status = response.status();
            let text = response.text().unwrap_or_default();
            return Err(anyhow!("Codex request failed: {} {}", status, text));
        }

        Err(anyhow!("Codex request failed: {} {}", status, text))
    }

    fn send_responses_request(
        &self,
        body: &ResponsesRequest,
        credentials: &OAuthCredentials,
        stream: bool,
    ) -> Result<Response> {
        let url = resolve_responses_url(&self.base_url);
        let headers = build_codex_headers(&credentials.account_id, &credentials.access, stream);
        let request = self.client.post(&url).headers(headers).json(body);
        request.send().context("failed to send Codex request")
    }

    fn read_response(&self, response: Response) -> Result<String> {
        let parsed: ResponsesResponse =
            response.json().context("failed to parse Codex response")?;
        extract_response_text(parsed)
    }

    fn read_stream<F>(&self, response: Response, on_delta: &mut F) -> Result<String>
    where
        F: FnMut(&str),
    {
        let mut reader = BufReader::new(response);
        let mut response_text = String::new();
        let mut line = String::new();

        loop {
            line.clear();
            let bytes = reader
                .read_line(&mut line)
                .context("failed to read Codex stream")?;
            if bytes == 0 {
                break;
            }

            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with(':') {
                continue;
            }

            let Some(data) = trimmed.strip_prefix("data:") else {
                continue;
            };
            let data = data.trim_start();
            if data == "[DONE]" {
                break;
            }

            let event: Value = match serde_json::from_str(data) {
                Ok(event) => event,
                Err(_) => continue,
            };

            if let Some(delta) = extract_stream_delta(&event) {
                if !delta.is_empty() {
                    on_delta(&delta);
                    response_text.push_str(&delta);
                }
            }
        }

        if response_text.trim().is_empty() {
            return Err(anyhow!("Codex stream did not contain text"));
        }

        Ok(response_text)
    }
}

impl ConversationState {
    pub fn is_active(&self) -> bool {
        self.active.is_some()
    }

    pub fn begin_turn(&mut self, prompt: impl Into<String>) {
        self.active = Some(ActiveTurn {
            prompt: prompt.into(),
            response: String::new(),
        });
    }

    pub fn finish_turn_with_response(&mut self, response: String) {
        if let Some(active) = self.active.take() {
            self.turns.push(ConversationTurn {
                prompt: active.prompt,
                response: if active.response.is_empty() {
                    response
                } else {
                    active.response
                },
            });
        }
    }

    pub fn abort_turn(&mut self) {
        self.active = None;
    }

    pub fn push_tool_output(&mut self, tool: impl Into<String>, output: impl Into<String>) {
        self.turns.push(ConversationTurn {
            prompt: tool.into(),
            response: output.into(),
        });
    }

    pub fn lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        for turn in &self.turns {
            push_turn_lines(&mut lines, turn);
        }
        if let Some(active) = &self.active {
            lines.push(format!("> {}", active.prompt));
            if active.response.is_empty() {
                lines.push(String::new());
            } else {
                for line in active.response.lines() {
                    lines.push(line.to_string());
                }
            }
        }
        lines
    }
}

pub fn login_and_save() -> Result<String> {
    let credentials = login_openai_codex()?;
    save_credentials(&credentials)?;
    Ok(format!(
        "codex login complete; account {}",
        credentials.account_id
    ))
}

fn load_or_refresh_credentials() -> Result<OAuthCredentials> {
    let path = config::auth_path();
    let file = load_auth_file(&path)?;
    let Some(stored) = file.openai_codex else {
        return Err(anyhow!(
            "no Codex subscription login found; run `codex login`"
        ));
    };

    if stored.kind != "oauth" {
        return Err(anyhow!("invalid Codex auth record"));
    }

    let mut creds = stored.creds;
    if DateTime::now_millis() >= creds.expires {
        creds = refresh_openai_codex_token(&creds.refresh)?;
        save_credentials(&creds)?;
    }

    Ok(creds)
}

fn save_credentials(credentials: &OAuthCredentials) -> Result<()> {
    let path = config::auth_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create auth dir {}", parent.display()))?;
    }
    let mut file = load_auth_file(&path).unwrap_or_default();
    file.openai_codex = Some(StoredOAuthCredentials {
        kind: "oauth".to_string(),
        creds: credentials.clone(),
    });
    let contents = serde_json::to_string_pretty(&file).context("failed to serialize auth file")?;
    fs::write(&path, contents).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

fn load_auth_file(path: &PathBuf) -> Result<AuthFile> {
    if !path.exists() {
        return Ok(AuthFile::default());
    }
    let contents = fs::read_to_string(path)
        .with_context(|| format!("failed to read auth file at {}", path.display()))?;
    serde_json::from_str(&contents).context("failed to parse auth file")
}

fn login_openai_codex() -> Result<OAuthCredentials> {
    let (verifier, challenge) = generate_pkce();
    let state = random_hex(16);
    let url = build_authorize_url(&state, &challenge);

    open_url(&url)?;
    let code = wait_for_oauth_code(&state)?;
    let token = exchange_authorization_code(&code, &verifier)?;
    let account_id = extract_account_id(&token.access)?;
    Ok(OAuthCredentials {
        refresh: token.refresh,
        access: token.access,
        expires: token.expires,
        account_id,
    })
}

fn refresh_openai_codex_token(refresh_token: &str) -> Result<OAuthCredentials> {
    let response = Client::new()
        .post("https://auth.openai.com/oauth/token")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", CODEX_CLIENT_ID),
        ])
        .send()
        .context("failed to refresh Codex token")?;

    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().unwrap_or_default();
        return Err(anyhow!("Codex token refresh failed: {} {}", status, text));
    }

    let token = parse_token_response(response)?;
    let account_id = extract_account_id(&token.access)?;
    Ok(OAuthCredentials {
        refresh: token.refresh,
        access: token.access,
        expires: token.expires,
        account_id,
    })
}

fn exchange_authorization_code(code: &str, verifier: &str) -> Result<TokenResponse> {
    let response = Client::new()
        .post("https://auth.openai.com/oauth/token")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .form(&[
            ("grant_type", "authorization_code"),
            ("client_id", CODEX_CLIENT_ID),
            ("code", code),
            ("code_verifier", verifier),
            ("redirect_uri", REDIRECT_URI),
        ])
        .send()
        .context("failed to exchange Codex authorization code")?;

    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().unwrap_or_default();
        return Err(anyhow!("Codex token exchange failed: {} {}", status, text));
    }

    parse_token_response(response)
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access: String,
    refresh: String,
    expires: i64,
}

fn parse_token_response(response: Response) -> Result<TokenResponse> {
    let json: Value = response
        .json()
        .context("failed to parse Codex token response")?;
    let access = json
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("missing access_token"))?;
    let refresh = json
        .get("refresh_token")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("missing refresh_token"))?;
    let expires_in = json
        .get("expires_in")
        .and_then(Value::as_i64)
        .ok_or_else(|| anyhow!("missing expires_in"))?;
    Ok(TokenResponse {
        access: access.to_string(),
        refresh: refresh.to_string(),
        expires: DateTime::now_millis() + expires_in * 1000,
    })
}

fn build_authorize_url(state: &str, challenge: &str) -> String {
    let mut url = String::from("https://auth.openai.com/oauth/authorize?");
    url.push_str("response_type=code");
    url.push_str("&client_id=");
    url.push_str(CODEX_CLIENT_ID);
    url.push_str("&redirect_uri=");
    url.push_str(urlencoding::encode(REDIRECT_URI).as_ref());
    url.push_str("&scope=");
    url.push_str(urlencoding::encode("openid profile email offline_access").as_ref());
    url.push_str("&code_challenge=");
    url.push_str(challenge);
    url.push_str("&code_challenge_method=S256");
    url.push_str("&state=");
    url.push_str(state);
    url.push_str("&id_token_add_organizations=true");
    url.push_str("&codex_cli_simplified_flow=true");
    url.push_str("&originator=pi");
    url
}

fn wait_for_oauth_code(state: &str) -> Result<String> {
    let listener = TcpListener::bind(("127.0.0.1", 1455))
        .context("failed to bind local OAuth callback server")?;
    for stream in listener.incoming() {
        let mut stream = stream.context("failed to accept OAuth callback")?;
        let mut buf = [0_u8; 8192];
        let n = stream
            .read(&mut buf)
            .context("failed to read OAuth callback")?;
        let request = String::from_utf8_lossy(&buf[..n]);
        let Some(first_line) = request.lines().next() else {
            continue;
        };
        let mut parts = first_line.split_whitespace();
        let _method = parts.next().unwrap_or_default();
        let target = parts.next().unwrap_or_default();
        let full_url = format!("http://localhost{}", target);
        let url = reqwest::Url::parse(&full_url).context("invalid OAuth callback URL")?;
        if url.path() != "/auth/callback" {
            write_oauth_response(&mut stream, 404, "Callback route not found.")?;
            continue;
        }
        if url
            .query_pairs()
            .find(|(k, _)| k == "state")
            .map(|(_, v)| v.to_string())
            != Some(state.to_string())
        {
            write_oauth_response(&mut stream, 400, "State mismatch.")?;
            continue;
        }
        let Some(code) = url
            .query_pairs()
            .find(|(k, _)| k == "code")
            .map(|(_, v)| v.to_string())
        else {
            write_oauth_response(&mut stream, 400, "Missing authorization code.")?;
            continue;
        };
        write_oauth_response(
            &mut stream,
            200,
            "OpenAI authentication completed. You can close this window.",
        )?;
        return Ok(code);
    }
    Err(anyhow!("OAuth callback server stopped unexpectedly"))
}

fn write_oauth_response(stream: &mut std::net::TcpStream, status: u16, body: &str) -> Result<()> {
    let status_text = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        _ => "OK",
    };
    let response = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        status,
        status_text,
        body.len(),
        body
    );
    stream
        .write_all(response.as_bytes())
        .context("failed to write OAuth response")?;
    Ok(())
}

fn open_url(url: &str) -> Result<()> {
    for cmd in ["xdg-open", "open", "gio"] {
        let mut child = Command::new(cmd);
        if cmd == "gio" {
            child.arg("open");
        }
        let status = child.arg(url).status();
        if let Ok(status) = status {
            if status.success() {
                return Ok(());
            }
        }
    }
    Err(anyhow!("failed to open browser for OAuth login: {}", url))
}

fn generate_pkce() -> (String, String) {
    let verifier = random_hex(64);
    let challenge = pkce_challenge(&verifier);
    (verifier, challenge)
}

fn random_hex(len: usize) -> String {
    let bytes: Vec<u8> = (0..len).map(|_| random::<u8>()).collect();
    URL_SAFE_NO_PAD.encode(bytes)
}

fn pkce_challenge(verifier: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(verifier.as_bytes());
    let digest = hasher.finalize();
    URL_SAFE_NO_PAD.encode(digest)
}

fn extract_account_id(access_token: &str) -> Result<String> {
    let parts: Vec<&str> = access_token.split('.').collect();
    if parts.len() != 3 {
        return Err(anyhow!("invalid Codex access token"));
    }
    let payload = URL_SAFE_NO_PAD
        .decode(parts[1])
        .context("failed to decode Codex access token")?;
    let json: Value = serde_json::from_slice(&payload).context("failed to parse Codex JWT")?;
    let account_id = json
        .get("https://api.openai.com/auth")
        .and_then(|v| v.get("chatgpt_account_id"))
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("failed to extract accountId from token"))?;
    Ok(account_id.to_string())
}

fn build_codex_headers(account_id: &str, token: &str, stream: bool) -> reqwest::header::HeaderMap {
    use reqwest::header::{HeaderMap, HeaderValue};

    let mut headers = HeaderMap::new();
    headers.insert(
        "Authorization",
        HeaderValue::from_str(&format!("Bearer {}", token))
            .unwrap_or_else(|_| HeaderValue::from_static("")),
    );
    headers.insert(
        "chatgpt-account-id",
        HeaderValue::from_str(account_id).unwrap_or_else(|_| HeaderValue::from_static("")),
    );
    headers.insert("originator", HeaderValue::from_static("pi"));
    headers.insert(
        "OpenAI-Beta",
        HeaderValue::from_static("responses=experimental"),
    );
    headers.insert(
        "accept",
        HeaderValue::from_static(if stream {
            "text/event-stream"
        } else {
            "application/json"
        }),
    );
    headers.insert("content-type", HeaderValue::from_static("application/json"));
    headers.insert(
        "User-Agent",
        HeaderValue::from_str(&codex_user_agent())
            .unwrap_or_else(|_| HeaderValue::from_static("pi (unix)")),
    );
    headers
}

fn codex_user_agent() -> String {
    let os = env::consts::OS;
    let arch = env::consts::ARCH;
    let release = Command::new("uname")
        .arg("-r")
        .output()
        .ok()
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string());
    format!("pi ({} {}; {})", os, release, arch)
}

fn resolve_responses_url(base_url: &str) -> String {
    let normalized = base_url.trim().trim_end_matches('/');
    if normalized.ends_with("/responses") {
        return normalized.to_string();
    }
    if normalized.ends_with("/codex") {
        return format!("{}/responses", normalized);
    }
    if normalized.ends_with("/backend-api") {
        return format!("{}/codex/responses", normalized);
    }
    format!("{}/codex/responses", normalized)
}

fn extract_response_text(response: ResponsesResponse) -> Result<String> {
    if let Some(text) = response.output_text.filter(|text| !text.trim().is_empty()) {
        return Ok(text);
    }

    let mut output = String::new();
    for item in response.output {
        if item.kind != "message" {
            continue;
        }
        for part in item.content {
            if part.kind == "output_text" {
                if let Some(text) = part.text {
                    output.push_str(&text);
                }
            }
        }
    }

    if output.trim().is_empty() {
        return Err(anyhow!("Codex response did not contain text"));
    }

    Ok(output)
}

fn extract_stream_delta(event: &Value) -> Option<String> {
    let kind = event.get("type")?.as_str()?;
    match kind {
        "response.output_text.delta"
        | "response.refusal.delta"
        | "response.reasoning_summary_text.delta" => event
            .get("delta")
            .and_then(Value::as_str)
            .map(ToString::to_string),
        "response.completed" | "response.done" | "response.incomplete" => None,
        _ => None,
    }
}

fn should_refresh_credentials(status: StatusCode, body: &str) -> bool {
    status == StatusCode::UNAUTHORIZED
        || (status == StatusCode::FORBIDDEN && body.to_ascii_lowercase().contains("token"))
}

struct DateTime;

impl DateTime {
    fn now_millis() -> i64 {
        let now = std::time::SystemTime::now();
        now.duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or_default()
    }
}

fn push_turn_lines(lines: &mut Vec<String>, turn: &ConversationTurn) {
    lines.push(format!("> {}", turn.prompt));
    if turn.response.is_empty() {
        lines.push(String::new());
        return;
    }

    for line in turn.response.lines() {
        lines.push(line.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DateTime, ResponsesResponse, extract_account_id, extract_response_text,
        extract_stream_delta, generate_pkce, resolve_responses_url,
    };
    use anyhow::Result;
    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use serde_json::Value;

    #[test]
    fn resolves_responses_url_from_backend() {
        assert_eq!(
            resolve_responses_url("https://chatgpt.com/backend-api"),
            "https://chatgpt.com/backend-api/codex/responses"
        );
    }

    #[test]
    fn parses_response_text() -> Result<()> {
        let response: ResponsesResponse = serde_json::from_str(
            r#"{"output":[{"type":"message","content":[{"type":"output_text","text":"hello"}]}]}"#,
        )?;
        assert_eq!(extract_response_text(response)?, "hello");
        Ok(())
    }

    #[test]
    fn pkce_is_non_empty() {
        let (verifier, challenge) = generate_pkce();
        assert!(!verifier.is_empty());
        assert!(!challenge.is_empty());
        assert_ne!(verifier, challenge);
    }

    #[test]
    fn extracts_account_id_from_jwt() -> Result<()> {
        let payload = serde_json::json!({
            "https://api.openai.com/auth": { "chatgpt_account_id": "abc123" }
        });
        let token = format!(
            "header.{}.sig",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload)?)
        );
        assert_eq!(extract_account_id(&token)?, "abc123");
        Ok(())
    }

    #[test]
    fn parses_stream_delta() {
        let event: Value =
            serde_json::from_str(r#"{"type":"response.output_text.delta","delta":"hello"}"#)
                .expect("valid json");
        assert_eq!(extract_stream_delta(&event), Some("hello".to_string()));
    }

    #[test]
    fn time_monotonic() {
        assert!(DateTime::now_millis() > 0);
    }
}
