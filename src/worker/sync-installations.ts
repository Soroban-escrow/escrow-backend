import { createGithubClient } from '../github/client.js';
import { syncInstallations } from '../github/backfill.js';
import { loadConfig, loadDotEnv } from '../config.js';
import { createDb } from '../db.js';

loadDotEnv();
const cfg = loadConfig();
if (!cfg.GITHUB_APP_ID || !cfg.GITHUB_APP_PRIVATE_KEY) throw new Error('GitHub App is not configured');
const db = createDb(cfg.DATABASE_URL);
const r = await syncInstallations(db, createGithubClient({ appId: cfg.GITHUB_APP_ID, privateKey: cfg.GITHUB_APP_PRIVATE_KEY }));
console.log(`synced ${r.installations} installation(s), ${r.repos} repo(s)`);
await db.$disconnect();
