#!/usr/bin/env node
// End-to-end test: drives the REAL claude-context MCP server over stdio.
//   create project (caller) → index_codebase → poll status → search_code → assert → clear_index
// Pure Node, no deps. Usage: node test/e2e.mjs <projectPath> <query> <expectedSubstring>
import { spawn } from 'node:child_process';

const [projectPath, query, expected] = process.argv.slice(2);
if (!projectPath || !query) {
  console.error('usage: node test/e2e.mjs <projectPath> <query> [expectedSubstring]');
  process.exit(2);
}

const INDEX_TIMEOUT_MS = 180_000;
const CALL_TIMEOUT_MS = 60_000;

// Spawn the MCP server; it inherits embedding/Milvus config from process.env.
const srv = spawn('npx', ['-y', '@zilliz/claude-context-mcp@latest'], {
  stdio: ['pipe', 'pipe', 'inherit'], env: process.env,
});

let buf = '';
const pending = new Map();
let nextId = 1;

srv.stdout.on('data', (chunk) => {
  buf += chunk.toString();
  let nl;
  while ((nl = buf.indexOf('\n')) >= 0) {
    const line = buf.slice(0, nl).trim();
    buf = buf.slice(nl + 1);
    if (!line) continue;
    let msg;
    try { msg = JSON.parse(line); } catch { continue; } // ignore non-JSON log lines
    if (msg.id && pending.has(msg.id)) {
      const { resolve, reject } = pending.get(msg.id);
      pending.delete(msg.id);
      msg.error ? reject(new Error(JSON.stringify(msg.error))) : resolve(msg.result);
    }
  }
});
srv.on('exit', (code) => {
  for (const { reject } of pending.values()) reject(new Error(`server exited (${code})`));
});

function rpc(method, params, timeout = CALL_TIMEOUT_MS) {
  const id = nextId++;
  srv.stdin.write(JSON.stringify({ jsonrpc: '2.0', id, method, params }) + '\n');
  return new Promise((resolve, reject) => {
    pending.set(id, { resolve, reject });
    setTimeout(() => { if (pending.has(id)) { pending.delete(id); reject(new Error(`timeout: ${method}`)); } }, timeout);
  });
}
function notify(method, params) {
  srv.stdin.write(JSON.stringify({ jsonrpc: '2.0', method, params }) + '\n');
}
const textOf = (r) => (r?.content || []).map((c) => c.text || '').join('\n');
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

let failed = false;
try {
  await rpc('initialize', {
    protocolVersion: '2024-11-05', capabilities: {},
    clientInfo: { name: 'uals-e2e', version: '1.0.0' },
  });
  notify('notifications/initialized', {});
  console.log('✓ MCP handshake');

  // 1. index
  await rpc('tools/call', { name: 'index_codebase', arguments: { path: projectPath, force: true } });
  console.log('▸ indexing started…');

  // 2. poll status
  const deadline = Date.now() + INDEX_TIMEOUT_MS;
  let done = false;
  while (Date.now() < deadline) {
    const s = textOf(await rpc('tools/call', { name: 'get_indexing_status', arguments: { path: projectPath } }));
    if (/completed|fully indexed|✅/i.test(s)) { done = true; break; }
    if (/fail|error/i.test(s)) throw new Error('indexing reported failure: ' + s);
    await sleep(3000);
  }
  if (!done) throw new Error('indexing did not complete in time');
  console.log('✓ indexing completed');

  // 3. search + assert
  const res = textOf(await rpc('tools/call', { name: 'search_code', arguments: { path: projectPath, query, limit: 5 } }));
  const empty = /no results found/i.test(res) || res.trim() === '';
  if (empty) throw new Error(`search returned no results for "${query}"`);
  console.log(`✓ search "${query}" returned results`);
  if (expected && !res.includes(expected)) {
    console.error(`✗ expected hit to mention "${expected}". Got:\n${res.slice(0, 600)}`);
    failed = true;
  } else if (expected) {
    console.log(`✓ top results mention "${expected}"`);
  }
} catch (e) {
  console.error('✗ e2e failed:', e.message);
  failed = true;
} finally {
  try { await rpc('tools/call', { name: 'clear_index', arguments: { path: projectPath } }); console.log('✓ cleaned up index'); } catch {}
  srv.kill();
}
console.log(failed ? '✗ E2E FAILED' : '✓ E2E PASSED');
process.exit(failed ? 1 : 0);
