import { scValToNative, type xdr } from '@stellar/stellar-sdk';

export type ChainEvent =
  | { kind: 'funded'; id: bigint; maintainer: string; repoId: bigint; issue: bigint; token: string; amount: bigint; feeBps: number; deadline: bigint; relayerAllowed: boolean }
  | { kind: 'released'; id: bigint; winner: string; net: bigint; fee: bigint; caller: string }
  | { kind: 'refunded'; id: bigint; amount: bigint }
  | { kind: 'disputed'; id: bigint; caller: string }
  | { kind: 'resolved'; id: bigint; winner: string | null; winnerNet: bigint; fee: bigint; maintainerAmount: bigint };

export interface RawEvent {
  id: string; // ordered, zero-padded: string comparison gives chain order
  ledger: number;
  txHash: string;
  topic: xdr.ScVal[];
  value: xdr.ScVal;
}

/** Returns null for events we do not track (config, paused, ...). Throws on malformed tracked events. */
export function decodeEvent(raw: Pick<RawEvent, 'topic' | 'value'>): ChainEvent | null {
  const topics = raw.topic.map((t) => scValToNative(t) as unknown);
  if (topics[0] !== 'bounty' || typeof topics[1] !== 'string') return null;
  const name = topics[1];
  if (!['funded', 'released', 'refunded', 'disputed', 'resolved'].includes(name)) return null;
  const id = topics[2];
  if (typeof id !== 'bigint') throw new Error(`event ${name}: missing bounty id topic`);
  const v = scValToNative(raw.value) as Record<string, unknown>;
  const big = (k: string) => {
    const x = v[k];
    if (typeof x !== 'bigint') throw new Error(`event ${name}: field ${k} is not an integer`);
    return x;
  };
  const str = (k: string) => {
    const x = v[k];
    if (typeof x !== 'string') throw new Error(`event ${name}: field ${k} is not a string`);
    return x;
  };
  switch (name) {
    case 'funded':
      return {
        kind: 'funded', id, maintainer: str('maintainer'), repoId: big('repo_id'), issue: big('issue'),
        token: str('token'), amount: big('amount'), feeBps: Number(v['fee_bps']), deadline: big('deadline'),
        relayerAllowed: v['relayer_allowed'] === true,
      };
    case 'released':
      return { kind: 'released', id, winner: str('winner'), net: big('net'), fee: big('fee'), caller: str('caller') };
    case 'refunded':
      return { kind: 'refunded', id, amount: big('amount') };
    case 'disputed':
      return { kind: 'disputed', id, caller: str('caller') };
    default:
      return {
        kind: 'resolved', id, winner: typeof v['winner'] === 'string' ? v['winner'] : null,
        winnerNet: big('winner_net'), fee: big('fee'), maintainerAmount: big('maintainer_amount'),
      };
  }
}
