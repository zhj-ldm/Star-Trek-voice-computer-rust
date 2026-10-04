//! aggrsearch —— 面向 AI 的多聚合搜索 HTTP 服务（单文件，单二进制）
//!
//! 对齐 SearchPIN 的搜索方式与接口：web_search + web_fetch 两能力。
//! - 引擎：Bing 国际 > Bing CN > 百度 > 搜狗（360 已排除），必应优先级最高
//! - 每引擎一页候选池（bing count=15），合并去重 → 语义重排 → 截断 max_results
//! - 反爬：CJK 预处理 + 零硬编码质量评分 + 多级 fallback + per-engine backoff + 搜狗 TLS1.2
//! - 接口：
//!   GET /search?q=&max_results=&freshness=&topic=&include_domains=&exclude_domains=
//!       freshness: d|w|m|y（仅作用于必应 &tbs=qdr:）
//!       topic:     general|news（news 走必应新闻垂直）
//!   GET /fetch?url=   抓取 URL 全文，质量分拦截检测 + blocked 缓存(2h)
//!   GET /health

use axum::{
    extract::Query,
    http::StatusCode,
    routing::get,
    Json, Router,
};
use futures::{future::join_all, FutureExt};
use scraper::{Html, Selector};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::sync::{LazyLock, Mutex};
use std::time::Instant;

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

static BACKOFF: LazyLock<Mutex<HashMap<&'static str, (Instant, u32)>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
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
        m.insert(engine, (Instant::now() + std::time::Duration::from_secs(secs), tries));
    }
}

// ---------------- fetch 拦截缓存（blocked host，2h TTL）----------------

static BLOCKED_FETCH: LazyLock<Mutex<HashMap<String, Instant>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
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
fn filter_domains(results: &mut Vec<SearchResult>, include: &HashSet<String>, exclude: &HashSet<String>) {
    if !include.is_empty() {
        results.retain(|r| {
            let h = url_host(&r.url);
            include.iter().any(|d| h == *d || h.ends_with(&format!(".{}", d)))
        });
    }
    if !exclude.is_empty() {
        results.retain(|r| {
            let h = url_host(&r.url);
            !exclude.iter().any(|d| h == *d || h.ends_with(&format!(".{}", d)))
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
        format!(
            "https://{host}{path}?q={q}&first=1{mkt}{fresh}"
        )
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
    let cleaned = txt.replace("</em>", "").replace("<em>", "").trim().to_string();
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
        if let Some(p) = url::Url::parse(u).ok() {
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

type Fetcher = fn(QueryCtx) -> futures::future::BoxFuture<'static, Result<Vec<SearchResult>, String>>;

struct Engine {
    name: &'static str,
    fetcher: Fetcher,
}

/// 每引擎候选抓取数（对齐 SearchPIN：bing count=15）
const CANDIDATE_LIMIT: usize = 15;

fn bing_intl_fetch(ctx: QueryCtx) -> futures::future::BoxFuture<'static, Result<Vec<SearchResult>, String>> {
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
        eprintln!("[bing_intl] status={} bytes={} quality={:.2}", status, body.len(), quality_score(&body));
        if is_blocked("bing_intl", &body, status) {
            set_backoff("bing_intl");
            return Ok(Vec::new());
        }
        Ok(parse_bing(&body, CANDIDATE_LIMIT, "bing_intl", "bing.com"))
    }
    .boxed()
}

fn bing_cn_fetch(ctx: QueryCtx) -> futures::future::BoxFuture<'static, Result<Vec<SearchResult>, String>> {
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
        eprintln!("[bing_cn] status={} bytes={} quality={:.2}", status, body.len(), quality_score(&body));
        if is_blocked("bing_cn", &body, status) {
            set_backoff("bing_cn");
            return Ok(Vec::new());
        }
        Ok(parse_bing(&body, CANDIDATE_LIMIT, "bing_cn", "cn.bing.com"))
    }
    .boxed()
}

fn baidu_fetch(ctx: QueryCtx) -> futures::future::BoxFuture<'static, Result<Vec<SearchResult>, String>> {
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
        eprintln!("[baidu] status={} bytes={} wappass={} quality={:.2}", status, body.len(), body.contains("wappass"), quality_score(&body));
        if is_blocked("baidu", &body, status) {
            set_backoff("baidu");
            return Ok(Vec::new());
        }
        Ok(parse_baidu(&body, CANDIDATE_LIMIT))
    }
    .boxed()
}

fn sogou_fetch(ctx: QueryCtx) -> futures::future::BoxFuture<'static, Result<Vec<SearchResult>, String>> {
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
        eprintln!("[sogou] status={} bytes={} antispider={} quality={:.2}", status, body.len(), body.contains("antispider"), quality_score(&body));
        if is_blocked("sogou", &body, status) {
            set_backoff("sogou");
            return Ok(Vec::new());
        }
        Ok(parse_sogou(&body, CANDIDATE_LIMIT))
    }
    .boxed()
}

const ENGINES: &[Engine] = &[
    Engine { name: "bing_intl", fetcher: bing_intl_fetch },
    Engine { name: "bing_cn", fetcher: bing_cn_fetch },
    Engine { name: "baidu", fetcher: baidu_fetch },
    Engine { name: "sogou", fetcher: sogou_fetch },
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
    let freshness = params.freshness.filter(|f| matches!(f.as_str(), "d" | "w" | "m" | "y"));
    let news = params.topic.as_deref() == Some("news");

    let include = parse_domains(params.include_domains.as_ref());
    let exclude = parse_domains(params.exclude_domains.as_ref());

    let requested: Vec<String> = match &params.engines {
        Some(s) => s.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect(),
        None => ENGINES.iter().map(|e| e.name.to_string()).collect(),
    };
    let engines: Vec<String> = ENGINES.iter().map(|e| e.name.to_string()).filter(|n| requested.contains(n)).collect();

    let ctx = QueryCtx { query: query.clone(), freshness: freshness.clone(), news };
    let t_search_start = Instant::now();
    // 总超时兜底：任一个引擎被墙/超时也不拖死整次搜索（每引擎自身 8s 超时）
    let mut all = match tokio::time::timeout(
        std::time::Duration::from_secs(10),
        fetch_candidates(&ctx, &engines),
    ).await {
        Ok(v) => v,
        Err(_) => Vec::new(),
    };
    let search_ms = t_search_start.elapsed().as_millis() as f64 / 1000.0;
    let merged = all.len();

    // domain 过滤（重排前，对齐 SearchPIN）
    filter_domains(&mut all, &include, &exclude);

    let t_rerank_start = Instant::now();
    rerank(&query, &mut all, max_results);
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
        return Json(serde_json::json!({"status": 0, "content_type": "", "body": "", "error": "empty url", "_timing": {"total": 0}}));
    }
    let host = url_host(&url);
    if !host.is_empty() && fetch_blocked_cached(&host) {
        return Json(serde_json::json!({"status": 0, "content_type": "", "body": "", "error": format!("Domain {} blocked by CDN/WAF (cached)", host), "_timing": {"total": round2(start.elapsed().as_secs_f64())}}));
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
            let body = String::from_utf8(bytes.to_vec()).unwrap_or_else(|_| String::from_utf8_lossy(&bytes).into_owned());
            (status, ct, body)
        }
        Err(e) => {
            return Json(serde_json::json!({"status": 0, "content_type": "", "body": "", "error": e.to_string(), "_timing": {"total": round2(start.elapsed().as_secs_f64())}}));
        }
    };

    // 质量分拦截检测（对齐 SearchPIN：<0.35 判 blocked）
    let q = quality_score(&body);
    let blocked = status >= 400 || q < 0.35;
    if blocked {
        if !host.is_empty() {
            cache_fetch_blocked(&host);
        }
        return Json(serde_json::json!({"status": 0, "content_type": content_type, "body": "", "error": format!("Domain {} blocked by CDN/WAF (quality {:.2})", host, q), "_timing": {"total": round2(start.elapsed().as_secs_f64())}}));
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

// ---------------- 健康检查 ----------------

async fn handle_health() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "status": "ok", "name": "aggrsearch", "version": "0.4.0" }))
}

async fn fallback_404() -> (StatusCode, Json<serde_json::Value>) {
    (StatusCode::NOT_FOUND, Json(serde_json::json!({"error": "not found"})))
}

#[tokio::main]
async fn main() {
    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8080);

    let app = Router::new()
        .route("/search", get(handle_search))
        .route("/fetch", get(handle_fetch))
        .route("/health", get(handle_health))
        .fallback(fallback_404);

    let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{port}"))
        .await
        .expect("端口绑定失败");
    println!("aggrsearch v0.4.0 已启动: http://127.0.0.1:{port}");
    println!("  引擎: bing_intl > bing_cn > baidu > sogou (360 已排除)");
    println!("  对齐 SearchPIN: web_search + web_fetch");
    println!("  GET /search?q=&max_results=&freshness=d|w|m|y&topic=general|news&include_domains=&exclude_domains=");
    println!("  GET /fetch?url=");
    println!("  GET /health");

    axum::serve(listener, app).await.expect("服务异常退出");
}
