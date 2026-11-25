import { afterAll, beforeEach, describe, expect, it } from 'vitest';
import { createDb } from '../src/db.js';
import { githubNotifier, refreshBountyCard, type GithubApi } from '../src/github/sync.js';

const db = createDb(process.env.TEST_DATABASE_URL ?? 'postgresql://skillescrow:skillescrow@localhost:5432/skillescrow_test');
afterAll(() => db.$disconnect());

let log: string[]; let issue = { title: 'Retry failed requests', body: '## Acceptance criteria\n- [ ] Retries 3 times\n- [ ] Adds a test', html_url: 'u' };
let failIssue = false;
const gh: GithubApi = {
  getIssue: async () => { if (failIssue) throw new Error('boom'); return issue; },
  postComment: async (i, r, n, b) => (await new Promise((res) => setTimeout(res, 40)), log.push(`post ${i} ${r}#${n} :: ${b.split('\n')[0]}`), 555n),
  updateComment: async (i, r, id, b) => void log.push(`update ${id} :: ${b.split('\n')[0]}`),
  setBountyLabel: async (_i, _r, n, l) => void log.push(`label #${n} ${l}`),
};
const deps = () => ({ db, gh, webUrl: 'https://app.test', symbolFor: (t: string) => (t === 'CUSDC' ? 'USDC' : 'XLM') });

beforeEach(async () => {
  log = []; failIssue = false;
  await db.$executeRawUnsafe('TRUNCATE "Payout","Bounty","Repo","Installation" CASCADE');
  await db.installation.create({ data: { id: 9n, accountLogin: 'o', accountType: 'User' } });
  await db.repo.create({ data: { id: 100n, installationId: 9n, fullName: 'o/r', private: false } });
  await db.bounty.create({ data: { id: 1n, repoId: 100n, issueNumber: 7, issueTitle: '', issueUrl: '', criteria: [], maintainer: 'GM', token: 'CUSDC', amount: '500000000', feeBps: 300, deadline: new Date('2026-12-01'), relayerAllowed: true, status: 'OPEN', fundTx: 't', createdAt: new Date() } });
});

describe('refreshBountyCard', () => {
  it('posts the card once, fills title and criteria from the issue, and labels it', async () => {
    expect(await refreshBountyCard(deps(), 1n)).toBe('posted');
    const b = await db.bounty.findUniqueOrThrow({ where: { id: 1n } });
    expect(b.cardCommentId).toBe(555n);
    expect(b.issueTitle).toBe('Retry failed requests');
    expect(b.criteria).toEqual(['Retries 3 times', 'Adds a test']);
    expect(log).toEqual(['post 9 o/r#7 :: ### SkillEscrow bounty: 50.00 USDC · 🟢 Open', 'label #7 bounty: 50 USDC']);
  });
  it('edits the same comment afterwards (never a second one) and drops the label once paid', async () => {
    await refreshBountyCard(deps(), 1n);
    log.length = 0;
    await db.bounty.update({ where: { id: 1n }, data: { status: 'RELEASED' } });
    expect(await refreshBountyCard(deps(), 1n)).toBe('updated');
    expect(log).toEqual(['update 555 :: ### SkillEscrow bounty: 50.00 USDC · ✅ Paid']);
  });
  it('still renders from stored data when the issue cannot be fetched', async () => {
    failIssue = true;
    expect(await refreshBountyCard(deps(), 1n)).toBe('posted');
  });
  it('concurrent refreshes post exactly one comment', async () => {
    const r = await Promise.all([refreshBountyCard(deps(), 1n), refreshBountyCard(deps(), 1n), refreshBountyCard(deps(), 1n)]);
    expect(r.filter((x) => x === 'posted')).toHaveLength(1);
    expect(log.filter((l) => l.startsWith('post '))).toHaveLength(1); // one comment on GitHub, not three
    expect(log.filter((l) => l.startsWith('update '))).toHaveLength(2);
  });
  it('unknown bounty is skipped', async () => {
    expect(await refreshBountyCard(deps(), 99n)).toBe('skipped');
  });
});

describe('githubNotifier', () => {
  it('comments on the PR, or on the issue when there is no PR number', async () => {
    const n = githubNotifier({ db, gh });
    await n.notify(1n, 3, 'hi');
    await n.notify(1n, 0, 'hello');
    expect(log).toEqual(['post 9 o/r#3 :: hi', 'post 9 o/r#7 :: hello']);
    await n.notify(404n, 1, 'nobody'); // unknown bounty: no throw, no comment
    expect(log).toHaveLength(2);
  });
});

describe('announcePayout', () => {
  const explorerTx = (h: string) => `https://explorer.test/tx/${h}`;
  const d = () => ({ ...deps(), explorerTx });
  beforeEach(async () => {
    await db.$executeRawUnsafe('TRUNCATE "AuditLog","PullRequestBounty","PullRequest" CASCADE');
    await db.pullRequest.create({ data: { id: 5n, number: 3, authorGithubId: 9n, authorLogin: 'contrib', headSha: 'a', state: 'merged' } });
    await db.pullRequestBounty.create({ data: { prId: 5n, bountyId: 1n } });
    await db.payout.create({ data: { bountyId: 1n, prNumber: 3, winnerGithubId: 9n, winnerAddress: 'GWINNERADDRESS1234567890', status: 'CONFIRMED', idempotencyKey: 'release:1' } });
  });
  const release = () => db.bounty.update({ where: { id: 1n }, data: { status: 'RELEASED', closeTx: 'abcdef1234567890', winnerAddress: 'GWINNERADDRESS1234567890' } });

  it('comments the net amount (after the 3% fee) on the PR, with a tx link, exactly once', async () => {
    const { announcePayout } = await import('../src/github/sync.js');
    await release();
    expect(await announcePayout(d(), 1n)).toBe('announced');
    expect(await announcePayout(d(), 1n)).toBe('skipped');
    expect(log).toEqual(['post 9 o/r#3 :: **Paid 48.50 USDC to @contrib.** [Transaction abcdef12…](https://explorer.test/tx/abcdef1234567890)']);
  });
  it('names the address when no payout record matches (e.g. the maintainer released to someone else)', async () => {
    const { announcePayout } = await import('../src/github/sync.js');
    await db.payout.deleteMany();
    await release();
    expect(await announcePayout(d(), 1n)).toBe('announced');
    expect(log[0]).toContain('o/r#7');
    expect(log[0]).toContain('GWINNE…7890');
  });
  it('does nothing for a bounty that is not released', async () => {
    const { announcePayout } = await import('../src/github/sync.js');
    expect(await announcePayout(d(), 1n)).toBe('skipped');
    expect(log).toEqual([]);
  });
});
