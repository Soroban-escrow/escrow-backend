import { createHash, randomBytes } from 'node:crypto';
import { Keypair, StrKey } from '@stellar/stellar-sdk';
import type { Db } from '../db.js';

export class WalletError extends Error {
  constructor(public code: string, message: string, public status = 400) { super(message); }
}

const TTL_MS = 5 * 60 * 1000;
const PREFIX = 'Stellar Signed Message:\n'; // SEP-53

export interface WalletDeps { db: Db; now?: () => Date }
const clock = (d: WalletDeps) => (d.now ?? (() => new Date()))();

/** The bytes Freighter signs under SEP-53: sha256(prefix + message). */
export function sep53Hash(message: string): Buffer {
  return createHash('sha256').update(Buffer.concat([Buffer.from(PREFIX, 'utf8'), Buffer.from(message, 'utf8')])).digest();
}

export async function createChallenge(d: WalletDeps, userId: string, address: string) {
  if (!StrKey.isValidEd25519PublicKey(address)) throw new WalletError('bad_address', 'invalid Stellar address');
  const user = await d.db.user.findUniqueOrThrow({ where: { id: userId } });
  const other = await d.db.user.findUnique({ where: { wallet: address } });
  if (other && other.id !== userId) throw new WalletError('address_taken', 'address already linked to another account', 409);
  const nonce = randomBytes(32).toString('hex');
  const expiresAt = new Date(clock(d).getTime() + TTL_MS);
  const message = `SkillEscrow wallet link\nGitHub: ${user.githubLogin} (${user.githubId})\nAddress: ${address}\nNonce: ${nonce}\nExpires: ${expiresAt.toISOString()}`;
  const c = await d.db.walletChallenge.create({ data: { userId, address, message, nonce, expiresAt } });
  return { challengeId: c.id, message };
}

/** Verifies the signature, burns the challenge (single use) and links the wallet. */
export async function verifyChallenge(d: WalletDeps, userId: string, challengeId: string, signatureB64: string) {
  const c = await d.db.walletChallenge.findUnique({ where: { id: challengeId } });
  if (!c || c.userId !== userId) throw new WalletError('challenge_not_found', 'challenge not found', 404);
  if (c.usedAt) throw new WalletError('challenge_used', 'challenge already used', 409);
  if (c.expiresAt.getTime() <= clock(d).getTime()) throw new WalletError('challenge_expired', 'challenge expired', 410);

  let ok = false;
  try {
    const sig = Buffer.from(signatureB64, 'base64');
    ok = sig.length === 64 && Keypair.fromPublicKey(c.address).verify(sep53Hash(c.message), sig);
  } catch { ok = false; }
  if (!ok) throw new WalletError('bad_signature', 'signature does not match', 400);

  try {
    await d.db.$transaction(async (tx) => {
      // atomic burn: of several simultaneous verifies exactly one wins
      const burned = await tx.walletChallenge.updateMany({ where: { id: c.id, usedAt: null }, data: { usedAt: clock(d) } });
      if (burned.count !== 1) throw new WalletError('challenge_used', 'challenge already used', 409);
      await tx.user.update({ where: { id: userId }, data: { wallet: c.address, walletLinkedAt: clock(d) } });
    });
  } catch (e) {
    if ((e as { code?: string }).code === 'P2002') throw new WalletError('address_taken', 'address already linked to another account', 409);
    throw e;
  }
  return { address: c.address };
}

export async function unlinkWallet(d: WalletDeps, userId: string) {
  await d.db.user.update({ where: { id: userId }, data: { wallet: null, walletLinkedAt: null } });
}
