import { z } from 'zod';

const schema = z.object({
  NODE_ENV: z.enum(['development', 'test', 'production']).default('development'),
  LOG_LEVEL: z.string().default('info'),
  PORT: z.coerce.number().int().default(3001),
  DATABASE_URL: z.string().min(1),
  REDIS_URL: z.string().default('redis://localhost:6379'),
  STELLAR_NETWORK: z.enum(['testnet', 'mainnet', 'local']).default('testnet'),
  STELLAR_RPC_URL: z.string().url().default('https://soroban-testnet.stellar.org'),
  STELLAR_NETWORK_PASSPHRASE: z.string().default('Test SDF Network ; September 2015'),
  BOUNTY_CONTRACT_ID: z.string().regex(/^C[A-Z2-7]{55}$/),
  API_PUBLIC_URL: z.string().url().default('http://localhost:3001'),
  WEB_PUBLIC_URL: z.string().url().default('http://localhost:3000'),
  SESSION_SECRET: z.string().min(32),
  CSRF_SECRET: z.string().min(32),
  GITHUB_WEBHOOK_SECRET: z.string().min(8),
  // GitHub App (docs/GITHUB_APP.md). All optional until registered; all-or-none once any is set.
  GITHUB_APP_ID: z.string().regex(/^\d+$/).optional(),
  GITHUB_APP_SLUG: z.string().min(1).optional(),
  GITHUB_APP_PRIVATE_KEY: z.string().includes('PRIVATE KEY').optional(),
  GITHUB_CLIENT_ID: z.string().min(1).optional(),
  GITHUB_CLIENT_SECRET: z.string().min(1).optional(),
  ADMIN_GITHUB_IDS: z.string().regex(/^\d+(,\d+)*$/).optional(),
  RELAYER_SECRET: z.string().regex(/^S[A-Z2-7]{55}$/).optional(),
  RELAYER_MIN_XLM_ALERT: z.coerce.number().default(50),
  ALERT_WEBHOOK_URL: z.string().url().optional(),
  TOKEN_USDC_CONTRACT_ID: z.string().default(''),
  TOKEN_XLM_CONTRACT_ID: z.string().default(''),
}).refine((c) => {
  const set = [c.GITHUB_APP_ID, c.GITHUB_APP_SLUG, c.GITHUB_APP_PRIVATE_KEY, c.GITHUB_CLIENT_ID, c.GITHUB_CLIENT_SECRET].filter(Boolean).length;
  return set === 0 || set === 5;
}, { message: 'set all five GitHub App variables (APP_ID, APP_SLUG, APP_PRIVATE_KEY, CLIENT_ID, CLIENT_SECRET) or none', path: ['GITHUB_APP_ID'] }).refine((c) => !(c.STELLAR_NETWORK === 'mainnet' && c.RELAYER_SECRET), {
  message: 'RELAYER_SECRET must not be used on mainnet; use a KMS-backed signer',
  path: ['RELAYER_SECRET'],
});

export type Config = z.infer<typeof schema>;

/** Loads apps/api/.env into process.env when present (real environment variables win). */
export function loadDotEnv(path = new URL('../.env', import.meta.url)) {
  try { process.loadEnvFile(path); } catch { /* no .env file: fine */ }
}

/** The only place that reads process.env's shape. Exits with a clear message when invalid. */
export function loadConfig(env: Record<string, string | undefined> = process.env): Config {
  const r = schema.safeParse(env);
  if (!r.success) {
    const lines = r.error.issues.map((i) => `  ${i.path.join('.')}: ${i.message}`);
    throw new Error(`Invalid environment:\n${lines.join('\n')}`);
  }
  return r.data;
}
