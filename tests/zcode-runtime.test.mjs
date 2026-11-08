import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, writeFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { startRuntime, authorized } from '../src-tauri/scripts/zcode-runtime.mjs';

test('private relay rejects public/distribution credentials', () => {
  assert.equal(authorized('Bearer local-secret', 'local-secret'), true);
  for (const s of [undefined, '', 'local-secret', 'Bearer distribution-key']) assert.equal(authorized(s, 'local-secret'), false);
});

test('relay preserves native tools/SSE, isolates concurrent account auth, and rejects malformed input', async t => {
  const root = await mkdtemp(join(tmpdir(), 'cockpit-zcode-test-'));
  await mkdir(join(root, 'src/config'), { recursive: true });
  await writeFile(join(root, 'package.json'), '{"type":"module"}');
  await writeFile(join(root, 'src/config/loader.ts'), `export function loadConfig() { return {
    claim:{enabled:true,auto:true},async:{enabled:true},highspeed:{enabled:true},mcp:{gateway:{enabled:true}},clientSigning:{enabled:true}
  }; }`);
  const seen = [];
  const runtime = await startRuntime({ source: root, config: 'unused' }, async (req, format, options) => {
    const body = await req.json();
    const credential = await options.auth.getCredential();
    seen.push({ jwt: credential.jwt, format, body, provider: options.config.provider });
    assert.equal(options.config.plan, 'start-plan');
    assert.equal(options.config.claim.enabled, false);
    assert.equal(options.config.async.enabled, false);
    assert.equal(options.config.clientSigning.enabled, false);
    await new Promise(resolve => setTimeout(resolve, 10));
    assert.equal((await options.auth.getCredential()).jwt, credential.jwt);
    return new Response('event: content_block_delta\ndata: {"delta":{"type":"input_json_delta","partial_json":"{}"}}\n\n', {
      headers: { 'Content-Type': 'text/event-stream' },
    });
  });
  t.after(async () => { runtime.server.closeAllConnections(); await new Promise(resolve => runtime.server.close(resolve)); await rm(root, { recursive: true, force: true }); });
  const base = `http://127.0.0.1:${runtime.port}`;
  assert.equal((await fetch(base + '/health')).status, 401);
  assert.equal((await fetch(base + '/health', { headers: { Authorization: 'Bearer distribution-key' } })).status, 401);
  const headers = { Authorization: `Bearer ${runtime.key}`, 'Content-Type': 'application/json' };
  const health = await (await fetch(base + '/health', { headers })).json();
  assert.equal(health.service, 'agentcockpit-zcode');
  const body = { model: 'GLM-5.3-Flash', stream: true, system: 'client system', messages: [{role:'user',content:'hi'}],
    tools: [{name:'read',input_schema:{type:'object'}}] };
  const send = (credential, format = 'anthropic') => fetch(base + '/relay', { method: 'POST', headers,
    body: JSON.stringify({ credential, format, body }) });
  const responses = await Promise.all([
    send({provider:'zai',jwt:'one',user_id:'zcode:zai:one'}),
    send({provider:'bigmodel',jwt:'two',user_id:'zcode:bigmodel:two'}, 'openai'),
  ]);
  for (const r of responses) { assert.equal(r.status,200); assert.match(await r.text(),/input_json_delta/); }
  assert.deepEqual(new Set(seen.map(s=>s.jwt)), new Set(['one','two']));
  assert.deepEqual(seen[0].body.tools,body.tools);
  assert.equal(seen.find(s=>s.jwt==='two').provider,'bigmodel');
  assert.equal((await send({provider:'zai',jwt:'one',user_id:'zcode:bigmodel:one'})).status,400);
  assert.equal((await fetch(base+'/relay',{method:'POST',headers,body:'not-json'})).status,400);
  assert.equal((await fetch(base+'/anything',{headers})).status,404);
});
