//! The bounty card the bot keeps up to date on the issue. Everything that came from a user is escaped.

use crate::{amounts::display, status::BountyStatus};
use chrono::{DateTime, Utc};

/// Maximum number of criteria rendered on the bounty card.
pub const MAX_CARD_CRITERIA: usize = 25;

/// Maximum character length of each rendered criterion before truncation with an ellipsis.
pub const MAX_CRITERION_LEN: usize = 200;

/// Maximum character length guaranteed for the rendered card comment body (well below GitHub's 65,536 limit).
pub const MAX_CARD_BODY_LEN: usize = 16_384;
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
        let total = c.criteria.len();
        let rendered_count = total.min(MAX_CARD_CRITERIA);
        for item in &c.criteria[..rendered_count] {
            let escaped = esc(item);
            let truncated = if escaped.chars().count() > MAX_CRITERION_LEN {
                let shortened: String = escaped.chars().take(MAX_CRITERION_LEN).collect();
                format!("{shortened}…")
            } else {
                escaped
            };
            lines.push(format!("- {truncated}"));
        }
        if total > rendered_count {
            lines.push(format!("*…and {} more*", total - rendered_count));
        }
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
    let rendered = lines.join("\n");
    if rendered.chars().count() > MAX_CARD_BODY_LEN {
        let footer = format!("\n\n[Bounty details]({}/b/{})", c.web_url, c.id);
        let budget = MAX_CARD_BODY_LEN.saturating_sub(footer.chars().count() + 1);
        let prefix: String = rendered.chars().take(budget).collect();
        format!("{prefix}…{footer}")
    } else {
        rendered
    }
}

/// The issue label, e.g. `bounty: 50 USDC`.
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
    fn label_matches_the_convention() {
        assert_eq!(label_for(500_000_000, "USDC"), "bounty: 50 USDC");
        assert_eq!(label_for(55_000_000, "XLM"), "bounty: 5.5 XLM");
    }

    #[test]
    fn card_caps_criteria_count_and_item_length() {
        let long_item = "a".repeat(500);
        let crit: Vec<String> = (0..1000).map(|i| format!("Criterion {i}: {long_item}")).collect();
        let card = render_card(&input(BountyStatus::Open, &crit));

        assert!(card.len() <= MAX_CARD_BODY_LEN, "card length {} must be <= {}", card.len(), MAX_CARD_BODY_LEN);
        assert!(card.contains(&format!("*…and {} more*", 1000 - MAX_CARD_CRITERIA)));
        assert!(card.contains("[Bounty details](https://app.example/b/4)"));
    }
}