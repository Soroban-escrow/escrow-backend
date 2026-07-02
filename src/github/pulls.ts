import type { Db } from '../db.js';
import { onMerge, type PayoutDeps } from '../payouts/payout.js';

const KEYWORD = /\b(?:close[sd]?|fix(?:e[sd])?|resolve[sd]?)\s*:?\s+(?:([\w.-]+\/[\w.-]+))?#(\d+)\b/gi;

/** Issue numbers a PR body closes in the given repo (GitHub closing keywords; cross-repo refs ignored). */
export function parseClosingRefs(body: string | null | undefined, repoFullName: string): number[] {
  const out = new Set<number>();
  for (const m of (body ?? '').matchAll(KEYWORD)) {
    if (m[1] && m[1].toLowerCase() !== repoFullName.toLowerCase()) continue;
    out.add(Number(m[2]));
  }
  return [...out].sort((a, b) => a - b);
}

export interface PullRequestPayload {
  action: string;
  repository: { id: number; full_name: string; owner?: { id: number } };
  pull_request: {
    id: number; number: number; body: string | null; draft?: boolean; merged: boolean; merged_at: string | null;
    state: string; head: { sha: string }; user: { id: number; login: string; type: string };
  };
}

export type PullDeps = PayoutDeps & { db: Db; reviewTrigger?: (prId: bigint) => void };

/** Tracks PRs against active bounties and triggers payouts on merge. Returns bounty ids touched. */
export async function handlePullRequest(d: PullDeps, p: PullRequestPayload): Promise<bigint[]> {
  const pr = p.pull_request;
  if (pr.user.type === 'Bot') return [];
  const issues = parseClosingRefs(pr.body, p.repository.full_name);
  if (issues.length === 0) return [];
  const bounties = await d.db.bounty.findMany({
    where: { repoId: BigInt(p.repository.id), issueNumber: { in: issues }, status: { in: ['OPEN', 'DISPUTED'] } },
  });
  const touched: bigint[] = [];
  for (const b of bounties) {
    // the maintainer's own PRs never earn the bounty (also blocked on-chain)
    if (b.maintainerGithubId === BigInt(pr.user.id)) continue;
    const state = pr.merged ? 'merged' : pr.state === 'closed' ? 'closed' : 'open';
    const data = {
      number: pr.number, authorGithubId: BigInt(pr.user.id), authorLogin: pr.user.login,
      headSha: pr.head.sha, state, mergedAt: pr.merged_at ? new Date(pr.merged_at) : null,
    };
    const existing = await d.db.pullRequest.findUnique({ where: { id: BigInt(pr.id) } });
    // once merged, the record is frozen
    if (existing?.state === 'merged') continue;
    await d.db.pullRequest.upsert({ where: { id: BigInt(pr.id) }, create: { id: BigInt(pr.id), bountyId: b.id, ...data }, update: data });
    touched.push(b.id);
    if (pr.merged && p.action === 'closed') {
      await onMerge(d, { bountyId: b.id, prNumber: pr.number, authorGithubId: BigInt(pr.user.id) });
    } else if (['opened', 'synchronize', 'reopened', 'ready_for_review'].includes(p.action) && !pr.draft) {
      d.reviewTrigger?.(BigInt(pr.id));
    }
  }
  return touched;
}
