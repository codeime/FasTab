import assert from "node:assert/strict";
import { mkdtemp, mkdir, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";

import { argvDigest } from "./hook-baseline-lib.mjs";
import {
  CLI_OUTPUT_MAX_BYTES,
  checkCliOutputs,
  collectCliArgvList,
  finalizeCliOutputSample,
  gitSubcommand,
  isAllowedArgv,
  recordCliOutputs,
  recordOneArgv,
  redactText,
  shellScriptAllowed,
  truncateText,
} from "./record-cli-output.mjs";

test("allowlist accepts documented readonly git/npm/docker argv", () => {
  assert.equal(isAllowedArgv(["git", "branch", "--no-color"]), true);
  assert.equal(isAllowedArgv(["git", "--no-optional-locks", "status", "--short"]), true);
  assert.equal(isAllowedArgv(["git", "config", "--get-regexp", "^alias."]), true);
  assert.equal(isAllowedArgv(["npm", "prefix"]), true);
  assert.equal(isAllowedArgv(["docker", "network", "list"]), true);
  assert.equal(isAllowedArgv(["kubectl", "api-resources", "-o", "name"]), true);
  assert.equal(isAllowedArgv(["brew", "tap"]), true);
  assert.equal(isAllowedArgv(["cargo", "metadata", "--format-version", "1"]), true);
  assert.equal(isAllowedArgv(["gh", "alias", "list"]), true);
  assert.equal(isAllowedArgv(["aws-vault", "list"]), true);
  assert.equal(isAllowedArgv(["bash", "-c", "until [[ -f package.json ]]; do cd ..; done; cat package.json"]), true);
});

test("allowlist rejects mutating or secret-bearing commands", () => {
  assert.equal(isAllowedArgv(["git", "checkout", "main"]), false);
  assert.equal(isAllowedArgv(["git", "commit", "-m", "x"]), false);
  assert.equal(isAllowedArgv(["npm", "install"]), false);
  assert.equal(isAllowedArgv(["npm", "run", "build"]), false);
  assert.equal(isAllowedArgv(["docker", "run", "alpine"]), false);
  assert.equal(isAllowedArgv(["kubectl", "apply", "-f", "x"]), false);
  assert.equal(isAllowedArgv(["brew", "install", "git"]), false);
  assert.equal(isAllowedArgv(["cargo", "build"]), false);
  assert.equal(isAllowedArgv(["gh", "pr", "create"]), false);
  assert.equal(isAllowedArgv(["aws-vault", "exec", "dev"]), false);
  assert.equal(isAllowedArgv(["curl", "-s", "https://example.com"]), false);
  assert.equal(isAllowedArgv(["env"]), false);
  assert.equal(isAllowedArgv(["pbpaste"]), false);
  assert.equal(isAllowedArgv(["unknown-cli", "list"]), false);
  assert.equal(isAllowedArgv(["bash", "-c", "rm -rf /"]), false);
});

test("gitSubcommand skips git global flags", () => {
  assert.equal(gitSubcommand(["git", "--no-optional-locks", "branch", "-a"]), "branch");
  assert.equal(gitSubcommand(["git", "-C", "/repo", "status"]), "status");
});

test("shellScriptAllowed blocks mutation and secret tools", () => {
  assert.equal(shellScriptAllowed("ls -1 node_modules/.bin"), true);
  assert.equal(shellScriptAllowed("curl -s https://example.com"), false);
  assert.equal(shellScriptAllowed("git checkout main"), false);
});

test("redactText replaces home, username, tokens, IPs and 40-char hex", () => {
  const text = redactText(
    [
      "path=/Users/ada/.ssh/id",
      "user=ada",
      "token=ghp_abcdefghijklmnopqrstuvwxyz0123",
      "openai=sk-abcdefghijklmnopqrstuvwxyz",
      "key=AKIAIOSFODNN7EXAMPLE",
      "sha=0123456789abcdef0123456789abcdef01234567",
      "ip=10.1.2.3",
      "note=task-level",
    ].join("\n"),
    { home: "/Users/ada", username: "ada" },
  );
  assert.equal(text.includes("/Users/"), false);
  assert.equal(text.includes("ghp_"), false);
  assert.equal(text.includes("sk-"), false);
  assert.match(text, /task_level/);
  assert.match(text, /REDACTED_GITHUB_TOKEN/);
  assert.match(text, /REDACTED_SECRET_KEY/);
  assert.match(text, /REDACTED_ACCESS_KEY/);
  assert.match(text, /REDACTED_HEX40/);
  assert.match(text, /0\.0\.0\.0/);
  assert.match(text, /\$HOME/);
  assert.match(text, /\$USER/);
});

test("truncateText caps UTF-8 length at 64 KiB", () => {
  const huge = "é".repeat(CLI_OUTPUT_MAX_BYTES);
  const cut = truncateText(huge);
  assert.equal(cut.truncated, true);
  assert.ok(Buffer.byteLength(cut.text, "utf8") <= CLI_OUTPUT_MAX_BYTES);
});

test("recordOneArgv skips denied argv without calling execute", async () => {
  let called = 0;
  const sample = await recordOneArgv(["git", "checkout", "main"], {
    executeArgv: async () => {
      called += 1;
      return { stdout: "nope", stderr: "", status: 0 };
    },
  });
  assert.equal(called, 0);
  assert.equal(sample.status, 127);
  assert.equal(sample.skipped, true);
  assert.equal(sample.skipReason, "not-whitelisted");
});

test("collects IR scripts and input-fixture exec requests", async () => {
  const irRoot = await mkdtemp(join(tmpdir(), "easy-complete-cli-ir-"));
  const inputsDir = await mkdtemp(join(tmpdir(), "easy-complete-cli-in-"));
  try {
    await writeFile(
      join(irRoot, "demo.json"),
      JSON.stringify({
        names: ["demo"],
        args: [{ script: ["git", "branch", "--no-color"], jsPostProcess: "demo#postProcess#0" }],
      }),
    );
    await mkdir(join(inputsDir, "custom"), { recursive: true });
    await writeFile(
      join(inputsDir, "custom", "body.json"),
      JSON.stringify({
        cases: [
          {
            exec: [{ command: "npm", args: ["prefix"], status: 127, stdout: "", stderr: "command not found" }],
          },
        ],
      }),
    );
    const argvList = await collectCliArgvList({ irRoot, inputsDir });
    assert.deepEqual(
      argvList.map((argv) => argv.join(" ")).sort(),
      ["git branch --no-color", "npm prefix"],
    );
  } finally {
    await Promise.all([
      rm(irRoot, { recursive: true, force: true }),
      rm(inputsDir, { recursive: true, force: true }),
    ]);
  }
});

test("record + check writes digest-named files and fails after tamper", async () => {
  const irRoot = await mkdtemp(join(tmpdir(), "easy-complete-cli-ir-"));
  const inputsDir = await mkdtemp(join(tmpdir(), "easy-complete-cli-in-"));
  const outputDir = await mkdtemp(join(tmpdir(), "easy-complete-cli-out-"));
  try {
    await writeFile(
      join(irRoot, "demo.json"),
      JSON.stringify({
        names: ["demo"],
        args: [{ script: ["git", "branch"], jsPostProcess: "demo#postProcess#0" }],
      }),
    );
    const recorded = await recordCliOutputs({
      irRoot,
      inputsDir,
      outputDir,
      write: true,
      executeArgv: async (argv) => ({
        stdout: `branches for ${argv.join(" ")}\n/Users/ada/secret ghp_abcdefghijklmnopqrstuvwxyz0123`,
        stderr: "",
        status: 0,
      }),
      extras: { home: "/Users/ada", username: "ada" },
    });
    assert.equal(recorded.count, 1);
    assert.equal(recorded.recorded, 1);
    const digest = argvDigest(["git", "branch"]);
    const file = join(outputDir, `${digest}.json`);
    const sample = JSON.parse(await readFile(file, "utf8"));
    assert.deepEqual(sample.argv, ["git", "branch"]);
    assert.equal(sample.stdout.includes("/Users/"), false);
    assert.match(sample.stdout, /REDACTED_GITHUB_TOKEN/);
    assert.equal(sample.status, 0);

    await checkCliOutputs({ irRoot, inputsDir, outputDir });
    await writeFile(file, JSON.stringify({ ...sample, stdout: "/Users/ada leaked" }));
    await assert.rejects(() => checkCliOutputs({ irRoot, inputsDir, outputDir }), /stale|unredacted/);
  } finally {
    await Promise.all([
      rm(irRoot, { recursive: true, force: true }),
      rm(inputsDir, { recursive: true, force: true }),
      rm(outputDir, { recursive: true, force: true }),
    ]);
  }
});

test("missing-only recording keeps existing samples and prunes stale ones", async () => {
  const irRoot = await mkdtemp(join(tmpdir(), "easy-complete-cli-ir-"));
  const inputsDir = await mkdtemp(join(tmpdir(), "easy-complete-cli-in-"));
  const outputDir = await mkdtemp(join(tmpdir(), "easy-complete-cli-out-"));
  const writeSpec = (scripts) =>
    writeFile(
      join(irRoot, "demo.json"),
      JSON.stringify({
        names: ["demo"],
        args: scripts.map((script, index) => ({ script, jsPostProcess: `demo#postProcess#${index}` })),
      }),
    );
  const executed = [];
  const executeArgv = async (argv) => {
    executed.push(argv.join(" "));
    return { stdout: `out ${argv.join(" ")}`, stderr: "", status: 0 };
  };
  try {
    await writeSpec([["git", "branch"], ["git", "tag", "--list"]]);
    await recordCliOutputs({ irRoot, inputsDir, outputDir, write: true, executeArgv });
    const kept = join(outputDir, `${argvDigest(["git", "branch"])}.json`);
    const keptText = await readFile(kept, "utf8");

    // The spec swaps one script: only the new argv runs, the dropped one goes.
    await writeSpec([["git", "branch"], ["git", "branch", "-a"]]);
    executed.length = 0;
    const result = await recordCliOutputs({
      irRoot,
      inputsDir,
      outputDir,
      write: true,
      executeArgv,
      missingOnly: true,
    });
    assert.deepEqual(executed, ["git branch -a"]);
    assert.equal(result.pruned, 1);
    assert.equal(await readFile(kept, "utf8"), keptText);
    await assert.rejects(
      readFile(join(outputDir, `${argvDigest(["git", "tag", "--list"])}.json`)),
      { code: "ENOENT" },
    );
    await checkCliOutputs({ irRoot, inputsDir, outputDir });
  } finally {
    await Promise.all([
      rm(irRoot, { recursive: true, force: true }),
      rm(inputsDir, { recursive: true, force: true }),
      rm(outputDir, { recursive: true, force: true }),
    ]);
  }
});

test("finalizeCliOutputSample marks truncated output", () => {
  const sample = finalizeCliOutputSample({
    argv: ["echo", "x"],
    stdout: "n".repeat(CLI_OUTPUT_MAX_BYTES + 8),
    stderr: "",
    status: 0,
  }, { redact: false });
  assert.equal(sample.truncated, true);
  assert.ok(Buffer.byteLength(sample.stdout, "utf8") <= CLI_OUTPUT_MAX_BYTES);
});
