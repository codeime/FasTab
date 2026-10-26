#!/usr/bin/env node
/**
 * Capture per-function-body hook output baselines from source closures.
 * Input fixtures live under testdata/native-hooks/inputs; outputs go to
 * testdata/native-hooks/baseline. Fully offline: the reference worker mocks
 * every executeCommand and the parent watchdog owns timeout cases.
 */
import { mkdir, readdir, readFile, writeFile } from "node:fs/promises";
import { dirname, join } from "node:path";
import { pathToFileURL } from "node:url";

import { withReferenceAudit } from "./reference-audit-worker.mjs";
import { comparePath } from "./spec-pair.mjs";
import {
  baselineRelativePath,
  baselineRoot,
  baselineTextsEquivalent,
  captureCases,
  finalizeBaseline,
  fixtureIdentity,
  inputRoot,
  loadBodyGroups,
  repoDir,
  stableStringify,
  validateInputFixture,
} from "./hook-baseline-lib.mjs";
import { scaffoldHookInputs } from "./scaffold-hook-inputs.mjs";

const defaultSourceRoot = join(repoDir, "bundle", "specs");
const defaultIrRoot = join(repoDir, "bundle", "specs-ir");

export async function listJsonFiles(root) {
  const files = [];
  let fields = [];
  try {
    fields = await readdir(root, { withFileTypes: true });
  } catch {
    return files;
  }
  for (const field of fields.sort((left, right) => comparePath(left.name, right.name))) {
    if (!field.isDirectory()) continue;
    const names = (await readdir(join(root, field.name))).sort(comparePath);
    for (const name of names) {
      if (name.endsWith(".json")) files.push({ field: field.name, name, path: join(root, field.name, name) });
    }
  }
  return files;
}

export async function loadInputFixture(path) {
  const value = JSON.parse(await readFile(path, "utf8"));
  return validateInputFixture(value);
}

function assertInputMatchesGroup(input, group) {
  if (input.field !== group.sourceField) {
    throw new Error(
      `input field ${input.field} does not match body group ${group.sourceField}`,
    );
  }
  if (input.bodySha256 !== group.bodySha256) {
    throw new Error(`input bodySha256 does not match group ${group.bodySha256}`);
  }
  if (input.cases.length < 3) {
    throw new Error(
      `${input.field}/${input.bodySha256} has ${input.cases.length} cases; need at least 3`,
    );
  }
  // The 50 ms is the parent watchdog's budget for the whole child: process
  // start, module compile under --experimental-vm-modules, then the body. 595
  // of the 603 bodies do not finish inside it and record `timeout`; the eight
  // that throw immediately record that error instead. A body near that
  // boundary therefore records a different outcome on a faster or less loaded
  // machine. --check treats timeout vs immediate error as equivalent on the
  // timeout case so neither a fast-side nor a slow-side recapture of those
  // eight turns the other machines red. Do not --update them just to pick a
  // spelling.
  if (!input.cases.some((item) => item.id === "timeout" && item.timeoutMs === 50)) {
    throw new Error(`${input.field}/${input.bodySha256} is missing a timeout case`);
  }
}

export async function captureHookBaselines({
  sourceRoot = defaultSourceRoot,
  irRoot = defaultIrRoot,
  inputsDir = inputRoot,
  baselinesDir = baselineRoot,
  write = false,
} = {}) {
  return withReferenceAudit({ sourceRoot, irRoot }, async () => {
    const { report, groups } = await loadBodyGroups({ sourceRoot, irRoot });
    const groupsByKey = new Map(
      groups.map((group) => [`${group.sourceField}/${group.bodySha256}`, group]),
    );
    const inputFiles = await listJsonFiles(inputsDir);
    if (inputFiles.length !== groups.length) {
      throw new Error(
        `input fixture count ${inputFiles.length} != unique bodies ${groups.length}; run scaffold-hook-inputs.mjs`,
      );
    }

    const baselines = [];
    const fieldTimeout = new Map();
    for (const file of inputFiles) {
      const input = await loadInputFixture(file.path);
      const group = groupsByKey.get(`${input.field}/${input.bodySha256}`);
      if (!group) {
        throw new Error(`no inventory body for ${input.field}/${input.bodySha256}`);
      }
      assertInputMatchesGroup(input, group);
      const hookId = input.representativeHookId;
      const expected = await captureCases({
        hookId,
        field: input.field,
        cases: input.cases,
        sourceRoot,
        irRoot,
      });
      const baseline = finalizeBaseline({
        ...fixtureIdentity(group),
        representativeHookId: hookId,
        cases: input.cases.map((item, index) => {
          const { synthetic: _synthetic, ...rest } = item;
          return { ...rest, expected: expected[index] };
        }),
      });
      if (baseline.cases.some((item) => item.expected.kind === "timeout")) {
        fieldTimeout.set(input.field, (fieldTimeout.get(input.field) ?? 0) + 1);
      }
      const text = stableStringify(baseline);
      const dest = join(baselinesDir, input.field, `${input.bodySha256}.json`);
      baselines.push({
        field: input.field,
        bodySha256: input.bodySha256,
        path: dest,
        relative: baselineRelativePath(input.field, input.bodySha256),
        text,
        baseline,
      });
    }

    for (const field of new Set(groups.map((group) => group.sourceField))) {
      if ((fieldTimeout.get(field) ?? 0) < 1) {
        throw new Error(`field ${field} has no timeout baseline case`);
      }
    }

    const existing = write ? [] : await listJsonFiles(baselinesDir);
    if (!write) {
      const existingByKey = new Map(
        existing.map((file) => [`${file.field}/${file.name}`, file]),
      );
      const mismatches = [];
      for (const item of baselines) {
        const key = `${item.field}/${item.bodySha256}.json`;
        const found = existingByKey.get(key);
        if (!found) {
          mismatches.push(`missing ${item.relative}`);
          continue;
        }
        const actual = await readFile(found.path, "utf8");
        if (!baselineTextsEquivalent(actual, item.text)) {
          mismatches.push(`drift ${item.relative}`);
        }
      }
      for (const file of existing) {
        const key = `${file.field}/${file.name}`;
        if (!baselines.some((item) => `${item.field}/${item.bodySha256}.json` === key)) {
          mismatches.push(`extra ${file.field}/${file.name}`);
        }
      }
      if (mismatches.length) {
        throw new Error(
          `hook output baselines are stale (${mismatches.length}): ${mismatches.slice(0, 8).join("; ")}`,
        );
      }
    } else {
      for (const item of baselines) {
        // --check accepts a timeout case recorded as an immediate error, and
        // which one a run records depends on the machine. Leave an equivalent
        // file as it was recorded so an update touches only real drift.
        const current = await readFile(item.path, "utf8").catch(() => null);
        if (current !== null && baselineTextsEquivalent(current, item.text)) continue;
        await mkdir(dirname(item.path), { recursive: true });
        await writeFile(item.path, item.text);
      }
    }

    return {
      count: baselines.length,
      uniqueBodies: report.coverage.uniqueBodies,
      written: write,
      report,
    };
  });
}

export async function updateHookBaselines(options = {}) {
  await scaffoldHookInputs({
    sourceRoot: options.sourceRoot,
    irRoot: options.irRoot,
    root: options.inputsDir,
  });
  return captureHookBaselines({ ...options, write: true });
}

export async function checkHookBaselines(options = {}) {
  return captureHookBaselines({ ...options, write: false });
}

const isMain =
  process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href;

if (isMain) {
  const check = process.argv.includes("--check");
  const update = process.argv.includes("--update");
  if (check === update) {
    process.stderr.write("error: choose exactly one of --check or --update\n");
    process.exitCode = 2;
  } else if (update) {
    const result = await updateHookBaselines();
    process.stdout.write(
      `Updated hook output baselines: ${result.count} files\n`,
    );
  } else {
    try {
      const result = await checkHookBaselines();
      process.stdout.write(
        `Verified hook output baselines: ${result.count} files\n`,
      );
    } catch (error) {
      process.stderr.write(
        `hook output baseline check failed: ${error instanceof Error ? error.message : error}\n`,
      );
      process.exitCode = 1;
    }
  }
}
