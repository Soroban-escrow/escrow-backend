//! The bounty card the bot keeps up to date on the issue. Everything that came from a user is escaped.

use crate::{amounts::display, status::BountyStatus};
use chrono::{DateTime, Utc};

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
pub fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' | '`' | '*' | '_' | '{' | '}' | '[' | ']' | '<' | '>' | '(' | ')' | '#' | '+' | '!' | '|' | '~' | '@' => {
                out.push('\\');
                out.push(c);
            }
            '\r' => {}
            '\n' => out.push(' '),
            c => out.push(c),
        }
    }
    out
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
        lines.extend(c.criteria.iter().map(|x| format!("- {}", esc(x))));
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
    fn label_matches_the_convention() {
        assert_eq!(label_for(500_000_000, "USDC"), "bounty: 50 USDC");
        assert_eq!(label_for(55_000_000, "XLM"), "bounty: 5.5 XLM");
    }
}
