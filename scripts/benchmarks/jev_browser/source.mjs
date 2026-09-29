import { readFileSync, copyFileSync } from 'node:fs';
import { execFileSync } from 'node:child_process';
import { createRequire } from 'node:module';
import { resolve, dirname } from 'node:path';
import { pathToFileURL, fileURLToPath } from 'node:url';
import { createHash } from 'node:crypto';

export const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '../../..');
export const CHECKOUT = resolve(ROOT, '../benchmark/sources/jev-browser');
export const REVISION = '990005a1b9c21258682d6da8cced4c7a9a5bc2da';
const normalize = text => text.replace(/\r\n/g, '\n').trim();
const git = (checkout, args) => execFileSync('git', args, { cwd: checkout, encoding: 'utf8', windowsHide: true });

export function verifySource(checkout = CHECKOUT) {
  if (git(checkout, ['rev-parse', 'HEAD']).trim() !== REVISION) throw new Error('Upstream revision mismatch');
  for (const [patch, paths] of [
    ['jev-browser-model-hooks.patch', ['src/decide.ts', 'src/run.ts']],
    ['jev-browser-windows-tests.patch', ['tests/browser.test.ts']],
  ]) {
    const expected = readFileSync(resolve(ROOT, 'configs/research', patch), 'utf8');
    const actual = git(checkout, ['diff', 'HEAD', '--no-ext-diff', '--unified=2', '--', ...paths]);
    if (process.platform !== 'win32' && paths[0].startsWith('tests/') && !actual.trim()) continue;
    if (normalize(actual) !== normalize(expected)) throw new Error(`Upstream patch mismatch: ${patch}`);
  }
  const changed = git(checkout, ['diff', 'HEAD', '--name-only']).trim().split(/\r?\n/).filter(Boolean);
  if (changed.some(p => !['src/decide.ts', 'src/run.ts', 'tests/browser.test.ts'].includes(p)))
    throw new Error('Unregistered upstream changes');
  const config = JSON.parse(readFileSync(resolve(ROOT, 'configs/research/jev-browser-preparation.json'), 'utf8'));
  const lock = readFileSync(resolve(checkout, 'pnpm-lock.yaml'), 'utf8').replace(/\r\n/g, '\n');
  if (createHash('sha256').update(lock).digest('hex') !== config.upstream.lockfile_sha256)
    throw new Error('Lockfile mismatch');
  return config;
}

export async function loadSource(checkout = CHECKOUT, { build = true } = {}) {
  const config = verifySource(checkout);
  const require = createRequire(resolve(checkout, 'package.json'));
  if (build) {
    execFileSync(process.execPath, [require.resolve('typescript/bin/tsc'), '-p', 'tsconfig.build.json'],
      { cwd: checkout, windowsHide: true, stdio: 'pipe' });
    copyFileSync(resolve(checkout, 'src/snapshot-dom.js'), resolve(checkout, 'dist/snapshot-dom.js'));
  }
  const moduleAt = path => import(pathToFileURL(resolve(checkout, path)).href);
  const ts = require('typescript');
  const source = ts.createSourceFile('journeys.ts', readFileSync(resolve(checkout, 'examples/journeys.ts'), 'utf8'),
    ts.ScriptTarget.Latest, true);
  const declaration = source.statements.find(s => ts.isVariableStatement(s)
    && s.declarationList.declarations.some(d => d.name.getText(source) === 'JOURNEYS'));
  if (!declaration) throw new Error('Missing upstream journeys');
  // Compile only the pinned task declarations; importing the example would start live models.
  const exported = ts.factory.updateVariableStatement(declaration,
    [ts.factory.createModifier(ts.SyntaxKind.ExportKeyword)], declaration.declarationList);
  const isolated = ts.factory.updateSourceFile(source, [exported]);
  const javascript = ts.transpileModule(ts.createPrinter().printFile(isolated),
    { compilerOptions: { target: ts.ScriptTarget.ES2022, module: ts.ModuleKind.ESNext } }).outputText;
  const { JOURNEYS: journeys } = await import('data:text/javascript;base64,' + Buffer.from(javascript).toString('base64'));
  if (JSON.stringify(journeys.map(j => j.name)) !== JSON.stringify(config.task_set.ids))
    throw new Error('Task manifest differs from upstream');
  return { ...(await moduleAt('dist/index.js')), ...(await moduleAt('dist/decide.js')),
    chromium: require('playwright').chromium, fixtures: await moduleAt('tests/fixtures/serve.ts'),
    journeys, config, revision: REVISION };
}
