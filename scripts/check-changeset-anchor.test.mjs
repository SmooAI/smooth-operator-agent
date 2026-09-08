/**
 * Self-check for the lockstep-anchor guard. `node scripts/check-changeset-anchor.test.mjs`.
 * The load-bearing case is `replays the #348 bug` — if that ever goes green, the guard is decorative.
 */
import assert from 'node:assert/strict';
import { evaluate, packagesInChangeset, stampedTreesFrom, ANCHOR } from './check-changeset-anchor.mjs';
import { targets } from './sync-versions.mjs';

const trees = stampedTreesFrom(targets);
const cs = (...pkgs) => `---\n${pkgs.map((p) => `'${p}': minor`).join('\n')}\n---\n\nbody\n`;
let n = 0;
const ok = (name, fn) => {
    fn();
    n++;
    console.log(`  ok  ${name}`);
};

ok('derives stamped trees from the real sync-versions target list', () => {
    assert.ok(trees.includes('dotnet/server/src'), `expected dotnet/server/src in ${trees.join(', ')}`);
    assert.ok(trees.includes('rust'));
    assert.ok(trees.includes('go'));
    // typescript/ is versioned by changesets itself — it must NOT be a stamped tree, or every TS PR
    // would be forced to bump the anchor.
    assert.ok(!trees.includes('typescript'));
});

ok('parses package names out of changeset frontmatter', () => {
    assert.deepEqual(packagesInChangeset(cs('@smooai/smooth-operator')), ['@smooai/smooth-operator']);
    assert.deepEqual(packagesInChangeset(cs('a', 'b')), ['a', 'b']);
    assert.deepEqual(packagesInChangeset('no frontmatter here'), []);
    // A `:` in the prose body must not be mistaken for a package line.
    assert.deepEqual(packagesInChangeset("---\n'a': minor\n---\n\nnote: this is prose\n"), ['a']);
});

ok('replays the #348 bug — .NET change, changeset names only the TS server package', () => {
    const r = evaluate({
        changedFiles: ['dotnet/server/src/FrameDispatcher.cs', 'dotnet/server/tests/FileTransferTests.cs'],
        changesets: [cs('@smooai/smooth-operator-server')],
        stampedTrees: trees,
    });
    assert.equal(r.ok, false, 'the exact shape that shipped #348 unpublished must FAIL');
    assert.deepEqual(r.touched, ['dotnet/server/src']);
});

ok('passes when the anchor is named alongside the TS package (the #346 shape)', () => {
    const r = evaluate({
        changedFiles: ['dotnet/server/src/FrameDispatcher.cs'],
        changesets: [cs('@smooai/smooth-operator-server', '@smooai/smooth-operator')],
        stampedTrees: trees,
    });
    assert.equal(r.ok, true);
});

ok('passes when any one of several changesets names the anchor', () => {
    const r = evaluate({
        changedFiles: ['rust/smooth-operator/src/mcp.rs'],
        changesets: [cs('@smooai/smooth-operator-server'), cs('@smooai/smooth-operator')],
        stampedTrees: trees,
    });
    assert.equal(r.ok, true);
});

ok('fails a PR that touches a stamped tree with NO changeset — merged is not shipped', () => {
    // The second failure mode, same symptom as #348: five PRs in a row changed stamped trees,
    // merged green, and published nothing because they carried no changeset at all.
    const r = evaluate({ changedFiles: ['dotnet/server/src/Foo.cs'], changesets: [], stampedTrees: trees });
    assert.equal(r.ok, false);
    assert.equal(r.reason, 'missing');
});

ok('stays quiet on a genuinely docs/test-only PR — nothing stamped is touched', () => {
    // The exemption the old guard was reaching for. Tests live BESIDE a stamped tree
    // (`dotnet/server/tests`), not inside it, so they never trip the check.
    const r = evaluate({
        changedFiles: ['README.md', 'dotnet/server/tests/FooTests.cs'],
        changesets: [],
        stampedTrees: trees,
    });
    assert.equal(r.ok, true);
    assert.deepEqual(r.touched, []);
});

ok('an empty changeset is an explicit no-release and needs no anchor', () => {
    // `pnpm changeset --empty` — a decision rather than an omission.
    const r = evaluate({ changedFiles: ['dotnet/server/src/Foo.cs'], changesets: ['---\n---\n'], stampedTrees: trees });
    assert.equal(r.ok, true);
    assert.equal(r.reason, 'empty');
});

ok('stays quiet on a TS-only PR that never touches a stamped tree', () => {
    const r = evaluate({
        changedFiles: ['typescript/server/src/index.ts'],
        changesets: [cs('@smooai/smooth-operator-server')],
        stampedTrees: trees,
    });
    assert.equal(r.ok, true);
    assert.deepEqual(r.touched, []);
});

ok('does not treat a sibling path as inside a stamped tree', () => {
    // `dotnet/server/srcgen/...` must not match the `dotnet/server/src` tree.
    const r = evaluate({
        changedFiles: ['dotnet/server/srcgen/Thing.cs'],
        changesets: [cs('@smooai/smooth-operator-server')],
        stampedTrees: trees,
    });
    assert.equal(r.ok, true, 'prefix match must be path-segment aware');
});

console.log(`\nanchor-guard self-check: ${n} passed`);
