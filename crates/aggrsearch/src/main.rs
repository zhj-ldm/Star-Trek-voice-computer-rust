//! aggrsearch —— 面向 AI 的多聚合搜索服务（单文件，单二进制，同时提供 HTTP 与 CLI）
//!
//! 对齐 SearchPIN 的搜索方式与接口：web_search + web_fetch 两能力。
//! - 引擎：Bing 国际 > Bing CN > 百度 > 搜狗（360 已排除），必应优先级最高
//! - 每引擎一页候选池（bing count=15），合并去重 → 语义重排 → 截断 max_results
//! - 反爬：CJK 预处理 + 零硬编码质量评分 + 多级 fallback + per-engine backoff + 搜狗 TLS1.2
//! - 接口：
//!   GET /search?q=&max_results=&freshness=&topic=&include_domains=&exclude_domains=
//!   freshness: d|w|m|y（仅作用于必应 &tbs=qdr:）
//!   topic:     general|news（news 走必应新闻垂直）
//!   GET /fetch?url=   抓取 URL 全文，质量分拦截检测 + blocked 缓存(2h)
//!   GET /health
//!
//! 命令行：serve / search / fetch / health / version（`--json` 输出原始结果）
//! 诊断日志：每引擎抓取状态、字节数、质量分与反爬命中一律写 stderr，结果走 stdout

use axum::{Json, Router, extract::Query, http::StatusCode, routing::get};
use futures::{FutureExt, future::join_all};
use scraper::{Html, Selector};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::sync::{LazyLock, Mutex};
use std::time::Instant;

mod embedding;

// ---------------- 数据结构（对齐 SearchPIN 返回）----------------

#[derive(Debug, Clone, Serialize)]
struct SearchResult {
    title: String,
    url: String,
    snippet: String,
    content: String,
    #[serde(rename = "_rerank_score")]
    rerank_score: f64,
    #[serde(rename = "_source_engine")]
    source_engine: String,
}

/// 每个引擎的抓取上下文
#[derive(Clone)]
struct QueryCtx {
    query: String,
    freshness: Option<String>,
    news: bool,
}

// ---------------- 查询预处理（CJK）----------------

fn is_cjk(c: char) -> bool {
    let u = c as u32;
    (0x4E00..=0x9FFF).contains(&u) || (0x3400..=0x4DBF).contains(&u)
}

/// 删除中文字符两侧空格，防必应分词器拆词
fn prep_query(q: &str) -> String {
    let chars: Vec<char> = q.chars().collect();
    let mut out = String::new();
    for (i, &c) in chars.iter().enumerate() {
        if c.is_whitespace() {
            let prev_cjk = i > 0 && is_cjk(chars[i - 1]);
            let next_cjk = i + 1 < chars.len() && is_cjk(chars[i + 1]);
            if !(prev_cjk || next_cjk) {
                out.push(' ');
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// 压缩连续空白为单个空格并 trim（搜索引擎高亮标签会产生多余空格）
fn normalize_ws(s: &str) -> String {
    let mut out = String::new();
    let mut prev_ws = false;
    for c in s.trim().chars() {
        if c.is_whitespace() {
            if !prev_ws {
                out.push(' ');
            }
            prev_ws = true;
        } else {
            out.push(c);
            prev_ws = false;
        }
    }
    out
}

// ---------------- 质量评分（零硬编码，移植 SearchPIN quality.py）----------------

fn push_clean_char(out: &mut String, c: char) {
    if c.is_whitespace() {
        if !out.ends_with(' ') {
            out.push(' ');
        }
    } else {
        out.push(c);
    }
}

/// 分析 HTML：剥离 script/style 与标签得到纯文本，统计唯一标签种类数。
fn analyze_html(html: &str) -> (String, usize) {
    let chars: Vec<char> = html.chars().collect();
    let n = chars.len();
    let mut i = 0;
    let mut in_script = false;
    let mut in_style = false;
    let mut text = String::new();
    let mut tags: HashSet<String> = HashSet::new();
    while i < n {
        if chars[i] == '<' {
            let mut j = i + 1;
            while j < n && chars[j] != '>' {
                j += 1;
            }
            let tag_raw: String = chars[i + 1..j.min(n)].iter().collect();
            let trimmed = tag_raw.trim();
            let (is_close, name_part) = match trimmed.strip_prefix('/') {
                Some(rest) => (true, rest),
                None => (false, trimmed),
            };
            let name: String = name_part
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric())
                .collect::<String>()
                .to_lowercase();
            if !name.is_empty() {
                tags.insert(name.clone());
                if name == "script" {
                    in_script = !is_close;
                } else if name == "style" {
                    in_style = !is_close;
                }
            }
            i = if j < n { j + 1 } else { n };
        } else {
            if !in_script && !in_style {
                push_clean_char(&mut text, chars[i]);
            }
            i += 1;
        }
    }
    (text.trim().to_string(), tags.len())
}

fn count_sentences(text: &str) -> usize {
    let mut seg = 0usize;
    let mut count = 0usize;
    for c in text.chars() {
        if ".。!！?？\n".contains(c) {
            if seg > 10 {
                count += 1;
            }
            seg = 0;
        } else if c != ' ' {
            seg += 1;
        }
    }
    if seg > 10 {
        count += 1;
    }
    count
}

/// 零硬编码统计学质量评分 0.0-1.0（只测结构不测内容）
fn quality_score(html: &str) -> f64 {
    let (clean, unique_tags) = analyze_html(html);
    let text_len = clean.chars().count() as f64;
    let html_len = (html.len().max(1)) as f64;
    let dom_score = (unique_tags as f64 / 20.0).min(1.0);
    let text_ratio = text_len / html_len;
    let volume_factor = (text_len / 1000.0).min(1.0);
    let ratio_score = (text_ratio / 0.30).min(1.0) * volume_factor;
    let sent_score = (count_sentences(&clean) as f64 / 10.0).min(1.0) * volume_factor;
    let mass_score = (text_len / 2000.0).min(1.0);
    0.30 * dom_score + 0.20 * ratio_score + 0.20 * sent_score + 0.30 * mass_score
}

/// 搜索页拦截检测：antispider / wappass / 质量分过低
fn is_blocked(engine: &str, body: &str, status: u16) -> bool {
    if status >= 400 {
        return true;
    }
    if engine == "baidu" && (body.contains("wappass") || body.contains("安全验证")) {
        return true;
    }
    if engine == "sogou" && (body.contains("antispider") || body.contains("/antispider/")) {
        return true;
    }
    quality_score(body) < 0.28
}

// ---------------- 每引擎退避（backoff，指数递增封顶 120s，对齐 SearchPIN）----------------

static BACKOFF: LazyLock<Mutex<HashMap<&'static str, (Instant, u32)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
const BACKOFF_BASE_SECS: u64 = 5;
const BACKOFF_MAX_SECS: u64 = 120;

fn in_backoff(engine: &str) -> bool {
    if let Ok(m) = BACKOFF.lock() {
        if let Some((until, _)) = m.get(engine) {
            return Instant::now() < *until;
        }
    }
    false
}

fn set_backoff(engine: &'static str) {
    if let Ok(mut m) = BACKOFF.lock() {
        let (_, tries) = m.get(engine).copied().unwrap_or((Instant::now(), 0));
        let tries = tries + 1;
        let secs = (BACKOFF_BASE_SECS.saturating_mul(1u64 << tries.min(5))).min(BACKOFF_MAX_SECS);
        m.insert(
            engine,
            (Instant::now() + std::time::Duration::from_secs(secs), tries),
        );
    }
}

// ---------------- fetch 拦截缓存（blocked host，2h TTL）----------------

static BLOCKED_FETCH: LazyLock<Mutex<HashMap<String, Instant>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
const BLOCKED_TTL: u64 = 7200;

fn fetch_blocked_cached(host: &str) -> bool {
    if let Ok(m) = BLOCKED_FETCH.lock() {
        if let Some(t) = m.get(host) {
            if t.elapsed().as_secs() < BLOCKED_TTL {
                return true;
            }
            // TTL 过期 → 移除并重试
            drop(m);
            let mut m2 = BLOCKED_FETCH.lock().unwrap();
            m2.remove(host);
        }
    }
    false
}

fn cache_fetch_blocked(host: &str) {
    if let Ok(mut m) = BLOCKED_FETCH.lock() {
        m.insert(host.to_string(), Instant::now());
    }
}

// ---------------- 语义重排（词法相关度 + 引擎偏置）----------------

fn tokenize(q: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut buf = String::new();
    for c in q.chars() {
        if c.is_whitespace() || c.is_ascii_punctuation() {
            if c == '.' && !buf.is_empty() && buf.chars().all(|x| x.is_ascii_digit()) {
                buf.push('.');
                continue;
            }
            if buf.len() >= 2 {
                out.push(buf.clone());
            }
            buf.clear();
        } else if c.is_ascii_alphanumeric() {
            buf.push(c.to_ascii_lowercase());
        } else if is_cjk(c) {
            if buf.len() >= 2 {
                out.push(buf.clone());
            }
            buf.clear();
            out.push(c.to_string());
        } else {
            buf.push(c);
        }
    }
    if buf.len() >= 2 {
        out.push(buf);
    }
    out
}

/// 相关度：query 特征在 title（权重3）/ snippet（权重1）命中率 ∈[0,1]
fn relevance(query: &str, title: &str, snippet: &str) -> f64 {
    let toks = tokenize(query);
    if toks.is_empty() {
        return 0.0;
    }
    let tl = title.to_lowercase();
    let sl = snippet.to_lowercase();
    let mut score = 0.0f64;
    for t in &toks {
        if tl.contains(t.as_str()) {
            score += 3.0;
        }
        if sl.contains(t.as_str()) {
            score += 1.0;
        }
    }
    (score / (toks.len() as f64 * 3.0 + 1.0)).min(1.0)
}

/// 引擎偏置：必应最高（满足"必应优先级最高"）
fn engine_bias(name: &str) -> f64 {
    match name {
        "bing_intl" => 1.0,
        "bing_cn" => 0.8,
        "baidu" => 0.5,
        "sogou" => 0.4,
        _ => 0.0,
    }
}

/// 语义重排：必应偏置最高，高相关可浮上；设置 rerank_score
fn rerank(query: &str, results: &mut Vec<SearchResult>, max_results: usize) {
    let prep = prep_query(query);
    for r in results.iter_mut() {
        r.rerank_score = relevance(&prep, &r.title, &r.snippet);
    }
    results.sort_by(|a, b| {
        let sa = engine_bias(&a.source_engine) + a.rerank_score * 2.0;
        let sb = engine_bias(&b.source_engine) + b.rerank_score * 2.0;
        sb.total_cmp(&sa)
    });
    results.truncate(max_results);
}

// ---------------- 域名过滤 ----------------

fn parse_domains(s: Option<&String>) -> HashSet<String> {
    match s {
        Some(v) => v
            .split(',')
            .map(|x| x.trim().to_lowercase())
            .filter(|x| !x.is_empty())
            .collect(),
        None => HashSet::new(),
    }
}

fn url_host(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|s| s.to_lowercase()))
        .unwrap_or_default()
}

/// 过滤 include/exclude domains（重排前）
fn filter_domains(
    results: &mut Vec<SearchResult>,
    include: &HashSet<String>,
    exclude: &HashSet<String>,
) {
    if !include.is_empty() {
        results.retain(|r| {
            let h = url_host(&r.url);
            include
                .iter()
                .any(|d| h == *d || h.ends_with(&format!(".{}", d)))
        });
    }
    if !exclude.is_empty() {
        results.retain(|r| {
            let h = url_host(&r.url);
            !exclude
                .iter()
                .any(|d| h == *d || h.ends_with(&format!(".{}", d)))
        });
    }
}

// ---------------- HTTP 客户端 ----------------

const USER_AGENTS: &[&str] = &[
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/125.0 Safari/537.36",
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.4 Safari/605.1.15",
];

/// 常规客户端：完整浏览器头 + 自身域名 Referer（对齐 SearchPIN：百度无 Referer 会返回 wappass 验证码）
fn new_cookie_client(referer: &str) -> reqwest::Client {
    use reqwest::header::{ACCEPT, ACCEPT_LANGUAGE, REFERER};
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        ACCEPT,
        "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8"
            .parse()
            .unwrap(),
    );
    headers.insert(ACCEPT_LANGUAGE, "zh-CN,zh;q=0.9,en;q=0.5".parse().unwrap());
    headers.insert(REFERER, referer.parse().unwrap());
    reqwest::Client::builder()
        .default_headers(headers)
        .cookie_store(true)
        .user_agent(USER_AGENTS[0])
        .timeout(std::time::Duration::from_secs(8))
        .build()
        .expect("构建 HTTP 客户端失败")
}

/// 搜狗专用：TLS1.2（规避无头指纹）+ 自身 Referer
fn new_sogou_client() -> reqwest::Client {
    use reqwest::header::REFERER;
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(REFERER, "https://www.sogou.com/".parse().unwrap());
    reqwest::Client::builder()
        .default_headers(headers)
        .cookie_store(true)
        .user_agent(USER_AGENTS[0])
        .min_tls_version(reqwest::tls::Version::TLS_1_2)
        .timeout(std::time::Duration::from_secs(8))
        .build()
        .expect("构建搜狗 HTTP 客户端失败")
}

fn escape_path(q: &str) -> String {
    url::form_urlencoded::byte_serialize(q.as_bytes()).collect::<String>()
}

/// 构建必应 URL（对齐 SearchPIN）：浏览器真实参数 + count=15 + freshness(&tbs) + topic(news)
fn bing_search_url(q: &str, cn: bool, news: bool, freshness: &Option<String>) -> String {
    let char_count = q.chars().count();
    let word_count = q.split_whitespace().count().max(1);
    let sc = format!("{}-{}", char_count, word_count);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let cvid = format!("{:032X}", now);
    let mkt = if cn { "" } else { "&setmkt=en-US" };
    let fresh = match freshness {
        Some(f) => format!("&tbs=qdr:{}", f),
        None => String::new(),
    };
    let host = if cn { "cn.bing.com" } else { "www.bing.com" };
    let path = if news { "/news/search" } else { "/search" };
    if news {
        format!("https://{host}{path}?q={q}&first=1{mkt}{fresh}")
    } else {
        format!(
            "https://{host}{path}?q={q}&qs=n&form=QBRE&sp=-1&lq=0&pq={q}&sc={sc}&sk=&cvid={cvid}&count=15{mkt}{fresh}"
        )
    }
}

// ---------------- 解析器（多级 fallback）----------------

fn pick(el: &scraper::ElementRef, sel_str: &str, attr_name: &str) -> Option<String> {
    let sel = Selector::parse(sel_str).ok()?;
    let inner = el.select(&sel).next()?;
    if attr_name == "_text" {
        Some(normalize_ws(&inner.text().collect::<Vec<_>>().join(" ")))
    } else {
        Some(normalize_ws(inner.value().attr(attr_name)?))
    }
}

fn text(el: &scraper::ElementRef, sel_str: &str) -> Option<String> {
    pick(el, sel_str, "_text")
}

fn new_result(url: String, title: String, content: String, engine: &str) -> SearchResult {
    SearchResult {
        title: normalize_ws(&title),
        url,
        snippet: normalize_ws(&content),
        content: normalize_ws(&content),
        rerank_score: 0.0,
        source_engine: engine.to_string(),
    }
}

/// 通用 <a> 链接兜底：过滤自身域名，去重，标题≥4 字
fn generic_fallback(body: &str, limit: usize, engine: &str, self_host: &str) -> Vec<SearchResult> {
    let Ok(a_sel) = Selector::parse("a[href]") else {
        return Vec::new();
    };
    let doc = Html::parse_document(body);
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for el in doc.select(&a_sel) {
        let href = el.value().attr("href").unwrap_or("").trim();
        if href.is_empty() {
            continue;
        }
        let host = url::Url::parse(href)
            .ok()
            .and_then(|u| u.host_str().map(|s| s.to_string()))
            .unwrap_or_default();
        if host.is_empty() || host == self_host || host.ends_with(&format!(".{}", self_host)) {
            continue;
        }
        let title = normalize_ws(&el.text().collect::<Vec<_>>().join(" "));
        if title.chars().count() < 4 {
            continue;
        }
        let key = href.to_lowercase();
        if !seen.insert(key) {
            continue;
        }
        out.push(new_result(href.to_string(), title, String::new(), engine));
        if out.len() >= limit {
            break;
        }
    }
    out
}

/// 解析 Bing 系：主解析 li.b_algo → 通用 <a> 兜底
fn parse_bing(body: &str, limit: usize, engine: &str, self_host: &str) -> Vec<SearchResult> {
    let doc = Html::parse_document(body);
    let mut out = Vec::new();
    if let Ok(sel) = Selector::parse("li.b_algo") {
        for item in doc.select(&sel) {
            let link = pick(&item, "h2 a", "href").unwrap_or_default();
            let title = text(&item, "h2 a").unwrap_or_default();
            let content = text(&item, ".b_caption p").unwrap_or_default();
            if !link.is_empty() && !title.is_empty() {
                out.push(new_result(link, title, content, engine));
            }
            if out.len() >= limit {
                break;
            }
        }
    }
    if out.is_empty() {
        out = generic_fallback(body, limit, engine, self_host);
    }
    out
}

/// 从指定位置向后提取百度 SSR hydration JSON（<!--s-data: -->）里的摘要 text
/// 从每条结果的 </h3> 结束位置就近找，避免命中 tabList/"问AI" 等页面级文案
fn extract_baidu_sdata_from(body: &str, from: usize) -> Option<String> {
    let marker = "<!--s-data:";
    let rel = body[from..].find(marker)?;
    let start = from + rel + marker.len();
    let after = &body[start..];
    let end = after.find("-->")?;
    let json = &after[..end];
    let key = "\"text\":\"";
    let tstart = json.find(key)?;
    let ts = &json[tstart + key.len()..];
    let mut txt = String::new();
    let mut i = 0;
    let cs: Vec<char> = ts.chars().collect();
    while i < cs.len() && cs[i] != '"' {
        if cs[i] == '\\' && i + 1 < cs.len() {
            let n = cs[i + 1];
            let c = match n {
                'n' => '\n',
                't' => '\t',
                '"' => '"',
                '/' => '/',
                '\\' => '\\',
                _ => n,
            };
            txt.push(c);
            i += 2;
        } else {
            txt.push(cs[i]);
            i += 1;
        }
    }
    let cleaned = txt
        .replace("</em>", "")
        .replace("<em>", "")
        .trim()
        .to_string();
    if cleaned.is_empty() {
        None
    } else {
        Some(normalize_ws(&cleaned))
    }
}

/// 收集所有 </h3> 结束位置
fn collect_h3_ends(body: &str) -> Vec<usize> {
    let mut ends = Vec::new();
    let mut from = 0usize;
    while let Some(p) = body[from..].find("</h3>") {
        let abs = from + p + 5;
        ends.push(abs);
        from = abs;
    }
    ends
}

/// 解析百度：SSR JSON 提取 url（过滤 baidu 子域）+ h3 配对标题 + 就近 s-data 摘要
fn parse_baidu(body: &str, limit: usize) -> Vec<SearchResult> {
    let mut real_urls: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut rest = body;
    while let Some(idx) = rest.find("\"url\":\"") {
        let s = &rest[idx + "\"url\":\"".len()..];
        let end = s.find('"').unwrap_or(s.len());
        let u = &s[..end];
        if let Ok(p) = url::Url::parse(u) {
            let host = p.host_str().unwrap_or("").to_lowercase();
            let keep = !host.is_empty() && host != "baidu.com" && !host.ends_with(".baidu.com");
            if keep {
                let key = u.trim_end_matches('/').to_lowercase().to_string();
                if seen.insert(key) {
                    real_urls.push(u.to_string());
                }
            }
        }
        rest = &rest[idx + 1..];
    }

    let h3_ends = collect_h3_ends(body);
    let doc = Html::parse_document(body);
    let mut pairs: Vec<(String, usize)> = Vec::new(); // (title, h3_end_pos)
    if let Ok(h3) = Selector::parse("h3") {
        for (i, el) in doc.select(&h3).enumerate() {
            let t = pick(&el, ".tts-b-hl", "_text")
                .or_else(|| text(&el, "h3"))
                .unwrap_or_default();
            let end = h3_ends.get(i).copied().unwrap_or(0);
            if t.chars().count() >= 3 {
                pairs.push((t, end));
            }
        }
    }

    let mut out = Vec::new();
    for (i, (title, end)) in pairs.iter().enumerate() {
        if out.len() >= limit || i >= real_urls.len() {
            break;
        }
        let url = &real_urls[i];
        if url.contains("baidu.php") {
            continue;
        }
        let content = extract_baidu_sdata_from(body, *end).unwrap_or_default();
        out.push(new_result(url.clone(), title.clone(), content, "baidu"));
    }
    if out.is_empty() {
        out = generic_fallback(body, limit, "baidu", "baidu.com");
    }
    out
}

/// 解析搜狗：div.vrwrap / div.rb → 通用 <a> 兜底
fn parse_sogou(body: &str, limit: usize) -> Vec<SearchResult> {
    let doc = Html::parse_document(body);
    let mut out = Vec::new();
    for sel_str in ["div.vrwrap", "div.rb"] {
        if out.len() >= limit {
            break;
        }
        let Ok(sel) = Selector::parse(sel_str) else {
            continue;
        };
        for item in doc.select(&sel) {
            let link = pick(&item, "h3 a, h4 a", "href").unwrap_or_default();
            let title = text(&item, "h3 a, h4 a").unwrap_or_default();
            let content = text(&item, ".space-txt, .text-layout, p").unwrap_or_default();
            if !link.is_empty() && !title.is_empty() {
                out.push(new_result(link, title, content, "sogou"));
            }
            if out.len() >= limit {
                break;
            }
        }
    }
    if out.is_empty() {
        out = generic_fallback(body, limit, "sogou", "sogou.com");
    }
    out
}

// ---------------- 引擎实现 ----------------

type Fetcher =
    fn(QueryCtx) -> futures::future::BoxFuture<'static, Result<Vec<SearchResult>, String>>;

struct Engine {
    name: &'static str,
    fetcher: Fetcher,
}

/// 每引擎候选抓取数（对齐 SearchPIN：bing count=15）
const CANDIDATE_LIMIT: usize = 15;

fn bing_intl_fetch(
    ctx: QueryCtx,
) -> futures::future::BoxFuture<'static, Result<Vec<SearchResult>, String>> {
    async move {
        if in_backoff("bing_intl") {
            return Ok(Vec::new());
        }
        let q = prep_query(&ctx.query);
        let client = new_cookie_client("https://www.bing.com/");
        let url = bing_search_url(&q, false, ctx.news, &ctx.freshness);
        let resp = client.get(&url).send().await.map_err(|e| e.to_string())?;
        let status = resp.status().as_u16();
        let body = resp.text().await.map_err(|e| e.to_string())?;
        eprintln!(
            "[bing_intl] status={} bytes={} quality={:.2}",
            status,
            body.len(),
            quality_score(&body)
        );
        if is_blocked("bing_intl", &body, status) {
            set_backoff("bing_intl");
            return Ok(Vec::new());
        }
        Ok(parse_bing(&body, CANDIDATE_LIMIT, "bing_intl", "bing.com"))
    }
    .boxed()
}

fn bing_cn_fetch(
    ctx: QueryCtx,
) -> futures::future::BoxFuture<'static, Result<Vec<SearchResult>, String>> {
    async move {
        if in_backoff("bing_cn") {
            return Ok(Vec::new());
        }
        let q = prep_query(&ctx.query);
        let client = new_cookie_client("https://cn.bing.com/");
        let url = bing_search_url(&q, true, ctx.news, &ctx.freshness);
        let resp = client.get(&url).send().await.map_err(|e| e.to_string())?;
        let status = resp.status().as_u16();
        let body = resp.text().await.map_err(|e| e.to_string())?;
        eprintln!(
            "[bing_cn] status={} bytes={} quality={:.2}",
            status,
            body.len(),
            quality_score(&body)
        );
        if is_blocked("bing_cn", &body, status) {
            set_backoff("bing_cn");
            return Ok(Vec::new());
        }
        Ok(parse_bing(&body, CANDIDATE_LIMIT, "bing_cn", "cn.bing.com"))
    }
    .boxed()
}

fn baidu_fetch(
    ctx: QueryCtx,
) -> futures::future::BoxFuture<'static, Result<Vec<SearchResult>, String>> {
    async move {
        if in_backoff("baidu") {
            return Ok(Vec::new());
        }
        let q = prep_query(&ctx.query);
        let client = new_cookie_client("https://www.baidu.com/");
        let _ = client.get("https://www.baidu.com/").send().await;
        let url = format!("https://www.baidu.com/s?wd={}&pn=0", escape_path(&q));
        let resp = client.get(&url).send().await.map_err(|e| e.to_string())?;
        let status = resp.status().as_u16();
        let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
        let body = String::from_utf8(bytes.to_vec())
            .unwrap_or_else(|_| String::from_utf8_lossy(&bytes).into_owned());
        eprintln!(
            "[baidu] status={} bytes={} wappass={} quality={:.2}",
            status,
            body.len(),
            body.contains("wappass"),
            quality_score(&body)
        );
        if is_blocked("baidu", &body, status) {
            set_backoff("baidu");
            return Ok(Vec::new());
        }
        Ok(parse_baidu(&body, CANDIDATE_LIMIT))
    }
    .boxed()
}

fn sogou_fetch(
    ctx: QueryCtx,
) -> futures::future::BoxFuture<'static, Result<Vec<SearchResult>, String>> {
    async move {
        if in_backoff("sogou") {
            return Ok(Vec::new());
        }
        let q = prep_query(&ctx.query);
        let client = new_sogou_client();
        let _ = client.get("https://www.sogou.com/").send().await;
        let url = format!("https://www.sogou.com/web?query={}&page=1", escape_path(&q));
        let resp = client.get(&url).send().await.map_err(|e| e.to_string())?;
        let status = resp.status().as_u16();
        let body = resp.text().await.map_err(|e| e.to_string())?;
        eprintln!(
            "[sogou] status={} bytes={} antispider={} quality={:.2}",
            status,
            body.len(),
            body.contains("antispider"),
            quality_score(&body)
        );
        if is_blocked("sogou", &body, status) {
            set_backoff("sogou");
            return Ok(Vec::new());
        }
        Ok(parse_sogou(&body, CANDIDATE_LIMIT))
    }
    .boxed()
}

const ENGINES: &[Engine] = &[
    Engine {
        name: "bing_intl",
        fetcher: bing_intl_fetch,
    },
    Engine {
        name: "bing_cn",
        fetcher: bing_cn_fetch,
    },
    Engine {
        name: "baidu",
        fetcher: baidu_fetch,
    },
    Engine {
        name: "sogou",
        fetcher: sogou_fetch,
    },
];

// ---------------- 搜索流程 ----------------

/// 并发抓取指定引擎 → 合并去重（按 url），返回候选池
async fn fetch_candidates(ctx: &QueryCtx, engines: &[String]) -> Vec<SearchResult> {
    let futures: Vec<_> = engines
        .iter()
        .map(|name| {
            let c = ctx.clone();
            let e = name.clone();
            tokio::spawn(async move {
                let en = ENGINES.iter().find(|en| en.name == e);
                if let Some(en) = en {
                    (en.fetcher)(c).await.unwrap_or_default()
                } else {
                    Vec::new()
                }
            })
        })
        .collect();

    let mut all: Vec<SearchResult> = join_all(futures)
        .await
        .into_iter()
        .filter_map(|r| r.ok())
        .flatten()
        .collect();

    // 按 url 去重（保留首个）
    let mut seen = HashSet::new();
    all.retain(|r| {
        let key = r.url.to_lowercase().trim_end_matches('/').to_string();
        seen.insert(key)
    });
    all
}

/// 用本地 embedding 余弦相似度重排（对齐 SearchPIN）；失败返回 false 交给词法重排
async fn apply_embedding_rerank(
    query: &str,
    results: &mut Vec<SearchResult>,
    max_results: usize,
) -> bool {
    if !embedding::enabled() || results.is_empty() {
        return false;
    }

    let docs: Vec<(String, String)> = results
        .iter()
        .map(|r| (r.title.clone(), r.snippet.clone()))
        .collect();
    let q = query.to_string();
    let joined = tokio::task::spawn_blocking(move || embedding::score(&q, &docs)).await;

    let scores = match joined {
        Ok(Ok(scores)) => scores,
        Ok(Err(e)) => {
            eprintln!("[embedding] 重排失败，回退词法: {e}");
            return false;
        }
        Err(e) => {
            eprintln!("[embedding] 重排任务失败，回退词法: {e}");
            return false;
        }
    };

    if scores.len() != results.len() {
        eprintln!(
            "[embedding] 分数数量不匹配 ({} != {})，回退词法",
            scores.len(),
            results.len()
        );
        return false;
    }

    for (result, score) in results.iter_mut().zip(scores) {
        result.rerank_score = score;
    }
    results.sort_by(|a, b| b.rerank_score.total_cmp(&a.rerank_score));
    results.truncate(max_results);
    eprintln!("[embedding] reranked {} results", results.len());
    true
}

// ---------------- 服务：/search ----------------

#[derive(Debug, serde::Deserialize)]
struct SearchParams {
    q: String,
    #[serde(rename = "max_results")]
    max_results: Option<usize>,
    freshness: Option<String>,
    topic: Option<String>,
    #[serde(rename = "include_domains")]
    include_domains: Option<String>,
    #[serde(rename = "exclude_domains")]
    exclude_domains: Option<String>,
    engines: Option<String>,
}

async fn handle_search(Query(params): Query<SearchParams>) -> Json<serde_json::Value> {
    let start = Instant::now();
    let query = params.q.trim().to_string();
    if query.is_empty() {
        return Json(serde_json::json!({"error": "empty query", "results": [], "query": ""}));
    }
    let max_results = params.max_results.unwrap_or(10).clamp(1, 20);
    let freshness = params
        .freshness
        .filter(|f| matches!(f.as_str(), "d" | "w" | "m" | "y"));
    let news = params.topic.as_deref() == Some("news");

    let include = parse_domains(params.include_domains.as_ref());
    let exclude = parse_domains(params.exclude_domains.as_ref());

    let requested: Vec<String> = match &params.engines {
        Some(s) => s
            .split(',')
            .map(|x| x.trim().to_string())
            .filter(|x| !x.is_empty())
            .collect(),
        None => ENGINES.iter().map(|e| e.name.to_string()).collect(),
    };
    let engines: Vec<String> = ENGINES
        .iter()
        .map(|e| e.name.to_string())
        .filter(|n| requested.contains(n))
        .collect();

    let ctx = QueryCtx {
        query: query.clone(),
        freshness: freshness.clone(),
        news,
    };
    let t_search_start = Instant::now();
    // 总超时兜底：任一个引擎被墙/超时也不拖死整次搜索（每引擎自身 8s 超时）
    let mut all: Vec<SearchResult> = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        fetch_candidates(&ctx, &engines),
    )
    .await
    .unwrap_or_default();
    let search_ms = t_search_start.elapsed().as_millis() as f64 / 1000.0;
    let merged = all.len();

    // domain 过滤（重排前，对齐 SearchPIN）
    filter_domains(&mut all, &include, &exclude);

    let t_rerank_start = Instant::now();
    let embedding_used = apply_embedding_rerank(&query, &mut all, max_results).await;
    if !embedding_used {
        rerank(&query, &mut all, max_results);
    }
    let rerank_ms = t_rerank_start.elapsed().as_millis() as f64 / 1000.0;

    // engine 分布
    let mut engine_counts: HashMap<String, usize> = HashMap::new();
    let out_results: Vec<serde_json::Value> = all
        .iter()
        .map(|r| {
            *engine_counts.entry(r.source_engine.clone()).or_insert(0) += 1;
            serde_json::json!({
                "title": r.title,
                "url": r.url,
                "snippet": r.snippet,
                "content": r.content,
                "_rerank_score": r.rerank_score,
                "_source_engine": r.source_engine,
            })
        })
        .collect();

    let total = start.elapsed().as_secs_f64();
    Json(serde_json::json!({
        "results": out_results,
        "query": query,
        "backend": "multi",
        "_timing": {
            "total": round2(total),
            "stages": {
                "search": round2(search_ms),
                "rerank": round2(rerank_ms),
                "pages": 1,
                "page_ms": [round2(search_ms * 1000.0)],
                "num_results_merged": merged,
                "embedding_rerank": embedding_used,
            },
            "engine_counts": engine_counts,
        }
    }))
}

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

// ---------------- 服务：/fetch ----------------

/// 提取正文文本（剥离 script/style + 标签），供 /fetch 使用
fn extract_clean_text(body: &str) -> String {
    let (clean, _) = analyze_html(body);
    clean
}

#[derive(Debug, serde::Deserialize)]
struct FetchParams {
    url: String,
}

async fn handle_fetch(Query(params): Query<FetchParams>) -> Json<serde_json::Value> {
    let start = Instant::now();
    let url = params.url.trim().to_string();
    if url.is_empty() {
        return Json(
            serde_json::json!({"status": 0, "content_type": "", "body": "", "error": "empty url", "_timing": {"total": 0}}),
        );
    }
    let host = url_host(&url);
    if !host.is_empty() && fetch_blocked_cached(&host) {
        return Json(
            serde_json::json!({"status": 0, "content_type": "", "body": "", "error": format!("Domain {} blocked by CDN/WAF (cached)", host), "_timing": {"total": round2(start.elapsed().as_secs_f64())}}),
        );
    }

    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::ACCEPT,
        "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8"
            .parse()
            .unwrap(),
    );
    headers.insert(
        reqwest::header::ACCEPT_LANGUAGE,
        "zh-CN,zh;q=0.9,en;q=0.7".parse().unwrap(),
    );
    if !host.is_empty() {
        headers.insert(
            reqwest::header::REFERER,
            format!("https://{}/", host).parse().unwrap(),
        );
    }
    let client = reqwest::Client::builder()
        .default_headers(headers)
        .cookie_store(true)
        .user_agent(USER_AGENTS[0])
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .expect("构建 fetch 客户端失败");

    let resp_result = client.get(&url).send().await;
    let (status, content_type, body) = match resp_result {
        Ok(resp) => {
            let status = resp.status().as_u16();
            let ct = resp
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            let bytes = resp.bytes().await.unwrap_or_default();
            let body = String::from_utf8(bytes.to_vec())
                .unwrap_or_else(|_| String::from_utf8_lossy(&bytes).into_owned());
            (status, ct, body)
        }
        Err(e) => {
            return Json(
                serde_json::json!({"status": 0, "content_type": "", "body": "", "error": e.to_string(), "_timing": {"total": round2(start.elapsed().as_secs_f64())}}),
            );
        }
    };

    // 质量分拦截检测（对齐 SearchPIN：<0.35 判 blocked）
    let q = quality_score(&body);
    let blocked = status >= 400 || q < 0.35;
    if blocked {
        if !host.is_empty() {
            cache_fetch_blocked(&host);
        }
        return Json(
            serde_json::json!({"status": 0, "content_type": content_type, "body": "", "error": format!("Domain {} blocked by CDN/WAF (quality {:.2})", host, q), "_timing": {"total": round2(start.elapsed().as_secs_f64())}}),
        );
    }

    let text = extract_clean_text(&body);
    Json(serde_json::json!({
        "status": status,
        "content_type": content_type,
        "body": text,
        "error": null,
        "_timing": {"total": round2(start.elapsed().as_secs_f64())},
    }))
}

// ---------------- 版本与健康检查 ----------------

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn health_payload() -> serde_json::Value {
    serde_json::json!({
        "status": "ok",
        "name": "aggrsearch",
        "version": VERSION,
        "embedding": embedding::status(),
    })
}

async fn handle_health() -> Json<serde_json::Value> {
    Json(health_payload())
}

async fn fallback_404() -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({"error": "not found"})),
    )
}

// ---------------- HTTP 服务 ----------------

fn build_router() -> Router {
    Router::new()
        .route("/search", get(handle_search))
        .route("/fetch", get(handle_fetch))
        .route("/health", get(handle_health))
        .fallback(fallback_404)
}

fn print_banner(port: u16) {
    println!("aggrsearch v{VERSION} 已启动: http://127.0.0.1:{port}");
    println!("  引擎: bing_intl > bing_cn > baidu > sogou (360 已排除)");
    println!("  能力: web_search + web_fetch");
    if embedding::enabled() {
        let model = embedding::status()["model"]
            .as_str()
            .unwrap_or("embedding")
            .to_string();
        println!("  重排: embedding ({model})");
    } else {
        let reason = match embedding::mode() {
            embedding::Mode::Off => "已通过 AGGRSEARCH_EMBED=off 关闭",
            embedding::Mode::On => "模型缺失",
            embedding::Mode::Auto => "未安装本地模型",
        };
        println!("  重排: 词法（{reason}；运行 `aggrsearch models download` 启用向量重排）");
    }
    println!(
        "  GET /search?q=&max_results=&freshness=d|w|m|y&topic=general|news&include_domains=&exclude_domains="
    );
    println!("  GET /fetch?url=");
    println!("  GET /health");
}

async fn serve(port: u16) {
    let app = build_router();
    let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{port}"))
        .await
        .unwrap_or_else(|e| {
            eprintln!("端口绑定失败: {e}");
            std::process::exit(1);
        });
    print_banner(port);
    tokio::task::spawn_blocking(embedding::warmup);
    if let Err(e) = axum::serve(listener, app).await {
        eprintln!("服务异常退出: {e}");
        std::process::exit(1);
    }
}

// ---------------- 模型管理子命令 ----------------

fn cmd_models(args: &[String]) -> i32 {
    let action = args.first().map(|s| s.as_str()).unwrap_or("status");
    let model = args
        .iter()
        .position(|a| a == "--model")
        .and_then(|i| args.get(i + 1))
        .map(|s| s.as_str());
    match action {
        "status" => {
            print_pretty(&embedding::status());
            0
        }
        "download" => match embedding::download(model) {
            Ok(dir) => {
                println!("模型已安装: {}", dir.display());
                println!("提示：保持 AGGRSEARCH_EMBED=auto（默认）即可自动使用向量重排。");
                0
            }
            Err(e) => {
                eprintln!("下载失败: {e}");
                1
            }
        },
        "remove" => match embedding::remove(model) {
            Ok(dir) => {
                println!("已删除本地模型缓存: {}", dir.display());
                println!("后续搜索将使用词法重排。");
                0
            }
            Err(e) => {
                eprintln!("删除失败: {e}");
                1
            }
        },
        other => bail(&format!(
            "未知 models 子命令: {other}（可用: status / download / remove）"
        )),
    }
}

// ---------------- 命令行接口 ----------------

const USAGE: &str = r#"aggrsearch —— 面向 AI 的多聚合搜索服务

用法:
  aggrsearch serve [--port <端口>]      启动本地 HTTP 服务（默认 127.0.0.1:8080）
  aggrsearch search <关键词...> [选项]  直接搜索并打印结果
  aggrsearch fetch <URL> [--json]       抓取网页正文
  aggrsearch models status              查看向量重排模型状态（默认）
  aggrsearch models download [--model <gemma|gemma-q4|minilm>]
                                        显式下载并安装本地模型（唯一联网入口）
  aggrsearch models remove [--model ...] 删除本地模型，回到词法重排
  aggrsearch health                     打印健康检查 JSON
  aggrsearch version                    打印版本号

向量重排默认 auto：本地有完整模型才启用，没有就用词法重排，绝不自动下载。

search 选项:
  --max <n>              返回条数（1-20，默认 10）
  --freshness <d|w|m|y>  时间范围（仅必应生效）
  --topic <news>         走必应新闻垂直
  --include <a.com,b>    仅保留指定域名
  --exclude <a.com,b>    排除指定域名
  --engines <a,b>        指定引擎：bing_intl,bing_cn,baidu,sogou
  --json                 输出原始 JSON（便于管道交给 LLM）
"#;

struct CliOpts {
    positional: Vec<String>,
    max: Option<usize>,
    freshness: Option<String>,
    topic: Option<String>,
    include: Option<String>,
    exclude: Option<String>,
    engines: Option<String>,
    json: bool,
}

fn next_value(args: &[String], i: &mut usize, name: &str) -> Result<String, String> {
    *i += 1;
    args.get(*i)
        .cloned()
        .ok_or_else(|| format!("{name} 缺少参数"))
}

fn parse_opts(args: &[String]) -> Result<CliOpts, String> {
    let mut o = CliOpts {
        positional: Vec::new(),
        max: None,
        freshness: None,
        topic: None,
        include: None,
        exclude: None,
        engines: None,
        json: false,
    };
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--max" | "--max-results" => {
                let v = next_value(args, &mut i, "--max")?;
                o.max = Some(
                    v.parse()
                        .map_err(|_| format!("--max 需要数字，收到: {v}"))?,
                );
            }
            "--freshness" => o.freshness = Some(next_value(args, &mut i, "--freshness")?),
            "--topic" => o.topic = Some(next_value(args, &mut i, "--topic")?),
            "--include" | "--include-domains" => {
                o.include = Some(next_value(args, &mut i, "--include")?)
            }
            "--exclude" | "--exclude-domains" => {
                o.exclude = Some(next_value(args, &mut i, "--exclude")?)
            }
            "--engines" => o.engines = Some(next_value(args, &mut i, "--engines")?),
            "--json" => o.json = true,
            "-h" | "--help" => return Err(String::new()),
            other if other.starts_with('-') && other.len() > 1 => {
                return Err(format!("未知选项: {other}"));
            }
            other => o.positional.push(other.to_string()),
        }
        i += 1;
    }
    Ok(o)
}

fn resolve_port(args: &[String]) -> u16 {
    let mut port: Option<u16> = std::env::var("PORT").ok().and_then(|s| s.parse().ok());
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--port" | "-p" => {
                if let Some(v) = args.get(i + 1).and_then(|s| s.parse::<u16>().ok()) {
                    port = Some(v);
                }
                i += 2;
            }
            v => {
                if let Ok(p) = v.parse::<u16>() {
                    port = Some(p);
                }
                i += 1;
            }
        }
    }
    port.unwrap_or(8080)
}

fn first_n_chars(s: &str, n: usize) -> String {
    let mut out: String = s.chars().take(n).collect();
    if s.chars().count() > n {
        out.push('~');
    }
    out
}

fn print_search_text(value: &serde_json::Value) {
    let empty: Vec<serde_json::Value> = Vec::new();
    let results = value
        .get("results")
        .and_then(|v| v.as_array())
        .unwrap_or(&empty);
    let total = value
        .pointer("/_timing/total")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    println!(
        "查询: {}",
        value.get("query").and_then(|v| v.as_str()).unwrap_or("")
    );
    println!("命中 {} 条 · 耗时 {total:.2}s", results.len());
    if results.is_empty() {
        println!(
            "（无结果：可能被反爬拦截，或所有引擎仍在退避中，可稍后重试 / 用 --engines 指定引擎）"
        );
        return;
    }
    for (i, r) in results.iter().enumerate() {
        let text = |k: &str| r.get(k).and_then(|v| v.as_str()).unwrap_or("");
        println!();
        println!("{}. {}", i + 1, text("title"));
        if !text("url").is_empty() {
            println!("   {}", text("url"));
        }
        if !text("snippet").is_empty() {
            println!("   {}", text("snippet"));
        }
        if !text("content").is_empty() {
            println!("   正文: {}", first_n_chars(text("content"), 160));
        }
        println!(
            "   [引擎 {} · 相关度 {:.2}]",
            text("_source_engine"),
            r.get("_rerank_score")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0)
        );
    }
}

fn print_pretty(value: &serde_json::Value) {
    match serde_json::to_string_pretty(value) {
        Ok(s) => println!("{s}"),
        Err(e) => {
            eprintln!("JSON 序列化失败: {e}");
            std::process::exit(1);
        }
    }
}

fn bail(msg: &str) -> i32 {
    eprintln!("参数错误: {msg}");
    eprintln!();
    eprint!("{USAGE}");
    2
}

async fn cmd_search(args: &[String]) -> i32 {
    let o = match parse_opts(args) {
        Ok(o) => o,
        Err(msg) if msg.is_empty() => {
            print!("{USAGE}");
            return 0;
        }
        Err(msg) => return bail(&msg),
    };
    let q = o.positional.join(" ");
    if q.trim().is_empty() {
        eprintln!("缺少搜索关键词");
        eprintln!();
        eprint!("{USAGE}");
        return 2;
    }
    let params = SearchParams {
        q: q.clone(),
        max_results: o.max,
        freshness: o.freshness,
        topic: o.topic,
        include_domains: o.include,
        exclude_domains: o.exclude,
        engines: o.engines,
    };
    // 模型加载会做同步 HTTP 下载，放到阻塞线程里避免阻塞 tokio
    let _ = tokio::task::spawn_blocking(embedding::warmup).await;
    let Json(value) = handle_search(Query(params)).await;
    if let Some(err) = value.get("error").and_then(|v| v.as_str()) {
        eprintln!("搜索失败: {err}");
        return 1;
    }
    if o.json {
        print_pretty(&value);
    } else {
        print_search_text(&value);
    }
    0
}

async fn cmd_fetch(args: &[String]) -> i32 {
    let o = match parse_opts(args) {
        Ok(o) => o,
        Err(msg) if msg.is_empty() => {
            print!("{USAGE}");
            return 0;
        }
        Err(msg) => return bail(&msg),
    };
    let url = o.positional.join(" ");
    if url.trim().is_empty() {
        eprintln!("缺少 URL");
        eprintln!();
        eprint!("{USAGE}");
        return 2;
    }
    let params = FetchParams { url: url.clone() };
    let Json(value) = handle_fetch(Query(params)).await;
    let err = value.get("error").and_then(|v| v.as_str()).unwrap_or("");
    if !err.is_empty() && !o.json {
        eprintln!("抓取失败: {err}");
        return 1;
    }
    if o.json {
        print_pretty(&value);
    } else {
        println!(
            "状态: {} · 类型: {}",
            value.get("status").and_then(|v| v.as_i64()).unwrap_or(0),
            value
                .get("content_type")
                .and_then(|v| v.as_str())
                .unwrap_or("")
        );
        println!();
        println!(
            "{}",
            value.get("body").and_then(|v| v.as_str()).unwrap_or("")
        );
    }
    0
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match args.first().map(|s| s.as_str()) {
        None => {
            serve(resolve_port(&[])).await;
            0
        }
        Some("serve") => {
            serve(resolve_port(&args[1..])).await;
            0
        }
        Some("search") => cmd_search(&args[1..]).await,
        Some("fetch") => cmd_fetch(&args[1..]).await,
        Some("models") => cmd_models(&args[1..]),
        Some("health") => {
            print_pretty(&health_payload());
            0
        }
        Some("version") | Some("--version") | Some("-V") => {
            println!("aggrsearch {VERSION}");
            0
        }
        Some("help") | Some("--help") | Some("-h") => {
            print!("{USAGE}");
            0
        }
        Some(other) => bail(&format!("未知子命令: {other}")),
    };
    std::process::exit(code);
}

// ---------------- 单元测试（纯函数，不触网）----------------

#[cfg(test)]
mod tests {
    use super::*;

    fn mk(url: &str, title: &str, snippet: &str, engine: &str) -> SearchResult {
        SearchResult {
            title: title.to_string(),
            url: url.to_string(),
            snippet: snippet.to_string(),
            content: String::new(),
            rerank_score: 0.0,
            source_engine: engine.to_string(),
        }
    }

    #[test]
    fn detects_cjk() {
        assert!(is_cjk('中'));
        assert!(is_cjk('编'));
        assert!(!is_cjk('a'));
        assert!(!is_cjk('1'));
    }

    #[test]
    fn prep_query_drops_space_next_to_cjk() {
        assert_eq!(prep_query("Rust 编程"), "Rust编程");
        assert_eq!(prep_query("hello world"), "hello world");
        assert_eq!(prep_query("深度 学习"), "深度学习");
    }

    #[test]
    fn normalize_ws_collapses_runs() {
        assert_eq!(normalize_ws("  a   b \n c "), "a b c");
        assert_eq!(normalize_ws(""), "");
    }

    #[test]
    fn analyze_html_strips_script_and_style() {
        let html = "<html><head><style>.a{color:red}</style></head>\
                   <body><script>var x=1;</script><p>Hello World</p></body></html>";
        let (text, tags) = analyze_html(html);
        assert_eq!(text, "Hello World");
        assert!(tags >= 4, "唯一标签数应 >= 4，实际 {tags}");
    }

    #[test]
    fn quality_score_rejects_thin_page() {
        assert!(quality_score("<html><body><div></div></body></html>") < 0.28);
    }

    #[test]
    fn quality_score_accepts_article() {
        let mut body = String::from("<html><body><article><h1>T</h1><p>");
        body.push_str(&"这是一段足够长的正文内容，用来验证质量评分不会误杀正常页面。".repeat(40));
        body.push_str("</p></article></body></html>");
        assert!(quality_score(&body) > 0.28);
    }

    #[test]
    fn blocked_detects_antiscraping_pages() {
        assert!(is_blocked("baidu", "请输入 wappass 验证码", 200));
        assert!(is_blocked("baidu", "需要安全验证", 200));
        assert!(is_blocked("sogou", "<a href=\"/antispider/\">", 200));
        assert!(is_blocked("bing_intl", "<html></html>", 503));
    }

    #[test]
    fn tokenize_keeps_versions_and_cjk() {
        let t = tokenize("Rust 1.90 async 编程");
        assert!(t.iter().any(|x| x == "rust"), "{t:?}");
        assert!(t.iter().any(|x| x == "1.90"), "{t:?}");
        assert!(t.iter().any(|x| x == "编"), "{t:?}");
    }

    #[test]
    fn relevance_weights_title_over_snippet() {
        let in_title = relevance("rust axum", "Rust Axum 实战", "无关摘要");
        let in_snippet = relevance("rust axum", "无关标题", "这里提到 rust axum");
        assert!(in_title > in_snippet);
        assert_eq!(relevance("", "a", "b"), 0.0);
    }

    #[test]
    fn engine_bias_matches_priority() {
        assert!(engine_bias("bing_intl") > engine_bias("bing_cn"));
        assert!(engine_bias("bing_cn") > engine_bias("baidu"));
        assert!(engine_bias("baidu") > engine_bias("sogou"));
        assert_eq!(engine_bias("unknown"), 0.0);
    }

    #[test]
    fn rerank_puts_bing_first() {
        let mut rs = vec![
            mk("https://a.com/1", "无关标题", "无关摘要", "sogou"),
            mk(
                "https://b.com/1",
                "Rust Axum 教程",
                "rust axum 教程",
                "bing_intl",
            ),
        ];
        rerank("rust axum", &mut rs, 10);
        assert_eq!(rs[0].source_engine, "bing_intl");
    }

    #[test]
    fn rerank_truncates_to_max() {
        let mut rs: Vec<SearchResult> = (0..5)
            .map(|i| mk(&format!("https://a.com/{i}"), "t", "s", "bing_intl"))
            .collect();
        rerank("t", &mut rs, 2);
        assert_eq!(rs.len(), 2);
    }

    #[test]
    fn parse_domains_normalizes_input() {
        let d = parse_domains(Some(&" Docs.rs , github.COM ,, ".to_string()));
        assert!(d.contains("docs.rs"));
        assert!(d.contains("github.com"));
        assert_eq!(d.len(), 2);
        assert!(parse_domains(None).is_empty());
    }

    #[test]
    fn domain_filter_include_then_exclude() {
        let mut rs = vec![
            mk("https://docs.rs/axum", "a", "s", "bing_intl"),
            mk("https://news.ycombinator.com/x", "b", "s", "bing_intl"),
        ];
        let rs_included = parse_domains(Some(&"docs.rs".to_string()));
        filter_domains(&mut rs, &rs_included, &HashSet::new());
        assert_eq!(rs.len(), 1);
        assert_eq!(url_host(&rs[0].url), "docs.rs");

        let rs_excluded = parse_domains(Some(&"docs.rs".to_string()));
        filter_domains(&mut rs, &HashSet::new(), &rs_excluded);
        assert!(rs.is_empty());
    }

    #[test]
    fn url_host_lowercases_and_tolerates_garbage() {
        assert_eq!(url_host("https://Docs.RS/axum"), "docs.rs");
        assert_eq!(url_host("not a url"), "");
    }

    #[test]
    fn escape_path_form_encodes() {
        assert_eq!(escape_path("a b&c"), "a+b%26c");
    }

    #[test]
    fn backoff_marks_engine_as_cooling() {
        assert!(!in_backoff("test_only_engine"));
        set_backoff("test_only_engine");
        assert!(in_backoff("test_only_engine"));
    }

    #[test]
    fn blocked_fetch_cache_roundtrip() {
        assert!(!fetch_blocked_cached("cache-test.example"));
        cache_fetch_blocked("cache-test.example");
        assert!(fetch_blocked_cached("cache-test.example"));
    }

    #[test]
    fn round2_truncates_two_decimals() {
        assert_eq!(round2(1.2345), 1.23);
        assert_eq!(round2(1.005), 1.0);
    }

    #[test]
    fn cli_parses_flags() {
        let args: Vec<String> = [
            "关键词",
            "--max",
            "5",
            "--json",
            "--engines",
            "bing_intl,baidu",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let o = parse_opts(&args).unwrap();
        assert_eq!(o.positional, vec!["关键词".to_string()]);
        assert_eq!(o.max, Some(5));
        assert!(o.json);
        assert_eq!(o.engines.as_deref(), Some("bing_intl,baidu"));
    }

    #[test]
    fn cli_rejects_unknown_flag() {
        assert!(parse_opts(&["--nope".to_string()]).is_err());
    }

    #[test]
    fn cli_reports_missing_flag_value() {
        assert!(parse_opts(&["--max".to_string()]).is_err());
    }

    #[test]
    fn resolve_port_reads_port_argument() {
        assert_eq!(
            resolve_port(&["--port".to_string(), "9999".to_string()]),
            9999
        );
        assert_eq!(resolve_port(&["7777".to_string()]), 7777);
    }

    #[test]
    fn health_payload_matches_version() {
        let v = health_payload();
        assert_eq!(v["status"], "ok");
        assert_eq!(v["name"], "aggrsearch");
        assert_eq!(v["version"], VERSION);
    }

    #[test]
    fn router_builds_with_all_routes() {
        let _router = build_router();
    }

    #[test]
    fn search_url_builders_encode_query() {
        let u = bing_search_url("rust 编程", false, false, &None);
        assert!(u.starts_with("https://www.bing.com/search?"), "{u}");
        assert!(u.contains("count=15"), "{u}");
        let news = bing_search_url("ai", true, true, &Some("w".to_string()));
        assert!(news.contains("qdr:w"), "{news}");
    }
}
