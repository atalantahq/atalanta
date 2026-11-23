import fs from 'node:fs';
const f = '.astro/doc-warnings.json';
if (fs.existsSync(f)) {
  const w = JSON.parse(fs.readFileSync(f, 'utf8'));
  if (w.length) {
    console.warn(`\n[docs] ${w.length} anchor warning(s): links to a #anchor that has no matching heading id`);
    for (const x of w) console.warn('  - ' + x);
  } else console.log('\n[docs] all anchors resolve');
}
