import assert from "node:assert/strict";
import { mkdtemp, readFile, readdir, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";

import { compileSpecsIr } from "./compile-spec-ir.mjs";
import { withReferenceAudit } from "./reference-audit-worker.mjs";
import { captureHookReference } from "./capture-hook-reference.mjs";
import {
  checkHookBaselines,
  updateHookBaselines,
} from "./capture-hook-baseline.mjs";
import {
  baselineTextsEquivalent,
  stableStringify,
} from "./hook-baseline-lib.mjs";

async function withFixture(body, run) {
  const sourceRoot = await mkdtemp(join(tmpdir(), "easy-complete-baseline-src-"));
  const irRoot = await mkdtemp(join(tmpdir(), "easy-complete-baseline-ir-"));
  const inputsDir = await mkdtemp(join(tmpdir(), "easy-complete-baseline-in-"));
  const baselinesDir = await mkdtemp(join(tmpdir(), "easy-complete-baseline-out-"));
  try {
    await writeFile(join(sourceRoot, "factory.js"), body);
    await compileSpecsIr({ srcDir: sourceRoot, outDir: irRoot });
    await run({ sourceRoot, irRoot, inputsDir, baselinesDir });
  } finally {
    await Promise.all([
      rm(sourceRoot, { recursive: true, force: true }),
      rm(irRoot, { recursive: true, force: true }),
      rm(inputsDir, { recursive: true, force: true }),
      rm(baselinesDir, { recursive: true, force: true }),
    ]);
  }
}

function factorySource() {
  return `export default {
    name: "factory",
    args: [
      {
        name: "rows",
        generators: {
          postProcess(out) {
            return out.trim() ? [{ name: out.trim() }] : [];
          },
          custom: async function(tokens, exec) {
            const extra = await exec({ command: "list", args: tokens });
            return [{ name: extra.stdout.trim() || tokens[0] }];
          },
        },
      },
    ],
  };\n`;
}

function sampleBaseline(timeoutExpected) {
  return {
    bodySha256: "a".repeat(64),
    cases: [
      {
        args: [["fig"]],
        context: { currentWorkingDirectory: "/repo" },
        exec: [],
        expected: { kind: "error", value: "Error" },
        id: "root",
        timeoutMs: 5000,
      },
      {
        args: [["fig"]],
        context: { currentWorkingDirectory: "/repo" },
        exec: [{ delayMs: 10_000 }],
        expected: timeoutExpected,
        id: "timeout",
        timeoutMs: 50,
      },
    ],
    field: "custom",
    hookCount: 1,
    kind: "native-hook-baseline",
    representativeHookId: "fig/1.0.0#custom#6",
    version: 1,
  };
}

test("timeout-case error and timeout are equivalent; other drift is not", () => {
  const errorText = stableStringify(sampleBaseline({ kind: "error", value: "Error" }));
  const timeoutText = stableStringify(sampleBaseline({ kind: "timeout" }));
  const otherErrorText = stableStringify(sampleBaseline({ kind: "error", value: "TypeError" }));
  const successText = stableStringify(sampleBaseline({ kind: "suggestions", value: [] }));
  assert.equal(baselineTextsEquivalent(errorText, timeoutText), true);
  assert.equal(baselineTextsEquivalent(timeoutText, errorText), true);
  assert.equal(baselineTextsEquivalent(errorText, errorText), true);
  assert.equal(baselineTextsEquivalent(errorText, otherErrorText), false);
  assert.equal(baselineTextsEquivalent(errorText, successText), false);
  assert.equal(baselineTextsEquivalent(timeoutText, successText), false);

  const tamperedRoot = sampleBaseline({ kind: "timeout" });
  tamperedRoot.cases[0].expected = { kind: "suggestions", value: [{ name: "x" }] };
  assert.equal(baselineTextsEquivalent(errorText, stableStringify(tamperedRoot)), false);
  assert.equal(baselineTextsEquivalent("{", timeoutText), false);
});

test("captures a small fixture and --check fails after tampering", async () => {
  await withFixture(factorySource(), async (roots) => {
    const updated = await updateHookBaselines(roots);
    assert.ok(updated.count >= 2, "postProcess and custom bodies");
    const checked = await checkHookBaselines(roots);
    assert.equal(checked.count, updated.count);

    const fields = await readdir(roots.baselinesDir);
    assert.ok(fields.includes("postProcess"));
    const names = await readdir(join(roots.baselinesDir, "postProcess"));
    const file = join(roots.baselinesDir, "postProcess", names[0]);
    const original = await readFile(file, "utf8");
    await writeFile(file, original.replace("factory", "tampered"));
    await assert.rejects(() => checkHookBaselines(roots), /stale|drift/);
  });
});

test("timeout-case error spelling does not fail --check", async () => {
  await withFixture(factorySource(), async (roots) => {
    await updateHookBaselines(roots);
    const names = await readdir(join(roots.baselinesDir, "postProcess"));
    const file = join(roots.baselinesDir, "postProcess", names[0]);
    const baseline = JSON.parse(await readFile(file, "utf8"));
    const timeout = baseline.cases.find((item) => item.id === "timeout");
    assert.equal(timeout.expected.kind, "timeout");
    timeout.expected = { kind: "error", value: "Error" };
    await writeFile(file, stableStringify(baseline));
    await checkHookBaselines(roots);

    timeout.expected = { kind: "suggestions", value: [] };
    await writeFile(file, stableStringify(baseline));
    await assert.rejects(() => checkHookBaselines(roots), /stale|drift/);
  });
});

test("--update keeps an equivalently spelled timeout case as recorded", async () => {
  await withFixture(factorySource(), async (roots) => {
    await updateHookBaselines(roots);
    const names = await readdir(join(roots.baselinesDir, "postProcess"));
    const file = join(roots.baselinesDir, "postProcess", names[0]);
    const baseline = JSON.parse(await readFile(file, "utf8"));
    baseline.cases.find((item) => item.id === "timeout").expected = { kind: "error", value: "Error" };
    const recorded = stableStringify(baseline);
    await writeFile(file, recorded);

    await updateHookBaselines(roots);
    assert.equal(await readFile(file, "utf8"), recorded);

    // Real drift is still rewritten.
    await writeFile(file, recorded.replace("factory", "tampered"));
    await updateHookBaselines(roots);
    assert.notEqual(await readFile(file, "utf8"), recorded.replace("factory", "tampered"));
    await checkHookBaselines(roots);
  });
});

test("mock delayMs triggers a parent timeout", async () => {
  await withFixture(factorySource(), async ({ sourceRoot, irRoot }) => {
    await withReferenceAudit({ sourceRoot, irRoot }, async (audit) => {
      const hookId = audit.sourceToIr[0].hookInstances.custom[0].id;
      const result = await captureHookReference({
        hookId,
        args: [["factory"], { $referenceExec: true }, {
          currentWorkingDirectory: "/repo",
          currentProcess: "zsh",
          sshPrefix: "",
          environmentVariables: { HOME: "/home/user" },
          searchTerm: "",
          isDangerous: false,
        }],
        mockExecRules: [{ command: "list", args: ["factory"], delayMs: 10_000, stdout: "x", stderr: "", status: 0 }],
        timeoutMs: 50,
        sourceRoot,
        irRoot,
      });
      assert.equal(result.status, "timeout");
    });
  });
});
