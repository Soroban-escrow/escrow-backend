//! Status vocabularies. Stored as text in Postgres (with CHECK constraints) and serialized in the
//! exact upper-case spelling the web app already expects.

use serde::{Deserialize, Serialize};
use std::{fmt, str::FromStr};

macro_rules! text_enum {
    ($(#[$m:meta])* $name:ident { $($variant:ident => $text:literal),+ $(,)? }) => {
        $(#[$m])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        pub enum $name { $(#[serde(rename = $text)] $variant),+ }

        impl $name {
            pub const ALL: &'static [$name] = &[$($name::$variant),+];
            pub fn as_str(self) -> &'static str { match self { $($name::$variant => $text),+ } }
        }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str(self.as_str()) }
        }
        impl FromStr for $name {
            type Err = String;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                match s { $($text => Ok($name::$variant),)+ other => Err(format!("unknown {}: {other}", stringify!($name))) }
            }
        }
    };
}

text_enum!(
    /// Mirrors the contract's `Status`. Only the indexer writes it, from chain events.
    BountyStatus {
        Open => "OPEN",
        Released => "RELEASED",
        Refunded => "REFUNDED",
        Disputed => "DISPUTED",
        Resolved => "RESOLVED",
    }
);

text_enum!(
    PayoutStatus {
        PendingWallet => "PENDING_WALLET",
        Queued => "QUEUED",
        Submitted => "SUBMITTED",
        Confirmed => "CONFIRMED",
        Failed => "FAILED",
        AwaitingMaintainer => "AWAITING_MAINTAINER",
    }
);

text_enum!(
    DisputeStatus {
        Requested => "requested",
        OpenedOnchain => "opened_onchain",
        Resolved => "resolved",
        Rejected => "rejected",
    }
);

impl BountyStatus {
    /// Names the contract reports (`Open`, `Released`, ...), as returned by `get_bounty`.
    pub fn from_contract_name(name: &str) -> Option<Self> {
        Some(match name {
            "Open" => Self::Open,
            "Released" => Self::Released,
            "Refunded" => Self::Refunded,
            "Disputed" => Self::Disputed,
            "Resolved" => Self::Resolved,
            _ => return None,
        })
    }

    /// A bounty that can still be claimed or disputed.
    pub fn is_active(self) -> bool {
        matches!(self, Self::Open | Self::Disputed)
    }
}

impl PayoutStatus {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Confirmed | Self::Failed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_every_variant() {
        for s in BountyStatus::ALL {
            assert_eq!(s.as_str().parse::<BountyStatus>().unwrap(), *s);
        }
        for s in PayoutStatus::ALL {
            assert_eq!(s.as_str().parse::<PayoutStatus>().unwrap(), *s);
        }
        for s in DisputeStatus::ALL {
            assert_eq!(s.as_str().parse::<DisputeStatus>().unwrap(), *s);
        }
    }

    #[test]
    fn json_spelling_matches_the_web_app() {
        assert_eq!(serde_json::to_string(&BountyStatus::Released).unwrap(), "\"RELEASED\"");
        assert_eq!(serde_json::to_string(&PayoutStatus::AwaitingMaintainer).unwrap(), "\"AWAITING_MAINTAINER\"");
        assert_eq!(serde_json::to_string(&DisputeStatus::OpenedOnchain).unwrap(), "\"opened_onchain\"");
    }

    #[test]
    fn unknown_values_are_errors() {
        assert!("open".parse::<BountyStatus>().is_err());
        assert!("".parse::<PayoutStatus>().is_err());
    }

    #[test]
    fn contract_names_map_to_statuses() {
        assert_eq!(BountyStatus::from_contract_name("Disputed"), Some(BountyStatus::Disputed));
        assert_eq!(BountyStatus::from_contract_name("nope"), None);
        assert!(BountyStatus::Open.is_active() && BountyStatus::Disputed.is_active());
        assert!(!BountyStatus::Released.is_active());
    }
}
