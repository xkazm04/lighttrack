#!/usr/bin/env node
// ARM B - reachability guard (NON-LOCAL decision procedure).
// Convention: no .unwrap()/.expect() in any function reachable from an HTTP
// request handler. The decision procedure needs a call graph over the WHOLE
// tree: 592 files, 17 crates. Nothing about one file decides one site.
import { readFileSync, readdirSync, statSync } from 'node:fs';
import { join } from 'node:path';

const ROOT = process.argv[2] || 'crates';
const DEPTH_ONE = process.env.DEPTH_ONE === '1'; // negative control (lesson 10)

function walk(dir, out = []) {
  for (const e of readdirSync(dir)) {
    const p = join(dir, e);
    const st = statSync(p);
    if (st.isDirectory()) {
      if (e === 'target' || e === 'node_modules' || e === 'benches' || e === 'tests') continue;
      walk(p, out);
    } else if (e.endsWith('.rs') && !/^tests?(_[a-z0-9_]+)?[.]rs$/.test(e)) out.push(p);
  }
  return out;
}

const files = walk(ROOT);
const src = new Map();
for (const f of files) src.set(f, readFileSync(f, 'utf8'));

// ---- 1. fn definitions, with body spans by brace matching ----------------
// key: bare fn name -> [{file, name, start, end}]   (names are NOT unique
// across 17 crates; that ambiguity is the point of the measurement)
const defs = new Map();
const defList = [];
const FN = /(?:^|\s)(?:pub(?:\([^)]*\))?\s+)?(?:default\s+)?(?:const\s+)?(?:async\s+)?(?:unsafe\s+)?(?:extern\s+"[^"]*"\s+)?fn\s+([A-Za-z_][A-Za-z0-9_]*)/g;

function spanFrom(text, idx) {
  // find the first '{' after the signature at idx, then match braces,
  // skipping string/char literals and comments.
  let i = text.indexOf('{', idx);
  if (i < 0) return null;
  // bail if a ';' comes first (trait method declaration, no body)
  const semi = text.indexOf(';', idx);
  if (semi >= 0 && semi < i) return null;
  let depth = 0;
  for (let j = i; j < text.length; j++) {
    const c = text[j];
    if (c === '"') { j++; while (j < text.length && !(text[j] === '"' && text[j - 1] !== '\\')) j++; continue; }
    if (c === '/' && text[j + 1] === '/') { while (j < text.length && text[j] !== '\n') j++; continue; }
    if (c === '{') depth++;
    else if (c === '}') { depth--; if (depth === 0) return [i, j]; }
  }
  return null;
}

const cfgCache = new Map();
function cfgTestSpans(text) {
  if (cfgCache.has(text)) return cfgCache.get(text);
  const out = [];
  const re = /#\[cfg\(test\)\]/g;
  let m;
  while ((m = re.exec(text))) { const sp = spanFrom(text, m.index); if (sp) out.push(sp); }
  cfgCache.set(text, out);
  return out;
}

for (const [file, text] of src) {
  FN.lastIndex = 0;
  let m;
  while ((m = FN.exec(text))) {
    const sp = spanFrom(text, m.index + m[0].length);
    if (!sp) continue;
    const before = text.slice(Math.max(0, m.index - 4000), m.index);
    if (/#\[cfg\(test\)\]/.test(before) && cfgTestSpans(text).some(([a, b]) => m.index > a && m.index < b)) continue;
    const d = { file, name: m[1], start: sp[0], end: sp[1], id: defList.length };
    defList.push(d);
    if (!defs.has(m[1])) defs.set(m[1], []);
    defs.get(m[1]).push(d);
  }
}

// ---- 2. roots: handlers registered on an axum route ----------------------
const ROUTE = /\.route\s*\(\s*"[^"]*"\s*,\s*([\s\S]{0,240}?)\)\s*[\),\n]/g;
const HANDLER = /\b(?:get|post|put|patch|delete|head|options|any)\s*\(\s*(?:[A-Za-z_][A-Za-z0-9_]*::)*([A-Za-z_][A-Za-z0-9_]*)/g;
const rootNames = new Set();
const rootPaths = new Map();
let routeCount = 0;
for (const [, text] of src) {
  ROUTE.lastIndex = 0;
  let m;
  while ((m = ROUTE.exec(text))) {
    routeCount++;
    HANDLER.lastIndex = 0;
    let h;
    while ((h = HANDLER.exec(m[1]))) {
      const path = h[0].slice(h[0].indexOf('(') + 1).trim();
      rootNames.add(h[1]);
      rootPaths.set(h[1], path);
    }
  }
}

// ---- 3. edges: calls inside a body that name a known fn -----------------
const CALL = /(?:\.\s*)?\b([A-Za-z_][A-Za-z0-9_]*)\s*\(/g;
function calleesOf(d) {
  const body = src.get(d.file).slice(d.start, d.end);
  const out = new Set();
  CALL.lastIndex = 0;
  let m;
  while ((m = CALL.exec(body))) if (defs.has(m[1])) out.add(m[1]);
  return out;
}
const edgeCache = new Map();

// ---- 4. BFS from the roots ----------------------------------------------
const reachable = new Set();
let frontier = [];
for (const n of rootNames) {
  const cands = defs.get(n) || [];
  const path = rootPaths.get(n) || '';
  const mod = path.includes('::') ? path.split('::').slice(-2)[0] : null;
    const norm = (f) => f.split(String.fromCharCode(92)).join('/');
    const scoped = mod ? cands.filter(d => norm(d.file).includes('/' + mod + '.rs') || norm(d.file).includes('/' + mod + '/')) : cands;
  for (const d of (scoped.length ? scoped : cands)) frontier.push(d);
}
const rootsResolved = frontier.length;
for (const d of frontier) reachable.add(d.id);
let hops = 0;
while (frontier.length && !(DEPTH_ONE && hops >= 1)) {
  const next = [];
  for (const d of frontier) {
    let cs = edgeCache.get(d.id);
    if (!cs) { cs = calleesOf(d); edgeCache.set(d.id, cs); }
    for (const name of cs) for (const t of defs.get(name)) {
      if (!reachable.has(t.id)) { reachable.add(t.id); next.push(t); }
    }
  }
  frontier = next;
  hops++;
}

// ---- 5. unwraps inside reachable bodies ---------------------------------
const findings = [];
for (const d of defList) {
  if (!reachable.has(d.id)) continue;
  const body = src.get(d.file).slice(d.start, d.end);
  const base = src.get(d.file).slice(0, d.start).split('\n').length;
  body.split('\n').forEach((line, i) => {
    if (/^\s*\/\//.test(line)) return;
    if (/\.unwrap\(\)|\.expect\(/.test(line))
      findings.push(`${d.file}:${base + i}: [${d.name}] ${line.trim().slice(0, 80)}`);
  });
}

console.log(`arm-B reachability: routes=${routeCount} rootNames=${rootNames.size} rootsResolved=${rootsResolved} ` +
  `defs=${defList.length} reachableFns=${reachable.size} hops=${hops} findings=${findings.length}` +
  (DEPTH_ONE ? '  [NEGATIVE CONTROL: depth-1]' : ''));
if (process.env.LIST) for (const f of findings) console.log('  ' + f);
if (process.env.REACHQ) {
  for (const q of process.env.REACHQ.split(',')) {
    const ds = defs.get(q) || [];
    console.log(`  reach? ${q}: defs=${ds.length} reachable=${ds.filter(d => reachable.has(d.id)).length}`);
  }
}
process.exit(findings.length ? 1 : 0);
