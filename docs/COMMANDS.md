# SkillEscrow Command Reference

Comments on GitHub issues and pull requests run commands that fund bounties, request reviews, and open disputes. Commands must appear in the **first line of a comment** only; anything after is ignored.

## Fund a bounty

**Syntax:**
```
/bounty <amount> [USDC|XLM] [<days>d]
```

**Permission:** Requires write access to the repository (admin, maintain, or write role).

**Where:** Issue comments only. Pull requests cannot be funded.

**Examples:**
```
/bounty 50              # Funds 50 USDC, default 14 days
/bounty 50 USDC 7d      # Funds 50 USDC, 7 days
/bounty 10 XLM 30d      # Funds 10 XLM, 30 days
```

**Parameters:**
- **amount**: Dollar amount (e.g., `50`, `5.5`, `100.75`). Must be greater than zero.
- **[USDC|XLM]**: Token symbol. Case-insensitive. Defaults to USDC if omitted.
- **[<days>d]**: Duration in days. Case-insensitive `d` required. Defaults to 14 days if omitted. Must be 1-999 days.

**Limits** (configured via environment):
- Minimum amount: `MIN_AMOUNT` (see configuration)
- Maximum amount: `MAX_AMOUNT` (see configuration)
- Minimum duration: `MIN_DAYS` (default 1)
- Maximum duration: `MAX_DAYS` (default 999)
- Token support: `USDC` and `XLM` must be configured; if only one is enabled, that token is always used

**What happens:**
1. A funding link appears in the bot's reply, valid for 24 hours
2. The issue maintainer clicks the link to send funds from their wallet into escrow
3. The bounty becomes active and shows on the issue
4. If another bounty is already active on the issue, the command is rejected

**Possible replies:**
- **Success:** "**Bounty requested: X TOKEN**, open for N days. [fund this bounty](link) (link valid 24 h)"
- **No write access:** "@{username} only collaborators with write access can fund a bounty."
- **On a PR:** "Bounties can only be placed on issues, not pull requests."
- **Issue closed:** "This issue is closed."
- **Bounty already active:** "This issue already has an active bounty."
- **Token disabled:** "{TOKEN} bounties are not enabled on this deployment."
- **Invalid amount:** "Amount must be between X and Y {TOKEN}."
- **Invalid duration:** "Duration must be between N and M days."
- **Parse error:** "Invalid amount "{value}". Usage: `/bounty <amount> [USDC|XLM] [<days>d]`"
- **Syntax help:** Shows full usage on any parse error

## Request a review

**Syntax:**
```
/bounty review
```

**Permission:** Requires write access to the repository.

**Where:** Pull request comments only. Issues are ignored.

**What happens:**
1. The PR is re-reviewed by the SkillEscrow AI reviewer
2. The existing review comment on the PR is updated with the new findings
3. If AI reviews are disabled, the command is silently ignored

**Possible replies:**
- **Success:** "Re-review queued. The review comment on this PR will be updated shortly."
- **No write access:** "@{username} only collaborators with write access can request a review."
- **On an issue:** (ignored, no reply)
- **PR not linked to bounty:** "This pull request is not linked to a bounty, so there is nothing to review against."
- **AI disabled:** (silently ignored)

## Open a dispute

**Syntax:**
```
/bounty dispute <reason>
```

**Permission:** No special permission needed. Only the bounty maintainer or an author of a linked PR can open a dispute.

**Where:** Issue comments only.

**What happens:**
1. The dispute is recorded and assigned to the arbiters
2. A human arbiter reviews it within 48 hours
3. The arbiter may resolve it by awarding funds to either side

**Reason:**
- Required and limited to 2000 characters
- Describe the problem (e.g., "PR was never reviewed", "code quality is poor")

**Example:**
```
/bounty dispute the PR was never actually reviewed before merging
```

**Possible replies:**
- **Success:** "Dispute recorded. A human will review this within 48 hours."
- **No bounty on issue:** "There is no bounty on this issue."
- **Permission denied:** "Only the maintainer or an author of a linked PR can open a dispute."
- **Missing reason:** "Please give a reason: `/bounty dispute <reason>`"

## Get help

**Syntax:**
```
/bounty
/bounty help
```

**Permission:** No special permission needed.

**What happens:**
The bot replies with a summary of available commands.

**Reply:**
```
**SkillEscrow commands**
- `/bounty <amount> [USDC|XLM] [<days>d]` fund this issue, e.g. `/bounty 50 USDC 14d` (needs write access)
- `/bounty dispute <reason>` flag a problem on a bountied issue
- `/bounty help`
```

## Parsing rules

- **First line only:** Only the first line of a comment is parsed. Quotes and examples further down are ignored, so you can safely write `/bounty` in quotes without triggering a command.
- **Case-insensitive:** `/bounty`, `/Bounty`, `/BOUNTY` all work. Token symbols (`USDC`, `usdc`, `Usdc`) work in any case.
- **Whitespace:** Extra spaces are allowed. `/bounty    50` works fine.
- **Order:** Token and duration can appear in any order. `/bounty 50 7d USDC` and `/bounty 50 USDC 7d` both work.

## Error handling

If a command has a syntax error:
1. The bot replies with a friendly error message
2. The full help text is included
3. Commonly mistyped tokens (e.g., "EURO" instead of "USDC") are caught and explained

**Example error reply:**
```
Unrecognised "EURO". Usage: /bounty <amount> [USDC|XLM] [<days>d]

**SkillEscrow commands**
- `/bounty <amount> [USDC|XLM] [<days>d]` fund this issue, e.g. `/bounty 50 USDC 14d` (needs write access)
- `/bounty dispute <reason>` flag a problem on a bountied issue
- `/bounty help`
```

## Configuration variables

These environment variables control the limits mentioned above:

- `MIN_AMOUNT`: Minimum bounty amount in base units (default: depends on token)
- `MAX_AMOUNT`: Maximum bounty amount in base units (default: depends on token)
- `MIN_DAYS`: Minimum bounty duration (default: 1)
- `MAX_DAYS`: Maximum bounty duration (default: 999)
- `BOUNTY_CONTRACT_ID`: The Soroban contract ID where bounties are funded
- `USDC_CONTRACT_ID`: The contract address for USDC token (optional; if set, `/bounty` accepts USDC)
- `XLM_CONTRACT_ID`: The contract address for XLM token (optional; if set, `/bounty` accepts XLM)

---

**Still have questions?** See [docs/API.md](API.md) for the HTTP API and [docs/DEVELOPMENT.md](DEVELOPMENT.md) for how to extend commands.
