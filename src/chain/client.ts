import { Client, type Bounty } from '@skillescrow/contract-client';
import { rpc, TransactionBuilder, Transaction } from '@stellar/stellar-sdk';

export interface ChainConfig {
  rpcUrl: string;
  networkPassphrase: string;
  contractId: string;
}

export function createChain(cfg: ChainConfig) {
  const server = new rpc.Server(cfg.rpcUrl, { allowHttp: cfg.rpcUrl.startsWith('http://') });
  const client = (publicKey?: string) =>
    new Client({ contractId: cfg.contractId, networkPassphrase: cfg.networkPassphrase, rpcUrl: cfg.rpcUrl, ...(publicKey ? { publicKey } : {}) });

  return {
    server,

    async getBounty(id: bigint): Promise<Bounty> {
      const tx = await client().get_bounty({ id });
      const r = tx.result;
      if (r.isErr()) throw new Error(`get_bounty failed: ${JSON.stringify(r.unwrapErr())}`);
      return r.unwrap();
    },

    async activeBounty(repoId: bigint, issue: bigint): Promise<bigint | undefined> {
      const tx = await client().active_bounty({ repo_id: repoId, issue });
      return tx.result ?? undefined;
    },

    /** Simulated + prepared fund_bounty, unsigned XDR ready for Freighter. */
    async buildFundTx(p: {
      maintainer: string; repoId: bigint; issue: bigint; token: string;
      amount: bigint; deadline: bigint; relayerAllowed: boolean;
    }): Promise<string> {
      const tx = await client(p.maintainer).fund_bounty({
        maintainer: p.maintainer, repo_id: p.repoId, issue: p.issue, token: p.token,
        amount: p.amount, deadline: p.deadline, relayer_allowed: p.relayerAllowed,
      });
      const sim = tx.simulation;
      if (sim && rpc.Api.isSimulationError(sim)) throw new Error(`simulation failed: ${sim.error}`);
      return tx.toXDR();
    },


    /** Unsigned release(maintainer, id, winner) for maintainers who did not opt in to the relayer. */
    async buildReleaseTx(maintainer: string, bountyId: bigint, winner: string): Promise<string> {
      const tx = await client(maintainer).release({ caller: maintainer, bounty_id: bountyId, winner });
      const sim = tx.simulation;
      if (sim && rpc.Api.isSimulationError(sim)) throw new Error(`simulation failed: ${sim.error}`);
      return tx.toXDR();
    },

    async buildRefundTx(maintainer: string, bountyId: bigint): Promise<string> {
      const tx = await client(maintainer).refund({ maintainer, bounty_id: bountyId });
      const sim = tx.simulation;
      if (sim && rpc.Api.isSimulationError(sim)) throw new Error(`simulation failed: ${sim.error}`);
      return tx.toXDR();
    },


    /** Arbiter-signed open_dispute; the arbiter key never touches the server. */
    async buildOpenDisputeTx(arbiter: string, bountyId: bigint): Promise<string> {
      const tx = await client(arbiter).open_dispute({ caller: arbiter, bounty_id: bountyId });
      const sim = tx.simulation;
      if (sim && rpc.Api.isSimulationError(sim)) throw new Error(`simulation failed: ${sim.error}`);
      return tx.toXDR();
    },

    async buildResolveTx(arbiter: string, bountyId: bigint, winner: string | undefined, winnerBps: number): Promise<string> {
      const tx = await client(arbiter).resolve({ arbiter, bounty_id: bountyId, winner, winner_bps: winnerBps });
      const sim = tx.simulation;
      if (sim && rpc.Api.isSimulationError(sim)) throw new Error(`simulation failed: ${sim.error}`);
      return tx.toXDR();
    },

    /** Relays a signed transaction. Only single-operation calls to our contract are accepted. */
    async submitSigned(xdr: string, expectedSource?: string): Promise<string> {
      const tx = TransactionBuilder.fromXDR(xdr, cfg.networkPassphrase);
      if (!(tx instanceof Transaction) || tx.operations.length !== 1) throw new Error('expected a single-operation transaction');
      const op = tx.operations[0]!;
      if (op.type !== 'invokeHostFunction') throw new Error('not a contract call');
      if (expectedSource && tx.source !== expectedSource) throw new Error('unexpected source account');
      const r = await server.sendTransaction(tx);
      if (r.status === 'ERROR') throw new Error(`submit failed: ${r.status}`);
      return r.hash;
    },

    async waitForTx(hash: string, timeoutMs = 60_000) {
      const start = Date.now();
      let delay = 500;
      for (;;) {
        const r = await server.getTransaction(hash);
        if (r.status !== 'NOT_FOUND') return r;
        if (Date.now() - start > timeoutMs) throw new Error(`timeout waiting for ${hash}`);
        await new Promise((res) => setTimeout(res, delay));
        delay = Math.min(delay * 1.5, 4000);
      }
    },
  };
}
