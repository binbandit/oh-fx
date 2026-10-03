const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const assert = require('node:assert/strict');
const test = require('node:test');
const { execFileSync } = require('node:child_process');
const lines = fs.readFileSync(path.join(__dirname, '../workflows/approval-guard.yml'), 'utf8').split('\n');
function stepLine(name, pattern) {
  const step = lines.findIndex(line => line.trim() === `- name: ${name}`);
  assert.notEqual(step, -1, `Missing step: ${name}`);
  const index = lines.findIndex((line, index) => index > step && pattern.test(line));
  assert.notEqual(index, -1, `Missing ${pattern} in step: ${name}`);
  return index;
}
function stepScript(name) {
  const start = stepLine(name, /^ +run: \|$/);
  const indent = ' '.repeat(lines[start].indexOf('run:') + 2);
  const script = [];
  for (const line of lines.slice(start + 1)) {
    if (line.trim() && !line.startsWith(indent)) break;
    script.push(line.slice(indent.length));
  }
  return script.join('\n');
}
const compare = stepScript('Compare the reviewed patches');
const remove = stepScript('Remove the approval');
const comment = lines[stepLine('Remove the approval', /^ +COMMENT: /)].replace(/^ +COMMENT: /, '');
const scratch = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), 'approval-guard-')));
test.after(() => fs.rmSync(scratch, { recursive: true, force: true }));
const env = {
  ...process.env, GIT_CONFIG_GLOBAL: os.devNull, GIT_CONFIG_NOSYSTEM: '1',
  GIT_AUTHOR_NAME: 'Test', GIT_AUTHOR_EMAIL: 'test@example.com',
  GIT_COMMITTER_NAME: 'Test', GIT_COMMITTER_EMAIL: 'test@example.com',
};
const bash = (script, extra) => execFileSync('bash', ['--noprofile', '--norc', '-eo', 'pipefail', '-c', script],
  { env: { ...env, ...extra }, encoding: 'utf8', stdio: 'pipe' });
const base = Array.from({ length: 20 }, (_, index) => `line ${index + 1}`);
const script = changes => `${base.map((line, index) => changes[index] ?? line).join('\n')}\n`;

function repository() {
  const root = fs.mkdtempSync(path.join(scratch, 'repo-'));
  const work = path.join(root, 'work');
  const origin = path.join(root, 'origin.git');
  const git = (...args) => execFileSync('git', args, { cwd: work, env, encoding: 'utf8' }).trim();
  execFileSync('git', ['init', '-q', '--bare', '-b', 'main', origin], { env });
  execFileSync('git', ['-C', origin, 'config', 'uploadpack.allowAnySHA1InWant', 'true'], { env });
  execFileSync('git', ['init', '-q', '-b', 'main', work], { env });
  git('remote', 'add', 'origin', origin);
  const repo = {
    git,
    commit(message, files, executable = []) {
      for (const [file, content] of Object.entries(files)) fs.writeFileSync(path.join(work, file), content);
      git('add', '--', ...Object.keys(files));
      for (const file of executable) git('update-index', '--chmod=+x', '--', file);
      git('commit', '-q', '-m', message);
      return git('rev-parse', 'HEAD');
    },
    push(branch = 'pull') {
      git('push', '-q', '--force', 'origin', `HEAD:refs/heads/${branch}`);
      return git('rev-parse', 'HEAD');
    },
    reviewed(before, after, baseRef = 'main') {
      const runner = fs.mkdtempSync(path.join(root, 'runner-'));
      const output = path.join(runner, 'output');
      bash(compare, {
        RUNNER_TEMP: runner, GITHUB_OUTPUT: output, GITHUB_SERVER_URL: `file://${root}`,
        GITHUB_REPOSITORY: 'origin.git', BASE_REF: baseRef, BEFORE: before, AFTER: after,
      });
      return fs.readFileSync(output, 'utf8').trim();
    },
    advanceMain() {
      git('switch', '-q', 'main');
      repo.commit('rename the first line', { 'run.sh': script({ 0: 'line one' }) });
      repo.push('main');
      git('switch', '-q', 'pull');
    },
  };
  repo.commit('base', { 'run.sh': script({}) });
  repo.push('main');
  git('switch', '-q', '-c', 'pull');
  repo.commit('clean the build', { 'run.sh': script({ 14: 'rm -rf /tmp/build' }) });
  repo.commit('add notes', { 'notes.txt': 'first\n' });
  repo.approved = repo.push();
  return repo;
}

test('a rebase that keeps every patch keeps the approval', () => {
  const repo = repository();
  repo.advanceMain();
  repo.git('rebase', '-q', 'main');
  assert.equal(repo.reviewed(repo.approved, repo.push()), 'reviewed=unchanged');
});

test('reworded commits keep the approval because their patches are unchanged', () => {
  const repo = repository();
  repo.git('commit', '-q', '--amend', '-m', 'add release notes');
  assert.equal(repo.reviewed(repo.approved, repo.push()), 'reviewed=unchanged');
});

test('a new commit or a rewritten patch drops the approval', () => {
  const repo = repository();
  repo.commit('more notes', { 'notes.txt': 'first\nsecond\n' });
  assert.equal(repo.reviewed(repo.approved, repo.push()), 'reviewed=changed');
  repo.git('reset', '-q', '--hard', `${repo.approved}~1`);
  repo.commit('add notes', { 'notes.txt': 'changed\n' });
  assert.equal(repo.reviewed(repo.approved, repo.push()), 'reviewed=changed');
});

test('whitespace inside a line, file modes, and binary content drop the approval', () => {
  for (const [file, reviewed, pushed, executable] of [
    ['clean.sh', 'rm -rf /tmp/cache\n', 'rm -rf / tmp/cache\n', []],
    ['tool.sh', 'make\n', 'make\n', ['tool.sh']],
    ['data.bin', Buffer.from([0, 1, 2]), Buffer.from([0, 1, 3]), []],
  ]) {
    const repo = repository();
    repo.commit('add a file', { [file]: reviewed });
    const approved = repo.push();
    repo.git('reset', '-q', '--hard', 'HEAD~1');
    repo.commit('add a file', { [file]: pushed }, executable);
    assert.equal(repo.reviewed(approved, repo.push()), 'reviewed=changed', file);
  }
});

test('a merge commit drops the approval even when it adds no patch of its own', () => {
  const repo = repository();
  repo.advanceMain();
  repo.git('merge', '-q', '--no-edit', 'main');
  assert.equal(repo.reviewed(repo.approved, repo.push()), 'reviewed=changed');
});

test('an unreachable approved commit or a missing base branch drops the approval', () => {
  const repo = repository();
  const unpushed = repo.commit('local only', { 'notes.txt': 'local\n' });
  repo.git('reset', '-q', '--hard', repo.approved);
  assert.equal(repo.reviewed(unpushed, repo.approved), 'reviewed=changed');
  assert.equal(repo.reviewed('0'.repeat(40), repo.approved), 'reviewed=changed');
  assert.equal(repo.reviewed(repo.approved, repo.approved), 'reviewed=unchanged');
  assert.equal(repo.reviewed(repo.approved, repo.approved, 'missing'), 'reviewed=changed');
});

test('remove the label and comment only while the label is present', () => {
  assert.equal(comment, 'New commits since approval; `codex-approved` removed until the maintainer re-reviews.');
  const bin = fs.mkdtempSync(path.join(scratch, 'bin-'));
  const log = path.join(bin, 'calls');
  fs.writeFileSync(path.join(bin, 'gh'), [
    '#!/usr/bin/env node',
    `require('node:fs').appendFileSync(${JSON.stringify(log)}, JSON.stringify(process.argv.slice(2)) + '\\n');`,
    "if (process.argv.includes('--jq')) process.stdout.write(process.env.FAKE_LABEL);",
  ].join('\n'), { mode: 0o755 });
  const calls = label => {
    fs.rmSync(log, { force: true });
    bash(remove, {
      PATH: `${bin}${path.delimiter}${process.env.PATH}`, FAKE_LABEL: label,
      GITHUB_REPOSITORY: 'owner/repo', PULL_REQUEST: '42', COMMENT: comment,
    });
    return fs.readFileSync(log, 'utf8').trim().split('\n').map(line => JSON.parse(line));
  };
  const issue = 'repos/owner/repo/issues/42';
  const list = ['api', '--paginate', `${issue}/labels`, '--jq', '.[] | select(.name == "codex-approved") | .name'];
  assert.deepEqual(calls('codex-approved\n'), [
    list,
    ['api', '--method', 'DELETE', `${issue}/labels/codex-approved`, '--silent'],
    ['api', `${issue}/comments`, '--silent', '-f', `body=${comment}`],
  ]);
  assert.deepEqual(calls(''), [list]);
});
