import type { Db } from '../db.js';
import { fromBaseUnits } from '@skillescrow/shared';
import { labelFor, renderCard } from './card.js';
import { parseCriteria } from './criteria.js';
import type { Notifier } from '../payouts/ports.js';
import type { IssueCommenter } from '../ops/reminders.js';

/** The slice of the GitHub client the sync code needs (tests pass a fake). */
export interface GithubApi {
  getIssue(installation: bigint, repo: string, issue: number): Promise<{ title: string; body: string | null; html_url: string }>;
  postComment(installation: bigint, repo: string, issue: number, body: string): Promise<bigint>;
  updateComment(installation: bigint, repo: string, commentId: bigint, body: string): Promise<void>;
  setBountyLabel(installation: bigint, repo: string, issue: number, label: string): Promise<void>;
}

export interface SyncDeps {
  db: Db;
  gh: GithubApi;
  webUrl: string;
  /** token contract id -> symbol, e.g. USDC */
  symbolFor: (token: string) => string;
  log?: (msg: string, extra?: unknown) => void;
}

const STATUS_LABEL = { OPEN: true, RELEASED: false, REFUNDED: false, DISPUTED: true, RESOLVED: false } as const;

/**
 * Brings the bounty's GitHub presence in line with its state: fills the issue title and criteria,
 * posts the card once and edits it in place afterwards, and keeps the bounty label current.
 * Safe to call repeatedly (every chain event for the bounty calls it).
 */
export async function refreshBountyCard(d: SyncDeps, bountyId: bigint): Promise<'posted' | 'updated' | 'skipped'> {
  const b = await d.db.bounty.findUnique({ where: { id: bountyId }, include: { repo: true } });
  if (!b) return 'skipped';
  const installation = b.repo.installationId;
  const repo = b.repo.fullName;

  // the indexer only knows what the chain says; the title and criteria come from the issue
  let title = b.issueTitle, criteria = b.criteria;
  try {
    const issue = await d.gh.getIssue(installation, repo, b.issueNumber);
    title = issue.title;
    criteria = parseCriteria(issue.body);
    if (title !== b.issueTitle || criteria.join('\n') !== b.criteria.join('\n')) {
      await d.db.bounty.update({ where: { id: b.id }, data: { issueTitle: title, criteria } });
    }
  } catch (e) { d.log?.('issue fetch failed; rendering with stored data', e instanceof Error ? e.message : String(e)); }

  const symbol = d.symbolFor(b.token);
  const body = renderCard({
    id: b.id, status: b.status, amount: BigInt(b.amount.toString()), symbol, deadline: b.deadline,
    relayerAllowed: b.relayerAllowed, criteria, issueNumber: b.issueNumber, webUrl: d.webUrl,
  });

  // One refresh at a time per bounty: without the lock two concurrent calls would both post a card.
  const outcome = await d.db.$transaction(async (tx) => {
    await tx.$executeRaw`SELECT pg_advisory_xact_lock(${b.id})`;
    const fresh = await tx.bounty.findUniqueOrThrow({ where: { id: b.id }, select: { cardCommentId: true } });
    if (fresh.cardCommentId) {
      await d.gh.updateComment(installation, repo, fresh.cardCommentId, body);
      return 'updated' as const;
    }
    const id = await d.gh.postComment(installation, repo, b.issueNumber, body);
    await tx.bounty.update({ where: { id: b.id }, data: { cardCommentId: id } });
    return 'posted' as const;
  }, { timeout: 20_000 });
  if (STATUS_LABEL[b.status]) await d.gh.setBountyLabel(installation, repo, b.issueNumber, labelFor(BigInt(b.amount.toString()), symbol));
  return outcome;
}

/** Comments for the payout flow: on the PR when there is one, otherwise on the bounty's issue. */
export function githubNotifier(d: Pick<SyncDeps, 'db' | 'gh'>): Notifier {
  return {
    async notify(bountyId, prNumber, message) {
      const b = await d.db.bounty.findUnique({ where: { id: bountyId }, include: { repo: true } });
      if (!b) return;
      await d.gh.postComment(b.repo.installationId, b.repo.fullName, prNumber > 0 ? prNumber : b.issueNumber, message);
    },
  };
}

export function githubIssueCommenter(d: Pick<SyncDeps, 'db' | 'gh'>): IssueCommenter {
  return {
    async comment(bountyId, body) {
      const b = await d.db.bounty.findUnique({ where: { id: bountyId }, include: { repo: true } });
      if (b) await d.gh.postComment(b.repo.installationId, b.repo.fullName, b.issueNumber, body);
    },
  };
}

/** "50.00 USDC" style text for paid comments. */
export const formatPaid = (amount: bigint, feeBps: number, symbol: string) => {
  const net = amount - (amount * BigInt(feeBps)) / 10_000n;
  return `${fromBaseUnits(net, 7, 2)} ${symbol}`;
};
