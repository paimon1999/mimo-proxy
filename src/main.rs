use axum::{
    body::{Body, Bytes},
    extract::{Request, State},
    http::{HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{any, get},
    Router,
};
use futures_util::StreamExt;
use http_body_util::BodyExt;
use percent_encoding::percent_decode_str;
use reqwest::Client;
use serde_json::{json, Value as JsonValue};
use std::{
    collections::{hash_map::DefaultHasher, HashMap},
    hash::{Hash, Hasher},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::RwLock;
use tower_http::cors::CorsLayer;
use tracing::{info, warn};

// ==================== 配置 ====================
const DEFAULT_TARGET: &str = "https://token-plan-cn.xiaomimimo.com/v1";
const LISTEN_PORT: u16 = 8899;
const CACHE_MAX_SIZE: usize = 2000;
const CACHE_TTL_SECS: u64 = 7200;
const CACHE_CLEAN_INTERVAL_SECS: u64 = 60;

// ==================== 缓存 ====================
#[derive(Clone)]
struct CacheEntry {
    reasoning: String,
    expires_at: Instant,
    inserted_at: Instant, // 用于 LRU 驱逐
}

type ReasoningCache = Arc<RwLock<HashMap<String, CacheEntry>>>;

#[derive(Clone)]
struct AppState {
    client: Client,
    target_base_url: String,
    target_api_key: Option<String>,
    cache: ReasoningCache,
    cache_ttl: Duration,
    cache_max_size: usize,
}

// ==================== 主函数 ====================
#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();

    let target_base_url = std::env::var("MIMO_TARGET_URL")
        .unwrap_or_else(|_| DEFAULT_TARGET.to_string())
        .trim_end_matches('/')
        .to_string();

    let target_api_key = std::env::var("MIMO_API_KEY").ok().filter(|s| !s.is_empty());

    let cache: ReasoningCache = Arc::new(RwLock::new(HashMap::new()));
    let cache_clone = cache.clone();

    // 后台清理过期缓存
    tokio::spawn(async move {
        let interval = Duration::from_secs(CACHE_CLEAN_INTERVAL_SECS);
        loop {
            tokio::time::sleep(interval).await;
            let now = Instant::now();
            let mut guard = cache_clone.write().await;
            let before = guard.len();
            guard.retain(|_, v| v.expires_at > now);
            let after = guard.len();
            if before != after {
                info!(
                    "[CACHE] Cleaned {} expired entries, remaining {}",
                    before - after,
                    after
                );
            }
        }
    });

    let state = AppState {
        client: Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .expect("Failed to build HTTP client"),
        target_base_url: target_base_url.clone(),
        target_api_key,
        cache,
        cache_ttl: Duration::from_secs(CACHE_TTL_SECS),
        cache_max_size: CACHE_MAX_SIZE,
    };

    let app = Router::new()
        .route("/", get(status_page))
        .route("/health", get(health_check))
        .fallback(any(proxy_handler))
        .layer(CorsLayer::permissive())
        .with_state(state);

    let addr = format!("127.0.0.1:{}", LISTEN_PORT);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .expect("Failed to bind TCP listener");

    info!("╔══════════════════════════════════════════════╗");
    info!("║        MiMo Reasoning Proxy (Rust)          ║");
    info!("╠══════════════════════════════════════════════╣");
    info!("║  监听地址: http://{}               ║", addr);
    info!("║  目标上游: {}       ║", target_base_url);
    info!("║  缓存容量: {} 条 | TTL: {} 秒            ║", CACHE_MAX_SIZE, CACHE_TTL_SECS);
    info!("║  请将 Extension Base URL 改为:              ║");
    info!("║  http://127.0.0.1:{}/v1                      ║", LISTEN_PORT);
    info!("╚══════════════════════════════════════════════╝");

    axum::serve(listener, app)
        .await
        .expect("Server error");
}

// ==================== 路由处理 ====================
async fn status_page(State(state): State<AppState>) -> impl IntoResponse {
    let guard = state.cache.read().await;
    axum::Json(json!({
        "status": "ok",
        "target_base_url": state.target_base_url,
        "cache_size": guard.len(),
        "cache_max": state.cache_max_size,
        "cache_ttl_secs": state.cache_ttl.as_secs(),
        "fix_reasoning_content": true
    }))
}

async fn health_check(State(state): State<AppState>) -> impl IntoResponse {
    let guard = state.cache.read().await;
    axum::Json(json!({
        "status": "ok",
        "target_base_url": state.target_base_url,
        "cache_size": guard.len(),
        "fix_reasoning_content": true
    }))
}

// ==================== 核心代理 ====================
async fn proxy_handler(State(state): State<AppState>, req: Request) -> Result<Response, StatusCode> {
    let method = req.method().clone();
    let uri = req.uri().clone();
    let headers = req.headers().clone();

    // ==================== 路径清理（修复 Bug 1 & 2） ====================
    // 原始路径如 /v1%20%20/chat/completions → 解码为 /v1  /chat/completions
    // 需要同时按 '/' 和空白字符分割，再重新拼接为干净路径
    let raw_path = uri.path();
    let decoded = percent_decode_str(raw_path).decode_utf8_lossy();

    // 同时按 '/' 和空白字符分割，过滤空段，重新拼接
    let cleaned_path = format!(
        "/{}",
        decoded
            .split(|c: char| c == '/' || c.is_whitespace())
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("/")
    );

    // 根据 target_base_url 是否已含 /v1 来决定 upstream_path
    // target_base_url = https://token-plan-cn.xiaomimimo.com/v1
    let upstream_path: String = if cleaned_path == "/" {
        // / → /
        "/".to_string()
    } else if cleaned_path.starts_with("/v1/") {
        // /v1/xxx → /xxx (去掉 /v1 前缀)
        cleaned_path[3..].to_string()
    } else if cleaned_path == "/v1" {
        // /v1 → /
        "/".to_string()
    } else {
        // 其他情况已经是干净路径
        cleaned_path.clone()
    };

    let query = uri.query().map(|q| format!("?{}", q)).unwrap_or_default();
    let target_url = format!("{}{}{}", state.target_base_url, upstream_path, query);

    // ==================== 读取请求体（修复 Bug 4） ====================
    let body_bytes = match req.into_body().collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(e) => {
            warn!("[ERROR] Failed to collect request body: {}", e);
            return Err(StatusCode::BAD_REQUEST);
        }
    };

    // ==================== 构建转发请求头 ====================
    let mut fwd = HeaderMap::new();
    for (k, v) in headers.iter() {
        let key = k.as_str().to_lowercase();
        if key != "host"
            && key != "content-length"
            && key != "connection"
            && key != "accept-encoding"
        {
            fwd.insert(k.clone(), v.clone());
        }
    }

    // API Key 处理：提取并转换为 api-key 头
    let mut api_key = None;
    if let Some(v) = fwd
        .get("api-key")
        .and_then(|v| v.to_str().ok())
    {
        api_key = Some(v.to_string());
    }
    if api_key.is_none() {
        if let Some(v) = fwd
            .get("authorization")
            .and_then(|v| v.to_str().ok())
        {
            api_key = Some(v.trim_start_matches("Bearer ").trim().to_string());
        }
    }
    if let Some(ref env_key) = state.target_api_key {
        api_key = Some(env_key.clone());
    }

    fwd.remove("authorization");
    fwd.remove("api-key");

    // ==================== 设置 API Key Header（修复 Bug 3） ====================
    if let Some(k) = api_key {
        match HeaderValue::from_str(&k) {
            Ok(val) => {
                fwd.insert("api-key", val);
            }
            Err(e) => {
                warn!("[WARN] Failed to set api-key header: {}", e);
                return Err(StatusCode::BAD_REQUEST);
            }
        }
    }

    // ==================== 注入 reasoning_content（仅处理 chat/completions 端点） ====================
    let mut body_to_send = body_bytes.clone();
    let mut cache_hits = 0;

    if upstream_path == "/chat/completions" {
        // 修复 Bug 7：JSON 解析失败时记录警告
        match serde_json::from_slice::<JsonValue>(&body_bytes) {
            Ok(mut json) => {
                if let Some(arr) = json.get_mut("messages").and_then(|m| m.as_array_mut()) {
                    let (modified, hits, _) = fix_request_messages(arr, &state.cache).await;
                    cache_hits = hits;
                    if modified {
                        if hits > 0 {
                            info!(
                                "[PATCH] Injected {} reasoning_content from cache",
                                hits
                            );
                        } else {
                            info!(
                                "[PATCH] Injected empty reasoning_content for API compliance"
                            );
                        }
                        // 修复 Bug 9：unwrap 改为 expect
                        body_to_send =
                            Bytes::from(serde_json::to_vec(&json).expect("Failed to serialize modified JSON"));
                    }
                }
            }
            Err(e) => {
                warn!("[WARN] Failed to parse request JSON: {}", e);
            }
        }
    }

    // ==================== 记录请求摘要（修复 Bug 5 & 6） ====================
    info!("[REQUEST] {} {}", method, upstream_path);
    info!("[REQUEST] URL: {}", target_url);

    for (k, v) in fwd.iter() {
        let val = v.to_str().unwrap_or("???");
        let show = if k.as_str().to_lowercase() == "api-key" {
            // Bug 5 修复：只显示前 6 字符
            format!("{}...", &val[..val.len().min(6)])
        } else {
            val.to_string()
        };
        info!("[REQUEST] Header: {} = {}", k, show);
    }

    if !body_to_send.is_empty() {
        // Bug 6 修复：限制日志长度
        let preview = String::from_utf8_lossy(&body_to_send);
        let preview = if preview.len() > 200 {
            format!("{}...[truncated {} chars]", &preview[..200], preview.len() - 200)
        } else {
            preview.to_string()
        };
        info!("[REQUEST] Body: {}", preview);
    }
    if cache_hits > 0 {
        info!("[REQUEST] Cache hits: {}", cache_hits);
    }

    // ==================== 发送转发请求 ====================
    let mut builder = state.client.request(method, &target_url);
    for (k, v) in fwd.iter() {
        builder = builder.header(k, v);
    }

    let resp = builder
        .body(body_to_send)
        .send()
        .await
        .map_err(|e| {
            warn!("[ERROR] Upstream failed: {}", e);
            StatusCode::BAD_GATEWAY
        })?;

    let status = resp.status();
    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    let is_stream = content_type.contains("event-stream");
    let is_json = content_type.contains("application/json");

    if status.is_success() {
        info!("[RESPONSE] HTTP {}", status.as_u16());
    } else {
        warn!("[RESPONSE] HTTP {}", status.as_u16());
    }

    for (k, v) in resp.headers().iter() {
        info!("[RESPONSE] Header: {} = {:?}", k, v);
    }

    // 准备响应头
    let mut res_headers = HeaderMap::new();
    for (k, v) in resp.headers().iter() {
        let key = k.as_str().to_lowercase();
        if key != "content-length" && key != "transfer-encoding" {
            res_headers.insert(k.clone(), v.clone());
        }
    }

    // ==================== 流式响应直接透传 ====================
    if is_stream {
        let stream = resp.bytes_stream().map(|r| {
            r.map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
        });
        let mut res = Response::new(Body::from_stream(stream));
        *res.status_mut() =
            StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::OK);
        *res.headers_mut() = res_headers;
        return Ok(res);
    }

    // 普通响应：读取完整 body
    let bytes = resp.bytes().await.map_err(|e| {
        warn!("[ERROR] Read body failed: {}", e);
        StatusCode::BAD_GATEWAY
    })?;

    // 尝试缓存 reasoning_content
    if is_json && status.is_success() {
        if let Ok(json_body) = serde_json::from_slice::<JsonValue>(&bytes) {
            let cached =
                cache_response_body(&json_body, &state.cache, state.cache_ttl, state.cache_max_size)
                    .await;
            if cached > 0 {
                info!("[CACHE] Cached {} reasoning_content entries", cached);
            }
        }
    }

    // Bug 6 修复：限制日志长度
    let text = String::from_utf8_lossy(&bytes);
    let text = if text.len() > 500 {
        format!(
            "{}...[truncated {} chars]",
            &text[..500],
            text.len() - 500
        )
    } else {
        text.to_string()
    };

    if !status.is_success() {
        warn!("[RESPONSE] Body: {}", text);
    } else {
        info!("[RESPONSE] Body: {}", text);
    }

    let mut res = Response::new(Body::from(bytes));
    *res.status_mut() = StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::OK);
    *res.headers_mut() = res_headers;
    Ok(res)
}

// ==================== 缓存 Key 生成 ====================
fn cache_key(content: &str, tool_calls: &JsonValue) -> String {
    let mut hasher = DefaultHasher::new();
    content.hash(&mut hasher);
    tool_calls.to_string().hash(&mut hasher);
    format!("{:x}", hasher.finish())
}

// ==================== 请求修复（注入 reasoning_content） ====================
async fn fix_request_messages(
    messages: &mut Vec<JsonValue>,
    cache: &ReasoningCache,
) -> (bool, usize, usize) {
    let mut modified = false;
    let mut hits = 0;

    for msg in messages.iter_mut() {
        // 只处理 assistant 消息
        if msg
            .get("role")
            .and_then(|r| r.as_str())
            .map(|r| r == "assistant")
            .unwrap_or(false)
        {
            let has_tool = msg
                .get("tool_calls")
                .map(|t| {
                    !t.is_null()
                        && !t
                            .as_array()
                            .map(|a| a.is_empty())
                            .unwrap_or(true)
                })
                .unwrap_or(false);

            let has_reasoning = msg.get("reasoning_content").is_some();

            // 如果没有工具调用，或者已有 reasoning_content，则跳过
            if !has_tool || has_reasoning {
                continue;
            }

            let content = msg.get("content").and_then(|c| c.as_str()).unwrap_or("");
            let tool_calls = msg.get("tool_calls").cloned().unwrap_or(JsonValue::Null);

            let key = cache_key(content, &tool_calls);

            // 尝试从缓存恢复
            let guard = cache.read().await;
            if let Some(entry) = guard.get(&key) {
                if entry.expires_at > Instant::now() {
                    let reasoning = entry.reasoning.clone();
                    drop(guard);
                    if let Some(obj) = msg.as_object_mut() {
                        obj.insert(
                            "reasoning_content".into(),
                            JsonValue::String(reasoning),
                        );
                        modified = true;
                        hits += 1;
                    }
                    continue;
                }
            }
            drop(guard);

            // 缓存未命中：注入空 reasoning_content 以满足 API 强制要求
            if let Some(obj) = msg.as_object_mut() {
                obj.insert(
                    "reasoning_content".into(),
                    JsonValue::String(String::new()),
                );
                modified = true;
            }
        }
    }

    (modified, hits, 0)
}

// ==================== 响应缓存（修复 Bug 8: LRU） ====================
async fn cache_response_body(
    body: &JsonValue,
    cache: &ReasoningCache,
    ttl: Duration,
    max_size: usize,
) -> usize {
    let mut cached = 0;

    let choices = match body.get("choices").and_then(|c| c.as_array()) {
        Some(c) => c,
        None => return 0,
    };

    for choice in choices {
        let msg = match choice.get("message") {
            Some(m) => m,
            None => continue,
        };

        if msg
            .get("role")
            .and_then(|r| r.as_str())
            .map(|r| r == "assistant")
            .unwrap_or(false)
        {
            let has_reasoning = msg.get("reasoning_content").is_some();

            let has_tool = msg
                .get("tool_calls")
                .map(|t| {
                    !t.is_null()
                        && !t
                            .as_array()
                            .map(|a| a.is_empty())
                            .unwrap_or(true)
                })
                .unwrap_or(false);

            // 只缓存同时包含 reasoning_content 和 tool_calls 的消息
            if !has_reasoning || !has_tool {
                continue;
            }

            let content = msg.get("content").and_then(|c| c.as_str()).unwrap_or("");
            let tool_calls = msg.get("tool_calls").cloned().unwrap_or(JsonValue::Null);

            let reasoning = msg
                .get("reasoning_content")
                .and_then(|r| r.as_str())
                .unwrap_or("")
                .to_string();

            if reasoning.is_empty() {
                continue;
            }

            let key = cache_key(content, &tool_calls);

            let mut guard = cache.write().await;

            guard.insert(
                key,
                CacheEntry {
                    reasoning,
                    expires_at: Instant::now() + ttl,
                    inserted_at: Instant::now(), // 记录插入时间
                },
            );

            cached += 1;

            // 清理过期条目
            let now = Instant::now();
            guard.retain(|_, v| v.expires_at > now);

            // Bug 8 修复：LRU 驱逐 - 按插入时间删除最老的条目
            while guard.len() > max_size {
                let oldest = guard
                    .iter()
                    .min_by_key(|(_, v)| v.inserted_at)
                    .map(|(k, _)| k.clone());
                if let Some(k) = oldest {
                    guard.remove(&k);
                } else {
                    break;
                }
            }
        }
    }

    cached
}
