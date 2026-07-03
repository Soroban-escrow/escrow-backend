import { afterAll, beforeEach, describe, expect, it } from 'vitest';
import { createDb } from '../src/db.js';
import { handleIssueComment, type CommandDeps, type IssueCommentPayload } from '../src/github/commands.js';

const db = createDb(process.env.TEST_DATABASE_URL ?? 'postgresql://skillescrow:skillescrow@localhost:5432/skillescrow_test');
afterAll(() => db.$disconnect());

let comments: string[];
let perm = 'write';
const deps = (): CommandDeps => ({
  db,
  gh: {
    getPermission: async () => perm,
    postComment: async (_i, _r, _n, body) => (comments.push(body), BigInt(1000 + comments.length)),
  },
  webUrl: 'https://app.test',
  tokens: {
    USDC: { address: 'CUSDC', min: 10_000_000n, max: 5_000_000_000n },
    XLM: { address: 'CXLM', min: 10_000_000n, max: 5_000_000_000n },
  },
  minDays: 1,
  maxDays: 90,
});
const payload = (body: string, over: Partial<IssueCommentPayload> = {}): IssueCommentPayload => ({
  installation: { id: 9 },
  repository: { id: 100, full_name: 'o/r' },
  issue: { number: 7, state: 'open' },
  comment: { id: 1, body, user: { id: 55, login: 'maint', type: 'User' } },
  ...over,
});

beforeEach(async () => {
  comments = [];
  perm = 'write';
  await db.$executeRawUnsafe('TRUNCATE "Payout","Dispute","AuditLog","PullRequestBounty","PullRequest","Bounty","FundingIntent","Repo","Installation" CASCADE');
  await db.installation.create({ data: { id: 9n, accountLogin: 'o', accountType: 'User' } });
  await db.repo.create({ data: { id: 100n, installationId: 9n, fullName: 'o/r', private: false } });
});

describe('/bounty fund', () => {
  it('creates an intent and replies with a funding link', async () => {
    expect(await handleIssueComment(deps(), payload('/bounty 50 USDC 14d'))).toBe('intent-created');
    const i = await db.fundingIntent.findFirstOrThrow();
    expect(i.amount.toString()).toBe('500000000');
    expect(i.token).toBe('CUSDC');
    expect(i.durationDays).toBe(14);
    expect(i.requestedBy).toBe(55n);
    expect(i.commentId).toBe(1001n);
    expect(comments[0]).toContain(`https://app.test/fund/${i.id}`);
    expect(comments[0]).toContain('50.00 USDC');
  });
  it('requires write permission', async () => {
    perm = 'read';
    expect(await handleIssueComment(deps(), payload('/bounty 50'))).toBe('denied');
    expect(await db.fundingIntent.count()).toBe(0);
  });
  it('rejects closed issues, PRs, and active bounties', async () => {
    expect(await handleIssueComment(deps(), payload('/bounty 50', { issue: { number: 7, state: 'closed' } }))).toBe('rejected:closed');
    expect(await handleIssueComment(deps(), payload('/bounty 50', { issue: { number: 7, state: 'open', pull_request: {} } }))).toBe('rejected:pr');
    await db.bounty.create({ data: { id: 1n, repoId: 100n, issueNumber: 7, issueTitle: '', issueUrl: '', criteria: [], maintainer: 'G', token: 'CUSDC', amount: '1', feeBps: 300, deadline: new Date(), relayerAllowed: true, status: 'OPEN', fundTx: 't', createdAt: new Date() } });
    expect(await handleIssueComment(deps(), payload('/bounty 50'))).toBe('rejected:active');
    expect(await db.fundingIntent.count()).toBe(0);
  });
  it('enforces amount and duration limits', async () => {
    expect(await handleIssueComment(deps(), payload('/bounty 0.5'))).toBe('rejected:amount');
    expect(await handleIssueComment(deps(), payload('/bounty 501'))).toBe('rejected:amount');
    expect(await handleIssueComment(deps(), payload('/bounty 5 USDC 91d'))).toBe('rejected:duration');
    expect(await handleIssueComment(deps(), payload('/bounty 5 USDC 1d'))).toBe('intent-created');
  });
  it('ignores bots, non-commands, and later lines; helps on errors', async () => {
    const bot = payload('/bounty 50');
    bot.comment.user.type = 'Bot';
    expect(await handleIssueComment(deps(), bot)).toBe('ignored:bot');
    expect(await handleIssueComment(deps(), payload('thanks!\n/bounty 50'))).toBe('ignored:not-a-command');
    expect(await handleIssueComment(deps(), payload('/bounty abc'))).toBe('error');
    expect(comments).toHaveLength(1);
    expect(await handleIssueComment(deps(), payload('/bounty help'))).toBe('help');
  });
});

describe('/bounty dispute', () => {
  beforeEach(async () => {
    await db.bounty.create({ data: { id: 1n, repoId: 100n, issueNumber: 7, issueTitle: '', issueUrl: '', criteria: [], maintainer: 'G', maintainerGithubId: 55n, token: 'CUSDC', amount: '1', feeBps: 300, deadline: new Date(), relayerAllowed: true, status: 'OPEN', fundTx: 't', createdAt: new Date() } });
    await db.pullRequest.create({ data: { id: 5n, number: 3, authorGithubId: 77n, authorLogin: 'contrib', headSha: 'abc', state: 'open' } });
    await db.pullRequestBounty.create({ data: { prId: 5n, bountyId: 1n } });
  });
  it('lets the maintainer and a PR author open one, but not strangers', async () => {
    expect(await handleIssueComment(deps(), payload('/bounty dispute not paid'))).toBe('dispute-requested');
    const c = payload('/bounty dispute unfair');
    c.comment.user = { id: 77, login: 'contrib', type: 'User' };
    expect(await handleIssueComment(deps(), c)).toBe('dispute-requested');
    const s = payload('/bounty dispute hi');
    s.comment.user = { id: 1, login: 'rando', type: 'User' };
    expect(await handleIssueComment(deps(), s)).toBe('denied');
    expect(await db.dispute.count()).toBe(2);
    expect(await db.auditLog.count()).toBe(2);
  });
  it('rejects when there is no bounty', async () => {
    expect(await handleIssueComment(deps(), payload('/bounty dispute x', { issue: { number: 99, state: 'open' } }))).toBe('rejected:no-bounty');
  });
});

describe('/bounty review (on a PR)', () => {
  let requested: bigint[];
  const withReview = (): CommandDeps => ({ ...deps(), requestReview: async (id) => void requested.push(id) });
  beforeEach(async () => {
    requested = [];
    await db.bounty.create({ data: { id: 1n, repoId: 100n, issueNumber: 7, issueTitle: '', issueUrl: '', criteria: [], maintainer: 'G', maintainerGithubId: 55n, token: 'CUSDC', amount: '1', feeBps: 300, deadline: new Date(), relayerAllowed: true, status: 'OPEN', fundTx: 't', createdAt: new Date() } });
    await db.pullRequest.create({ data: { id: 5n, number: 12, authorGithubId: 77n, authorLogin: 'contrib', headSha: 'abc', state: 'open' } });
    await db.pullRequestBounty.create({ data: { prId: 5n, bountyId: 1n } });
  });
  const onPr = (n: number) => payload('/bounty review', { issue: { number: n, state: 'open', pull_request: {} } });

  it('queues a forced re-review for a linked PR when a writer asks', async () => {
    expect(await handleIssueComment(withReview(), onPr(12))).toBe('review-requested');
    expect(requested).toEqual([5n]);
    expect(comments[0]).toContain('Re-review queued');
  });
  it('refuses non-writers, unlinked PRs, and does nothing on plain issues', async () => {
    perm = 'read';
    expect(await handleIssueComment(withReview(), onPr(12))).toBe('denied');
    perm = 'write';
    expect(await handleIssueComment(withReview(), onPr(99))).toBe('rejected:not-linked');
    expect(await handleIssueComment(withReview(), payload('/bounty review'))).toBe('ignored:review-not-on-issues');
    expect(requested).toEqual([]);
  });
});
