#!/usr/bin/env node
// ARM A - window-scan guard (syntactically LOCAL decision procedure).
// Convention: every .unwrap()/.expect() in non-test source carries a
// justification comment within the WINDOW lines above it.
// The decision procedure reads ONE FILE and a bounded window. Nothing else.
import { readFileSync, readdirSync, statSync } from 'node:fs';
import { join } from 'node:path';

const ROOT = process.argv[2] || 'crates';
const WINDOW = 24;
const JUSTIFY = /(SAFETY|unwrap|expect|PANIC)\s*:/i;

function walk(dir, out = []) {
  for (const e of readdirSync(dir)) {
    const p = join(dir, e);
    const st = statSync(p);
    if (st.isDirectory()) {
      if (e === 'target' || e === 'node_modules' || e === 'tests' || e === 'benches') continue;
      walk(p, out);
    } else if (e.endsWith('.rs')) out.push(p);
  }
  return out;
}

// Bounded, single-file test-region detection: lines inside a `#[cfg(test)]`
// module. Tracked by brace depth from the attribute. Still local.
function testLines(lines) {
  const inTest = new Set();
  for (let i = 0; i < lines.length; i++) {
    if (!/#\[cfg\(test\)\]/.test(lines[i])) continue;
    let depth = 0, started = false;
    for (let j = i; j < lines.length; j++) {
      for (const ch of lines[j]) {
        if (ch === '{') { depth++; started = true; }
        else if (ch === '}') depth--;
      }
      inTest.add(j);
      if (started && depth <= 0) break;
    }
  }
  return inTest;
}

const findings = [];
let sites = 0;
for (const file of walk(ROOT)) {
  const src = readFileSync(file, 'utf8');
  const lines = src.split('\n');
  const skip = testLines(lines);
  for (let i = 0; i < lines.length; i++) {
    if (skip.has(i)) continue;
    const line = lines[i];
    if (/^\s*\/\//.test(line)) continue;
    if (!/\.unwrap\(\)|\.expect\(/.test(line)) continue;
    sites++;
    let justified = false;
    for (let k = Math.max(0, i - WINDOW); k <= i; k++) {
      const t = lines[k].trim();
      if ((t.startsWith('//') || t.startsWith('*')) && JUSTIFY.test(t)) { justified = true; break; }
    }
    if (/\/\/.*$/.test(line) && JUSTIFY.test(line.replace(/^[^/]*/, ''))) justified = true;
    if (!justified) findings.push(`${file}:${i + 1}: unjustified ${line.trim().slice(0, 90)}`);
  }
}

console.log(`arm-A window-scan: ${sites} sites, ${findings.length} unjustified`);
for (const f of findings.slice(0, Number(process.env.SHOW || 0))) console.log('  ' + f);
process.exit(findings.length ? 1 : 0);
