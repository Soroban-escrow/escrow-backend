import { createHmac, timingSafeEqual } from 'node:crypto';

/** Verifies X-Hub-Signature-256 over the raw body in constant time. */
export function verifySignature(secret: string, rawBody: Buffer, header: string | undefined): boolean {
  if (!header || !header.startsWith('sha256=')) return false;
  const given = Buffer.from(header.slice(7), 'hex');
  const expected = createHmac('sha256', secret).update(rawBody).digest();
  return given.length === expected.length && timingSafeEqual(given, expected);
}
