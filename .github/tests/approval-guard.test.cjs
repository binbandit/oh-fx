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
function stepScript(name, key = 'run') {
  const start = stepLine(name, new RegExp(`^ +${key}: \\|$`));
  const indent = ' '.repeat(lines[start].indexOf(`${key}:`) + 2);
  const script = [];
  for (const line of lines.slice(start + 1)) {
    if (line.trim() && !line.startsWith(indent)) break;
    script.push(line.slice(indent.length));
  }
  return script.join('\n');
}
const compare = stepScript('Replay the reviewed change');
const record = stepScript('Record maintainer approval', 'script');
const comment = lines[stepLine('Record maintainer approval', /^ +COMMENT: /)].replace(/^ +COMMENT: /, '');
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
    advanceMain(files = { 'main.txt': 'main\n' }) {
      git('switch', '-q', 'main');
      repo.commit('advance main', files);
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

test('a clean rebase keeps the approval, even when main changed nearby lines', () => {
  for (const files of [undefined, { 'run.sh': script({ 12: 'line thirteen' }) }]) {
    const repo = repository();
    repo.advanceMain(files);
    repo.git('rebase', '-q', 'main');
    assert.equal(repo.reviewed(repo.approved, repo.push()), 'reviewed=unchanged');
  }
});

test('a rebase that resolves a conflict drops the approval', () => {
  const repo = repository();
  repo.advanceMain({ 'run.sh': script({ 14: 'line fifteen' }) });
  repo.git('reset', '-q', '--hard', 'main');
  repo.commit('clean the build', { 'run.sh': script({ 14: 'rm -rf /tmp/build' }) });
  repo.commit('add notes', { 'notes.txt': 'first\n' });
  assert.equal(repo.reviewed(repo.approved, repo.push()), 'reviewed=changed');
});

test('reworded or squashed commits keep the approval because the change is the same', () => {
  const repo = repository();
  repo.git('commit', '-q', '--amend', '-m', 'add release notes');
  assert.equal(repo.reviewed(repo.approved, repo.push()), 'reviewed=unchanged');
  repo.git('reset', '-q', '--soft', 'HEAD~1');
  repo.git('commit', '-q', '--amend', '-m', 'clean the build and add notes');
  assert.equal(repo.reviewed(repo.approved, repo.push()), 'reviewed=unchanged');
});

test('the same edit moved to another function with identical context drops the approval', () => {
  const body = name => [`${name}() {`, ...Array(6).fill('  :'), '  printf "%s\\n" "safe"', ...Array(6).fill('  :'), '}'];
  const functions = (one, two) => `${[...body('one'), ...body('two')].map((line, index) =>
    (index === 7 && one) || (index === 22 && two) || line).join('\n')}\n`;
  const repo = repository();
  const parent = repo.commit('add functions', { 'functions.sh': functions() });
  repo.commit('print unsafe', { 'functions.sh': functions('  printf "%s\\n" "unsafe"') });
  const approved = repo.push();
  repo.git('reset', '-q', '--hard', parent);
  repo.commit('print unsafe', { 'functions.sh': functions(undefined, '  printf "%s\\n" "unsafe"') });
  assert.equal(repo.reviewed(approved, repo.push()), 'reviewed=changed');
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

const AsyncFunction = Object.getPrototypeOf(async function() {}).constructor;
const head = 'a'.repeat(40);
const before = 'b'.repeat(40);
const approval = { context: 'maintainer-approval', state: 'success', creator: { login: 'github-actions[bot]' } };
async function decide(action, {
  label = 'codex-approved', role = 'admin', labels = ['codex', 'codex-approved'], statuses = [approval],
  reviewed = 'unchanged', failing = {},
} = {}) {
  const writes = [];
  const endpoint = (name, respond) => async params => {
    if (failing[name]) throw Object.assign(new Error(`${name} failed`), { status: failing[name] });
    return { data: respond(params) };
  };
  const github = {
    paginate: async (method, params) => (await method(params)).data,
    rest: {
      repos: {
        getCollaboratorPermissionLevel: endpoint('permission', ({ owner, repo, username }) => {
          assert.deepEqual([owner, repo, username], ['owner', 'repo', 'sender']);
          return { permission: role === 'maintain' ? 'write' : role, role_name: role };
        }),
        listCommitStatusesForRef: endpoint('statuses', ({ ref }) => {
          assert.equal(ref, before);
          return statuses;
        }),
        createCommitStatus: endpoint('status', ({ sha, context, state, description }) => {
          assert.deepEqual([sha, context], [head, 'maintainer-approval']);
          writes.push(`${state}: ${description}`);
          return {};
        }),
      },
      issues: {
        listLabelsOnIssue: endpoint('labels', () => labels.map(name => ({ name }))),
        removeLabel: endpoint('unlabel', ({ issue_number, name }) => {
          writes.push(`unlabel #${issue_number} ${name}`);
          return [];
        }),
        createComment: endpoint('comment', ({ issue_number, body }) => {
          writes.push(`comment #${issue_number}: ${body}`);
          return {};
        }),
      },
    },
  };
  const context = { repo: { owner: 'owner', repo: 'repo' }, payload: {
    action, before, after: head, sender: { login: 'sender' }, label: { name: label },
    pull_request: { number: 42, head: { sha: head } },
  } };
  Object.assign(process.env, { REVIEWED: reviewed, COMMENT: comment });
  try {
    await new AsyncFunction('github', 'context', 'core', record)(github, context, {});
    return { writes };
  } catch (error) {
    return { writes, error: error.message };
  } finally {
    delete process.env.REVIEWED;
    delete process.env.COMMENT;
  }
}
const revoked = ['failure: new commits need maintainer review', 'unlabel #42 codex-approved', `comment #42: ${comment}`];

test('a maintainer or admin adding the label approves the current head', async () => {
  for (const role of ['admin', 'maintain']) {
    assert.deepEqual(await decide('labeled', { role }), { writes: ['success: approved at aaaaaaa'] });
  }
});

test('the label from anyone else is removed and fails the head', async () => {
  for (const role of ['write', 'triage', 'read', 'none']) {
    assert.deepEqual(await decide('labeled', { role }),
      { writes: ['failure: codex-approved needs a maintainer', 'unlabel #42 codex-approved'] });
  }
});

test('removing the label fails the head, and other labels change nothing', async () => {
  assert.deepEqual(await decide('unlabeled', { labels: ['codex'] }), { writes: ['failure: codex-approved removed'] });
  for (const action of ['labeled', 'unlabeled']) {
    assert.deepEqual(await decide(action, { label: 'codex' }), { writes: [] });
  }
});

test('a push that replays a guard-approved head carries the approval to the new head', async () => {
  assert.deepEqual(await decide('synchronize'), { writes: ['success: replays approved bbbbbbb'] });
});

test('a changed push, or a push from a head the guard never approved, revokes the approval', async () => {
  for (const options of [
    { reviewed: 'changed' },
    { reviewed: '' },
    { statuses: [] },
    { statuses: [{ ...approval, state: 'failure' }, approval] },
    { statuses: [{ ...approval, creator: { login: 'someone' } }] },
  ]) {
    assert.deepEqual(await decide('synchronize', options), { writes: revoked }, JSON.stringify(options));
  }
  assert.deepEqual(await decide('synchronize', { reviewed: 'changed', failing: { unlabel: 404 } }),
    { writes: revoked.slice(0, 1) });
});

test('a push to a PR without the label leaves its new head without a status', async () => {
  assert.deepEqual(await decide('synchronize', { labels: ['codex'] }), { writes: [] });
});

test('API failures fail closed', async () => {
  const unverified = 'failure: maintainer approval could not be verified';
  assert.deepEqual(await decide('labeled', { failing: { permission: 502 } }),
    { writes: [unverified], error: 'permission failed' });
  assert.deepEqual(await decide('synchronize', { failing: { labels: 502 } }),
    { writes: [unverified], error: 'labels failed' });
  assert.deepEqual(await decide('synchronize', { failing: { statuses: 502 } }),
    { writes: [unverified], error: 'statuses failed' });
  assert.deepEqual(await decide('synchronize', { reviewed: 'changed', failing: { unlabel: 502 } }),
    { writes: [revoked[0], unverified], error: 'unlabel failed' });
  assert.deepEqual(await decide('labeled', { failing: { status: 502 } }), { writes: [], error: 'status failed' });
});

test('the comment matches the agreed wording', () => {
  assert.equal(comment, 'New commits since approval; `codex-approved` removed until the maintainer re-reviews.');
});
