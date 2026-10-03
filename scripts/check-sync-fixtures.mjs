import {readFileSync} from 'node:fs';
import {createHash} from 'node:crypto';
import assert from 'node:assert/strict';
const fixtures = JSON.parse(readFileSync(new URL('../tests/sync/fixtures/manifests.json', import.meta.url)));
function canonical(value) {
  if (Array.isArray(value)) return value.map(canonical);
  if (value && typeof value === 'object') return Object.fromEntries(Object.keys(value).sort().map(key => [key, canonical(value[key])]));
  return value;
}
for (const fixture of fixtures) {
  assert.equal(JSON.stringify(canonical(JSON.parse(fixture.canonical))), fixture.canonical);
  assert.equal(createHash('sha256').update(fixture.canonical).digest('hex'), fixture.sha256);
}
console.log(`Verified ${fixtures.length} shared manifest fixtures`);
