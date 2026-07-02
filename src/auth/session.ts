import { createHash, createHmac, randomBytes, timingSafeEqual } from 'node:crypto';
import type { Db } from '../db.js';

export const COOKIE = 'se_session';
const DAYS = 30;
const hash = (s: string) => createHash('sha256').update(s).digest('hex');

/** Session ids are stored hashed: a leaked table is not a set of logins. */
export async function createSession(db: Db, userId: string, now = new Date()) {
  const id = randomBytes(32).toString('hex');
  await db.session.create({ data: { id: hash(id), userId, expiresAt: new Date(now.getTime() + DAYS * 864e5) } });
  return { id, maxAgeSeconds: DAYS * 86400 };
}

export async function getSessionUser(db: Db, id: string | undefined, now = new Date()) {
  if (!id || !/^[0-9a-f]{64}$/.test(id)) return null;
  const s = await db.session.findUnique({ where: { id: hash(id) }, include: { user: true } });
  if (!s || s.expiresAt <= now) return null;
  return s.user;
}

export const destroySession = (db: Db, id: string) => db.session.deleteMany({ where: { id: hash(id) } });

export const csrfToken = (secret: string, sessionId: string) => createHmac('sha256', secret).update(sessionId).digest('hex');
export function csrfValid(secret: string, sessionId: string, given: unknown): boolean {
  if (typeof given !== 'string') return false;
  const a = Buffer.from(given), b = Buffer.from(csrfToken(secret, sessionId));
  return a.length === b.length && timingSafeEqual(a, b);
}
