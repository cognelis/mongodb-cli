'use strict';

const crypto = require('node:crypto');
const fs = require('node:fs');
const https = require('node:https');
const path = require('node:path');
const pkg = require('./package.json');

const MAX_BINARY_BYTES = 64 * 1024 * 1024;
const MAX_CHECKSUM_BYTES = 64 * 1024;
const DOWNLOAD_TIMEOUT_MS = 30_000;
const inFlight = new Map();

function assetName(platform = process.platform, arch = process.arch) {
  const os = {darwin: 'darwin', linux: 'linux', win32: 'windows'}[platform];
  const cpu = {x64: 'amd64', arm64: 'arm64'}[arch];
  if (!os || !cpu) throw new Error(`Unsupported platform ${platform}/${arch}; build mongodb-cli from source.`);
  return `mongodb-cli-${os}-${cpu}${platform === 'win32' ? '.exe' : ''}`;
}

function releaseBaseURL(repository = pkg.repository?.url, version = pkg.version) {
  if (!/^\d+\.\d+\.\d+$/.test(version)) throw new Error('Package version must be a release version.');
  if (typeof repository !== 'string') throw new Error('The package has no release repository.');
  const url = new URL(repository.replace(/^git\+/, '').replace(/\.git$/, ''));
  const parts = url.pathname.split('/').filter(Boolean);
  if (url.protocol !== 'https:' || url.hostname !== 'github.com' || url.username || url.password || url.search || url.hash || parts.length !== 2 || parts.some(part => !/^[A-Za-z0-9_.-]+$/.test(part))) {
    throw new Error('The package repository must be a public GitHub HTTPS repository.');
  }
  return `https://github.com/${parts.join('/')}/releases/download/v${version}`;
}

function sha256(data) {
  return crypto.createHash('sha256').update(data).digest('hex');
}

function checksumFor(text, asset) {
  const matches = [];
  for (const line of text.split(/\r?\n/)) {
    const match = /^([a-fA-F0-9]{64})\s+\*?(\S+)$/.exec(line.trim());
    if (match && match[2] === asset) matches.push(match[1].toLowerCase());
  }
  if (matches.length !== 1) throw new Error(`checksums.txt must contain exactly one SHA-256 for ${asset}.`);
  return matches[0];
}

function download(address, maxBytes, options = {}) {
  const request = options.request || https.get;
  const timeoutMs = options.timeoutMs || DOWNLOAD_TIMEOUT_MS;
  const started = Date.now();
  function get(url, redirects) {
    return new Promise((resolve, reject) => {
      const parsed = new URL(url);
      if (parsed.protocol !== 'https:' || parsed.username || parsed.password) {
        reject(new Error('Binary downloads require HTTPS without embedded credentials.'));
        return;
      }
      if (redirects > 5) { reject(new Error('Too many download redirects.')); return; }
      const remaining = timeoutMs - (Date.now() - started);
      if (remaining <= 0) { reject(new Error('Binary download timed out.')); return; }
      let timer;
      const req = request(parsed, {method: options.method || 'GET', headers: {'User-Agent': 'mongodb-cli-npm-installer'}}, res => {
        if ([301, 302, 303, 307, 308].includes(res.statusCode) && res.headers.location) {
          res.destroy();
          clearTimeout(timer);
          try {
            resolve(get(new URL(res.headers.location, parsed).href, redirects + 1));
          } catch { reject(new Error('Invalid release download redirect.')); }
          return;
        }
        if (res.statusCode !== 200) {
          res.destroy();
          clearTimeout(timer);
          reject(new Error(`Release download returned HTTP ${res.statusCode} from ${parsed.hostname}.`));
          return;
        }
        const declared = Number(res.headers['content-length']);
        if (options.requireContentLength && (!Number.isFinite(declared) || declared <= 0)) {
          res.destroy();
          clearTimeout(timer);
          reject(new Error('Release asset has no valid content length.'));
          return;
        }
        if (Number.isFinite(declared) && declared > maxBytes) {
          res.destroy();
          clearTimeout(timer);
          reject(new Error(`Release download exceeds ${maxBytes} bytes.`));
          return;
        }
        const chunks = [];
        let size = 0;
        res.on('data', chunk => {
          size += chunk.length;
          if (size > maxBytes) {
            clearTimeout(timer);
            res.destroy();
            reject(new Error(`Release download exceeds ${maxBytes} bytes.`));
          } else chunks.push(chunk);
        });
        res.on('end', () => { clearTimeout(timer); resolve(Buffer.concat(chunks)); });
        res.on('error', error => { clearTimeout(timer); reject(error); });
        res.on('aborted', () => { clearTimeout(timer); reject(new Error('Release download was interrupted.')); });
      });
      req.on('error', error => { clearTimeout(timer); reject(error); });
      timer = setTimeout(() => req.destroy(new Error('Binary download timed out.')), remaining);
    });
  }
  return get(address, 0);
}

function paths(root, platform) {
  const directory = path.join(root, 'binary');
  return {directory, file: path.join(directory, platform === 'win32' ? 'mongodb-cli.exe' : 'mongodb-cli'), metadata: path.join(directory, 'install.json')};
}

function cachedBinary(root, platform, asset, version) {
  const target = paths(root, platform);
  try {
    const metadata = JSON.parse(fs.readFileSync(target.metadata, 'utf8'));
    if (metadata.version !== version || metadata.asset !== asset || !/^[a-f0-9]{64}$/.test(metadata.sha256)) return null;
    const stat = fs.statSync(target.file);
    if (!stat.isFile() || stat.size > MAX_BINARY_BYTES || stat.size === 0) return null;
    if (sha256(fs.readFileSync(target.file)) !== metadata.sha256) return null;
    return target.file;
  } catch { return null; }
}

async function performInstall(options) {
  const {root, platform, arch, version, repository, fetcher} = options;
  const asset = assetName(platform, arch);
  const cached = cachedBinary(root, platform, asset, version);
  if (cached) return cached;
  const base = releaseBaseURL(repository, version);
  const sums = await fetcher(`${base}/checksums.txt`, MAX_CHECKSUM_BYTES);
  const expected = checksumFor(sums.toString('utf8'), asset);
  const data = await fetcher(`${base}/${asset}`, MAX_BINARY_BYTES);
  if (data.length === 0 || data.length > MAX_BINARY_BYTES) throw new Error('Release binary is empty or too large.');
  if (sha256(data) !== expected) throw new Error(`SHA-256 mismatch for ${asset}; the binary was not installed.`);
  const target = paths(root, platform);
  fs.mkdirSync(target.directory, {recursive: true});
  const staging = fs.mkdtempSync(path.join(target.directory, '.download-'));
  try {
    const temporary = path.join(staging, 'binary');
    fs.writeFileSync(temporary, data, {mode: 0o755});
    const metadata = path.join(staging, 'install.json');
    fs.writeFileSync(metadata, JSON.stringify({version, asset, sha256: expected}) + '\n');
    fs.renameSync(temporary, target.file);
    fs.renameSync(metadata, target.metadata);
  } finally { fs.rmSync(staging, {recursive: true, force: true}); }
  return target.file;
}

function install(options = {}) {
  const settings = {
    root: __dirname, platform: process.platform, arch: process.arch,
    version: pkg.version, repository: pkg.repository?.url, fetcher: download,
    ...options,
  };
  const key = `${settings.root}:${settings.platform}:${settings.arch}:${settings.version}`;
  if (!inFlight.has(key)) {
    inFlight.set(key, performInstall(settings).finally(() => inFlight.delete(key)));
  }
  return inFlight.get(key);
}

module.exports = {assetName, releaseBaseURL, checksumFor, sha256, download, install, cachedBinary, MAX_BINARY_BYTES, MAX_CHECKSUM_BYTES};

if (require.main === module) {
  install().catch(error => {
    process.stderr.write(`mongodb-cli: ${error.message}\n`);
    process.exitCode = 1;
  });
}
