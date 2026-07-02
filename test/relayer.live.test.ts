import { Client } from '@skillescrow/contract-client';
import { contract, Keypair } from '@stellar/stellar-sdk';
import { afterAll, describe, expect, it } from 'vitest';
import { createChain } from '../src/chain/client.js';
import { createDb } from '../src/db.js';
import { relayerPass } from '../src/payouts/loop.js';
import { createRelayer } from '../src/payouts/relayer.js';

const db = createDb(process.env.TEST_DATABASE_URL ?? 'postgresql://skillescrow:skillescrow@localhost:5432/skillescrow_test');
afterAll(() => db.$disconnect());
const CFG = { rpcUrl: 'https://soroban-testnet.stellar.org', networkPassphrase: 'Test SDF Network ; September 2015', contractId: 'CCEKBPYCWKNTXIQLXJHVAF7RYGUZZ6MITA7VVJBOK5BHXUO44ELZ4F4V' };
const { LIVE_RELAYER_SECRET: RS, LIVE_MAINTAINER_SECRET: MS, LIVE_WINNER: WINNER, LIVE_TOKEN: TOKEN } = process.env;

// Real funds on testnet: funds a bounty from the maintainer, queues the payout, the relayer releases it for real.
describe.runIf(process.env.LIVE_CHAIN && RS && MS && WINNER && TOKEN)('relayer (testnet)', () => {
  it('releases once, pays the winner, and never pays twice', async () => {
    const mk = Keypair.fromSecret(MS!);
    const maint = new Client({ ...CFG, publicKey: mk.publicKey(), ...contract.basicNodeSigner(mk, CFG.networkPassphrase) });
    const issue = BigInt(Date.now());
    const funded = await (await maint.fund_bounty({ maintainer: mk.publicKey(), repo_id: 4242n, issue, token: TOKEN!, amount: 20_000_000n, deadline: BigInt(Math.floor(Date.now() / 1000) + 2 * 86400), relayer_allowed: true })).signAndSend();
    const id = funded.result.unwrap();

    await db.$executeRawUnsafe('TRUNCATE "Payout","Bounty","Repo","Installation" CASCADE');
    await db.installation.create({ data: { id: 1n, accountLogin: 'o', accountType: 'User' } });
    await db.repo.create({ data: { id: 4242n, installationId: 1n, fullName: 'o/r', private: false } });
    await db.bounty.create({ data: { id, repoId: 4242n, issueNumber: 1, issueTitle: '', issueUrl: '', criteria: [], maintainer: mk.publicKey(), token: TOKEN!, amount: '20000000', feeBps: 300, deadline: new Date(), relayerAllowed: true, status: 'OPEN', fundTx: 't', createdAt: new Date() } });
    await db.payout.create({ data: { bountyId: id, prNumber: 1, winnerGithubId: 9n, winnerAddress: WINNER!, status: 'QUEUED', idempotencyKey: `release:${id}` } });

    const chain = createChain(CFG);
    const relayer = createRelayer({ ...CFG, secret: RS! });
    const deps = { db, webUrl: 'x', notifier: { notify: async () => {} }, chain: { getBounty: async (b: bigint) => { const x = await chain.getBounty(b); return { status: x.status.tag, relayerAllowed: x.relayer_allowed, maintainer: x.maintainer }; } } };

    expect(await relayerPass(deps, relayer, new Map())).toBe(1);
    const p = await db.payout.findUniqueOrThrow({ where: { idempotencyKey: `release:${id}` } });
    expect(p.status).toBe('CONFIRMED');
    expect(p.txHash).toMatch(/^[0-9a-f]{64}$/);
    expect((await chain.getBounty(id)).status.tag).toBe('Released');
    expect((await chain.getBounty(id)).winner).toBe(WINNER);
    expect(await relayerPass(deps, relayer, new Map())).toBe(0); // confirmed payouts are never picked up again
  }, 120_000);
});
