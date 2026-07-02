import { describe, expect, it } from 'vitest';
import { createChain } from '../src/chain/client.js';
import { contractErrorName } from '../src/chain/errors.js';

// Simulates real transactions against the deployed testnet contract. LIVE_CHAIN=1 LIVE_MAINTAINER=G... LIVE_TOKEN=C...
describe.runIf(process.env.LIVE_CHAIN && process.env.LIVE_MAINTAINER && process.env.LIVE_TOKEN)('transaction builders (testnet)', () => {
  const chain = createChain({ rpcUrl: 'https://soroban-testnet.stellar.org', networkPassphrase: 'Test SDF Network ; September 2015', contractId: 'CCEKBPYCWKNTXIQLXJHVAF7RYGUZZ6MITA7VVJBOK5BHXUO44ELZ4F4V' });
  const maintainer = process.env.LIVE_MAINTAINER!;
  it('builds a prepared fund_bounty XDR', async () => {
    const xdr = await chain.buildFundTx({ maintainer, repoId: 777n, issue: BigInt(Date.now()), token: process.env.LIVE_TOKEN!, amount: 20_000_000n, deadline: BigInt(Math.floor(Date.now() / 1000) + 3 * 86400), relayerAllowed: true });
    expect(xdr.length).toBeGreaterThan(100);
  });
  it('refuses to build when the contract would reject (amount over max)', async () => {
    await expect(chain.buildFundTx({ maintainer, repoId: 777n, issue: 1n, token: process.env.LIVE_TOKEN!, amount: 9_000_000_000n, deadline: BigInt(Math.floor(Date.now() / 1000) + 3 * 86400), relayerAllowed: true }))
      .rejects.toThrow(/simulation failed/);
  });
  it('maps the real rejection to its contract error name', async () => {
    const e = await chain.buildRefundTx(maintainer, 2n).catch((x: Error) => x); // bounty 2 is already resolved
    expect(e).toBeInstanceOf(Error);
    expect(contractErrorName((e as Error).message)).toBe('InvalidStatus');
  });
});
