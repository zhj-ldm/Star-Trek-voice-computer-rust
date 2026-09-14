/// Generate a unified diff between two strings.
pub fn unified_diff(old: &str, new: &str, filename: &str) -> String {
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();

    let mut result = String::new();
    result.push_str(&format!("--- a/{}\n", filename));
    result.push_str(&format!("+++ b/{}\n", filename));

    let hunks = compute_hunks(&old_lines, &new_lines);

    for hunk in hunks {
        result.push_str(&format!(
            "@@ -{},{} +{},{} @@\n",
            hunk.old_start + 1,
            hunk.old_count,
            hunk.new_start + 1,
            hunk.new_count
        ));

        for line in &hunk.lines {
            match line {
                DiffLine::Context(s) => {
                    result.push(' ');
                    result.push_str(s);
                    result.push('\n');
                }
                DiffLine::Added(s) => {
                    result.push('+');
                    result.push_str(s);
                    result.push('\n');
                }
                DiffLine::Removed(s) => {
                    result.push('-');
                    result.push_str(s);
                    result.push('\n');
                }
            }
        }
    }

    result
}

/// Count lines added and removed.
pub fn count_changes(old: &str, new: &str) -> (usize, usize) {
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();
    let hunks = compute_hunks(&old_lines, &new_lines);

    let mut added = 0;
    let mut removed = 0;
    for hunk in hunks {
        for line in &hunk.lines {
            match line {
                DiffLine::Added(_) => added += 1,
                DiffLine::Removed(_) => removed += 1,
                DiffLine::Context(_) => {}
            }
        }
    }
    (added, removed)
}

enum DiffLine<'a> {
    Context(&'a str),
    Added(&'a str),
    Removed(&'a str),
}

struct Hunk<'a> {
    old_start: usize,
    old_count: usize,
    new_start: usize,
    new_count: usize,
    lines: Vec<DiffLine<'a>>,
}

fn compute_hunks<'a>(old: &[&'a str], new: &[&'a str]) -> Vec<Hunk<'a>> {
    // Simple LCS-based diff
    let n = old.len();
    let m = new.len();

    // Compute edit script using Myers-like approach (simplified)
    let mut edits: Vec<DiffLine<'a>> = Vec::new();
    let mut old_idx = 0;
    let mut new_idx = 0;

    // Simple line-by-line comparison with longest common subsequence
    let lcs = compute_lcs(old, new);
    let mut lcs_idx = 0;

    while old_idx < n || new_idx < m {
        if lcs_idx < lcs.len() && old_idx == lcs[lcs_idx].0 && new_idx == lcs[lcs_idx].1 {
            edits.push(DiffLine::Context(old[old_idx]));
            old_idx += 1;
            new_idx += 1;
            lcs_idx += 1;
        } else if old_idx < n
            && (lcs_idx >= lcs.len() || old_idx < lcs[lcs_idx].0)
        {
            edits.push(DiffLine::Removed(old[old_idx]));
            old_idx += 1;
        } else if new_idx < m {
            edits.push(DiffLine::Added(new[new_idx]));
            new_idx += 1;
        }
    }

    // Group edits into hunks with 3 lines of context
    let context_lines = 3;
    let mut hunks = Vec::new();
    let mut i = 0;

    while i < edits.len() {
        // Find next change
        while i < edits.len() {
            if !matches!(edits[i], DiffLine::Context(_)) {
                break;
            }
            i += 1;
        }
        if i >= edits.len() {
            break;
        }

        // Start hunk with context before
        let context_start = if i >= context_lines {
            i - context_lines
        } else {
            0
        };

        let mut hunk_lines = Vec::new();
        let mut old_start = 0;
        let mut new_start = 0;

        // Calculate starting positions
        let mut oi = 0;
        let mut ni = 0;
        for j in 0..context_start {
            match &edits[j] {
                DiffLine::Context(_) => {
                    oi += 1;
                    ni += 1;
                }
                DiffLine::Removed(_) => oi += 1,
                DiffLine::Added(_) => ni += 1,
            }
        }
        old_start = oi;
        new_start = ni;

        // Add context before
        let mut j = context_start;
        while j < i {
            hunk_lines.push(DiffLine::Context(match &edits[j] {
                DiffLine::Context(s) => s,
                _ => unreachable!(),
            }));
            j += 1;
        }

        // Add changes and merge close hunks
        let mut consecutive_context = 0;
        while j < edits.len() {
            match &edits[j] {
                DiffLine::Context(s) => {
                    consecutive_context += 1;
                    if consecutive_context > context_lines * 2 {
                        // End hunk, remove trailing context beyond limit
                        let to_remove = consecutive_context - context_lines;
                        for _ in 0..to_remove {
                            hunk_lines.pop();
                        }
                        break;
                    }
                    hunk_lines.push(DiffLine::Context(s));
                }
                DiffLine::Added(s) => {
                    consecutive_context = 0;
                    hunk_lines.push(DiffLine::Added(s));
                }
                DiffLine::Removed(s) => {
                    consecutive_context = 0;
                    hunk_lines.push(DiffLine::Removed(s));
                }
            }
            j += 1;
        }

        // Remove trailing context beyond limit
        while hunk_lines.len() > 0 {
            if matches!(hunk_lines.last(), Some(DiffLine::Context(_))) {
                let trailing_context = hunk_lines
                    .iter()
                    .rev()
                    .take_while(|l| matches!(l, DiffLine::Context(_)))
                    .count();
                if trailing_context > context_lines {
                    hunk_lines.pop();
                } else {
                    break;
                }
            } else {
                break;
            }
        }

        let mut old_count = 0;
        let mut new_count = 0;
        for line in &hunk_lines {
            match line {
                DiffLine::Context(_) => {
                    old_count += 1;
                    new_count += 1;
                }
                DiffLine::Removed(_) => old_count += 1,
                DiffLine::Added(_) => new_count += 1,
            }
        }

        if old_count > 0 || new_count > 0 {
            hunks.push(Hunk {
                old_start,
                old_count,
                new_start,
                new_count,
                lines: hunk_lines,
            });
        }

        i = j;
    }

    hunks
}

fn compute_lcs<'a>(old: &[&'a str], new: &[&'a str]) -> Vec<(usize, usize)> {
    let n = old.len();
    let m = new.len();

    if n == 0 || m == 0 {
        return Vec::new();
    }

    // DP table (optimized for memory with two rows)
    let mut dp = vec![vec![0u32; m + 1]; n + 1];

    for i in 1..=n {
        for j in 1..=m {
            if old[i - 1] == new[j - 1] {
                dp[i][j] = dp[i - 1][j - 1] + 1;
            } else {
                dp[i][j] = dp[i - 1][j].max(dp[i][j - 1]);
            }
        }
    }

    // Backtrack to find LCS
    let mut result = Vec::new();
    let mut i = n;
    let mut j = m;

    while i > 0 && j > 0 {
        if old[i - 1] == new[j - 1] {
            result.push((i - 1, j - 1));
            i -= 1;
            j -= 1;
        } else if dp[i - 1][j] > dp[i][j - 1] {
            i -= 1;
        } else {
            j -= 1;
        }
    }

    result.reverse();
    result
}
