import type { Db } from '../db.js';
import type { PullFile } from '../github/client.js';
import { diffHash, rankPrs, renderReview, reviewPr, type ModelPort, type Rankable } from './review.js';
import type { Review } from './schema.js';

/** The slice of the GitHub client the pipeline uses (tests pass a fake). */
export interface ReviewGithub {
  getIssue(i: bigint, repo: string, issue: number): Promise<{ title: string; body: string | null }>;
  getPull(i: bigint, repo: string, pr: number): Promise<{ title: string; body: string | null; draft: boolean; state: string; head: { sha: string }; created_at: string }>;
  getPullFiles(i: bigint, repo: string, pr: number): Promise<PullFile[]>;
  getCiConclusion(i: bigint, repo: string, sha: string): Promise<'success' | 'failure' | 'pending' | 'none'>;
  getReadmeExcerpt(i: bigint, repo: string): Promise<string>;
  postComment(i: bigint, repo: string, issue: number, body: string): Promise<bigint>;
  updateComment(i: bigint, repo: string, id: bigint, body: string): Promise<void>;
}

export interface ReviewDeps {
  db: Db;
  gh: ReviewGithub;
  model: ModelPort;
  modelName: string;
  maxPerPrPerDay?: number;
  maxPerRepoPerDay?: number;
  now?: () => Date;
  log?: (msg: string, extra?: unknown) => void;
}

export type ReviewOutcome = 'reviewed' | 'cached' | 'draft' | 'closed' | 'rate_limited' | 'no_bounty' | 'no_pr';
const DAY = 86_400_000;

/**
 * Defense in depth on top of the prompt: a review that reports an injection attempt can never
 * stand as "likely meets", whatever the model said. (It is advisory either way; it never moves money.)
 */
export function sanitizeReview(r: Review, extraFlags: Review['flags'] = []): Review {
  const flags = [...new Set([...r.flags, ...extraFlags])];
  const injected = flags.includes('injection_attempt');
  return { ...r, flags, verdict: injected && r.verdict === 'likely_meets' ? 'needs_human' : r.verdict, score: injected ? Math.min(r.score, 40) : r.score };
}

/** Reviews one PR against its bounty and posts or edits the single review comment. */
export async function runPrReview(d: ReviewDeps, prId: bigint, opts: { force?: boolean } = {}): Promise<ReviewOutcome> {
  const now = (d.now ?? (() => new Date()))();
  const pr = await d.db.pullRequest.findUnique({ where: { id: prId }, include: { bounties: { include: { bounty: { include: { repo: true } } } } } });
  if (!pr) return 'no_pr';
  // review against the first bounty it closes that is still open
  const link = pr.bounties.find((l) => l.bounty.status === 'OPEN' || l.bounty.status === 'DISPUTED');
  if (!link) return 'no_bounty';
  const bounty = link.bounty;
  const installation = bounty.repo.installationId, repo = bounty.repo.fullName;

  const live = await d.gh.getPull(installation, repo, pr.number);
  if (live.state !== 'open') return 'closed';
  if (live.draft) return 'draft';
  const sha = live.head.sha;

  const cached = await d.db.aiReview.findUnique({ where: { prId_headSha: { prId, headSha: sha } } });
  if (cached && !opts.force) return 'cached';

  // cost control: caps per PR and per repository per day
  const since = new Date(now.getTime() - DAY);
  const perPr = await d.db.aiReview.count({ where: { prId, createdAt: { gte: since } } });
  const perRepo = await d.db.aiReview.count({ where: { createdAt: { gte: since }, pr: { bounties: { some: { bounty: { repoId: bounty.repoId } } } } } });
  if (perPr >= (d.maxPerPrPerDay ?? 5) || perRepo >= (d.maxPerRepoPerDay ?? 50)) { d.log?.('review skipped: daily cap', { pr: String(prId) }); return 'rate_limited'; }

  const [issue, files, ci, readme] = await Promise.all([
    d.gh.getIssue(installation, repo, bounty.issueNumber),
    d.gh.getPullFiles(installation, repo, pr.number),
    d.gh.getCiConclusion(installation, repo, sha).catch(() => 'none' as const),
    d.gh.getReadmeExcerpt(installation, repo).catch(() => ''),
  ]);
  const patchFiles = files.map((f) => ({ path: f.filename, additions: f.additions, deletions: f.deletions, ...(f.patch !== undefined ? { patch: f.patch } : {}) }));

  // signals computed by code, not by the model
  const hash = diffHash(patchFiles);
  const extra: Review['flags'] = [];
  if (patchFiles.length > 0) {
    const dup = await d.db.pullRequest.findFirst({ where: { id: { not: prId }, diffHash: hash, bounties: { some: { bountyId: bounty.id } } } });
    if (dup) extra.push('possible_duplicate');
  }
  if (ci === 'failure') extra.push('ci_failing');

  const out = await reviewPr(d.model, {
    issueTitle: issue.title, issueBody: issue.body ?? '', criteria: bounty.criteria, prTitle: live.title, prBody: live.body ?? '',
    files: patchFiles, ciConclusion: ci, readmeExcerpt: readme,
  });
  const review = sanitizeReview(out.review, extra);

  // the one comment per PR: edit the earlier one if we have it
  const previous = await d.db.aiReview.findFirst({ where: { prId, commentId: { not: null } }, orderBy: { createdAt: 'desc' } });
  const saved = await d.db.aiReview.upsert({
    where: { prId_headSha: { prId, headSha: sha } },
    create: { prId, headSha: sha, model: d.modelName, verdict: review.verdict, score: review.score, result: review as never, flags: review.flags, inputTokens: out.inputTokens, outputTokens: out.outputTokens, commentId: previous?.commentId ?? null },
    update: { model: d.modelName, verdict: review.verdict, score: review.score, result: review as never, flags: review.flags, inputTokens: out.inputTokens, outputTokens: out.outputTokens },
  });
  await d.db.pullRequest.update({ where: { id: prId }, data: { diffHash: hash, headSha: sha } });

  // rank among this bounty's open PRs by their latest review
  const siblings = await d.db.pullRequest.findMany({
    where: { state: 'open', bounties: { some: { bountyId: bounty.id } } },
    include: { reviews: { orderBy: { createdAt: 'desc' }, take: 1 } },
  });
  const rankables: Rankable[] = siblings.filter((p) => p.reviews[0]).map((p) => ({
    id: String(p.id), verdict: p.reviews[0]!.verdict as Review['verdict'], score: p.reviews[0]!.score, ci: p.id === prId ? ci : 'none', submittedAt: new Date(live.created_at).getTime(),
  }));
  const order = rankPrs(rankables);
  const body = renderReview(review, { rank: order.indexOf(String(prId)) + 1 || 1, of: Math.max(order.length, 1), sha, model: d.modelName });

  if (saved.commentId) {
    await d.gh.updateComment(installation, repo, saved.commentId, body);
  } else {
    const id = await d.gh.postComment(installation, repo, pr.number, body);
    await d.db.aiReview.update({ where: { id: saved.id }, data: { commentId: id } });
  }
  return 'reviewed';
}
