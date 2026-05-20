# MiMo Reasoning Proxy (Rust)

[![License: AGPL-3.0](https://img.shields.io/badge/License-AGPL--3.0-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/Rust-1.75+-orange.svg)](https://www.rust-lang.org/)
[![CI](https://github.com/YOUR_USERNAME/mimo-proxy/actions/workflows/ci.yml/badge.svg)](https://github.com/YOUR_USERNAME/mimo-proxy/actions/workflows/ci.yml)

一个为 **MiMo API**（小米大模型）设计的透明代理，主要解决以下两个问题：

1. **强制回传 `reasoning_content`**：MiMo 在"思考模式"下要求客户端必须在后续请求中回传 assistant 消息中的 `reasoning_content` 字段。多数第三方客户端（如 Roo Code）并不会自动处理该字段，导致 `400` 错误。代理会自动注入缺失的 `reasoning_content`（优先从缓存恢复，缓存未命中则补空字符串），确保请求合规。

2. **请求路径清理与容错**：自动解码并清理 Base URL 中可能存在的多余空格、编码（`%20`），避免 `404` 错误。

此外，代理还会缓存带有 `reasoning_content` 的响应，供后续相同工具调用直接复用，减少重复计算，提升效率。

## 目录

- [功能特性](#功能特性)
- [技术栈](#技术栈)
- [快速开始](#快速开始)
- [客户端配置](#客户端配置)
- [缓存说明](#缓存说明)
- [日志示例](#日志示例)
- [注意事项](#注意事项)
- [项目结构](#项目结构)
- [开发指南](#开发指南)
- [参与贡献](#参与贡献)
- [License](#license)

---

## 功能特性

- **透明代理**：位于客户端与 MiMo API 之间，无需修改客户端核心逻辑。
- **智能注入 `reasoning_content`**
    - 检查历史 assistant 消息，若缺少 `reasoning_content` 且包含 `tool_calls`，自动从缓存恢复或注入空字符串，满足 API 强制要求。
- **响应缓存**
    - 缓存同时包含 `reasoning_content` 和 `tool_calls` 的 assistant 消息，以 `content + tool_calls` 作为键，TTL 可配置，支持缓存大小上限与定期清理。
- **路径容错**
    - 自动对请求路径进行 URL 解码，去除多余空格与重复斜杠，防止因配置错误导致的 `404`。
- **灵活的 API Key 处理**
    - 支持从 `Authorization: Bearer` 或 `api-key` 请求头提取密钥，也可通过环境变量 `MIMO_API_KEY` 强制覆盖。
- **跨域支持**：内置 `CORS` 允许任何来源访问，方便本地开发。
- **健康检查与状态页面**：提供 `/health` 和 `/` 端点，实时查看代理状态和缓存统计。

---

## 技术栈

| 组件 | 技术 |
|------|------|
| Web 框架 | [Axum](https://github.com/tokio-rs/axum) 0.7 |
| 异步运行时 | [Tokio](https://tokio.rs/) 1.x |
| HTTP 客户端 | [Reqwest](https://github.com/seanmonstar/reqwest) 0.12 |
| 序列化 | [Serde](https://serde.rs/) + [Serde JSON](https://github.com/serde-rs/json) |
| CORS | [tower-http](https://github.com/tower-rs/tower-http) |
| 日志 | [tracing](https://tokio.rs/tokio/tracing) + tracing-subscriber |

---

## 快速开始

### 前置条件

- [Rust 工具链](https://www.rust-lang.org/tools/install)（稳定版即可，建议 1.75+）
- 一个可用的 MiMo API 密钥（`tp-xxx` 格式）

### 安装与编译

```bash
# 克隆仓库
git clone https://github.com/YOUR_USERNAME/mimo-proxy.git
cd mimo-proxy

# 编译（release 模式推荐）
cargo build --release
```

### 配置环境变量（可选）

| 变量名 | 说明 | 默认值 |
|--------|------|--------|
| `MIMO_TARGET_URL` | 上游 MiMo API 基础地址 | `https://token-plan-cn.xiaomimimo.com/v1` |
| `MIMO_API_KEY` | 强制使用的 API 密钥（优先级最高） | 空（使用请求头中的密钥） |

示例：

<details>
<summary>Windows PowerShell</summary>

```powershell
$env:MIMO_API_KEY="tp-xxxxxxxxxxxx"
```
</details>

<details>
<summary>Linux / macOS</summary>

```bash
export MIMO_API_KEY="tp-xxxxxxxxxxxx"
```
</details>

### 运行代理

```bash
cargo run --release
```

启动后，终端会显示监听地址和目标上游地址，例如：

```
╔══════════════════════════════════════════════╗
║         MiMo Reasoning Proxy (Rust)          ║
╠══════════════════════════════════════════════╣
║ 监听地址: http://127.0.0.1:8899
║ 目标上游: https://token-plan-cn.xiaomimimo.com/v1
║ 请将 Extension Base URL 改为:
║ http://127.0.0.1:8899/v1
╚══════════════════════════════════════════════╝
```

---

## 客户端配置

以 **Roo Code**（VS Code 扩展）为例：

1. 打开 Roo Code 设置。
2. **Provider** 选择 `openai`（OpenAI 兼容接口）。
3. **Base URL** 填写：`http://127.0.0.1:8899/v1`
   *注意：末尾必须带 `/v1`，且前后不能有多余空格。*
4. **API Key** 填入你的 MiMo 密钥（若已通过环境变量 `MIMO_API_KEY` 配置，此处可随意填写，代理会覆盖）。

保存后即可正常使用 MiMo 模型，无需担心 `reasoning_content` 问题。

---

## 缓存说明

代理缓存的是 assistant 消息中同时含有 `reasoning_content` 和 `tool_calls` 的组合，以 `content` 与 `tool_calls` 的哈希作为键。默认配置：

| 参数 | 常量名 | 默认值 |
|------|--------|--------|
| 最大缓存条目 | `CACHE_MAX_SIZE` | 2000 |
| 有效期 | `CACHE_TTL_SECS` | 7200 秒（2 小时） |
| 清理间隔 | `CACHE_CLEAN_INTERVAL_SECS` | 60 秒 |

可在 [`src/main.rs`](src/main.rs:27) 顶部的常量区域直接修改这些值。

缓存命中时，代理会将对应的 `reasoning_content` 注入到请求中，并记录日志：

```
[PATCH] Injected 1 reasoning_content from cache
```

缓存未命中时，注入空字符串并记录：

```
[PATCH] Injected empty reasoning_content for API compliance
```

---

## 日志示例

成功请求示例：

```
[REQUEST] POST /chat/completions
[REQUEST] URL: https://token-plan-cn.xiaomimimo.com/v1/chat/completions
[REQUEST] Header: api-key = tp-ctus0ahnh...
[RESPONSE] HTTP 200
```

错误情况（路径含空格）将被自动纠正：

```
原始路径：/v1%20%20%20/chat/completions → 清理后：/chat/completions
```

---

## 注意事项

1. **流式响应**：代理对流式响应（`text/event-stream`）不做任何修改，直接透传，也不缓存其中的 `reasoning_content`。
2. **仅处理 `/chat/completions`**：其他端点（如模型列表、TTS 等）直接代理，不干预消息内容。
3. **性能**：缓存使用内存中的 `HashMap`，并发访问通过 `RwLock` 控制，可承受中等负载。
4. **安全性**：代理仅在本地回环地址 `127.0.0.1` 监听，请勿直接暴露到公网。

---

## 项目结构

```
mimo-proxy/
├── .github/
│   ├── workflows/
│   │   └── ci.yml              # GitHub Actions CI 配置
│   ├── ISSUE_TEMPLATE/
│   │   ├── bug_report.md       # Bug 报告模板
│   │   └── feature_request.md  # 功能请求模板
│   └── PULL_REQUEST_TEMPLATE.md
├── src/
│   └── main.rs                 # 全部代理逻辑
├── Cargo.toml                  # 项目配置与依赖
├── LICENSE                     # AGPL-3.0 协议全文
├── .gitignore
└── README.md
```

所有核心功能均在单一文件中，方便快速部署与修改。

---

## 开发指南

### 构建

```bash
# Debug 构建（编译更快，适合开发）
cargo build

# Release 构建（优化性能）
cargo build --release
```

### 代码检查

```bash
# 格式化代码
cargo fmt

# 静态分析
cargo clippy

# 运行测试（如有）
cargo test
```

---

## 参与贡献

欢迎提交 Issue 和 Pull Request！

1. Fork 本仓库
2. 创建你的特性分支：`git checkout -b feature/amazing-feature`
3. 提交你的改动：`git commit -m 'feat: add amazing feature'`
4. 推送到分支：`git push origin feature/amazing-feature`
5. 打开一个 Pull Request

请确保：
- 代码通过 `cargo fmt` 格式化
- 代码通过 `cargo clippy` 检查无警告
- 提交信息遵循 [Conventional Commits](https://www.conventionalcommits.org/) 规范

---

## License

本项目采用 [AGPL-3.0](LICENSE) 许可证。
