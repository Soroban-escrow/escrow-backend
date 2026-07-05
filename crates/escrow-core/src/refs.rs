//! Issues a pull request closes, using GitHub's closing keywords.

use regex::Regex;
use std::{collections::BTreeSet, sync::LazyLock};

static KEYWORD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(?:close[sd]?|fix(?:e[sd])?|resolve[sd]?)\s*:?\s+(?:([\w.-]+/[\w.-]+))?#(\d+)\b").unwrap());

/// Issue numbers in `repo_full_name` that the PR body closes, ascending. Cross-repo references are ignored.
pub fn parse_closing_refs(body: Option<&str>, repo_full_name: &str) -> Vec<u32> {
    let mut out = BTreeSet::new();
    for m in KEYWORD.captures_iter(body.unwrap_or("")) {
        if let Some(r) = m.get(1)
            && !r.as_str().eq_ignore_ascii_case(repo_full_name)
        {
            continue;
        }
        if let Ok(n) = m[2].parse::<u32>() {
            out.insert(n);
        }
    }
    out.into_iter().collect()
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
}
