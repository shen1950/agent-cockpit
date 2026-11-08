// The separately installed, pinned zcode-api dependency retains its own license.
// Only this small integration layer belongs to AgentCockpit.
import './zcode-loader.mjs';
import { createServer } from 'node:http';
import { randomBytes, timingSafeEqual } from 'node:crypto';
import { createInterface } from 'node:readline';
import { pathToFileURL } from 'node:url';
import { join } from 'node:path';
import { Readable } from 'node:stream';

export const REVISION = '37155a5f75f3f2d7bde1b83054c8d6eb2e23a935';
const MAX_BODY = 32 * 1024 * 1024;
export function authorized(actual, secret) {
  const a = Buffer.from(actual || ''), b = Buffer.from(`Bearer ${secret}`);
  return a.length === b.length && timingSafeEqual(a, b);
}
export function validateEnvelope(input) {
  if (!['zai', 'bigmodel'].includes(input?.credential?.provider) || !input.credential.jwt
      || input.credential.user_id?.split(':')[1] !== input.credential.provider) throw new Error('Invalid account');
  if (!['openai', 'anthropic'].includes(input.format) || !input.body || typeof input.body !== 'object') throw new Error('Invalid request');
  if (typeof input.body.model !== 'string' || !input.body.model.trim()) throw new Error('Missing model');
  return input;
}

export async function startRuntime(init, injectedProxy) {
  const source = (file) => pathToFileURL(join(init.source, 'src', file)).href;
  const { proxyRequest } = injectedProxy ? { proxyRequest: injectedProxy } : await import(source('proxy/handler.ts'));
  const { loadConfig } = await import(source('config/loader.ts'));
  const config = loadConfig(init.config);
  // Disable non-model services even if inherited environment overrides YAML.
  config.plan = 'start-plan'; config.claim.enabled = config.claim.auto = false;
  config.async.enabled = config.highspeed.enabled = config.mcp.gateway.enabled = false;
  config.clientSigning.enabled = false;
  const secret = randomBytes(32).toString('hex');
  const server = createServer(async (req, res) => {
    if (!authorized(req.headers.authorization, secret)) { res.writeHead(401).end(); return; }
    if (req.method === 'GET' && req.url === '/health') {
      res.setHeader('Content-Type', 'application/json');
      res.end(JSON.stringify({ revision: REVISION, service: 'agentcockpit-zcode' })); return;
    }
    if (req.method !== 'POST' || req.url !== '/relay') { res.writeHead(404).end(); return; }
    const abort = new AbortController();
    req.on('aborted', () => abort.abort());
    res.on('close', () => { if (!res.writableEnded) abort.abort(); });
    try {
      const chunks = []; let size = 0;
      for await (const chunk of req) {
        size += chunk.length;
        if (size > MAX_BODY) { res.writeHead(413).end(); return; }
        chunks.push(chunk);
      }
      let envelope;
      try { envelope = validateEnvelope(JSON.parse(Buffer.concat(chunks).toString('utf8'))); }
      catch { res.writeHead(400).end('{"error":{"message":"Invalid relay request"}}'); return; }
      const { credential, body, format } = envelope;
      // Each request captures its own account; concurrent accounts never share auth.
      const auth = { getCredential: async () => ({ apiKey: '', jwt: credential.jwt, provider: credential.provider }) };
      const client = new Request('http://localhost/v1/' + (format === 'anthropic' ? 'messages' : 'chat/completions'), {
        method: 'POST', headers: { 'Content-Type': 'application/json', 'Accept-Encoding': 'identity' },
        body: JSON.stringify(body), signal: abort.signal,
      });
      const response = await proxyRequest(client, format, { config: { ...config, provider: credential.provider }, auth,
        highspeed: null, clientSigning: null, debug: false });
      res.writeHead(response.status, {
        'Content-Type': response.headers.get('content-type') || 'application/json',
        'Cache-Control': 'no-cache',
      });
      if (response.body) Readable.fromWeb(response.body).on('error', () => res.destroy()).pipe(res);
      else res.end();
    } catch {
      // Never expose credential-bearing request objects or upstream stack traces.
      if (!res.headersSent) res.writeHead(502).end('{"error":{"message":"ZCode component request failed"}}');
      else res.destroy();
    }
  });
  await new Promise((resolve, reject) => { server.once('error', reject); server.listen(0, '127.0.0.1', resolve); });
  return { server, port: server.address().port, key: secret, revision: REVISION };
}

async function main() {
  const input = createInterface({ input: process.stdin });
  input.on('close', () => process.exit(0));
  const init = await new Promise((resolve, reject) => {
    input.once('line', line => { try { resolve(JSON.parse(line)); } catch { reject(new Error('Invalid init')); } });
    input.once('close', () => reject(new Error('Parent closed')));
  });
  // Third-party diagnostics must never corrupt the startup pipe or log bodies.
  console.log = console.error = console.warn = () => {};
  const runtime = await startRuntime(init);
  process.stdout.write(JSON.stringify({ port: runtime.port, key: runtime.key, revision: runtime.revision }) + '\n');
  // Keeping stdin open is the lease. A crashed/killed cockpit closes it as well.
  process.on('SIGTERM', () => process.exit(0));
}
if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  main().catch(() => { process.stderr.write('ZCode component startup failed\n'); process.exit(1); });
}
