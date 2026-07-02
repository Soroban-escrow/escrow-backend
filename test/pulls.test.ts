import { afterAll, beforeEach, describe, expect, it } from 'vitest';
import { createDb } from '../src/db.js';
import { handlePullRequest, parseClosingRefs, type PullRequestPayload } from '../src/github/pulls.js';

const db = createDb(process.env.TEST_DATABASE_URL ?? 'postgresql://skillescrow:skillescrow@localhost:5432/skillescrow_test');
afterAll(() => db.$disconnect());

describe('parseClosingRefs', () => {
  it('finds closing keywords in the same repo only', () => {
    expect(parseClosingRefs('Closes #12 and fixes: #7', 'o/r')).toEqual([7, 12]);
    expect(parseClosingRefs('Resolved o/r#3, closes other/x#4', 'o/r')).toEqual([3]);
    expect(parseClosingRefs('see #5, related to #6', 'o/r')).toEqual([]);
    expect(parseClosingRefs('CLOSES #9', 'o/r')).toEqual([9]);
    expect(parseClosingRefs(null, 'o/r')).toEqual([]);
    expect(parseClosingRefs('closes #1 closes #1', 'o/r')).toEqual([1]);
  });
});

let notes: string[];
let reviews: bigint[];
const deps = () => ({
  db, webUrl: 'https://app.test',
  chain: { getBounty: async () => ({ status: 'Open' as const, relayerAllowed: true, maintainer: 'GMAINT' }) },
  notifier: { notify: async (_b: bigint, _p: number, m: string) => void notes.push(m) },
  reviewTrigger: (id: bigint) => void reviews.push(id),
});
const pr = (action: string, over: Partial<PullRequestPayload['pull_request']> = {}): PullRequestPayload => ({
  action,
  repository: { id: 100, full_name: 'o/r' },
  pull_request: { id: 500, number: 3, body: 'Closes #7', merged: false, merged_at: null, state: 'open', head: { sha: 'abc' }, user: { id: 9, login: 'c', type: 'User' }, ...over },
});

beforeEach(async () => {
  notes = []; reviews = [];
  await db.$executeRawUnsafe('TRUNCATE "Payout","PullRequestBounty","PullRequest","Bounty","Repo","Installation","User" CASCADE');
  await db.installation.create({ data: { id: 1n, accountLogin: 'o', accountType: 'User' } });
  await db.repo.create({ data: { id: 100n, installationId: 1n, fullName: 'o/r', private: false } });
  await db.bounty.create({ data: { id: 1n, repoId: 100n, issueNumber: 7, issueTitle: '', issueUrl: '', criteria: [], maintainer: 'GMAINT', maintainerGithubId: 55n, token: 'C', amount: '1000', feeBps: 300, deadline: new Date(), relayerAllowed: true, status: 'OPEN', fundTx: 't', createdAt: new Date() } });
});

describe('handlePullRequest', () => {
  it('tracks an opened PR and triggers a review', async () => {
    expect(await handlePullRequest(deps(), pr('opened'))).toEqual([1n]);
    expect((await db.pullRequest.findUniqueOrThrow({ where: { id: 500n } })).state).toBe('open');
    expect(reviews).toEqual([500n]);
  });
  it('skips drafts for review, PRs with no bounty, and bots', async () => {
    await handlePullRequest(deps(), pr('opened', { draft: true }));
    expect(reviews).toEqual([]);
    expect(await handlePullRequest(deps(), pr('opened', { body: 'Closes #99' }))).toEqual([]);
    expect(await handlePullRequest(deps(), pr('opened', { user: { id: 9, login: 'dependabot', type: 'Bot' } }))).toEqual([]);
  });
  it('ignores the maintainer\'s own PR', async () => {
    expect(await handlePullRequest(deps(), pr('opened', { user: { id: 55, login: 'maint', type: 'User' } }))).toEqual([]);
    expect(await db.pullRequest.count()).toBe(0);
  });
  it('merge creates the payout and freezes the PR record', async () => {
    await db.user.create({ data: { githubId: 9n, githubLogin: 'c', wallet: 'GWIN' } });
    await handlePullRequest(deps(), pr('opened'));
    await handlePullRequest(deps(), pr('closed', { merged: true, merged_at: '2026-11-02T10:00:00Z', state: 'closed' }));
    const p = await db.payout.findFirstOrThrow();
    expect(p.status).toBe('QUEUED');
    expect(p.prNumber).toBe(3);
    // a later edit does not unmerge it
    await handlePullRequest(deps(), pr('synchronize', { head: { sha: 'zzz' } }));
    const rec = await db.pullRequest.findUniqueOrThrow({ where: { id: 500n } });
    expect(rec.state).toBe('merged');
    expect(rec.headSha).toBe('abc');
  });
  it('closed without merge does not pay', async () => {
    await handlePullRequest(deps(), pr('opened'));
    await handlePullRequest(deps(), pr('closed', { state: 'closed' }));
    expect(await db.payout.count()).toBe(0);
    expect((await db.pullRequest.findUniqueOrThrow({ where: { id: 500n } })).state).toBe('closed');
  });

  it('one PR can close several bounties: one PR row, one link per bounty, a payout for each on merge', async () => {
    await db.bounty.create({ data: { id: 2n, repoId: 100n, issueNumber: 8, issueTitle: '', issueUrl: '', criteria: [], maintainer: 'GMAINT', maintainerGithubId: 55n, token: 'C', amount: '2000', feeBps: 300, deadline: new Date(), relayerAllowed: true, status: 'OPEN', fundTx: 't2', createdAt: new Date() } });
    await db.user.create({ data: { githubId: 9n, githubLogin: 'c', wallet: 'GWIN' } });
    const body = 'Closes #7 and fixes #8';
    expect((await handlePullRequest(deps(), pr('opened', { body }))).sort()).toEqual([1n, 2n]);
    expect(await db.pullRequest.count()).toBe(1);
    expect(await db.pullRequestBounty.count()).toBe(2);
    await handlePullRequest(deps(), pr('closed', { body, merged: true, merged_at: '2026-11-02T10:00:00Z', state: 'closed' }));
    const payouts = await db.payout.findMany({ orderBy: { bountyId: 'asc' } });
    expect(payouts.map((p) => [p.bountyId, p.status])).toEqual([[1n, 'QUEUED'], [2n, 'QUEUED']]);
    // replaying the delivery changes nothing
    await handlePullRequest(deps(), pr('closed', { body, merged: true, merged_at: '2026-11-02T10:00:00Z', state: 'closed' }));
    expect(await db.payout.count()).toBe(2);
    expect(await db.pullRequestBounty.count()).toBe(2);
  });
  it('a PR that only closes a bounty it may not earn creates nothing', async () => {
    expect(await handlePullRequest(deps(), pr('opened', { user: { id: 55, login: 'maint', type: 'User' } }))).toEqual([]);
    expect(await db.pullRequestBounty.count()).toBe(0);
  });
});
