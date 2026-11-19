import { describe, expect, it } from 'vitest';
import { createChain } from '../src/chain/client.js';

// Reads the real testnet deployment; set LIVE_CHAIN=1 to run.
describe.runIf(process.env.LIVE_CHAIN)('chain (testnet)', () => {
  const chain = createChain({
    rpcUrl: 'https://soroban-testnet.stellar.org',
    networkPassphrase: 'Test SDF Network ; September 2015',
    contractId: 'CCEKBPYCWKNTXIQLXJHVAF7RYGUZZ6MITA7VVJBOK5BHXUO44ELZ4F4V',
  });
  it('reads the released smoke bounty', async () => {
    const b = await chain.getBounty(1n);
    expect(b.amount).toBe(100000000n);
    expect(b.status.tag).toBe('Released');
    expect(await chain.activeBounty(1n, BigInt(b.issue))).toBeUndefined();
  });
});
