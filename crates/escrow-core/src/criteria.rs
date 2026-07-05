//! Acceptance criteria from an issue body. An empty result means "derive them with the AI reviewer".

use regex::Regex;
use std::sync::LazyLock;

static HEADING: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^#{1,6}\s*acceptance criteria\s*:?\s*$").unwrap());
static ANY_HEADING: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^#{1,6}\s").unwrap());
static ITEM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*(?:[-*+]|\d+[.)])\s+(?:\[[ xX]\]\s+)?(.+?)\s*$").unwrap());
static CHECKBOX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*[-*+]\s+\[[ xX]\]\s+(.+?)\s*$").unwrap());

/// Prefers the list under an "Acceptance criteria" heading; falls back to every task-list checkbox.
pub fn parse_criteria(body: Option<&str>) -> Vec<String> {
    let text = body.unwrap_or("").replace("\r\n", "\n");
    let lines: Vec<&str> = text.split('\n').collect();

    if let Some(h) = lines.iter().position(|l| HEADING.is_match(l.trim())) {
        let mut out = Vec::new();
        for l in &lines[h + 1..] {
            if ANY_HEADING.is_match(l) {
                break;
            }
            if let Some(m) = ITEM.captures(l) {
                out.push(m[1].to_string());
            }
        }
        if !out.is_empty() {
            return out;
        }
    }
    lines.iter().filter_map(|l| CHECKBOX.captures(l).map(|m| m[1].to_string())).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_heading_section() {
        let body = "Intro\n\n## Acceptance criteria\n- first\n* second\n1. third\n2) fourth\n\n## Notes\n- not a criterion";
        assert_eq!(parse_criteria(Some(body)), ["first", "second", "third", "fourth"]);
    }

    #[test]
    fn heading_is_case_insensitive_and_allows_a_colon() {
        assert_eq!(parse_criteria(Some("### ACCEPTANCE CRITERIA:\n- a")), ["a"]);
    }

    #[test]
    fn strips_checkbox_markers_inside_the_section() {
        assert_eq!(parse_criteria(Some("## Acceptance Criteria\n- [ ] one\n- [x] two")), ["one", "two"]);
    }

    #[test]
    fn falls_back_to_task_list_checkboxes() {
        assert_eq!(parse_criteria(Some("Do it\n- [ ] alpha\n- plain bullet\n- [X] beta")), ["alpha", "beta"]);
    }

    #[test]
    fn empty_heading_section_falls_back() {
        assert_eq!(parse_criteria(Some("## Acceptance criteria\n\n## Other\n- [ ] x")), ["x"]);
    }

    #[test]
    fn nothing_found_is_an_empty_list() {
        assert!(parse_criteria(Some("just prose")).is_empty());
        assert!(parse_criteria(None).is_empty());
        assert_eq!(parse_criteria(Some("## Acceptance criteria\r\n- crlf ok\r\n")), ["crlf ok"]);
    }
}
