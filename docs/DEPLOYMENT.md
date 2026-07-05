# Deploying

The backend has to **stay awake**: the worker runs the indexer, the relayer loop and the job queue. Hosts that put free services to sleep when idle (Render's free web services, for example) will miss events and delay payouts, so the free options below are the ones that don't sleep. Free-tier terms change often; check each provider's current terms before you rely on one.

- [What you need](#what-you-need)
- [Free and always on: an Oracle Cloud VM + Vercel](#free-and-always-on-an-oracle-cloud-vm--vercel)
- [Any other server](#any-other-server)
- [Without Docker](#without-docker)
- [Pointing the pieces at each other](#pointing-the-pieces-at-each-other)
- [Updating](#updating)

## What you need

| Piece | Where it runs | Notes |
|---|---|---|
| This backend (`escrowd all`) | an always-on host | ~12 MB binary; modest CPU and RAM |
| PostgreSQL | same host (compose) or managed | the only stateful piece |
| The web app | Vercel (free Hobby tier works) | it only calls this API; it can sleep freely |
| A public HTTPS hostname for the API | DNS + Caddy | GitHub must reach `POST /github/webhooks`; browsers need https for `Secure` cookies |

Redis is **not** needed.

## Free and always on: an Oracle Cloud VM + Vercel

Oracle's "Always Free" tier includes ARM virtual machines that never sleep. It requires a card for identity verification and is not charged while you stay within the free resources.

**1. Create the VM.** Ubuntu 22.04/24.04, Ampere A1 (ARM), 1–2 OCPU and 6–12 GB RAM is plenty. Open ports **80 and 443** in the VCN security list (and `ufw allow 80,443/tcp` if the host firewall is on). Keep SSH restricted to your address.

**2. Get a hostname.** Any domain you own, or a free one (for example DuckDNS: `yourname.duckdns.org`). Point an **A record at the VM's public IP**.

**3. Install Docker.**
```bash
curl -fsSL https://get.docker.com | sudo sh
sudo usermod -aG docker $USER && newgrp docker
```

**4. Get the code and configure it.**
```bash
git clone https://github.com/Soroban-escrow/escrow-backend.git && cd escrow-backend
git checkout rust-backend
cp .env.example .env
```
Edit `.env`:
```bash
POSTGRES_PASSWORD=$(openssl rand -hex 24)     # compose uses this for the database
API_DOMAIN=api.yourname.duckdns.org
API_PUBLIC_URL=https://api.yourname.duckdns.org
WEB_PUBLIC_URL=https://your-app.vercel.app
SESSION_SECRET=…   CSRF_SECRET=…   GITHUB_WEBHOOK_SECRET=…       # openssl rand -hex 32 each
BOUNTY_CONTRACT_ID=C…                                           # from escrow-contracts/deployments/
TOKEN_XLM_CONTRACT_ID=…                                         # and TOKEN_USDC_CONTRACT_ID if you have one
INDEXER_START_LEDGER=…                                          # the contract's deployment ledger
GITHUB_APP_ID=… GITHUB_APP_SLUG=… GITHUB_APP_PRIVATE_KEY=… GITHUB_CLIENT_ID=… GITHUB_CLIENT_SECRET=…
ADMIN_GITHUB_IDS=…
RELAYER_SECRET=…                                                # testnet only
```
`DATABASE_URL` is set by compose for the `app` service; you don't need it in `.env`.

**5. Start it.**
```bash
docker compose up -d --build
docker compose logs -f app            # "API listening", "worker running", "relayer on"
docker compose exec app escrowd doctor
```
Caddy obtains the TLS certificate automatically on first request to `https://$API_DOMAIN` (ports 80/443 must be reachable and the DNS record must resolve to the VM).

**6. Tell GitHub where the API is.** In the GitHub App's settings: webhook URL `https://$API_DOMAIN/github/webhooks`, the same webhook secret, and the callback URL `https://$API_DOMAIN/auth/callback`. Use GitHub's "Recent deliveries" page to redeliver and watch the worker pick them up.

**7. Deploy the web app to Vercel** (from the `escrow-app` repo, root `apps/web`). Set `API_INTERNAL_URL=https://$API_DOMAIN` **at build time** (Next bakes its rewrites at build), plus the `NEXT_PUBLIC_*` variables from that repo's README.

**8. Keep it healthy for free.** Point a free uptime monitor (UptimeRobot, Better Stack) at `https://$API_DOMAIN/health`: it returns `503` when the database or RPC is down or the indexer is behind. Set `ALERT_WEBHOOK_URL` to a Slack/Discord webhook for the worker's own alerts. Schedule database backups (see the runbook).

## Any other server

The same `docker compose` flow works on any Linux host with Docker (a small VPS from any provider). Hosted platforms that run a container and a managed Postgres also work (Fly.io, Railway, a managed Kubernetes): run **one** instance of `escrowd all`, or one `escrowd serve` plus exactly one `escrowd worker`; give it `DATABASE_URL`; expose `PORT`; put it behind HTTPS and set `TRUST_PROXY=1`. They are cheap but not free, and are the right choice when you outgrow one VM.

If you scale the API (`escrowd serve` ×N) that is fine: sessions, deliveries and jobs live in Postgres. Run **one** worker that holds `RELAYER_SECRET` (the relayer assumes a single signing account); additional workers without the secret can still process the job queue.

## Without Docker

```bash
cargo build --release --locked -p escrowd           # ~12 MB binary at target/release/escrowd
sudo install target/release/escrowd /usr/local/bin/
```
A minimal systemd unit:
```ini
# /etc/systemd/system/escrowd.service
[Unit]
Description=SkillEscrow backend
After=network-online.target postgresql.service
Wants=network-online.target

[Service]
User=escrow
EnvironmentFile=/etc/escrowd/env       # chmod 600; the same variables as .env
ExecStart=/usr/local/bin/escrowd all
Restart=always
RestartSec=3
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
PrivateTmp=true

[Install]
WantedBy=multi-user.target
```
Put Caddy or nginx in front for HTTPS and set `TRUST_PROXY=1`.

## Pointing the pieces at each other

```
GitHub App  ── webhook ──▶  https://API/github/webhooks
GitHub App  ── OAuth cb ─▶  https://API/auth/callback        (= API_PUBLIC_URL + /auth/callback)
Web app     ── API_INTERNAL_URL ─▶ https://API               (build time)
API         ── CORS allows ──▶ WEB_PUBLIC_URL only
Bot comments link to ─────▶ WEB_PUBLIC_URL/fund/<id>, /me, /b/<id>
```

## Updating

```bash
git pull && docker compose up -d --build        # migrations apply on start (AUTO_MIGRATE)
docker compose exec app escrowd doctor
```
Migrations are forward-only and additive. Take a backup first when a release adds one (the changelog in `docs/PROGRESS.md` says so).
