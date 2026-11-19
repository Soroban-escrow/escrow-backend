import { Client } from '@skillescrow/contract-client';
import { contract, Keypair, rpc } from '@stellar/stellar-sdk';
import { contractErrorName } from '../chain/errors.js';
import { ContractRejection, type RelayerPort } from './ports.js';

export interface RelayerConfig { rpcUrl: string; networkPassphrase: string; contractId: string; secret: string }

/**
 * Signs and submits release(relayer, bountyId, winner). The secret comes from the environment on
 * testnet only; mainnet must use a KMS-backed signer (see docs/RUNBOOK.md).
 */
export function createRelayer(cfg: RelayerConfig): RelayerPort & { address: string; balanceXlm(): Promise<number> } {
  const kp = Keypair.fromSecret(cfg.secret);
  const server = new rpc.Server(cfg.rpcUrl, { allowHttp: cfg.rpcUrl.startsWith('http://') });
  const signer = contract.basicNodeSigner(kp, cfg.networkPassphrase);
  const client = new Client({ contractId: cfg.contractId, networkPassphrase: cfg.networkPassphrase, rpcUrl: cfg.rpcUrl, publicKey: kp.publicKey(), ...signer });

  return {
    address: kp.publicKey(),

    async submitRelease(bountyId, winner) {
      const tx = await client.release({ caller: kp.publicKey(), bounty_id: bountyId, winner });
      const sim = tx.simulation;
      if (sim && rpc.Api.isSimulationError(sim)) {
        const name = contractErrorName(sim.error);
        if (name) throw new ContractRejection(name); // deterministic: retrying cannot help
        throw new Error(`simulation failed: ${sim.error}`);
      }
      await tx.sign();
      const signed = tx.signed;
      if (!signed) throw new Error('transaction was not signed');
      const hash = signed.hash().toString('hex');
      const r = await server.sendTransaction(signed);
      if (r.status === 'ERROR') throw new Error('submit failed');
      return hash; // caller stores this BEFORE waiting
    },

    async waitForTx(hash) {
      const start = Date.now();
      for (;;) {
        const r = await server.getTransaction(hash);
        if (r.status === 'SUCCESS') return 'SUCCESS';
        if (r.status === 'FAILED') return 'FAILED';
        if (Date.now() - start > 60_000) throw new Error(`timeout waiting for ${hash}`);
        await new Promise((res) => setTimeout(res, 1500));
      }
    },

    async balanceXlm() {
      const acct = await fetch(`${cfg.rpcUrl.includes('testnet') ? 'https://horizon-testnet.stellar.org' : ''}/accounts/${kp.publicKey()}`).then((x) => x.json() as Promise<{ balances?: { asset_type: string; balance: string }[] }>);
      return Number(acct.balances?.find((b) => b.asset_type === 'native')?.balance ?? 0);
    },
  };
}
