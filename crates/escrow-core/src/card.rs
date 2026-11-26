//! The bounty card the bot keeps up to date on the issue. Everything that came from a user is escaped.

use crate::{amounts::display, status::BountyStatus};
use chrono::{DateTime, Utc};

/// GitHub rejects issue comments longer than this many characters.
pub const GITHUB_COMMENT_LIMIT: usize = 65_536;
/// At most this many criteria are listed on the card; the rest are summarised in one "and K more" line.
pub const MAX_CARD_CRITERIA: usize = 25;
/// Each listed criterion is cut to this many characters (before escaping) and ends in `…` when cut.
pub const MAX_CRITERION_CHARS: usize = 200;
/// Upper bound on a rendered card, in characters, whatever the criteria. Well under
/// [`GITHUB_COMMENT_LIMIT`], so the card can always be posted and edited. The criteria take at most
/// `MAX_CARD_CRITERIA * (2 * MAX_CRITERION_CHARS + 3)` (escaping at most doubles a character) plus the summary
/// line; the remaining few thousand characters are for the header and footer, whose only variable parts are
/// operator configuration (token symbol, web URL).
pub const MAX_CARD_CHARS: usize = 16_000;
// the criteria budget fits in the card budget, and the card budget is far below GitHub's limit
const _: () = assert!(MAX_CARD_CRITERIA * (2 * MAX_CRITERION_CHARS + 3) + 64 < MAX_CARD_CHARS / 3 * 2);
const _: () = assert!(MAX_CARD_CHARS < GITHUB_COMMENT_LIMIT / 4);

pub struct CardInput<'a> {
    pub id: i64,
    pub status: BountyStatus,
    pub amount: i128,
    pub symbol: &'a str,
    pub deadline: DateTime<Utc>,
    pub relayer_allowed: bool,
    pub criteria: &'a [String],
    pub issue_number: i32,
    pub web_url: &'a str,
}

fn badge(s: BountyStatus) -> &'static str {
    match s {
        BountyStatus::Open => "🟢 Open",
        BountyStatus::Released => "✅ Paid",
        BountyStatus::Refunded => "↩️ Refunded",
        BountyStatus::Disputed => "⚠️ Disputed",
        BountyStatus::Resolved => "⚖️ Resolved",
    }
}

/// Escapes markdown/HTML-significant characters in untrusted text and flattens newlines.
///
/// Beyond the per-character escapes, two GitHub autolinks are broken up: `&` is escaped so entities such
/// as `&lt;` render as text, `://` in a URL gets its colon escaped, and `www.` gets its dot escaped, so
/// bare URLs are not linked.
/// `owner/repo#123` and `#123` are neutralised by escaping the `#`.
pub fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' | '`' | '*' | '_' | '{' | '}' | '[' | ']' | '<' | '>' | '(' | ')' | '#' | '+' | '!' | '|' | '~' | '@' | '&' => {
                out.push('\\');
                out.push(c);
            }
            '\r' => {}
            '\n' => out.push(' '),
            c => out.push(c),
        }
    }
    // Autolinks: a scheme separator and a `www.` prefix are what GitHub turns into links.
    out.replace("://", "\\://").replace("www.", "www\\.")
}

/// `s` cut to `max` characters, with `…` replacing the last one when anything was cut. Cuts on a character
/// boundary and before escaping, so an escape sequence is never split.
fn truncate_chars(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.truncate(out.trim_end().len());
    out.push('…');
    out
}

/// The criteria lines of the card: at most [`MAX_CARD_CRITERIA`], each escaped and cut to
/// [`MAX_CRITERION_CHARS`], then an "and K more" line for the rest.
fn criteria_lines(criteria: &[String]) -> Vec<String> {
    let mut lines: Vec<String> =
        criteria.iter().take(MAX_CARD_CRITERIA).map(|x| format!("- {}", esc(&truncate_chars(x, MAX_CRITERION_CHARS)))).collect();
    let rest = criteria.len().saturating_sub(MAX_CARD_CRITERIA);
    if rest > 0 {
        lines.push(format!("- _…and {rest} more (see the issue)_"));
    }
    lines
}

/// Renders the card. With any criteria (any count, any length) the result stays within [`MAX_CARD_CHARS`].
pub fn render_card(c: &CardInput<'_>) -> String {
    let mut lines = vec![
        format!("### SkillEscrow bounty: {} {} · {}", display(c.amount, 2), c.symbol, badge(c.status)),
        format!(
            "**Deadline:** {} · **Release on merge:** {}",
            c.deadline.format("%Y-%m-%d"),
            if c.relayer_allowed { "automatic" } else { "maintainer signs" }
        ),
    ];
    if !c.criteria.is_empty() {
        lines.push(String::new());
        lines.push("**Acceptance criteria**".into());
        lines.extend(criteria_lines(c.criteria));
    }
    if c.status == BountyStatus::Open {
        lines.push(String::new());
        lines.push(format!(
            "**How to claim:** open a PR that says `Closes #{}`. Link your wallet at {}/me before the merge to be paid instantly.",
            c.issue_number, c.web_url
        ));
    }
    lines.push(String::new());
    lines.push(format!("[Bounty details]({}/b/{})", c.web_url, c.id));
    lines.join("\n")
}

/// Prefix for all issue bounty labels.
pub const BOUNTY_LABEL_PREFIX: &str = "bounty:";

/// Checks whether a label name represents a bounty label.
pub fn is_bounty_label(label: &str) -> bool {
    label.starts_with(BOUNTY_LABEL_PREFIX)
}

/// The issue label, e.g. `bounty: 50 USDC`.
///
/// Active bounties carry this label indicating their reward amount and token.
/// Once a bounty is closed (e.g. released or refunded), the `bounty:` label
/// is removed from the issue so that label-based searches only return active bounties.
pub fn label_for(amount: i128, symbol: &str) -> String {
    format!("bounty: {} {}", display(amount, 0), symbol)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn input<'a>(status: BountyStatus, criteria: &'a [String]) -> CardInput<'a> {
        CardInput {
            id: 4,
            status,
            amount: 50_000_000,
            symbol: "XLM",
            deadline: Utc.with_ymd_and_hms(2026, 11, 17, 12, 0, 0).unwrap(),
            relayer_allowed: true,
            criteria,
            issue_number: 3,
            web_url: "https://app.example",
        }
    }

    #[test]
    fn open_card_explains_how_to_claim() {
        let crit = vec!["adds tests".to_string()];
        let card = render_card(&input(BountyStatus::Open, &crit));
        assert!(card.contains("### SkillEscrow bounty: 5.00 XLM · 🟢 Open"));
        assert!(card.contains("**Deadline:** 2026-11-17 · **Release on merge:** automatic"));
        assert!(card.contains("- adds tests"));
        assert!(card.contains("`Closes #3`"));
        assert!(card.ends_with("[Bounty details](https://app.example/b/4)"));
    }

    #[test]
    fn closed_cards_drop_the_claim_instructions() {
        let card = render_card(&input(BountyStatus::Released, &[]));
        assert!(card.contains("✅ Paid"));
        assert!(!card.contains("How to claim"));
        assert!(!card.contains("Acceptance criteria"));
    }

    #[test]
    fn user_text_cannot_inject_markup_or_mentions() {
        let evil = vec!["[click](http://x) @everyone <script>\n# heading".to_string()];
        let card = render_card(&input(BountyStatus::Open, &evil));
        assert!(!card.contains("[click]("));
        assert!(!card.contains("<script>"));
        assert!(card.contains("\\@everyone"));
        assert!(!card.contains("\n# heading"));
    }

    #[test]
    fn ampersand_is_escaped_so_entities_do_not_render() {
        assert_eq!(esc("&lt;script&gt;"), "\\&lt;script\\&gt;");
        assert_eq!(esc("a & b"), "a \\& b");
    }

    #[test]
    fn bare_urls_do_not_autolink() {
        assert_eq!(esc("see https://evil.example/x"), "see https\\://evil.example/x");
        assert_eq!(esc("HTTP://EVIL.example"), "HTTP\\://EVIL.example");
        assert_eq!(esc("http://a.example and https://b.example"), "http\\://a.example and https\\://b.example");
        assert_eq!(esc("www.evil.example"), "www\\.evil.example");
    }

    #[test]
    fn issue_and_cross_repo_references_do_not_autolink() {
        assert_eq!(esc("fixes #123"), "fixes \\#123");
        assert_eq!(esc("owner/repo#123"), "owner/repo\\#123");
        assert_eq!(esc("see Soroban-escrow/escrow-backend#7"), "see Soroban-escrow/escrow-backend\\#7");
    }

    #[test]
    fn plain_text_is_left_alone() {
        assert_eq!(esc("adds tests."), "adds tests.");
        assert_eq!(esc("Plain words, numbers 42"), "Plain words, numbers 42");
    }

    #[test]
    fn rendered_card_with_hostile_criteria_matches_snapshot() {
        let crit = vec![
            "Fix &lt;b&gt; rendering".to_string(),
            "Docs at https://evil.example/x and www.evil.example".to_string(),
            "Refs #123 and owner/repo#123".to_string(),
        ];
        let card = render_card(&input(BountyStatus::Open, &crit));
        let expected = "\
### SkillEscrow bounty: 5.00 XLM · 🟢 Open
**Deadline:** 2026-11-17 · **Release on merge:** automatic

**Acceptance criteria**
- Fix \\&lt;b\\&gt; rendering
- Docs at https\\://evil.example/x and www\\.evil.example
- Refs \\#123 and owner/repo\\#123

**How to claim:** open a PR that says `Closes #3`. Link your wallet at https://app.example/me before the merge to be paid instantly.

[Bounty details](https://app.example/b/4)";
        assert_eq!(card, expected);
    }

    #[test]
    fn a_huge_checklist_stays_far_below_the_comment_limit() {
        // worst case for escaping: every character gains a backslash
        let escapable: Vec<String> = (0..1000).map(|i| format!("{i}{}", "*_[]<>#@&".repeat(1_000))).collect();
        // worst case for bytes: 4-byte characters that escaping leaves alone
        let wide: Vec<String> = (0..1000).map(|_| "🦀".repeat(10_000)).collect();
        for crit in [&escapable, &wide] {
            let card = render_card(&input(BountyStatus::Open, crit));
            let n = card.chars().count();
            assert!(n <= MAX_CARD_CHARS, "{n} characters");
            assert_eq!(card.lines().filter(|l| l.starts_with("- ")).count(), MAX_CARD_CRITERIA + 1);
            assert!(card.contains("- _…and 975 more (see the issue)_"), "{card}");
            assert!(card.ends_with("[Bounty details](https://app.example/b/4)"));
        }
    }

    #[test]
    fn long_criteria_are_cut_with_an_ellipsis() {
        let crit = vec![format!("{}  tail", "a".repeat(MAX_CRITERION_CHARS - 3))];
        let card = render_card(&input(BountyStatus::Open, &crit));
        let line = card.lines().find(|l| l.starts_with("- a")).unwrap();
        assert_eq!(line, format!("- {}…", "a".repeat(MAX_CRITERION_CHARS - 3)), "trailing space before the cut is dropped");
        assert_eq!(truncate_chars("short", 10), "short");
        assert_eq!(truncate_chars(&"é".repeat(300), 5), "éééé…");
    }

    #[test]
    fn exactly_the_cap_has_no_summary_line() {
        let crit: Vec<String> = (0..MAX_CARD_CRITERIA).map(|i| format!("c{i}")).collect();
        let card = render_card(&input(BountyStatus::Open, &crit));
        assert!(card.contains(&format!("- c{}", MAX_CARD_CRITERIA - 1)));
        assert!(!card.contains("more (see the issue)"));
        let one_more: Vec<String> = (0..=MAX_CARD_CRITERIA).map(|i| format!("c{i}")).collect();
        let card = render_card(&input(BountyStatus::Open, &one_more));
        assert!(!card.contains(&format!("- c{MAX_CARD_CRITERIA}")));
        assert!(card.contains("- _…and 1 more (see the issue)_"));
    }

    #[test]
    fn label_matches_the_convention() {
        assert_eq!(label_for(500_000_000, "USDC"), "bounty: 50 USDC");
        assert_eq!(label_for(55_000_000, "XLM"), "bounty: 5.5 XLM");
    }

    #[test]
    fn detects_bounty_labels() {
        assert!(is_bounty_label("bounty: 50 USDC"));
        assert!(is_bounty_label("bounty: 5.5 XLM"));
        assert!(is_bounty_label("bounty: paid"));
        assert!(!is_bounty_label("bug"));
        assert!(!is_bounty_label("documentation"));
        assert!(!is_bounty_label(""));
    }
}
