import type { Db } from '../db.js';
import type { ChainPort, Notifier, RelayerPort } from './ports.js';
import { ContractRejection } from './ports.js';

export interface PayoutDeps {
  db: Db;
  chain: ChainPort;
  notifier: Notifier;
  webUrl: string;
}

const key = (bountyId: bigint) => `release:${bountyId}`;
const safeNotify = async (d: PayoutDeps, b: bigint, pr: number, m: string) => {
  try { await d.notifier.notify(b, pr, m); } catch { /* comments are best effort */ }
};

/**
 * A linked PR was merged. First merge wins: the idempotency key is one per bounty.
 * Re-reads the bounty from chain before deciding anything.
 */
export async function onMerge(
  d: PayoutDeps, p: { bountyId: bigint; prNumber: number; authorGithubId: bigint },
): Promise<'created' | 'duplicate' | 'not-open'> {
  const chain = await d.chain.getBounty(p.bountyId);
  if (chain.status !== 'Open') return 'not-open';
  try {
    await d.db.payout.create({
      data: {
        bountyId: p.bountyId, prNumber: p.prNumber, winnerGithubId: p.authorGithubId,
        status: 'PENDING_WALLET', idempotencyKey: key(p.bountyId),
      },
    });
  } catch (e) {
    if ((e as { code?: string }).code === 'P2002') {
      await safeNotify(d, p.bountyId, p.prNumber, 'Another merged PR already claimed this bounty; no second payout is made.');
      return 'duplicate';
    }
    throw e;
  }
  await advance(d, p.bountyId);
  return 'created';
}

/** Moves a PENDING_WALLET payout forward (also used when a wallet is linked late). */
export async function advance(d: PayoutDeps, bountyId: bigint): Promise<string> {
  const payout = await d.db.payout.findUnique({ where: { idempotencyKey: key(bountyId) } });
  if (!payout || payout.status !== 'PENDING_WALLET') return payout?.status ?? 'none';
  const chain = await d.chain.getBounty(bountyId);
  if (chain.status !== 'Open') return 'not-open';
  const user = await d.db.user.findUnique({ where: { githubId: payout.winnerGithubId } });
  if (!user?.wallet) {
    await safeNotify(d, bountyId, payout.prNumber, `Link your wallet at ${d.webUrl}/me to receive your payout.`);
    return 'PENDING_WALLET';
  }
  if (user.wallet === chain.maintainer) {
    await d.db.payout.update({ where: { id: payout.id }, data: { status: 'FAILED', lastError: 'winner is the maintainer' } });
    return 'FAILED';
  }
  const status = chain.relayerAllowed ? 'QUEUED' : 'AWAITING_MAINTAINER';
  // guarded update: only one caller can move it out of PENDING_WALLET
  const r = await d.db.payout.updateMany({
    where: { id: payout.id, status: 'PENDING_WALLET' }, data: { status, winnerAddress: user.wallet },
  });
  if (r.count === 0) return 'raced';
  if (status === 'AWAITING_MAINTAINER') {
    await safeNotify(d, bountyId, payout.prNumber, `The maintainer must release this bounty at ${d.webUrl}/b/${bountyId}.`);
  }
  return status;
}

/** Late wallet link: continue every pending payout for that GitHub user. */
export async function onWalletLinked(d: PayoutDeps, githubId: bigint): Promise<number> {
  const pending = await d.db.payout.findMany({ where: { winnerGithubId: githubId, status: 'PENDING_WALLET' } });
  for (const p of pending) await advance(d, p.bountyId);
  return pending.length;
}

export const MAX_ATTEMPTS = 5;

/**
 * Relayer job. Transient failures are rethrown so the queue can retry with backoff
 * (until MAX_ATTEMPTS); contract rejections mark the payout FAILED without retrying.
 */
export async function relayRelease(d: PayoutDeps, r: RelayerPort, bountyId: bigint): Promise<string> {
  const payout = await d.db.payout.findUnique({ where: { idempotencyKey: key(bountyId) } });
  if (!payout) throw new Error('no payout');
  if (payout.status === 'CONFIRMED' || payout.status === 'FAILED') return payout.status;
  if (!payout.winnerAddress) throw new Error('payout has no winner address');

  // a hash we already submitted: check it instead of submitting again
  if (payout.status === 'SUBMITTED' && payout.txHash) return settle(d, r, payout.id, payout.txHash);

  const chain = await d.chain.getBounty(bountyId);
  // our earlier transaction landed but we crashed before recording it: the chain is the truth
  if (chain.status === 'Released' && chain.winner && chain.winner === payout.winnerAddress) {
    await d.db.payout.update({ where: { id: payout.id }, data: { status: 'CONFIRMED', lastError: null } });
    return 'CONFIRMED';
  }
  if (chain.status !== 'Open' || !chain.relayerAllowed) {
    await d.db.payout.update({ where: { id: payout.id }, data: { status: 'FAILED', lastError: `bounty is ${chain.status}, relayerAllowed=${chain.relayerAllowed}` } });
    return 'FAILED';
  }
  let hash: string;
  try {
    hash = await r.submitRelease(bountyId, payout.winnerAddress);
  } catch (e) {
    return fail(d, payout.id, payout.attempts, e);
  }
  // record the hash BEFORE waiting, so a crash never leads to a second submission
  await d.db.payout.update({ where: { id: payout.id }, data: { status: 'SUBMITTED', txHash: hash, attempts: { increment: 1 } } });
  return settle(d, r, payout.id, hash);
}

async function settle(d: PayoutDeps, r: RelayerPort, id: string, hash: string): Promise<string> {
  const row = await d.db.payout.findUniqueOrThrow({ where: { id } });
  let res: 'SUCCESS' | 'FAILED';
  try {
    res = await r.waitForTx(hash);
  } catch (e) {
    return fail(d, id, row.attempts, e);
  }
  if (res === 'SUCCESS') {
    await d.db.payout.update({ where: { id }, data: { status: 'CONFIRMED', txHash: hash } });
    // the "Paid X to @login" comment is posted when the indexer sees the release event (covers every release path)
    return 'CONFIRMED';
  }
  await d.db.payout.update({ where: { id }, data: { status: 'FAILED', lastError: 'transaction failed on chain' } });
  return 'FAILED';
}

async function fail(d: PayoutDeps, id: string, attempts: number, e: unknown): Promise<string> {
  const msg = e instanceof Error ? e.message : String(e);
  if (e instanceof ContractRejection || attempts + 1 >= MAX_ATTEMPTS) {
    await d.db.payout.update({ where: { id }, data: { status: 'FAILED', lastError: msg, attempts: { increment: 1 } } });
    return 'FAILED';
  }
  // transient: keep the row retryable (QUEUED or SUBMITTED with its hash) and let the queue back off
  await d.db.payout.update({ where: { id }, data: { lastError: msg, attempts: { increment: 1 } } });
  throw e;
}
