"use strict";

const assert = require("node:assert/strict");
const ui = require("../web/ui-core.js");

const patch = [
  "*** Begin Patch",
  "*** Update File: src/app.js",
  "@@",
  '-const theme = "light";',
  '+const theme = "dark";',
  " render();",
  "*** End Patch",
].join("\n");

const diff = ui.patchDiffHtml(patch);
assert.match(diff, /diff-deletion/);
assert.match(diff, /diff-addition/);
assert.match(diff, /diff-file/);
assert.match(diff, /<span class="diff-old">1<\/span>/);
assert.match(diff, /<span class="diff-new">1<\/span>/);
assert.equal(ui.patchLineKind("-deleted"), "deletion");
assert.equal(ui.patchLineKind("+added"), "addition");

const hostile = ui.patchDiffHtml("+<script>alert(1)</script>");
assert.doesNotMatch(hostile, /<script>/);
assert.match(hostile, /&lt;script&gt;/);

const patchCall = ui.toolCallHtml({
  name: "apply_patch",
  patch_preview: patch,
});
assert.match(patchCall, /Applying workspace changes/);
assert.match(patchCall, /diff-deletion/);
assert.doesNotMatch(patchCall, /\{"patch"/);

const commandCall = ui.toolCallHtml({
  name: "exec_command",
  arguments: JSON.stringify({ command: "cargo test", workdir: "/workspace" }),
});
assert.match(commandCall, /cargo test/);
assert.match(commandCall, /workdir/);
assert.doesNotMatch(commandCall, /\{"command"/);

const result = ui.toolResultHtml({
  result: {
    ok: false,
    status: "completed",
    exit_code: 2,
    output: { content: "failed cleanly" },
  },
});
assert.match(result, /exit 2/);
assert.match(result, /failed cleanly/);
assert.doesNotMatch(result, /\{"ok"/);

assert.equal(ui.noticeText({ type: "message_sent" }), "Message queued");
assert.equal(
  ui.noticeText({ type: "agent_renamed", name: "Builder" }),
  "Agent renamed to Builder",
);
assert.equal(ui.isNearBottom(1000, 780, 100, 120), true);
assert.equal(ui.isNearBottom(1000, 700, 100, 120), false);
assert.equal(ui.anchoredScrollTop(40, 1000, 1450), 490);

console.log("ui_core tests passed");
