'use strict';

const assert = require('node:assert/strict');
const {EventEmitter} = require('node:events');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const {Readable} = require('node:stream');
const {spawnSync} = require('node:child_process');
const {test} = require('node:test');
const installer = require('./install.js');
const {verifyRelease} = require('./check-release.js');

const repository = 'git+https://github.com/example/mongodb-cli.git';

function fixture(t, extra = {}) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'mongodb-npm-'));
  t.after(() => fs.rmSync(root, {recursive: true, force: true}));
  return {root, platform: 'darwin', arch: 'arm64', version: '0.3.1', repository, ...extra};
}

function downloadFixture(data, counter = {calls: 0}) {
  return async (url, maxBytes) => {
    counter.calls++;
    const value = url.endsWith('/checksums.txt')
      ? Buffer.from(`${installer.sha256(data)}  mongodb-cli-darwin-arm64\n`)
      : data;
    assert.ok(value.length < maxBytes);
    return value;
  };
}

function requests(responses) {
  let count = 0;
  const request = (url, options, callback) => {
    const req = new EventEmitter();
    req.destroy = error => { if (error) queueMicrotask(() => req.emit('error', error)); };
    const fixture = responses[count++];
    queueMicrotask(() => {
      if (fixture.hang) return;
      const res = Readable.from(fixture.chunks || []);
      res.statusCode = fixture.status || 200;
      res.headers = fixture.headers || {};
      callback(res);
    });
    return req;
  };
  return {request, count: () => count};
}

test('maps only the six supported native artifacts', () => {
  for (const [platform, osName] of Object.entries({darwin: 'darwin', linux: 'linux', win32: 'windows'})) {
    for (const [arch, cpu] of Object.entries({x64: 'amd64', arm64: 'arm64'})) {
      assert.equal(installer.assetName(platform, arch), `mongodb-cli-${osName}-${cpu}${platform === 'win32' ? '.exe' : ''}`);
    }
  }
  assert.throws(() => installer.assetName('linux', 'ia32'), /Unsupported platform/);
  assert.throws(() => installer.assetName('freebsd', 'x64'), /Unsupported platform/);
});

test('release URLs are tied to the package repository and exact version', () => {
  assert.equal(installer.releaseBaseURL(repository, '0.4.0'), 'https://github.com/example/mongodb-cli/releases/download/v0.4.0');
  for (const url of ['http://github.com/example/repo', 'https://user:secret@github.com/example/repo', 'https://evil.example/example/repo', 'https://github.com/example/repo?token=secret']) {
    assert.throws(() => installer.releaseBaseURL(url, '0.4.0'));
  }
  assert.throws(() => installer.releaseBaseURL(repository, '0.4.0/../other'));
});

test('checksum parsing rejects missing, malformed and duplicate entries', () => {
  const hash = 'ab'.repeat(32);
  const asset = 'mongodb-cli-darwin-arm64';
  assert.equal(installer.checksumFor(`${hash}  ${asset}\r\n`, asset), hash);
  for (const text of ['', `bad  ${asset}`, `${hash} other`, `${hash} ${asset}\n${hash} ${asset}`]) {
    assert.throws(() => installer.checksumFor(text, asset), /exactly one/);
  }
});

test('installs verified bytes once, uses the cache and detects corruption', async t => {
  const data = Buffer.from('native-binary-fixture');
  const count = {calls: 0};
  const options = fixture(t, {fetcher: downloadFixture(data, count)});
  const [first, second] = await Promise.all([installer.install(options), installer.install(options)]);
  assert.equal(first, second);
  assert.deepEqual(fs.readFileSync(first), data);
  assert.equal(count.calls, 2);
  await installer.install(options);
  assert.equal(count.calls, 2);
  fs.writeFileSync(first, 'corrupted');
  await installer.install(options);
  assert.equal(count.calls, 4);
  assert.deepEqual(fs.readFileSync(first), data);
  if (process.platform !== 'win32') assert.ok(fs.statSync(first).mode & 0o111);
});

test('checksum and network failures never create a runnable binary', async t => {
  for (const failure of ['mismatch', 'missing-checksum', 'network']) {
    const options = fixture(t, {fetcher: async url => {
      if (failure === 'network') throw new Error('network unavailable');
      if (url.endsWith('/checksums.txt')) return Buffer.from(failure === 'missing-checksum' ? '' : `${'00'.repeat(32)} mongodb-cli-darwin-arm64\n`);
      return Buffer.from('unverified');
    }});
    await assert.rejects(installer.install(options));
    assert.equal(fs.existsSync(path.join(options.root, 'binary', 'mongodb-cli')), false);
  }
});

test('downloads enforce HTTPS, status, redirect, size and timeout limits', async () => {
  await assert.rejects(installer.download('http://example.invalid', 10), /HTTPS/);
  let transport = requests([{status: 302, headers: {location: '/binary'}}, {chunks: [Buffer.from('ok')]}]);
  assert.equal((await installer.download('https://example.invalid/start', 10, transport)).toString(), 'ok');
  assert.equal(transport.count(), 2);
  transport = requests([{status: 302, headers: {location: 'http://example.invalid/insecure'}}]);
  await assert.rejects(installer.download('https://example.invalid', 10, transport), /HTTPS/);
  transport = requests(Array.from({length: 6}, () => ({status: 302, headers: {location: '/again'}})));
  await assert.rejects(installer.download('https://example.invalid', 10, transport), /redirects/);
  for (const response of [{status: 404}, {headers: {'content-length': '20'}}, {chunks: [Buffer.from('01234567890')]}]) {
    await assert.rejects(installer.download('https://example.invalid', 10, requests([response])), /HTTP|exceeds/);
  }
  await assert.rejects(installer.download('https://example.invalid', 10, {...requests([{hang: true}]), timeoutMs: 5}), /timed out/);
});

test('launcher preserves arguments, stdout, stderr and failure exit codes', {skip: process.platform === 'win32'}, async t => {
  const options = fixture(t, {platform: process.platform, arch: process.arch});
  fs.mkdirSync(path.join(options.root, 'bin'));
  fs.copyFileSync(path.join(__dirname, 'install.js'), path.join(options.root, 'install.js'));
  fs.copyFileSync(path.join(__dirname, 'bin/mongodb-cli.js'), path.join(options.root, 'bin/mongodb-cli.js'));
  fs.writeFileSync(path.join(options.root, 'package.json'), JSON.stringify({version: options.version, repository: {url: repository}}));
  const data = Buffer.from('#!/usr/bin/env node\nprocess.stdout.write(JSON.stringify(process.argv.slice(2))); process.stderr.write("diagnostic"); process.exit(3);\n');
  const asset = installer.assetName(process.platform, process.arch);
  await installer.install({...options, fetcher: async url => url.endsWith('/checksums.txt') ? Buffer.from(`${installer.sha256(data)} ${asset}\n`) : data});
  const result = spawnSync(process.execPath, [path.join(options.root, 'bin/mongodb-cli.js'), 'documents', 'find', 'shop', 'orders', '--filter', '{"status": "hello world"}'], {encoding: 'utf8'});
  assert.equal(result.status, 3);
  assert.deepEqual(JSON.parse(result.stdout), ['documents', 'find', 'shop', 'orders', '--filter', '{"status": "hello world"}']);
  assert.equal(result.stderr, 'diagnostic');
});

test('publishing requires checksums and reachable binaries for all platforms', async () => {
  const assets = ['darwin', 'linux', 'win32'].flatMap(platform => ['x64', 'arm64'].map(arch => installer.assetName(platform, arch)));
  const manifest = assets.map(asset => `${'ab'.repeat(32)} ${asset}`).join('\n');
  const heads = [];
  const options = {version: '0.4.0', repository, fetcher: async (url, size, request) => {
    if (url.endsWith('checksums.txt')) return Buffer.from(manifest);
    assert.equal(request.method, 'HEAD');
    assert.equal(request.requireContentLength, true);
    heads.push(url);
    return Buffer.alloc(0);
  }};
  assert.equal((await verifyRelease(options)).assets.length, 6);
  assert.equal(heads.length, 6);
  await assert.rejects(verifyRelease({...options, fetcher: async () => Buffer.from(manifest.split('\n').slice(1).join('\n'))}), /exactly one/);
  await assert.rejects(verifyRelease({...options, fetcher: async url => {
    if (url.endsWith('checksums.txt')) return Buffer.from(manifest);
    throw new Error('missing release asset');
  }}), /missing release asset/);
});
