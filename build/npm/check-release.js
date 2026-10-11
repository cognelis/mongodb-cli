'use strict';

const {assetName, releaseBaseURL, checksumFor, download, MAX_BINARY_BYTES, MAX_CHECKSUM_BYTES} = require('./install.js');
const pkg = require('./package.json');

async function verifyRelease(options = {}) {
  const fetcher = options.fetcher || download;
  const version = options.version || pkg.version;
  const repository = options.repository || pkg.repository?.url;
  const base = releaseBaseURL(repository, version);
  const sums = (await fetcher(`${base}/checksums.txt`, MAX_CHECKSUM_BYTES)).toString('utf8');
  const assets = ['darwin', 'linux', 'win32'].flatMap(platform => ['x64', 'arm64'].map(arch => assetName(platform, arch)));
  for (const asset of assets) checksumFor(sums, asset);
  await Promise.all(assets.map(asset => fetcher(`${base}/${asset}`, MAX_BINARY_BYTES, {method: 'HEAD', requireContentLength: true})));
  return {version, assets};
}

module.exports = {verifyRelease};

if (require.main === module) {
  verifyRelease().then(result => {
    process.stdout.write(`Verified all ${result.assets.length} release assets for ${result.version}.\n`);
  }).catch(error => {
    process.stderr.write(`Cannot publish @cognelis/mongodb-cli: ${error.message}\n`);
    process.exitCode = 1;
  });
}
