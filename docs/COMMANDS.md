# `/bounty` Command Reference

The SkillEscrow GitHub App listens for issue and pull request comments starting with `/bounty`.

- **First line only:** Only the first line of a comment is evaluated; any quoted text, signatures, or notes below the first line are ignored.
- **Bot safety:** Comments posted by GitHub bots (`user.kind == "Bot"`) are completely ignored.
- **Case-insensitivity:** Command keywords (`/bounty`, `help`, `review`, `dispute`, `USDC`, `XLM`) are case-insensitive.

---

## Command Matrix

| Command | Location | Permissions | Config / Limits | Description |
|---|---|---|---|---|
| `/bounty <amount> [USDC\|XLM] [<days>d]` | Issues only | Collaborators with write access (`admin`, `maintain`, `write`) | `TOKEN_MIN_AMOUNT`, `TOKEN_MAX_AMOUNT`, `MIN_DAYS`, `MAX_DAYS`, `TOKEN_USDC_CONTRACT_ID`, `TOKEN_XLM_CONTRACT_ID` | Creates a 24-hour funding intent and returns a link to fund the bounty |
| `/bounty review` | Pull Requests only | Collaborators with write access (`admin`, `maintain`, `write`) | `ANTHROPIC_API_KEY`, repo opt-in setting | Triggers an immediate AI code review on a pull request linked to a bounty |
| `/bounty dispute <reason>` | Bountied Issues only | Bounty maintainer or linked PR author | 2,000 characters maximum length | Records a dispute on the bounty for human arbiter review within 48 hours |
| `/bounty help` (or `/bounty`) | Issues and PRs | Anyone (public) | None | Displays the quick command cheat sheet |

---

## Detailed Specifications

### 1. `/bounty <amount> [USDC|XLM] [<days>d]`

Requests a new bounty on a GitHub issue. Creates a funding intent valid for 24 hours and posts a link for the maintainer to sign and submit the funding transaction.

#### Location & Permissions
- **Location:** Issues only. Comments on pull requests are rejected.
- **Permissions:** Collaborators with repository write access (`admin`, `maintain`, or `write`).

#### Syntax & Arguments
- `<amount>` (**required**): Whole token or decimal amount (e.g. `50`, `10.5`). Must be greater than zero.
- `[token]` (*optional*): `USDC` or `XLM`. Defaults to `USDC` if omitted.
- `[<days>d]` (*optional*): Number of days before the bounty expires, followed by `d` or `D` (e.g. `14d`, `30D`). Defaults to `14` days (`DEFAULT_DAYS`).
- Token and duration arguments may appear in any order after the amount (e.g. `/bounty 50 USDC 14d` or `/bounty 50 14d USDC`).

#### Applicable Configuration Variables
- `TOKEN_MIN_AMOUNT`: Minimum whole token amount allowed (default: `1`).
- `TOKEN_MAX_AMOUNT`: Maximum whole token amount allowed (default: `500`).
- `TOKEN_USDC_CONTRACT_ID`: Deployed contract ID for USDC. If empty, USDC bounties are rejected.
- `TOKEN_XLM_CONTRACT_ID`: Deployed contract ID for XLM (Stellar Asset Contract). If empty, XLM bounties are rejected.
- `MIN_DAYS`: Minimum duration in days (default: `1`).
- `MAX_DAYS`: Maximum duration in days (default: `90`).

#### Bot Replies

##### Success
```markdown
**Bounty requested: <amount> <token>**, open for <days> days.

@<login>, [fund this bounty](<WEB_PUBLIC_URL>/fund/<intent_id>) to activate it (link valid 24 h).
```

##### Rejections & Validations
- **Pull request target:**
  `Bounties can only be placed on issues, not pull requests.`
- **Closed issue:**
  `This issue is closed.`
- **Existing active bounty:**
  `This issue already has an active bounty.`
- **Disabled token:**
  `<token> bounties are not enabled on this deployment.`
- **Amount out of range:**
  `Amount must be between <min> and <max> <token>.`
- **Duration out of range:**
  `Duration must be between <min_days> and <max_days> days.`
- **Insufficient permissions:**
  `@<login> only collaborators with write access can fund a bounty.`
- **Parsing errors:**
  - `Invalid amount "<input>".` followed by the help text.
  - `Amount must be greater than zero.` followed by the help text.
  - `Unrecognised "<input>". Usage: /bounty <amount> [USDC|XLM] [<days>d]` followed by the help text.
  - `Duration must be at least 1 day.` followed by the help text.

---

### 2. `/bounty review`

Requests an on-demand AI code review on a pull request that addresses a bountied issue.

#### Location & Permissions
- **Location:** Pull requests only. Silently ignored on issues.
- **Permissions:** Collaborators with repository write access (`admin`, `maintain`, or `write`).

#### Applicable Configuration & Prerequisites
- The pull request must link to an active bounty (via standard closing keywords such as `Closes #<issue>`).
- AI review worker must be enabled (requires `ANTHROPIC_API_KEY` configured in the backend).
- The repository must have AI reviews enabled in its settings.

#### Bot Replies

##### Success
```markdown
Re-review queued. The review comment on this PR will be updated shortly.
```

##### Rejections & Notices
- **Insufficient permissions:**
  `@<login> only collaborators with write access can request a review.`
- **PR not linked to bounty:**
  `This pull request is not linked to a bounty, so there is nothing to review against.`
- **Repository opted out:**
  `AI reviews are turned off for this repository.`

---

### 3. `/bounty dispute <reason>`

Flags a problem or dispute on an active bounty for human arbiter intervention (e.g. maintainer unresponsive, scope dispute, unmerged PR).

#### Location & Permissions
- **Location:** Bountied issues only.
- **Permissions:** Only the bounty's maintainer or an author of a linked pull request may open a dispute.

#### Syntax & Limits
- `<reason>` (**required**): Explanation of why the bounty is being disputed.
- **Length limit:** The reason is capped at **2,000 characters** (`reason.chars().take(2000)`).
- **Audit trail:** Records a structured audit event `dispute.requested` in the database.

#### Bot Replies

##### Success
```markdown
Dispute recorded. A human will review this within 48 hours.
```

##### Rejections & Validations
- **Missing reason:**
  `Please give a reason: /bounty dispute <reason>` followed by the help text.
- **No bounty on issue:**
  `There is no bounty on this issue.`
- **Unauthorized user:**
  `Only the maintainer or an author of a linked PR can open a dispute.`

---

### 4. `/bounty help`

Displays the quick reference command summary.

#### Location & Permissions
- **Location:** Issues and pull requests.
- **Permissions:** Anyone (public). Also triggered by bare `/bounty` with no arguments.

#### Bot Reply
```markdown
**SkillEscrow commands**
- `/bounty <amount> [USDC|XLM] [<days>d]` fund this issue, e.g. `/bounty 50 USDC 14d` (needs write access)
- `/bounty dispute <reason>` flag a problem on a bountied issue
- `/bounty help`
```
