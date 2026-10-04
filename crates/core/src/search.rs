//! 多引擎联网搜索（内嵌 aggrsearch 引擎，对齐 SearchPIN 的反爬 / 多级解析 / CJK 预处理 / 词法重排）。
//! 以同名工具 WebSearch 覆盖 SDK 内置占位实现。不依赖 searchpin-ai 二进制，无 embedding（轻量）。

use async_trait::async_trait;
use open_agent_sdk::types::{Tool, ToolError, ToolInputSchema, ToolResult, ToolUseContext};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::{LazyLock, Mutex};
use std::time::Instant;
use futures::{future::join_all, FutureExt};
use scraper::{Html, Selector};

// ============================================================
// 结果模型
// ============================================================

#[derive(Clone)]
pub struct SearchItem {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

#[derive(Clone)]
struct SearchResult {
    title: String,
    url: String,
    snippet: String,
    content: String,
    rerank_score: f64,
    source_engine: String,
}

#[derive(Clone)]
struct QueryCtx {
    query: String,
    freshness: Option<String>,
    news: bool,
}

// ============================================================
// 查询预处理（CJK）——对齐 SearchPIN：删除中文字符两侧空格，防必应分词器拆词
// ============================================================

fn is_cjk(c: char) -> bool {
    let u = c as u32;
    (0x4E00..=0x9FFF).contains(&u) || (0x3400..=0x4DBF).contains(&u)
}

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

// ============================================================
// 质量评分（零硬编码，移植 SearchPIN quality.py）
// ============================================================

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

// ============================================================
// 每引擎退避（指数递增封顶 120s，对齐 SearchPIN engine.py）
// ============================================================

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
        m.insert(engine, (Instant::now() + std::time::Duration::from_secs(secs), tries));
    }
}

// ============================================================
// 词法相关度重排 + 引擎偏置（无 embedding，轻量）
// ============================================================

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

fn engine_bias(name: &str) -> f64 {
    match name {
        "bing_intl" => 1.0,
        "bing_cn" => 0.8,
        "baidu" => 0.5,
        "sogou" => 0.4,
        _ => 0.0,
    }
}

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

// ============================================================
// HTTP 客户端（对齐 SearchPIN：完整浏览器头 + 自身域名 Referer / 搜狗 TLS1.2）
// ============================================================

const USER_AGENTS: &[&str] = &[
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/125.0 Safari/537.36",
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.4 Safari/605.1.15",
];

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

// ============================================================
// 解析器（多级 fallback：主解析 → 通用 <a> 兜底）
// ============================================================

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
    let mut pairs: Vec<(String, usize)> = Vec::new();
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

// ============================================================
// 引擎实现（并发，每引擎 8s 超时 + 指数退避）
// ============================================================

type Fetcher = fn(QueryCtx) -> futures::future::BoxFuture<'static, Result<Vec<SearchResult>, String>>;

struct Engine {
    name: &'static str,
    fetcher: Fetcher,
}

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

/// 并发抓取全部引擎 → 合并去重（按 url），返回候选池
async fn fetch_candidates(ctx: &QueryCtx) -> Vec<SearchResult> {
    let futures: Vec<_> = ENGINES
        .iter()
        .map(|en| {
            let c = ctx.clone();
            let fetcher = en.fetcher;
            tokio::spawn(async move { fetcher(c).await.unwrap_or_default() })
        })
        .collect();

    let mut all: Vec<SearchResult> = join_all(futures)
        .await
        .into_iter()
        .filter_map(|r| r.ok())
        .flatten()
        .collect();

    let mut seen = HashSet::new();
    all.retain(|r| {
        let key = r.url.to_lowercase().trim_end_matches('/').to_string();
        seen.insert(key)
    });
    all
}

// ============================================================
// WebSearch 自定义工具（覆盖 SDK 占位）
// ============================================================

pub struct WebSearchTool;

impl Default for WebSearchTool {
    fn default() -> Self {
        Self
    }
}

impl WebSearchTool {
    /// 执行搜索：并发四引擎 → 去重 → 词法重排 → 返回结构化结果
    pub async fn search(&self, query: &str, max: usize) -> Result<Vec<SearchItem>, String> {
        let q = query.trim();
        if q.is_empty() {
            return Err("缺少搜索关键词".into());
        }
        let max = max.clamp(1, 20);
        let ctx = QueryCtx {
            query: q.to_string(),
            freshness: None,
            news: false,
        };
        // 总超时兜底：任一个引擎被墙/超时也不拖死整次搜索（每引擎自身 8s 超时）
        let mut all = match tokio::time::timeout(
            std::time::Duration::from_secs(10),
            fetch_candidates(&ctx),
        )
        .await
        {
            Ok(v) => v,
            Err(_) => Vec::new(),
        };
        if all.is_empty() {
            return Err("所有搜索引擎均未返回结果（可能触发反爬冷却）".into());
        }
        rerank(&q, &mut all, max);
        let items: Vec<SearchItem> = all
            .into_iter()
            .map(|r| SearchItem {
                title: r.title,
                url: r.url,
                snippet: r.snippet,
            })
            .collect();
        Ok(items)
    }
}

#[async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &str {
        "WebSearch"
    }
    fn description(&self) -> &str {
        "联网搜索获取最新信息。输入搜索关键词（query），返回相关网页标题、链接与摘要。"
    }
    fn input_schema(&self) -> ToolInputSchema {
        ToolInputSchema {
            schema_type: "object".to_string(),
            properties: HashMap::from([
                (
                    "query".to_string(),
                    json!({"type": "string", "description": "搜索关键词"}),
                ),
                (
                    "max_results".to_string(),
                    json!({"type": "number", "description": "最多返回条数（默认5）"}),
                ),
            ]),
            required: vec!["query".to_string()],
            additional_properties: Some(false),
        }
    }
    fn is_read_only(&self, _input: &Value) -> bool {
        true
    }
    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        true
    }
    async fn call(&self, input: Value, _ctx: &ToolUseContext) -> Result<ToolResult, ToolError> {
        let query = input
            .get("query")
            .and_then(|q| q.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if query.is_empty() {
            return Ok(ToolResult::error("缺少搜索关键词"));
        }
        let max = input.get("max_results").and_then(|m| m.as_u64()).unwrap_or(5) as usize;
        match self.search(&query, max).await {
            Ok(items) => {
                let mut out = String::new();
                for (i, it) in items.iter().enumerate() {
                    out.push_str(&format!(
                        "{}. {}\n   {}\n   {}\n",
                        i + 1,
                        it.title,
                        it.url,
                        it.snippet
                    ));
                }
                if out.is_empty() {
                    Ok(ToolResult::error("搜索未返回结果"))
                } else {
                    Ok(ToolResult::text(out))
                }
            }
            Err(e) => Ok(ToolResult::error(format!("搜索失败: {e}"))),
        }
    }
}
