import { createHash } from 'node:crypto';
import type { FastifyRequest } from 'fastify';
import { COOKIE } from '../auth/session.js';

/** Rate-limit key: the signed-in session when there is one (per user), otherwise the IP. */
export function userKey(req: FastifyRequest): string {
  const m = new RegExp(`(?:^|;\\s*)${COOKIE}=([0-9a-f]{64})`).exec(req.headers.cookie ?? '');
  return m ? `u:${createHash('sha256').update(m[1]!).digest('hex').slice(0, 16)}` : `ip:${req.ip}`;
}

export const mutationLimit = (max = 20) => ({ config: { rateLimit: { max, timeWindow: '1 minute', keyGenerator: userKey } } });
