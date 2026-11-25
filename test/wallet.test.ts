import { Keypair } from '@stellar/stellar-sdk';
import { afterAll, beforeEach, describe, expect, it } from 'vitest';
import { sep53Hash, createChallenge, unlinkWallet, verifyChallenge, WalletError } from '../src/auth/wallet.js';
import { createDb } from '../src/db.js';

const db = createDb(process.env.TEST_DATABASE_URL ?? 'postgresql://skillescrow:skillescrow@localhost:5432/skillescrow_test');
afterAll(() => db.$disconnect());

let t = new Date('2026-11-02T10:00:00Z');
const d = () => ({ db, now: () => t });
const sign = (kp: Keypair, msg: string) => kp.sign(sep53Hash(msg)).toString('base64');
const code = async (p: Promise<unknown>) => p.then(() => 'ok', (e: WalletError) => e.code);

let u1: string, u2: string;
beforeEach(async () => {
  t = new Date('2026-11-02T10:00:00Z');
  await db.$executeRawUnsafe('TRUNCATE "WalletChallenge","Session","User" CASCADE');
  u1 = (await db.user.create({ data: { githubId: 1n, githubLogin: 'alice' } })).id;
  u2 = (await db.user.create({ data: { githubId: 2n, githubLogin: 'bob' } })).id;
});

describe('wallet linking', () => {
  it('links with a valid SEP-53 signature', async () => {
    const kp = Keypair.random();
    const c = await createChallenge(d(), u1, kp.publicKey());
    expect(c.message).toContain('GitHub: alice (1)');
    await verifyChallenge(d(), u1, c.challengeId, sign(kp, c.message));
    expect((await db.user.findUniqueOrThrow({ where: { id: u1 } })).wallet).toBe(kp.publicKey());
  });
  it('rejects expired, reused, and foreign challenges', async () => {
    const kp = Keypair.random();
    const c = await createChallenge(d(), u1, kp.publicKey());
    const sig = sign(kp, c.message);
    expect(await code(verifyChallenge(d(), u2, c.challengeId, sig))).toBe('challenge_not_found');
    t = new Date(t.getTime() + 6 * 60_000);
    expect(await code(verifyChallenge(d(), u1, c.challengeId, sig))).toBe('challenge_expired');
    t = new Date('2026-11-02T10:00:00Z');
    expect(await code(verifyChallenge(d(), u1, c.challengeId, sig))).toBe('ok');
    expect(await code(verifyChallenge(d(), u1, c.challengeId, sig))).toBe('challenge_used');
  });
  it('rejects wrong keys, altered text, missing prefix and bad encodings; challenge stays usable', async () => {
    const kp = Keypair.random();
    const c = await createChallenge(d(), u1, kp.publicKey());
    const bad = [
      sign(Keypair.random(), c.message),
      sign(kp, c.message + 'x'),
      kp.sign(Buffer.from(c.message)).toString('base64'),
      'not-base64!!', '', Buffer.alloc(10).toString('base64'),
    ];
    for (const s of bad) expect(await code(verifyChallenge(d(), u1, c.challengeId, s))).toBe('bad_signature');
    expect(await code(verifyChallenge(d(), u1, c.challengeId, sign(kp, c.message)))).toBe('ok');
  });
  it('one address belongs to one user', async () => {
    const kp = Keypair.random();
    const c = await createChallenge(d(), u1, kp.publicKey());
    await verifyChallenge(d(), u1, c.challengeId, sign(kp, c.message));
    expect(await code(createChallenge(d(), u2, kp.publicKey()))).toBe('address_taken');
  });
  it('two users racing for one address: exactly one wins', async () => {
    const kp = Keypair.random();
    const c1 = await createChallenge(d(), u1, kp.publicKey());
    const c2 = await createChallenge(d(), u2, kp.publicKey());
    const r = await Promise.all([
      code(verifyChallenge(d(), u1, c1.challengeId, sign(kp, c1.message))),
      code(verifyChallenge(d(), u2, c2.challengeId, sign(kp, c2.message))),
    ]);
    expect(r.filter((x) => x === 'ok')).toHaveLength(1);
    expect(r).toContain('address_taken');
  });
  it('five simultaneous verifies of one challenge: exactly one succeeds', async () => {
    const kp = Keypair.random();
    const c = await createChallenge(d(), u1, kp.publicKey());
    const sig = sign(kp, c.message);
    const r = await Promise.all(Array.from({ length: 5 }, () => code(verifyChallenge(d(), u1, c.challengeId, sig))));
    expect(r.filter((x) => x === 'ok')).toHaveLength(1);
  });
  it('validates addresses and unlinks', async () => {
    expect(await code(createChallenge(d(), u1, 'GNOPE'))).toBe('bad_address');
    const kp = Keypair.random();
    const c = await createChallenge(d(), u1, kp.publicKey());
    await verifyChallenge(d(), u1, c.challengeId, sign(kp, c.message));
    await unlinkWallet(d(), u1);
    expect((await db.user.findUniqueOrThrow({ where: { id: u1 } })).wallet).toBeNull();
  });
});
