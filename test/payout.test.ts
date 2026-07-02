import { afterAll, beforeEach, describe, expect, it } from 'vitest';
import { createDb } from '../src/db.js';
import { advance, onMerge, onWalletLinked, relayRelease, type PayoutDeps } from '../src/payouts/payout.js';
import { ContractRejection, type ChainBounty, type RelayerPort } from '../src/payouts/ports.js';

const db = createDb(process.env.TEST_DATABASE_URL ?? 'postgresql://skillescrow:skillescrow@localhost:5432/skillescrow_test');
afterAll(() => db.$disconnect());

let chainState: ChainBounty;
let notes: string[];
const deps = (): PayoutDeps => ({
  db, webUrl: 'https://app.test',
  chain: { getBounty: async () => chainState },
  notifier: { notify: async (_b, _p, m) => void notes.push(m) },
});

beforeEach(async () => {
  chainState = { status: 'Open', relayerAllowed: true, maintainer: 'GMAINT' };
  notes = [];
  await db.$executeRawUnsafe('TRUNCATE "Payout","Dispute","PullRequest","Bounty","Repo","Installation","User" CASCADE');
  await db.installation.create({ data: { id: 1n, accountLogin: 'o', accountType: 'User' } });
  await db.repo.create({ data: { id: 100n, installationId: 1n, fullName: 'o/r', private: false } });
  await db.bounty.create({ data: { id: 1n, repoId: 100n, issueNumber: 7, issueTitle: '', issueUrl: '', criteria: [], maintainer: 'GMAINT', token: 'C', amount: '1000', feeBps: 300, deadline: new Date(), relayerAllowed: true, status: 'OPEN', fundTx: 't', createdAt: new Date() } });
});
const user = (wallet: string | null) => db.user.create({ data: { githubId: 9n, githubLogin: 'c', wallet } });
const merge = () => onMerge(deps(), { bountyId: 1n, prNumber: 3, authorGithubId: 9n });
const status = async () => (await db.payout.findUniqueOrThrow({ where: { idempotencyKey: 'release:1' } })).status;

describe('onMerge', () => {
  it('queues when the author has a wallet and relayer is allowed', async () => {
    await user('GWINNER');
    expect(await merge()).toBe('created');
    expect(await status()).toBe('QUEUED');
    expect((await db.payout.findFirstOrThrow()).winnerAddress).toBe('GWINNER');
  });
  it('waits for a wallet, then continues after linking', async () => {
    await user(null);
    await merge();
    expect(await status()).toBe('PENDING_WALLET');
    expect(notes[0]).toContain('/me');
    await db.user.update({ where: { githubId: 9n }, data: { wallet: 'GWINNER' } });
    expect(await onWalletLinked(deps(), 9n)).toBe(1);
    expect(await status()).toBe('QUEUED');
  });
  it('asks the maintainer when relayer is not allowed', async () => {
    chainState.relayerAllowed = false;
    await user('GWINNER');
    await merge();
    expect(await status()).toBe('AWAITING_MAINTAINER');
    expect(notes.at(-1)).toContain('/b/1');
  });
  it('first merge wins; the second is a duplicate', async () => {
    await user('GWINNER');
    await merge();
    expect(await onMerge(deps(), { bountyId: 1n, prNumber: 4, authorGithubId: 10n })).toBe('duplicate');
    expect(await db.payout.count()).toBe(1);
    expect((await db.payout.findFirstOrThrow()).prNumber).toBe(3);
  });
  it('does nothing when chain says the bounty is not open (ignores the DB)', async () => {
    chainState.status = 'Refunded';
    expect(await merge()).toBe('not-open');
    expect(await db.payout.count()).toBe(0);
  });
  it('refuses to pay the maintainer themselves', async () => {
    await user('GMAINT');
    await merge();
    expect(await status()).toBe('FAILED');
  });
  it('concurrent advances move the payout once', async () => {
    await user('GWINNER');
    await db.payout.create({ data: { bountyId: 1n, prNumber: 3, winnerGithubId: 9n, status: 'PENDING_WALLET', idempotencyKey: 'release:1' } });
    const r = await Promise.all([advance(deps(), 1n), advance(deps(), 1n)]);
    expect(r.filter((x) => x === 'QUEUED')).toHaveLength(1);
  });
});

describe('relayRelease', () => {
  const relayer = (over: Partial<RelayerPort> = {}) => {
    const calls = { submit: 0, wait: 0 };
    const r: RelayerPort = {
      submitRelease: async () => (calls.submit++, 'HASH1'),
      waitForTx: async () => (calls.wait++, 'SUCCESS'),
      ...over,
    };
    return { r, calls };
  };
  beforeEach(async () => {
    await user('GWINNER');
    await merge();
  });

  it('pays once and confirms', async () => {
    const { r, calls } = relayer();
    expect(await relayRelease(deps(), r, 1n)).toBe('CONFIRMED');
    expect(await relayRelease(deps(), r, 1n)).toBe('CONFIRMED');
    expect(calls.submit).toBe(1);
    expect((await db.payout.findFirstOrThrow()).txHash).toBe('HASH1');
  });
  it('transient failure is retryable and never double pays', async () => {
    let n = 0;
    const { r, calls } = relayer({ submitRelease: async () => { if (n++ === 0) throw new Error('rpc down'); calls.submit++; return 'HASH2'; } });
    await expect(relayRelease(deps(), r, 1n)).rejects.toThrow('rpc down');
    expect(await status()).toBe('QUEUED');
    expect(await relayRelease(deps(), r, 1n)).toBe('CONFIRMED');
    expect(calls.submit).toBe(1);
  });
  it('after submit, a timeout keeps the hash and the retry checks it instead of resubmitting', async () => {
    let waits = 0;
    const { r, calls } = relayer({ waitForTx: async () => { if (waits++ === 0) throw new Error('timeout'); return 'SUCCESS'; } });
    await expect(relayRelease(deps(), r, 1n)).rejects.toThrow('timeout');
    expect(await status()).toBe('SUBMITTED');
    expect(await relayRelease(deps(), r, 1n)).toBe('CONFIRMED');
    expect(calls.submit).toBe(1);
  });
  it('contract rejections fail at once without retry', async () => {
    const { r } = relayer({ submitRelease: async () => { throw new ContractRejection('InvalidStatus'); } });
    expect(await relayRelease(deps(), r, 1n)).toBe('FAILED');
    expect((await db.payout.findFirstOrThrow()).lastError).toBe('InvalidStatus');
  });
  it('confirms (instead of failing) when the chain shows our payment already landed', async () => {
    chainState = { status: 'Released', relayerAllowed: true, maintainer: 'GMAINT', winner: 'GWINNER' };
    const { r, calls } = relayer();
    expect(await relayRelease(deps(), r, 1n)).toBe('CONFIRMED');
    expect(calls.submit).toBe(0);
  });
  it('but a release to someone else is still a failure', async () => {
    chainState = { status: 'Released', relayerAllowed: true, maintainer: 'GMAINT', winner: 'GSOMEONEELSE' };
    const { r } = relayer();
    expect(await relayRelease(deps(), r, 1n)).toBe('FAILED');
  });
  it('gives up after 5 transient attempts', async () => {
    const { r } = relayer({ submitRelease: async () => { throw new Error('rpc'); } });
    for (let i = 0; i < 4; i++) await expect(relayRelease(deps(), r, 1n)).rejects.toThrow();
    expect(await relayRelease(deps(), r, 1n)).toBe('FAILED');
  });
  it('re-reads the chain: not open or relayer not allowed means FAILED, nothing submitted', async () => {
    chainState.status = 'Refunded';
    const { r, calls } = relayer();
    expect(await relayRelease(deps(), r, 1n)).toBe('FAILED');
    expect(calls.submit).toBe(0);
  });
});
