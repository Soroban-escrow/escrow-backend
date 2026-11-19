//! The `/bounty` comment commands. Parsed from the FIRST line of an issue comment only; anything
//! after it is ignored, so quoted text further down can never trigger a command.

use crate::amounts::to_base_units;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TokenSymbol {
    Usdc,
    Xlm,
}

impl TokenSymbol {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Usdc => "USDC",
            Self::Xlm => "XLM",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Fund { amount: i128, token: TokenSymbol, days: u32 },
    Dispute { reason: String },
    Review,
    Help,
    Error(String),
}

pub const DEFAULT_DAYS: u32 = 14;
pub const USAGE: &str = "Usage: /bounty <amount> [USDC|XLM] [<days>d]";

/// Returns `None` if the comment is not a `/bounty` command at all.
pub fn parse_command(body: &str) -> Option<Command> {
    let first = body.trim_start().lines().next().unwrap_or("");
    let parts: Vec<&str> = first.split_whitespace().collect();
    if !parts.first()?.eq_ignore_ascii_case("/bounty") {
        return None;
    }
    let rest = &parts[1..];
    let head = rest.first().map(|s| s.to_ascii_lowercase()).unwrap_or_default();
    if head == "help" || rest.is_empty() {
        return Some(Command::Help);
    }
    if head == "review" {
        return Some(Command::Review);
    }
    if head == "dispute" {
        let reason = rest[1..].join(" ");
        let reason = reason.trim();
        return Some(if reason.is_empty() {
            Command::Error("Please give a reason: /bounty dispute <reason>".into())
        } else {
            Command::Dispute { reason: reason.to_string() }
        });
    }
    let amount = match to_base_units(rest[0]) {
        Ok(a) => a,
        Err(_) => return Some(Command::Error(format!("Invalid amount \"{}\".", rest[0]))),
    };
    if amount <= 0 {
        return Some(Command::Error("Amount must be greater than zero.".into()));
    }
    let mut token = TokenSymbol::Usdc;
    let mut days = DEFAULT_DAYS;
    for p in &rest[1..] {
        let up = p.to_ascii_uppercase();
        if up == "USDC" {
            token = TokenSymbol::Usdc;
        } else if up == "XLM" {
            token = TokenSymbol::Xlm;
        } else if let Some(n) = up.strip_suffix('D').filter(|n| (1..=3).contains(&n.len()) && n.bytes().all(|b| b.is_ascii_digit())) {
            days = n.parse().unwrap_or(0);
        } else {
            return Some(Command::Error(format!("Unrecognised \"{p}\". {USAGE}")));
        }
    }
    if days < 1 {
        return Some(Command::Error("Duration must be at least 1 day.".into()));
    }
    Some(Command::Fund { amount, token, days })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fund(amount: i128, token: TokenSymbol, days: u32) -> Option<Command> {
        Some(Command::Fund { amount, token, days })
    }

    #[test]
    fn plain_amount_uses_defaults() {
        assert_eq!(parse_command("/bounty 50"), fund(500_000_000, TokenSymbol::Usdc, 14));
    }

    #[test]
    fn token_and_days_in_any_order_and_case() {
        assert_eq!(parse_command("/bounty 50 USDC 14d"), fund(500_000_000, TokenSymbol::Usdc, 14));
        assert_eq!(parse_command("/Bounty 5.5 xlm 7D"), fund(55_000_000, TokenSymbol::Xlm, 7));
        assert_eq!(parse_command("  /bounty 10 30d XLM"), fund(100_000_000, TokenSymbol::Xlm, 30));
    }

    #[test]
    fn only_the_first_line_counts() {
        assert_eq!(parse_command("/bounty 5\n/bounty 500"), fund(50_000_000, TokenSymbol::Usdc, 14));
        assert_eq!(parse_command("thanks!\n/bounty 5"), None);
        assert_eq!(parse_command("> /bounty 5"), None);
    }

    #[test]
    fn not_a_command() {
        assert_eq!(parse_command("looks good to me"), None);
        assert_eq!(parse_command("/bountyx 5"), None);
        assert_eq!(parse_command(""), None);
    }

    #[test]
    fn help_review_dispute() {
        assert_eq!(parse_command("/bounty"), Some(Command::Help));
        assert_eq!(parse_command("/bounty HELP"), Some(Command::Help));
        assert_eq!(parse_command("/bounty review"), Some(Command::Review));
        assert_eq!(
            parse_command("/bounty dispute the PR was never reviewed"),
            Some(Command::Dispute { reason: "the PR was never reviewed".into() })
        );
        assert!(matches!(parse_command("/bounty dispute"), Some(Command::Error(_))));
    }

    #[test]
    fn bad_input_gets_a_friendly_error() {
        assert!(matches!(parse_command("/bounty abc"), Some(Command::Error(m)) if m.contains("Invalid amount")));
        assert!(matches!(parse_command("/bounty 0"), Some(Command::Error(m)) if m.contains("greater than zero")));
        assert!(matches!(parse_command("/bounty 5 EURO"), Some(Command::Error(m)) if m.contains("Unrecognised")));
        assert!(matches!(parse_command("/bounty 5 0d"), Some(Command::Error(m)) if m.contains("at least 1 day")));
        assert!(matches!(parse_command("/bounty 5 1000d"), Some(Command::Error(_))));
        assert!(matches!(parse_command("/bounty -5"), Some(Command::Error(_))));
    }
}
