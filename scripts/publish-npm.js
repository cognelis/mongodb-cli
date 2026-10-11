'use strict';

const path = require('node:path');
const {spawnSync} = require('node:child_process');
const directory = path.resolve(__dirname, '../build/npm');
const pkg = require(path.join(directory, 'package.json'));

function main() {
  if (!process.env.npm_execpath) throw new Error('Run npm --prefix build/npm run release:publish');
  const executable = process.env.npm_execpath;
  const registry = 'https://registry.npmjs.org/';
  const query = spawnSync(process.execPath, [executable, 'view', `${pkg.name}@${pkg.version}`, 'version', '--json', `--registry=${registry}`], {cwd: directory, encoding: 'utf8'});
  if (query.error) throw query.error;
  if (query.status === 0) {
    if (JSON.parse(query.stdout) !== pkg.version) throw new Error('Registry returned an unexpected package version.');
    console.log(`${pkg.name}@${pkg.version} is already published; preserving it.`);
    return;
  }
  let error;
  try { error = JSON.parse(query.stdout).error; } catch { /* report the query failure below */ }
  if (!error || error.code !== 'E404') throw new Error(`Could not determine registry state: ${query.stderr.trim()}`);
  const result = spawnSync(process.execPath, [executable, 'publish', '--access', 'public', `--registry=${registry}`], {cwd: directory, stdio: 'inherit'});
  if (result.error) throw result.error;
  process.exitCode = result.status === null ? 1 : result.status;
}

try { main(); } catch (error) { console.error(error.message); process.exitCode = 1; }
