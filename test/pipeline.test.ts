import { afterAll, beforeEach, describe, expect, it } from 'vitest';
import { runPrReview, sanitizeReview, type ReviewGithub } from '../src/ai/pipeline.js';
import type { ModelPort } from '../src/ai/review.js';
import type { Review } from '../src/ai/schema.js';
import { createDb } from '../src/db.js';

const db = createDb(process.env.TEST_DATABASE_URL ?? 'postgresql://skillescrow:skillescrow@localhost:5432/skillescrow_test');
afterAll(() => db.$disconnect());

const GOOD: Review = { verdict: 'partially_meets', score: 62, criteria: [{ criterion: 'retries', derived: false, status: 'partial', evidence: 'a.ts:3' }], summary: 'ok', risks: [], flags: ['tests_missing'], questions_for_maintainer: [] };
let model: Review | unknown; let modelCalls: string[]; let posted: string[]; let updated: string[];
let live: { draft: boolean; state: string; sha: string }; let files: { filename: string; additions: number; deletions: number; patch?: string }[]; let ci: 'success' | 'failure' | 'pending' | 'none';
const modelPort: ModelPort = { complete: async (_s, u) => (modelCalls.push(u), { output: model, inputTokens: 100, outputTokens: 20 }) };
const gh: ReviewGithub = {
  getIssue: async () => ({ title: 'Retry', body: 'b' }),
  getPull: async () => ({ title: 'Add retries', body: 'Closes #7', draft: live.draft, state: live.state, head: { sha: live.sha }, created_at: '2026-11-02T00:00:00Z' }),
  getPullFiles: async () => files,
  getCiConclusion: async () => ci,
  getReadmeExcerpt: async () => 'readme',
  postComment: async (_i, _r, n, b) => (posted.push(`#${n} ${b.split('\n').find((l) => l.startsWith('**Verdict'))}`), 900n + BigInt(posted.length)),
  updateComment: async (_i, _r, id, b) => void updated.push(`${id} ${b.split('\n').find((l) => l.startsWith('**Verdict'))}`),
};
const deps = (over = {}) => ({ db, gh, model: modelPort, modelName: 'test-model', now: () => new Date('2026-11-02T12:00:00Z'), ...over });

beforeEach(async () => {
  model = GOOD; modelCalls = []; posted = []; updated = [];
  live = { draft: false, state: 'open', sha: 'a'.repeat(40) }; ci = 'success';
  files = [{ filename: 'src/a.ts', additions: 3, deletions: 1, patch: '+retry()' }];
  await db.$executeRawUnsafe('TRUNCATE "AiReview","PullRequestBounty","PullRequest","Payout","Bounty","Repo","Installation" CASCADE');
  await db.installation.create({ data: { id: 9n, accountLogin: 'o', accountType: 'User' } });
  await db.repo.create({ data: { id: 100n, installationId: 9n, fullName: 'o/r', private: false } });
  await db.bounty.create({ data: { id: 1n, repoId: 100n, issueNumber: 7, issueTitle: 'Retry', issueUrl: '', criteria: ['retries 3 times'], maintainer: 'GM', token: 'C', amount: '1', feeBps: 300, deadline: new Date(), relayerAllowed: true, status: 'OPEN', fundTx: 't', createdAt: new Date() } });
});
const pr = async (id: bigint, number: number) => {
  await db.pullRequest.create({ data: { id, number, authorGithubId: 9n, authorLogin: 'c', headSha: 'x', state: 'open' } });
  await db.pullRequestBounty.create({ data: { prId: id, bountyId: 1n } });
};

describe('runPrReview', () => {
  it('reviews, stores the result and posts one comment', async () => {
    await pr(500n, 3);
    expect(await runPrReview(deps(), 500n)).toBe('reviewed');
    const r = await db.aiReview.findFirstOrThrow();
    expect(r).toMatchObject({ verdict: 'partially_meets', score: 62, model: 'test-model', inputTokens: 100 });
    expect(r.commentId).toBe(901n);
    expect(posted).toEqual(['#3 **Verdict:** Partially meets (score 62) · Rank 1 of 1 on this bounty']);
  });
  it('is cached by head sha; a new push edits the same comment instead of posting another', async () => {
    await pr(500n, 3);
    await runPrReview(deps(), 500n);
    expect(await runPrReview(deps(), 500n)).toBe('cached');
    expect(modelCalls).toHaveLength(1);
    live.sha = 'b'.repeat(40);
    expect(await runPrReview(deps(), 500n)).toBe('reviewed');
    expect(posted).toHaveLength(1);
    expect(updated).toHaveLength(1);
    expect(await db.aiReview.count()).toBe(2);
  });
  it('force re-reviews the same commit and replaces the stored review', async () => {
    await pr(500n, 3);
    await runPrReview(deps(), 500n);
    model = { ...GOOD, score: 80 };
    expect(await runPrReview(deps(), 500n, { force: true })).toBe('reviewed');
    expect(await db.aiReview.count()).toBe(1);
    expect((await db.aiReview.findFirstOrThrow()).score).toBe(80);
    expect(posted).toHaveLength(1);
  });
  it('skips drafts, closed PRs, PRs without an open bounty and unknown PRs, without calling the model', async () => {
    await pr(500n, 3);
    live.draft = true; expect(await runPrReview(deps(), 500n)).toBe('draft');
    live.draft = false; live.state = 'closed'; expect(await runPrReview(deps(), 500n)).toBe('closed');
    live.state = 'open'; await db.bounty.update({ where: { id: 1n }, data: { status: 'RELEASED' } });
    expect(await runPrReview(deps(), 500n)).toBe('no_bounty');
    expect(await runPrReview(deps(), 12345n)).toBe('no_pr');
    expect(modelCalls).toHaveLength(0);
  });
  it('enforces the per-PR and per-repo daily caps', async () => {
    await pr(500n, 3);
    for (let i = 0; i < 2; i++) { live.sha = String(i).repeat(40); await runPrReview(deps({ maxPerPrPerDay: 2 }), 500n); }
    live.sha = 'c'.repeat(40);
    expect(await runPrReview(deps({ maxPerPrPerDay: 2 }), 500n)).toBe('rate_limited');
    await pr(501n, 4);
    expect(await runPrReview(deps({ maxPerRepoPerDay: 2 }), 501n)).toBe('rate_limited');
    expect(modelCalls).toHaveLength(2);
  });
  it('a model that returns invalid output twice ends as needs_human', async () => {
    await pr(500n, 3);
    model = { nonsense: true };
    await runPrReview(deps(), 500n);
    expect((await db.aiReview.findFirstOrThrow()).verdict).toBe('needs_human');
    expect(modelCalls).toHaveLength(2); // one retry
  });
  it('flags an identical diff on another PR as a possible duplicate, and failing CI', async () => {
    await pr(500n, 3);
    await runPrReview(deps(), 500n);
    await pr(501n, 4);
    ci = 'failure';
    await runPrReview(deps(), 501n);
    const second = await db.aiReview.findFirstOrThrow({ where: { prId: 501n } });
    expect(second.flags).toEqual(expect.arrayContaining(['possible_duplicate', 'ci_failing']));
    const first = await db.aiReview.findFirstOrThrow({ where: { prId: 500n } });
    expect(first.flags).not.toContain('possible_duplicate');
  });
  it('ranks PRs: the better review gets rank 1, and a note appears in each comment', async () => {
    await pr(500n, 3);
    model = { ...GOOD, verdict: 'does_not_meet', score: 20 };
    await runPrReview(deps(), 500n);
    await pr(501n, 4);
    files = [{ filename: 'src/b.ts', additions: 9, deletions: 0, patch: '+better()' }];
    model = { ...GOOD, verdict: 'likely_meets', score: 90, flags: [] };
    await runPrReview(deps(), 501n);
    expect(posted.at(-1)).toContain('Rank 1 of 2');
  });
});

describe('sanitizeReview', () => {
  it('never lets an injection attempt stand as "likely meets"', () => {
    const r = sanitizeReview({ ...GOOD, verdict: 'likely_meets', score: 100, flags: ['injection_attempt'] });
    expect(r.verdict).toBe('needs_human');
    expect(r.score).toBeLessThanOrEqual(40);
    expect(sanitizeReview({ ...GOOD, verdict: 'likely_meets', score: 95, flags: [] }).verdict).toBe('likely_meets');
  });
  it('merges code-computed flags without duplicates', () => {
    expect(sanitizeReview({ ...GOOD, flags: ['tests_missing'] }, ['tests_missing', 'ci_failing']).flags).toEqual(['tests_missing', 'ci_failing']);
  });
});
