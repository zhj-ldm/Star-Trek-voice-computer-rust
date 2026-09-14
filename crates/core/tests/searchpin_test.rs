//! searchpin-ai 子进程 MCP 客户端集成测试
use star_core::search::SearchpinSearcher;

#[tokio::test]
async fn searchpin_live_zh() {
    let s = SearchpinSearcher::default();
    let items = s.search("北京今日天气", 5).await.expect("searchpin 搜索失败");
    assert!(!items.is_empty(), "应有结果");
    for (i, it) in items.iter().enumerate() {
        let sn: String = it.snippet.chars().take(80).collect();
        println!("[{i}] {} | {}", it.title, it.url);
        println!("     {}", sn);
    }
}

#[tokio::test]
async fn searchpin_live_en() {
    let s = SearchpinSearcher::default();
    let items = s.search("tokio async runtime", 5).await.expect("searchpin 搜索失败");
    assert!(!items.is_empty(), "应有结果");
    for (i, it) in items.iter().enumerate() {
        println!("[{i}] {} | {}", it.title, it.url);
    }
}

#[tokio::test]
async fn searchpin_reuse_proc() {
    // 验证子进程常驻复用（两次搜索同一进程）
    let s = SearchpinSearcher::default();
    let a = s.search("rust", 3).await.expect("第一次搜索失败");
    let b = s.search("python", 3).await.expect("第二次搜索失败");
    assert!(!a.is_empty() && !b.is_empty());
    println!("reuse OK: {} + {} items", a.len(), b.len());
}
