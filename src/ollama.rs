//! Cliente HTTP/1.1 ligero en Rust puro para el servidor local de Ollama.
//!
//! Soporta descubrimiento automático (`127.0.0.1:11434` con prioridad sobre
//! variables `OLLAMA_HOST` remotas inalcanzables), listado de modelos (`/api/tags`),
//! conversación con `tool_calls` nativos (`/api/chat`) y fallback de extracción JSON
//! en texto plano para modelos sin soporte nativo de herramientas.

use crate::get_vmm_env;
use crate::json::JsonValue;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream, ToSocketAddrs};
use std::process::Command;
use std::time::Duration;

pub const DEFAULT_BOGO_OLLAMA_URL: &str = "http://100.86.219.90:11434";
pub const DEFAULT_LOCAL_OLLAMA_URL: &str = "http://127.0.0.1:11434";
pub const DEFAULT_OLLAMA_URL: &str = DEFAULT_BOGO_OLLAMA_URL;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OllamaEndpoint {
    pub label: String,
    pub url: String,
    pub is_remote: bool,
}

#[derive(Debug, Clone)]
pub struct DiscoveredOllama {
    pub client: OllamaClient,
    pub endpoint: OllamaEndpoint,
    pub models: Vec<OllamaModelInfo>,
    pub fallback_note: Option<String>,
    pub candidates: Vec<OllamaEndpoint>,
}

#[derive(Debug, Clone)]
pub struct OllamaModelInfo {
    pub name: String,
    pub parameter_size: String,
    pub supports_tools: bool,
}

#[derive(Debug, Clone)]
pub struct ToolCall {
    pub id: Option<String>,
    pub name: String,
    pub arguments: JsonValue,
}

#[derive(Debug, Clone)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
    pub tool_name: Option<String>,
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".to_string(),
            content: content.into(),
            tool_calls: Vec::new(),
            tool_name: None,
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".to_string(),
            content: content.into(),
            tool_calls: Vec::new(),
            tool_name: None,
        }
    }

    pub fn assistant(content: impl Into<String>, tool_calls: Vec<ToolCall>) -> Self {
        Self {
            role: "assistant".to_string(),
            content: content.into(),
            tool_calls,
            tool_name: None,
        }
    }

    pub fn tool(name: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: "tool".to_string(),
            content: content.into(),
            tool_calls: Vec::new(),
            tool_name: Some(name.into()),
        }
    }

    pub fn to_json(&self) -> JsonValue {
        let mut entries = vec![
            ("role", JsonValue::Str(self.role.clone())),
            ("content", JsonValue::Str(self.content.clone())),
        ];
        if let Some(ref tn) = self.tool_name {
            entries.push(("tool_name", JsonValue::Str(tn.clone())));
        }
        if !self.tool_calls.is_empty() {
            let calls: Vec<JsonValue> = self
                .tool_calls
                .iter()
                .enumerate()
                .map(|(idx, tc)| {
                    let mut tc_entries = vec![(
                        "function",
                        JsonValue::obj(vec![
                            ("index", JsonValue::Number(idx as f64)),
                            ("name", JsonValue::Str(tc.name.clone())),
                            ("arguments", tc.arguments.clone()),
                        ]),
                    )];
                    if let Some(ref id) = tc.id {
                        tc_entries.push(("id", JsonValue::Str(id.clone())));
                    }
                    JsonValue::obj(tc_entries)
                })
                .collect();
            entries.push(("tool_calls", JsonValue::Array(calls)));
        }
        JsonValue::obj(entries)
    }
}

#[derive(Debug, Clone)]
pub struct ChatResponse {
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
    pub model: String,
}

#[derive(Debug, Clone)]
pub struct OllamaClient {
    pub base_url: String,
}

impl OllamaClient {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: normalize_base_url(&base_url.into()),
        }
    }

    /// Resuelve la URL inicial dando prioridad a `TWO_FIVE_FIVE_OLLAMA_URL`, luego
    /// al servidor GPU `bogo` en Tailscale (`100.86.219.90:11434`), `OLLAMA_HOST`,
    /// y finalmente Ollama local (`127.0.0.1:11434`).
    pub fn resolve_default_url() -> String {
        Self::candidate_endpoints()
            .into_iter()
            .next()
            .map(|e| e.url)
            .unwrap_or_else(|| DEFAULT_BOGO_OLLAMA_URL.to_string())
    }

    /// Lista ordenada de endpoints candidatos (priorizando servidor GPU `bogo` por Tailscale).
    pub fn candidate_endpoints() -> Vec<OllamaEndpoint> {
        let mut out: Vec<OllamaEndpoint> = Vec::new();
        let mut push_unique = |label: &str, raw_url: &str, force_remote: Option<bool>| {
            let trimmed = raw_url.trim();
            if trimmed.is_empty() {
                return;
            }
            let url = normalize_base_url(trimmed);
            if out.iter().any(|e| e.url == url) {
                return;
            }
            let is_remote = force_remote.unwrap_or_else(|| !is_loopback_url(&url));
            out.push(OllamaEndpoint {
                label: label.to_string(),
                url,
                is_remote,
            });
        };

        if let Ok(explicit) = get_vmm_env("OLLAMA_URL") {
            let is_rem = !is_loopback_url(&normalize_base_url(&explicit));
            let lbl = if explicit.to_ascii_lowercase().contains("bogo")
                || explicit.contains("100.")
            {
                "bogo"
            } else if is_rem {
                "custom"
            } else {
                "local"
            };
            push_unique(lbl, &explicit, Some(is_rem));
        }

        if let Some(ts_url) = discover_tailscale_bogo_url() {
            push_unique("bogo", &ts_url, Some(true));
        }

        if let Ok(env_host) = std::env::var("OLLAMA_HOST") {
            let norm = normalize_base_url(&env_host);
            let is_rem = !is_loopback_url(&norm);
            let lbl = if env_host.contains("100.") || env_host.to_ascii_lowercase().contains("bogo") {
                "bogo"
            } else if is_rem {
                "remoto"
            } else {
                "local"
            };
            push_unique(lbl, &norm, Some(is_rem));
        }

        push_unique("bogo", DEFAULT_BOGO_OLLAMA_URL, Some(true));
        push_unique("local", DEFAULT_LOCAL_OLLAMA_URL, Some(false));
        out
    }

    /// Descubre el mejor endpoint disponible dando prioridad absoluta a `bogo` sobre Tailscale.
    /// Si `strict_remote` es `true` (o `TWO_FIVE_FIVE_OLLAMA_BOGO_ONLY=1`), no hace fallback a local.
    pub fn discover_endpoint_detailed(strict_remote: bool) -> Result<DiscoveredOllama, String> {
        let strict = strict_remote
            || get_vmm_env("OLLAMA_BOGO_ONLY")
                .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                .unwrap_or(false);
        let candidates = Self::candidate_endpoints();
        let mut bogo_error: Option<(String, String)> = None;
        let mut last_err = String::from("No hay endpoints Ollama disponibles");

        for ep in &candidates {
            if strict && !ep.is_remote {
                continue;
            }
            let client = Self::new(&ep.url);
            let connect_timeout = if ep.is_remote {
                Duration::from_millis(2500)
            } else {
                Duration::from_millis(1500)
            };
            match client.list_models_with_timeout(connect_timeout) {
                Ok(models) => {
                    let fallback_note = if !ep.is_remote {
                        bogo_error.as_ref().map(|(b_url, b_err)| {
                            format!(
                                "Servidor GPU bogo ({}) no accesible por Tailscale ({}); usando respaldo local.",
                                b_url, b_err
                            )
                        })
                    } else {
                        None
                    };
                    return Ok(DiscoveredOllama {
                        client,
                        endpoint: ep.clone(),
                        models,
                        fallback_note,
                        candidates,
                    });
                }
                Err(e) => {
                    if ep.label == "bogo" && bogo_error.is_none() {
                        bogo_error = Some((ep.url.clone(), e.clone()));
                    }
                    last_err = e;
                }
            }
        }
        Err(last_err)
    }

    /// Compatibilidad simple: devuelve cliente y modelos del mejor endpoint disponible.
    pub fn discover_endpoint() -> Result<(Self, Vec<OllamaModelInfo>), String> {
        let d = Self::discover_endpoint_detailed(false)?;
        Ok((d.client, d.models))
    }

    /// Lista modelos vía `GET /api/tags` con timeout estándar.
    pub fn list_models(&self) -> Result<Vec<OllamaModelInfo>, String> {
        self.list_models_with_timeout(Duration::from_millis(2500))
    }

    /// Lista modelos vía `GET /api/tags` con timeout configurable.
    pub fn list_models_with_timeout(
        &self,
        connect_timeout: Duration,
    ) -> Result<Vec<OllamaModelInfo>, String> {
        let (status, body) = http_request(
            &self.base_url,
            "GET",
            "/api/tags",
            None,
            connect_timeout,
            Duration::from_secs(5),
        )?;
        if status >= 400 {
            return Err(format!("Ollama /api/tags devolvió HTTP {}: {}", status, body));
        }
        parse_tags_response(&body)
    }

    /// Elige automáticamente el mejor modelo disponible según si corre en GPU remota (`bogo`) o CPU local.
    pub fn pick_best_model(models: &[OllamaModelInfo]) -> Option<String> {
        Self::pick_best_model_for(models, true)
    }

    pub fn pick_best_model_for(models: &[OllamaModelInfo], is_remote: bool) -> Option<String> {
        if models.is_empty() {
            return None;
        }
        if let Ok(pref) = get_vmm_env("OLLAMA_MODEL") {
            let p = pref.trim();
            if let Some(found) = models.iter().find(|m| m.name == p) {
                return Some(found.name.clone());
            }
        }
        let preferred_prefixes: &[&str] = if is_remote {
            &[
                "legion",
                "northstar-7b",
                "gemma4",
                "qwen3",
                "qwen2.5",
                "northstar-4b",
                "llama3.1",
                "mistral",
            ]
        } else {
            &[
                "northstar-4b",
                "qwen2.5",
                "llama3.2",
                "llama3.1",
                "mistral",
                "gemma4",
                "northstar-7b",
            ]
        };
        for prefix in preferred_prefixes {
            if let Some(found) = models
                .iter()
                .find(|m| m.supports_tools && m.name.to_ascii_lowercase().starts_with(prefix))
            {
                return Some(found.name.clone());
            }
        }
        if let Some(any_tool) = models.iter().find(|m| m.supports_tools) {
            return Some(any_tool.name.clone());
        }
        models.first().map(|m| m.name.clone())
    }

    /// Ejecuta una petición `POST /api/chat` con herramientas (`tools`) opcionales.
    pub fn chat(
        &self,
        model: &str,
        messages: &[ChatMessage],
        tools: &[JsonValue],
    ) -> Result<ChatResponse, String> {
        let msgs_json = JsonValue::Array(messages.iter().map(|m| m.to_json()).collect());
        let num_ctx = if is_loopback_url(&self.base_url) {
            4096.0
        } else {
            8192.0
        };
        let mut req_entries = vec![
            ("model", JsonValue::Str(model.to_string())),
            ("stream", JsonValue::Bool(false)),
            ("think", JsonValue::Bool(false)),
            ("keep_alive", JsonValue::Str("10m".to_string())),
            ("messages", msgs_json),
            (
                "options",
                JsonValue::obj(vec![
                    ("temperature", JsonValue::Number(0.2)),
                    ("num_ctx", JsonValue::Number(num_ctx)),
                ]),
            ),
        ];
        if !tools.is_empty() {
            req_entries.push(("tools", JsonValue::Array(tools.to_vec())));
        }
        let payload = JsonValue::obj(req_entries).to_json_string();

        let (mut status, mut body) = http_request(
            &self.base_url,
            "POST",
            "/api/chat",
            Some(&payload),
            Duration::from_secs(3),
            Duration::from_secs(180),
        )?;

        // Algunos modelos antiguos/personalizados devuelven 400 si no aceptan `"think": false` o `tools`
        if status == 400 && (body.contains("think") || body.contains("does not support tools")) {
            let mut retry_entries = vec![
                ("model", JsonValue::Str(model.to_string())),
                ("stream", JsonValue::Bool(false)),
                ("messages", JsonValue::Array(messages.iter().map(|m| m.to_json()).collect())),
                ("options", JsonValue::obj(vec![("temperature", JsonValue::Number(0.2))])),
            ];
            if !tools.is_empty() && !body.contains("does not support tools") {
                retry_entries.push(("tools", JsonValue::Array(tools.to_vec())));
            }
            let retry_payload = JsonValue::obj(retry_entries).to_json_string();
            let res = http_request(
                &self.base_url,
                "POST",
                "/api/chat",
                Some(&retry_payload),
                Duration::from_secs(3),
                Duration::from_secs(180),
            )?;
            status = res.0;
            body = res.1;
        }

        if status >= 400 {
            return Err(format!("Ollama /api/chat devolvió HTTP {}: {}", status, body));
        }
        parse_chat_response(&body)
    }
}

pub fn normalize_base_url(raw: &str) -> String {
    let t = raw.trim().trim_end_matches('/');
    if t.starts_with("http://") || t.starts_with("https://") {
        t.to_string()
    } else if t.contains(':') {
        format!("http://{}", t)
    } else {
        format!("http://{}:11434", t)
    }
}

pub fn parse_host_port(base_url: &str) -> Result<(String, u16), String> {
    let without_scheme = base_url
        .trim()
        .strip_prefix("http://")
        .or_else(|| base_url.trim().strip_prefix("https://"))
        .unwrap_or(base_url.trim());
    let host_port = without_scheme.split('/').next().unwrap_or("");
    if host_port.is_empty() {
        return Err(format!("URL de Ollama vacía: '{}'", base_url));
    }
    if let Some((h, p)) = host_port.rsplit_once(':') {
        let port: u16 = p
            .parse()
            .map_err(|_| format!("Puerto inválido en URL '{}'", base_url))?;
        Ok((h.trim_matches(['[', ']']).to_string(), port))
    } else {
        Ok((host_port.to_string(), 11434))
    }
}

pub fn parse_tags_response(body: &str) -> Result<Vec<OllamaModelInfo>, String> {
    let root = JsonValue::parse(body)?;
    let arr = root
        .get("models")
        .and_then(|m| m.as_array())
        .ok_or_else(|| "Respuesta /api/tags sin arreglo 'models'".to_string())?;
    let mut out = Vec::new();
    for item in arr {
        let name = item
            .get("name")
            .and_then(|v| v.as_str())
            .or_else(|| item.get("model").and_then(|v| v.as_str()))
            .unwrap_or("")
            .to_string();
        if name.is_empty() {
            continue;
        }
        let parameter_size = item
            .get("details")
            .and_then(|d| d.get("parameter_size"))
            .and_then(|p| p.as_str())
            .unwrap_or("?")
            .to_string();
        let supports_tools = item
            .get("capabilities")
            .and_then(|c| c.as_array())
            .map(|caps| caps.iter().any(|x| x.as_str() == Some("tools")))
            .unwrap_or_else(|| {
                let low = name.to_ascii_lowercase();
                low.contains("qwen")
                    || low.contains("northstar")
                    || low.contains("gemma4")
                    || low.contains("llama3")
                    || low.contains("mistral")
            });
        out.push(OllamaModelInfo {
            name,
            parameter_size,
            supports_tools,
        });
    }
    Ok(out)
}

pub fn parse_chat_response(body: &str) -> Result<ChatResponse, String> {
    let root = JsonValue::parse(body)?;
    if let Some(err) = root.get("error").and_then(|e| e.as_str()) {
        return Err(format!("Error de Ollama: {}", err));
    }
    let model = root
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .to_string();
    let msg = root
        .get("message")
        .ok_or_else(|| "Respuesta /api/chat sin objeto 'message'".to_string())?;
    let mut content = msg
        .get("content")
        .and_then(|c| c.as_str())
        .unwrap_or("")
        .to_string();

    if content.trim().is_empty() {
        if let Some(thinking) = msg.get("thinking").and_then(|t| t.as_str()) {
            if !thinking.trim().is_empty() {
                content = thinking.to_string();
            }
        }
    }

    let mut tool_calls = Vec::new();
    if let Some(tcs) = msg.get("tool_calls").and_then(|a| a.as_array()) {
        for tc in tcs {
            let id = tc.get("id").and_then(|v| v.as_str()).map(|s| s.to_string());
            if let Some(func) = tc.get("function") {
                if let Some(name) = func.get("name").and_then(|v| v.as_str()) {
                    let raw_args = func.get("arguments").cloned().unwrap_or(JsonValue::obj(vec![]));
                    let arguments = match raw_args {
                        JsonValue::Str(ref s) => {
                            JsonValue::parse(s).unwrap_or_else(|_| JsonValue::obj(vec![]))
                        }
                        other => other,
                    };
                    tool_calls.push(ToolCall {
                        id,
                        name: name.to_string(),
                        arguments,
                    });
                }
            }
        }
    }

    if tool_calls.is_empty() {
        if let Some(fallback) = extract_fallback_tool_call(&content) {
            tool_calls.push(fallback);
        }
    }

    Ok(ChatResponse {
        content,
        tool_calls,
        model,
    })
}

/// Extrae una llamada JSON `{"tool":"...","arguments":{...}}` o `{"name":"...","arguments":{...}}`
/// si un modelo sin tool-calling nativo responde con un bloque JSON en texto.
pub fn extract_fallback_tool_call(content: &str) -> Option<ToolCall> {
    let start = content.find('{')?;
    let end = content.rfind('}')?;
    if end <= start {
        return None;
    }
    let slice = &content[start..=end];
    let val = JsonValue::parse(slice).ok()?;
    let name = val
        .get("tool")
        .or_else(|| val.get("name"))
        .and_then(|v| v.as_str())?;
    let arguments = val
        .get("arguments")
        .or_else(|| val.get("args"))
        .cloned()
        .unwrap_or_else(|| JsonValue::obj(vec![]));
    Some(ToolCall {
        id: None,
        name: name.to_string(),
        arguments,
    })
}

pub fn http_request(
    base_url: &str,
    method: &str,
    path: &str,
    body: Option<&str>,
    connect_timeout: Duration,
    read_timeout: Duration,
) -> Result<(u16, String), String> {
    let (host, port) = parse_host_port(base_url)?;
    let addrs: Vec<SocketAddr> = format!("{}:{}", host, port)
        .to_socket_addrs()
        .map_err(|e| format!("No se pudo resolver '{}:{}': {}", host, port, e))?
        .collect();
    let addr = addrs
        .first()
        .copied()
        .ok_or_else(|| format!("Sin direcciones IP para '{}:{}'", host, port))?;

    let mut stream = TcpStream::connect_timeout(&addr, connect_timeout)
        .map_err(|e| format!("No se pudo conectar a Ollama ({}): {}", base_url, e))?;
    let _ = stream.set_read_timeout(Some(read_timeout));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));

    let body_bytes = body.unwrap_or("").as_bytes();
    let mut req = format!(
        "{} {} HTTP/1.1\r\nHost: {}:{}\r\nConnection: close\r\nAccept: application/json\r\n",
        method, path, host, port
    );
    if !body_bytes.is_empty() {
        req.push_str("Content-Type: application/json\r\n");
        req.push_str(&format!("Content-Length: {}\r\n", body_bytes.len()));
    }
    req.push_str("\r\n");

    stream
        .write_all(req.as_bytes())
        .map_err(|e| format!("Error enviando cabecera HTTP a Ollama: {}", e))?;
    if !body_bytes.is_empty() {
        stream
            .write_all(body_bytes)
            .map_err(|e| format!("Error enviando cuerpo HTTP a Ollama: {}", e))?;
    }
    let _ = stream.flush();

    let mut raw = Vec::new();
    stream
        .read_to_end(&mut raw)
        .map_err(|e| format!("Error leyendo respuesta de Ollama: {}", e))?;

    parse_http_response(&raw)
}

pub fn parse_http_response(raw: &[u8]) -> Result<(u16, String), String> {
    let hdr_end = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| "Respuesta HTTP sin terminador de cabeceras".to_string())?;
    let hdr_str = String::from_utf8_lossy(&raw[..hdr_end]);
    let mut lines = hdr_str.lines();
    let status_line = lines
        .next()
        .ok_or_else(|| "Respuesta HTTP sin línea de estado".to_string())?;
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| format!("Línea de estado HTTP inválida: '{}'", status_line))?;

    let mut is_chunked = false;
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            if k.trim().eq_ignore_ascii_case("transfer-encoding")
                && v.to_ascii_lowercase().contains("chunked")
            {
                is_chunked = true;
            }
        }
    }

    let body_bytes = &raw[hdr_end + 4..];
    let decoded = if is_chunked {
        decode_chunked_body(body_bytes)?
    } else {
        body_bytes.to_vec()
    };
    Ok((status, String::from_utf8_lossy(&decoded).to_string()))
}

pub fn decode_chunked_body(mut data: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    loop {
        let pos = data
            .windows(2)
            .position(|w| w == b"\r\n")
            .ok_or_else(|| "Bloque HTTP chunked truncado".to_string())?;
        let size_line = String::from_utf8_lossy(&data[..pos]);
        let size_hex = size_line.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_hex, 16)
            .map_err(|_| format!("Tamaño de chunk inválido: '{}'", size_hex))?;
        data = &data[pos + 2..];
        if size == 0 {
            break;
        }
        if data.len() < size + 2 {
            return Err("Datos chunked incompletos".to_string());
        }
        out.extend_from_slice(&data[..size]);
        data = &data[size + 2..];
    }
    Ok(out)
}

pub fn is_loopback_url(url: &str) -> bool {
    match parse_host_port(url) {
        Ok((host, _)) => {
            host == "127.0.0.1" || host.eq_ignore_ascii_case("localhost") || host == "::1"
        }
        Err(_) => false,
    }
}

/// Extrae la IPv4 de Tailscale del peer `bogo` desde la salida de `tailscale status --json`.
pub fn parse_tailscale_bogo_ip(json_str: &str) -> Option<String> {
    let root = JsonValue::parse(json_str).ok()?;
    let peers = root.get("Peer")?.as_object()?;
    for (_key, peer) in peers {
        let host_name = peer
            .get("HostName")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let dns_name = peer
            .get("DNSName")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if host_name.contains("bogo") || dns_name.contains("bogo") {
            if let Some(ips) = peer.get("TailscaleIPs").and_then(|v| v.as_array()) {
                for ip_val in ips {
                    if let Some(ip_str) = ip_val.as_str() {
                        if ip_str.trim().parse::<Ipv4Addr>().is_ok() {
                            return Some(ip_str.trim().to_string());
                        }
                    }
                }
            }
        }
    }
    None
}

/// Consulta `tailscale status --json` para descubrir la IP actual del servidor `bogo`.
pub fn discover_tailscale_bogo_url() -> Option<String> {
    let out = Command::new("tailscale")
        .args(["status", "--json"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let raw = String::from_utf8_lossy(&out.stdout);
    let ip = parse_tailscale_bogo_ip(&raw)?;
    Some(format!("http://{}:11434", ip))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_url_and_chunked_http_parsing() {
        assert_eq!(normalize_base_url("127.0.0.1:11434/"), "http://127.0.0.1:11434");
        assert_eq!(parse_host_port("http://127.0.0.1:11434").unwrap(), ("127.0.0.1".to_string(), 11434));
        assert!(is_loopback_url("http://127.0.0.1:11434"));
        assert!(!is_loopback_url("http://100.86.219.90:11434"));

        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\n{\"a\":\r\n2\r\n1}\r\n0\r\n\r\n";
        let (st, body) = parse_http_response(raw).expect("chunked parse");
        assert_eq!(st, 200);
        assert_eq!(body, "{\"a\":1}");
    }

    #[test]
    fn test_parse_tailscale_bogo_ip_and_priority() {
        let ts_json = r#"{
            "Peer": {
                "nodekey:111": {"HostName": "debian", "TailscaleIPs": ["100.79.153.31"]},
                "nodekey:222": {"HostName": "bogo-MS-7E28", "DNSName": "bogo-ms-7e28.ts.net.", "TailscaleIPs": ["100.86.219.90", "fd7a:115c:a1e0::1"]}
            }
        }"#;
        assert_eq!(
            parse_tailscale_bogo_ip(ts_json).as_deref(),
            Some("100.86.219.90")
        );

        let candidates = OllamaClient::candidate_endpoints();
        let bogo_pos = candidates.iter().position(|c| c.label == "bogo");
        let local_pos = candidates.iter().position(|c| !c.is_remote);
        assert!(bogo_pos.is_some());
        if let (Some(b), Some(l)) = (bogo_pos, local_pos) {
            assert!(b < l, "bogo por Tailscale debe tener prioridad sobre local");
        }
    }

    #[test]
    fn test_tags_and_best_model_selection() {
        let json = r#"{
            "models": [
                {"name":"frankesqwen-v7:latest","details":{"parameter_size":"494M"},"capabilities":["completion"]},
                {"name":"northstar-4b:latest","details":{"parameter_size":"4.2B"},"capabilities":["completion","tools"]},
                {"name":"gemma4:e4b","details":{"parameter_size":"7.5B"},"capabilities":["completion","tools","thinking"]}
            ]
        }"#;
        let models = parse_tags_response(json).expect("tags parse");
        assert_eq!(models.len(), 3);
        assert!(!models[0].supports_tools);
        assert!(models[1].supports_tools);
        assert_eq!(
            OllamaClient::pick_best_model_for(&models, false).as_deref(),
            Some("northstar-4b:latest")
        );
        assert_eq!(
            OllamaClient::pick_best_model_for(&models, true).as_deref(),
            Some("gemma4:e4b")
        );
    }

    #[test]
    fn test_fallback_tool_call_extraction() {
        let resp = parse_chat_response(
            r#"{"model":"frankesqwen-v7","message":{"role":"assistant","content":"Voy a listar:\n{\"tool\":\"list_vms\",\"arguments\":{}}"}}"#,
        )
        .expect("chat response");
        assert_eq!(resp.tool_calls.len(), 1);
        assert_eq!(resp.tool_calls[0].name, "list_vms");
    }
}
