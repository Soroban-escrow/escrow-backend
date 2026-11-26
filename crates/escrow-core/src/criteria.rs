//! Acceptance criteria from an issue body. An empty result means "derive them with the AI reviewer".
//!
//! # Where the section starts
//! The first line that is only the words "Acceptance criteria" (any case, optional trailing colon) in one of
//! these forms:
//! * an ATX heading: `## Acceptance criteria`
//! * a bold label: `**Acceptance criteria**`, `**Acceptance criteria:**`, `**Acceptance criteria**:` or `__…__`
//! * a plain line: `Acceptance criteria:` or `Acceptance criteria`
//!
//! The section runs until the next heading or the next label-like line (a whole-line bold label such as
//! `**Notes**`, or an unindented, non-list line ending in `:` such as `Out of scope:`). That keeps a task list
//! further down the issue out of the criteria. With no such section (or an empty one), every task-list
//! checkbox in the body is used instead.
//!
//! # Nested items
//! A list item indented at least two columns deeper than the section's top-level items is a sub-item. Sub-items
//! are never criteria of their own: they are joined onto their parent, `parent: child one; child two`, so the
//! detail is kept (the AI reviewer sees it) without inflating the number of criteria. The same rule applies to
//! the checkbox fallback. Tabs count as four columns.

use regex::Regex;
use std::sync::LazyLock;

/// The section start in any of the styles above, matched against the trimmed line.
static HEADING: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(?:#{1,6}\s*)?(?:\*\*|__)?\s*acceptance\s+criteria\s*:?\s*(?:\*\*|__)?\s*:?\s*#*\s*$").unwrap());
static ANY_HEADING: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s{0,3}#{1,6}(?:\s|$)").unwrap());
/// A whole-line bold label (`**Notes**`, `__Out of scope:__`) or a plain `Something:` line, both unindented.
static LABEL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(?:(?:\*\*|__)[^*_].*?(?:\*\*|__)\s*:?|[^\s\-*+#|>].*:)\s*$").unwrap());
static ITEM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(\s*)(?:[-*+]|\d+[.)])\s+(?:\[[ xX]\]\s+)?(.+?)\s*$").unwrap());
static CHECKBOX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(\s*)[-*+]\s+\[[ xX]\]\s+(.+?)\s*$").unwrap());

/// Leading whitespace width, tabs counted as four columns.
fn indent(ws: &str) -> usize {
    ws.chars().map(|c| if c == '\t' { 4 } else { 1 }).sum()
}

/// Groups `(indent, text)` items into top-level criteria with their sub-items joined on (see the module docs).
#[derive(Default)]
struct Collector {
    top_indent: Option<usize>,
    out: Vec<(String, Vec<String>)>,
}

impl Collector {
    fn push(&mut self, indent: usize, text: &str) {
        match (self.top_indent, self.out.last_mut()) {
            (Some(top), Some((_, children))) if indent >= top + 2 => children.push(text.to_string()),
            _ => {
                self.top_indent = Some(self.top_indent.map_or(indent, |t| t.min(indent)));
                self.out.push((text.to_string(), Vec::new()));
            }
        }
    }

    fn is_empty(&self) -> bool {
        self.out.is_empty()
    }

    fn finish(self) -> Vec<String> {
        self.out
            .into_iter()
            .map(
                |(parent, children)| {
                    if children.is_empty() {
                        parent
                    } else {
                        format!("{}: {}", parent.trim_end_matches(':').trim_end(), children.join("; "))
                    }
                },
            )
            .collect()
    }
}

/// Prefers the list under an "Acceptance criteria" heading or label; falls back to every task-list checkbox.
pub fn parse_criteria(body: Option<&str>) -> Vec<String> {
    let text = body.unwrap_or("").replace("\r\n", "\n");
    let lines: Vec<&str> = text.split('\n').collect();

    if let Some(h) = lines.iter().position(|l| HEADING.is_match(l.trim())) {
        let mut c = Collector::default();
        for l in &lines[h + 1..] {
            // list items first: `1. Supports:` is an item, not a label that ends the section
            if let Some(m) = ITEM.captures(l) {
                c.push(indent(&m[1]), &m[2]);
            } else if ANY_HEADING.is_match(l) || LABEL.is_match(l) {
                break;
            }
        }
        if !c.is_empty() {
            return c.finish();
        }
    }
    let mut c = Collector::default();
    for m in lines.iter().filter_map(|l| CHECKBOX.captures(l)) {
        c.push(indent(&m[1]), &m[2]);
    }
    c.finish()
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
    fn every_heading_style_starts_the_section() {
        for heading in [
            "## Acceptance criteria",
            "### Acceptance Criteria:",
            "## Acceptance criteria ##",
            "**Acceptance criteria**",
            "**Acceptance criteria:**",
            "**Acceptance criteria**:",
            "__Acceptance criteria__",
            "## **Acceptance criteria**",
            "Acceptance criteria:",
            "acceptance criteria",
            "  Acceptance  Criteria :  ",
        ] {
            let body = format!("Intro text\n\n{heading}\n- [ ] first\n- second\n");
            assert_eq!(parse_criteria(Some(&body)), ["first", "second"], "heading {heading:?}");
        }
    }

    #[test]
    fn sentences_mentioning_acceptance_criteria_are_not_headings() {
        let body = "The acceptance criteria are below.\nSee acceptance criteria: later\n- [ ] only checkbox";
        assert_eq!(parse_criteria(Some(body)), ["only checkbox"]);
    }

    #[test]
    fn an_unrelated_task_list_elsewhere_is_ignored() {
        for heading in ["## Acceptance criteria", "**Acceptance criteria**", "Acceptance criteria:"] {
            let body = format!(
                "## Tasks\n- [ ] set up CI\n- [x] write the RFC\n\n{heading}\n- [ ] parses bold labels\n- [ ] keeps nesting\n\n\
                 **Notes**\n- [ ] not a criterion\n\nOut of scope:\n- [ ] also not one\n"
            );
            assert_eq!(parse_criteria(Some(&body)), ["parses bold labels", "keeps nesting"], "heading {heading:?}");
        }
    }

    #[test]
    fn the_section_ends_at_a_plain_label_line() {
        let body = "Acceptance criteria:\n- a\n- b\nOut of scope:\n- c";
        assert_eq!(parse_criteria(Some(body)), ["a", "b"]);
    }

    #[test]
    fn an_item_ending_in_a_colon_does_not_end_the_section() {
        let body = "**Acceptance criteria**\n1. Supports:\n   - bold labels\n   - plain labels\n2. Has tests";
        assert_eq!(parse_criteria(Some(body)), ["Supports: bold labels; plain labels", "Has tests"]);
    }

    #[test]
    fn nested_items_are_joined_to_their_parent() {
        let body = "## Acceptance criteria\n\
                    - [ ] API returns the new URL\n  - [ ] on /bounties/{id}\n  - [ ] on /me/earnings\n\
                    - [ ] Docs updated\n\
                    \t- README\n\
                    - [ ] Tests\n    - unit\n        - deeply nested\n";
        assert_eq!(
            parse_criteria(Some(body)),
            ["API returns the new URL: on /bounties/{id}; on /me/earnings", "Docs updated: README", "Tests: unit; deeply nested"]
        );
    }

    #[test]
    fn one_space_of_drift_is_not_nesting() {
        assert_eq!(parse_criteria(Some("## Acceptance criteria\n- a\n - b\n- c")), ["a", "b", "c"]);
    }

    #[test]
    fn an_indented_section_uses_its_own_top_level() {
        assert_eq!(parse_criteria(Some("## Acceptance criteria\n  - a\n    - a1\n  - b")), ["a: a1", "b"]);
    }

    #[test]
    fn the_checkbox_fallback_also_joins_nested_items() {
        let body = "Todo\n- [ ] alpha\n  - [ ] alpha detail\n- [x] beta";
        assert_eq!(parse_criteria(Some(body)), ["alpha: alpha detail", "beta"]);
    }

    #[test]
    fn nothing_found_is_an_empty_list() {
        assert!(parse_criteria(Some("just prose")).is_empty());
        assert!(parse_criteria(None).is_empty());
        assert_eq!(parse_criteria(Some("## Acceptance criteria\r\n- crlf ok\r\n")), ["crlf ok"]);
    }
}
