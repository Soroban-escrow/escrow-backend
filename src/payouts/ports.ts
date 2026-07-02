export interface ChainBounty {
  status: 'Open' | 'Released' | 'Refunded' | 'Disputed' | 'Resolved';
  relayerAllowed: boolean;
  maintainer: string;
}

export interface ChainPort {
  /** always read from chain; never from the DB, for money decisions */
  getBounty(id: bigint): Promise<ChainBounty>;
}

export interface RelayerPort {
  /** signs and submits release(relayer, bountyId, winner); returns the tx hash */
  submitRelease(bountyId: bigint, winner: string): Promise<string>;
  /** waits for a submitted tx; resolves 'SUCCESS' or throws */
  waitForTx(hash: string): Promise<'SUCCESS' | 'FAILED'>;
}

export interface Notifier {
  /** best effort; failures must not break payouts */
  notify(bountyId: bigint, prNumber: number, message: string): Promise<void>;
}

/** Errors the contract reports deterministically; retrying cannot help. */
export class ContractRejection extends Error {}
