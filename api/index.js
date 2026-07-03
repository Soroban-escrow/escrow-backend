// Vercel function: serves requests with the app from the bundle produced by `pnpm build:vercel`.
import app from '../dist/server.mjs';

export default async function handler(req, res) {
  await app.ready();
  app.server.emit('request', req, res);
}
