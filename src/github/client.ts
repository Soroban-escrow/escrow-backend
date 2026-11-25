import { createSign } from 'node:crypto';
import type { GithubPort } from './commands.js';

export class GithubError extends Error {
  constructor(public status: number, message: string) { super(message); }
}

export interface GithubClientConfig {
  appId: string;
  /** PEM, with real newlines or literal \n */
  privateKey: string;
  apiUrl?: string;
  fetchImpl?: typeof fetch;
  now?: () => number;
}

export interface PullFile { filename: string; additions: number; deletions: number; patch?: string }

const REPO = /^[A-Za-z0-9_.-]+\/[A-Za-z0-9_.-]+$/;
const LOGIN = /^[A-Za-z0-9-]+(\[bot\])?$/;
const SHA = /^[0-9a-f]{7,64}$/i;
const positive = (n: number, what: string) => { if (!Number.isInteger(n) || n < 1) throw new Error(`invalid ${what}`); return n; };
const repoOk = (r: string) => { if (!REPO.test(r)) throw new Error('invalid repository name'); return r; };
const b64 = (o: object) => Buffer.from(JSON.stringify(o)).toString('base64url');

/**
 * GitHub App client: JWT -> installation token (cached, refreshed on expiry or a 401) -> REST calls.
 * Every path segment that comes from outside is validated before it is put in a URL.
 */
export function createGithubClient(cfg: GithubClientConfig) {
  const api = cfg.apiUrl ?? 'https://api.github.com';
  const doFetch = cfg.fetchImpl ?? fetch;
  const now = cfg.now ?? Date.now;
  const pem = cfg.privateKey.replace(/\\n/g, '\n');
  const tokens = new Map<string, { token: string; expiresAt: number }>();

  const appJwt = () => {
    const t = Math.floor(now() / 1000);
    const head = `${b64({ alg: 'RS256', typ: 'JWT' })}.${b64({ iat: t - 30, exp: t + 540, iss: cfg.appId })}`;
    return `${head}.${createSign('RSA-SHA256').update(head).sign(pem, 'base64url')}`;
  };
  const base = { accept: 'application/vnd.github+json', 'x-github-api-version': '2022-11-28', 'user-agent': 'skillescrow' };

  async function installationToken(id: bigint, force = false): Promise<string> {
    const key = id.toString();
    const hit = tokens.get(key);
    if (!force && hit && hit.expiresAt - 60_000 > now()) return hit.token;
    const r = await doFetch(`${api}/app/installations/${key}/access_tokens`, { method: 'POST', headers: { ...base, authorization: `Bearer ${appJwt()}` } });
    if (!r.ok) throw new GithubError(r.status, `installation token: ${r.status}`);
    const j = (await r.json()) as { token: string; expires_at: string };
    tokens.set(key, { token: j.token, expiresAt: Date.parse(j.expires_at) });
    return j.token;
  }

  async function call(installation: bigint, method: string, path: string, body?: unknown, retried = false): Promise<Response> {
    const token = await installationToken(installation, retried);
    const r = await doFetch(`${api}${path}`, {
      method, headers: { ...base, authorization: `Bearer ${token}`, ...(body ? { 'content-type': 'application/json' } : {}) },
      ...(body ? { body: JSON.stringify(body) } : {}),
    });
    if (r.status === 401 && !retried) return call(installation, method, path, body, true); // token revoked or expired early
    return r;
  }
  async function json<T>(installation: bigint, method: string, path: string, body?: unknown): Promise<T> {
    const r = await call(installation, method, path, body);
    if (!r.ok) throw new GithubError(r.status, `${method} ${path.split('?')[0]}: ${r.status}`);
    return (await r.json()) as T;
  }

  const client = {
    /** All installations of this App (authenticated as the App itself, not an installation). */
    async listInstallations(): Promise<{ id: number; account: { login: string; type: string }; suspended_at: string | null }[]> {
      const r = await doFetch(`${api}/app/installations?per_page=100`, { headers: { ...base, authorization: `Bearer ${appJwt()}` } });
      if (!r.ok) throw new GithubError(r.status, `list installations: ${r.status}`);
      return (await r.json()) as never;
    },

    async listRepositories(installation: bigint): Promise<{ id: number; full_name: string; private: boolean }[]> {
      const j = await json<{ repositories: { id: number; full_name: string; private: boolean }[] }>(installation, 'GET', '/installation/repositories?per_page=100');
      return j.repositories;
    },

    /** repo permission of a user: admin | maintain | write | triage | read | none */
    async getPermission(installation: bigint, repo: string, login: string): Promise<string> {
      if (!LOGIN.test(login)) throw new Error('invalid login');
      const r = await call(installation, 'GET', `/repos/${repoOk(repo)}/collaborators/${login}/permission`);
      if (r.status === 404) return 'none';
      if (!r.ok) throw new GithubError(r.status, `permission lookup: ${r.status}`);
      return ((await r.json()) as { permission: string }).permission;
    },

    async postComment(installation: bigint, repo: string, issue: number, body: string): Promise<bigint> {
      const j = await json<{ id: number }>(installation, 'POST', `/repos/${repoOk(repo)}/issues/${positive(issue, 'issue')}/comments`, { body });
      return BigInt(j.id);
    },
    async updateComment(installation: bigint, repo: string, commentId: bigint, body: string): Promise<void> {
      await json(installation, 'PATCH', `/repos/${repoOk(repo)}/issues/comments/${positive(Number(commentId), 'comment id')}`, { body });
    },

    async getIssue(installation: bigint, repo: string, issue: number) {
      return json<{ number: number; title: string; body: string | null; state: string; html_url: string; user: { login: string; id: number } }>(
        installation, 'GET', `/repos/${repoOk(repo)}/issues/${positive(issue, 'issue')}`);
    },

    /** Creates the label if needed, then sets it on the issue (replacing earlier "bounty:" labels). */
    async setBountyLabel(installation: bigint, repo: string, issue: number, label: string, color = 'c9f25d'): Promise<void> {
      repoOk(repo); positive(issue, 'issue');
      if (!/^bounty: [\w .]{1,40}$/.test(label)) throw new Error('invalid label');
      const existing = await json<{ name: string }[]>(installation, 'GET', `/repos/${repo}/issues/${issue}/labels?per_page=100`);
      for (const l of existing) {
        if (l.name.startsWith('bounty:') && l.name !== label) {
          await call(installation, 'DELETE', `/repos/${repo}/issues/${issue}/labels/${encodeURIComponent(l.name)}`);
        }
      }
      const made = await call(installation, 'POST', `/repos/${repo}/labels`, { name: label, color });
      if (!made.ok && made.status !== 422) throw new GithubError(made.status, `create label: ${made.status}`); // 422 = already exists
      await json(installation, 'POST', `/repos/${repo}/issues/${issue}/labels`, { labels: [label] });
    },

    /** Changed files with patches (paginated, capped at 300 files). */
    async getPullFiles(installation: bigint, repo: string, pr: number): Promise<PullFile[]> {
      repoOk(repo); positive(pr, 'pull request');
      const out: PullFile[] = [];
      for (let page = 1; page <= 3; page++) {
        const files = await json<PullFile[]>(installation, 'GET', `/repos/${repo}/pulls/${pr}/files?per_page=100&page=${page}`);
        out.push(...files);
        if (files.length < 100) break;
      }
      return out;
    },

    async getPull(installation: bigint, repo: string, pr: number) {
      return json<{ number: number; title: string; body: string | null; draft: boolean; state: string; merged: boolean; head: { sha: string }; user: { login: string; id: number; type: string }; created_at: string }>(
        installation, 'GET', `/repos/${repoOk(repo)}/pulls/${positive(pr, 'pull request')}`);
    },

    /** Overall CI result for a commit from its check runs. */
    async getCiConclusion(installation: bigint, repo: string, sha: string): Promise<'success' | 'failure' | 'pending' | 'none'> {
      if (!SHA.test(sha)) throw new Error('invalid sha');
      const j = await json<{ check_runs: { status: string; conclusion: string | null }[] }>(installation, 'GET', `/repos/${repoOk(repo)}/commits/${sha}/check-runs?per_page=100`);
      if (j.check_runs.length === 0) return 'none';
      if (j.check_runs.some((c) => c.status !== 'completed')) return 'pending';
      return j.check_runs.every((c) => c.conclusion === 'success' || c.conclusion === 'neutral' || c.conclusion === 'skipped') ? 'success' : 'failure';
    },

    async getReadmeExcerpt(installation: bigint, repo: string, max = 2000): Promise<string> {
      const r = await call(installation, 'GET', `/repos/${repoOk(repo)}/readme`);
      if (!r.ok) return '';
      const j = (await r.json()) as { content?: string; encoding?: string };
      return j.encoding === 'base64' && j.content ? Buffer.from(j.content, 'base64').toString('utf8').slice(0, max) : '';
    },
  };
  return client;
}

export type GithubClient = ReturnType<typeof createGithubClient>;
/** The commands handler only needs these two. */
export const asPort = (c: GithubClient): GithubPort => ({ getPermission: c.getPermission, postComment: c.postComment });
