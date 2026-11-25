//! Issues a pull request closes, using GitHub's closing keywords.
//!
//! Two reference forms are recognised, both after a closing keyword:
//! - `#12` or `owner/repo#12`
//! - `https://github.com/owner/repo/issues/12` (http or https, optional trailing slash)
//!
//! Text GitHub does not treat as live (fenced code, inline code, quoted `>` lines and HTML comments)
//! is blanked out before matching, so a template example never links a PR to a bounty.

use regex::Regex;
use std::{collections::BTreeSet, sync::LazyLock};

static KEYWORD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(?:close[sd]?|fix(?:e[sd])?|resolve[sd]?)\s*:?\s+(?:(?P<short_repo>[\w.-]+/[\w.-]+)?#(?P<short_num>\d+)\b|https?://github\.com/(?P<url_repo>[\w.-]+/[\w.-]+)/issues/(?P<url_num>\d+)\b/?)",
    )
    .unwrap()
});

/// Issue numbers in `repo_full_name` that the PR body closes, ascending. Cross-repo references are ignored.
pub fn parse_closing_refs(body: Option<&str>, repo_full_name: &str) -> Vec<u32> {
    let text = strip_non_closing(body.unwrap_or(""));
    let mut out = BTreeSet::new();
    for m in KEYWORD.captures_iter(&text) {
        let (repo, num) = match (m.name("short_num"), m.name("url_num")) {
            (Some(n), _) => (m.name("short_repo").map(|r| r.as_str()), n.as_str()),
            (None, Some(n)) => (m.name("url_repo").map(|r| r.as_str()), n.as_str()),
            (None, None) => continue,
        };
        if repo.is_some_and(|r| !r.eq_ignore_ascii_case(repo_full_name)) {
            continue;
        }
        if let Ok(n) = num.parse::<u32>() {
            out.insert(n);
        }
    }
    out.into_iter().collect()
}

/// Replaces the parts of a PR body that GitHub does not read as closing references with spaces:
/// fenced code blocks, lines quoted with `>`, inline code spans and HTML comments (an unclosed comment
/// or fence runs to the end of the body, as in CommonMark). Line breaks are kept.
fn strip_non_closing(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    let mut fence: Option<(char, usize)> = None;
    let mut in_comment = false;
    for line in body.lines() {
        if !in_comment {
            if let Some((ch, min)) = fence {
                if closes_fence(line, ch, min) {
                    fence = None;
                }
                out.push('\n');
                continue;
            }
            if let Some(open) = opens_fence(line) {
                fence = Some(open);
                out.push('\n');
                continue;
            }
            if line.trim_start().starts_with('>') {
                out.push('\n');
                continue;
            }
        }
        out.push_str(&strip_line(line, &mut in_comment));
        out.push('\n');
    }
    out
}

/// A fence opener: up to three spaces, then at least three backticks or tildes.
fn opens_fence(line: &str) -> Option<(char, usize)> {
    let t = line.trim_start_matches(' ');
    if line.len() - t.len() > 3 {
        return None;
    }
    let ch = t.chars().next().filter(|c| *c == '`' || *c == '~')?;
    let len = t.chars().take_while(|c| *c == ch).count();
    (len >= 3).then_some((ch, len))
}

/// A fence closer: the same fence character, at least as many as the opener, and nothing else.
fn closes_fence(line: &str, ch: char, min: usize) -> bool {
    let t = line.trim_start_matches(' ');
    if line.len() - t.len() > 3 {
        return false;
    }
    let len = t.chars().take_while(|c| *c == ch).count();
    len >= min && t[len..].trim().is_empty()
}

/// Blanks inline code spans and HTML comments on one line. `in_comment` carries an open comment
/// across lines.
fn strip_line(line: &str, in_comment: &mut bool) -> String {
    let c: Vec<char> = line.chars().collect();
    let mut out = String::with_capacity(line.len());
    let mut i = 0;
    while i < c.len() {
        if *in_comment {
            match (i..c.len()).find(|&j| starts_at(&c, j, "-->")) {
                Some(end) => {
                    *in_comment = false;
                    i = end + 3;
                }
                None => return out,
            }
            continue;
        }
        if starts_at(&c, i, "<!--") {
            *in_comment = true;
            out.push(' ');
            i += 4;
            continue;
        }
        if c[i] == '`' {
            let start = i;
            while i < c.len() && c[i] == '`' {
                i += 1;
            }
            let run = i - start;
            // a code span closes with a backtick run of exactly the same length
            let mut j = i;
            let mut close = None;
            while j < c.len() {
                if c[j] == '`' {
                    let s = j;
                    while j < c.len() && c[j] == '`' {
                        j += 1;
                    }
                    if j - s == run {
                        close = Some(j);
                        break;
                    }
                } else {
                    j += 1;
                }
            }
            match close {
                Some(end) => {
                    out.push(' ');
                    i = end;
                }
                None => out.extend(&c[start..i]),
            }
            continue;
        }
        out.push(c[i]);
        i += 1;
    }
    out
}

fn starts_at(c: &[char], i: usize, pat: &str) -> bool {
    pat.chars().enumerate().all(|(k, p)| c.get(i + k) == Some(&p))
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPO: &str = "acme/widgets";

    #[test]
    fn recognises_every_keyword_form() {
        for kw in ["Closes", "closes", "close", "closed", "Fixes", "fix", "fixed", "Resolves", "resolved"] {
            assert_eq!(parse_closing_refs(Some(&format!("{kw} #12")), REPO), [12], "{kw}");
        }
        assert_eq!(parse_closing_refs(Some("Closes: #7"), REPO), [7]);
    }

    #[test]
    fn several_issues_sorted_and_deduplicated() {
        assert_eq!(parse_closing_refs(Some("Fixes #9\nCloses #3 and closes #9"), REPO), [3, 9]);
    }

    #[test]
    fn same_repo_qualified_refs_count_other_repos_do_not() {
        assert_eq!(parse_closing_refs(Some("Closes acme/widgets#5"), REPO), [5]);
        assert_eq!(parse_closing_refs(Some("Closes ACME/Widgets#5"), REPO), [5]);
        assert!(parse_closing_refs(Some("Closes other/repo#5"), REPO).is_empty());
    }

    #[test]
    fn mentions_without_a_keyword_do_not_count() {
        assert!(parse_closing_refs(Some("See #4, related to #8, unclosed #1"), REPO).is_empty());
        assert!(parse_closing_refs(None, REPO).is_empty());
    }

    #[test]
    fn full_url_same_repo_counts() {
        assert_eq!(parse_closing_refs(Some("Closes https://github.com/acme/widgets/issues/12"), REPO), [12]);
        assert_eq!(parse_closing_refs(Some("Fixes http://github.com/acme/widgets/issues/12"), REPO), [12]);
        assert_eq!(parse_closing_refs(Some("resolves https://github.com/acme/widgets/issues/12/"), REPO), [12]);
    }

    #[test]
    fn full_url_repo_match_is_case_insensitive() {
        assert_eq!(parse_closing_refs(Some("Closes https://github.com/ACME/Widgets/issues/12"), REPO), [12]);
        assert_eq!(parse_closing_refs(Some("Closes https://GitHub.com/acme/widgets/issues/12"), REPO), [12]);
    }

    #[test]
    fn full_url_other_repo_is_ignored() {
        assert!(parse_closing_refs(Some("Closes https://github.com/other/repo/issues/12"), REPO).is_empty());
        assert!(parse_closing_refs(Some("Closes https://github.com/acme/widgets-fork/issues/12"), REPO).is_empty());
    }

    #[test]
    fn pull_request_urls_do_not_match() {
        assert!(parse_closing_refs(Some("Closes https://github.com/acme/widgets/pull/12"), REPO).is_empty());
        assert!(parse_closing_refs(Some("Closes https://github.com/acme/widgets/pull/12/"), REPO).is_empty());
    }

    #[test]
    fn url_and_short_forms_mix_and_dedupe() {
        let body = "Closes https://github.com/acme/widgets/issues/3\nFixes #3 and closes acme/widgets#9";
        assert_eq!(parse_closing_refs(Some(body), REPO), [3, 9]);
    }

    #[test]
    fn fenced_code_blocks_do_not_count() {
        let body = "Example:\n```\nCloses #1\n```\nReal: nothing\n~~~md\nFixes #2\n~~~";
        assert!(parse_closing_refs(Some(body), REPO).is_empty());
    }

    #[test]
    fn unclosed_fence_runs_to_the_end() {
        assert!(parse_closing_refs(Some("```\nCloses #1\n"), REPO).is_empty());
    }

    #[test]
    fn inline_code_does_not_count() {
        assert!(parse_closing_refs(Some("Use `Closes #1` to link"), REPO).is_empty());
        assert!(parse_closing_refs(Some("Use ``Closes #1`` to link"), REPO).is_empty());
    }

    #[test]
    fn unmatched_backtick_is_literal() {
        assert_eq!(parse_closing_refs(Some("oops ` Closes #4"), REPO), [4]);
    }

    #[test]
    fn html_comments_do_not_count() {
        assert!(parse_closing_refs(Some("<!-- Closes #123 -->"), REPO).is_empty());
        assert!(parse_closing_refs(Some("<!--\nCloses #5\n-->"), REPO).is_empty());
        assert!(parse_closing_refs(Some("<!-- Closes #6"), REPO).is_empty());
    }

    #[test]
    fn quoted_lines_do_not_count() {
        assert!(parse_closing_refs(Some("> Closes #7\n>Fixes #8"), REPO).is_empty());
    }

    #[test]
    fn genuine_keyword_next_to_a_commented_out_one() {
        let body = "<!-- Closes #1 -->\nCloses #2";
        assert_eq!(parse_closing_refs(Some(body), REPO), [2]);
        let body = "<!-- template: Fixes #1 -->\n```\nFixes #9\n```\n\nCloses #2 and Fixes https://github.com/acme/widgets/issues/4";
        assert_eq!(parse_closing_refs(Some(body), REPO), [2, 4]);
    }

    #[test]
    fn comment_after_real_text_on_the_same_line() {
        assert_eq!(parse_closing_refs(Some("Closes #3 <!-- Closes #4 -->"), REPO), [3]);
    }
}
