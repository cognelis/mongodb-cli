#!/usr/bin/env node
'use strict';

const {spawnSync} = require('node:child_process');
const {install} = require('../install.js');

async function main() {
  const file = await install();
  const child = spawnSync(file, process.argv.slice(2), {stdio: 'inherit'});
  if (child.error) throw child.error;
  if (child.signal) {
    process.kill(process.pid, child.signal);
    return;
  }
  process.exitCode = child.status === null ? 1 : child.status;
}

main().catch(error => {
  process.stderr.write(`mongodb-cli: ${error.message}\n`);
  process.exitCode = 1;
});
