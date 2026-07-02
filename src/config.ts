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
});

export type Config = z.infer<typeof schema>;

/** The only place that reads process.env's shape. Exits with a clear message when invalid. */
export function loadConfig(env: Record<string, string | undefined> = process.env): Config {
  const r = schema.safeParse(env);
  if (!r.success) {
    const lines = r.error.issues.map((i) => `  ${i.path.join('.')}: ${i.message}`);
    throw new Error(`Invalid environment:\n${lines.join('\n')}`);
  }
  return r.data;
}
