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
  // the maintainer's own PRs never earn the bounty (also blocked on-chain)
  const eligible = bounties.filter((b) => b.maintainerGithubId !== BigInt(pr.user.id));
  if (eligible.length === 0) return [];
  const prId = BigInt(pr.id);
  const existing = await d.db.pullRequest.findUnique({ where: { id: prId } });
  if (existing?.state === 'merged') return []; // once merged, the record is frozen
  const data = {
    number: pr.number, authorGithubId: BigInt(pr.user.id), authorLogin: pr.user.login,
    headSha: pr.head.sha, state: pr.merged ? 'merged' : pr.state === 'closed' ? 'closed' : 'open',
    mergedAt: pr.merged_at ? new Date(pr.merged_at) : null,
  };
  await d.db.pullRequest.upsert({ where: { id: prId }, create: { id: prId, ...data }, update: data });
  // one PR can close several bountied issues: one link per (PR, bounty)
  for (const b of eligible) {
    await d.db.pullRequestBounty.upsert({ where: { prId_bountyId: { prId, bountyId: b.id } }, create: { prId, bountyId: b.id }, update: {} });
    if (pr.merged && p.action === 'closed') await onMerge(d, { bountyId: b.id, prNumber: pr.number, authorGithubId: BigInt(pr.user.id) });
  }
  if (!pr.merged && ['opened', 'synchronize', 'reopened', 'ready_for_review'].includes(p.action) && !pr.draft) d.reviewTrigger?.(prId);
  return eligible.map((b) => b.id);
}
