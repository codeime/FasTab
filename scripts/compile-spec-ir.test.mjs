import assert from "node:assert/strict";
import { execFile, execFileSync } from "node:child_process";
import {
  lstat,
  mkdir,
  mkdtemp,
  readFile,
  readdir,
  rm,
  writeFile,
  symlink,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { promisify } from "node:util";
import test from "node:test";

const execFileAsync = promisify(execFile);

const compilerCli = join(
  dirname(fileURLToPath(import.meta.url)),
  "compile-spec-ir.mjs",
);
const canonicalSourceDir = join(
  dirname(fileURLToPath(import.meta.url)),
  "..",
  "bundle",
  "specs",
);
const canonicalIrDir = join(
  dirname(fileURLToPath(import.meta.url)),
  "..",
  "bundle",
  "specs-ir",
);

async function runCompilerCli(environment) {
  try {
    const { stdout, stderr } = await execFileAsync(
      process.execPath,
      [compilerCli],
      {
        env: { ...process.env, ...environment },
        maxBuffer: 1_000_000,
      },
    );
    return { status: 0, output: `${stdout}${stderr}` };
  } catch (error) {
    return {
      status: error.status ?? 1,
      output: `${error.stdout ?? ""}${error.stderr ?? ""}`,
    };
  }
}

async function readIfPresent(path) {
  try {
    return await readFile(path);
  } catch (error) {
    if (error?.code === "ENOENT") return null;
    throw error;
  }
}

const bundledBun = join(
  dirname(fileURLToPath(import.meta.url)),
  "..",
  "bundle",
  "specs",
  "bun.js",
);

import {
  auditSpecsHooks,
  freeVariableCandidates,
} from "./audit-spec-hooks.mjs";
import {
  compileSpecsIr,
  hookFileName,
  TYPED_HOOK_SIDECAR,
  transformDefaultExport,
} from "./compile-spec-ir.mjs";
import {
  createFilepathsBinder,
  functionSource,
  parseFilepathsCalls,
} from "./filepaths-helper.mjs";
import { compileTypedHook } from "./typed-hook-ir.mjs";

test("method-source normalization preserves valid JS method variants and arrows", async () => {
  const hooks = {
    async(value) {
      return value;
    },
    default(value) {
      return value;
    },
    delete(value) {
      return value;
    },
    *z(value) {
      yield value;
    },
    postProcess /* comment */(value) {
      return [value].map((item) => item);
    },
    custom(value) {
      return value;
    },
    ["computed"](value) {
      return value;
    },
    async *generateSpec(value) {
      yield value;
    },
    async *postProcess(value) {
      yield value;
    },
    selfBinding(value) {
      return typeof selfBinding + value;
    },
  };
  for (const hook of Object.values(hooks)) {
    const source = functionSource(hook);
    const converted = new Function(`return (${source})`)();
    assert.equal(typeof converted, "function");
  }
  assert.equal(
    new Function(`return (${functionSource(hooks.selfBinding)})`)()(1),
    hooks.selfBinding(1),
  );
  assert.equal(functionSource(hooks.selfBinding).startsWith("function("), true);
  const asyncArrow = async (value) => value;
  assert.equal(functionSource(asyncArrow), asyncArrow.toString());
});

test("compiler emits shared hook modules that preserve closures and custom receivers", async () => {
  const srcDir = await mkdtemp(join(tmpdir(), "easy-complete-closure-src-"));
  const outDir = await mkdtemp(join(tmpdir(), "easy-complete-closure-ir-"));
  try {
    await writeFile(
      join(srcDir, "closure.js"),
      `const metadata = { priority: 73, moduleThis: typeof this };
       const post = (label) => (stdout) => [{ name: label + ":" + stdout, ...metadata }];
       const custom = (label) => ({
         label,
         custom(tokens) { return [{ name: this.label + ":" + tokens.at(-1) }]; },
       });
       export default {
         name: "closure",
         args: [
           { name: "one", generators: { postProcess: post("one") } },
           { name: "two", generators: { postProcess: post("two") } },
           { name: "three", generators: custom("three") },
           { name: "four", generators: custom("four") },
         ],
       };
      `,
    );
    await compileSpecsIr({ srcDir, outDir });
    const ir = JSON.parse(await readFile(join(outDir, "closure.json"), "utf8"));
    const sidecar = JSON.parse(
      await readFile(join(outDir, "typed-hooks.json"), "utf8"),
    );
    await assert.rejects(lstat(join(outDir, "hook-modules.json")), {
      code: "ENOENT",
    });
    await assert.rejects(lstat(join(outDir, "source-modules")), {
      code: "ENOENT",
    });
    await assert.rejects(lstat(join(outDir, "hooks")), { code: "ENOENT" });
    const ids = ir.args.map(
      (arg) => arg.generators[0].jsPostProcess ?? arg.generators[0].jsCustom,
    );
    assert.equal(new Set(ids).size, 4);
    const customIds = ir.args
      .map((arg) => arg.generators[0].jsCustom)
      .filter(Boolean);
    assert.equal(
      customIds.every((id) => !sidecar.hooks[id] && !sidecar.adapters?.[id]),
      true,
      "this-binding customs stay off the sidecar until a named adapter exists",
    );
    const typedModules = new Set(
      Object.values(sidecar.hooks)
        .map((entry) => entry.module)
        .filter(Boolean),
    );
    assert.ok(typedModules.size <= 1);
  } finally {
    await Promise.all([
      rm(srcDir, { recursive: true, force: true }),
      rm(outDir, { recursive: true, force: true }),
    ]);
  }
});

test("a git script with a shaper keeps its argv instead of a native builtin", async () => {
  const srcDir = await mkdtemp(join(tmpdir(), "easy-complete-git-builtin-src-"));
  const outDir = await mkdtemp(join(tmpdir(), "easy-complete-git-builtin-ir-"));
  try {
    await writeFile(
      join(srcDir, "git.js"),
      `const branches = ["git", "--no-optional-locks", "branch", "-a", "--no-color", "--sort=-committerdate"];
       export default {
         name: "git",
         subcommands: [
           {
             name: "checkout",
             args: {
               name: "branch",
               generators: {
                 script: branches,
                 postProcess: (out) => out.split("\\n").map((name) => ({ name: name.trim() })),
               },
             },
           },
           { name: "log", args: { name: "commit", generators: { script: ["git", "log", "--oneline"], splitOn: "\\n" } } },
           { name: "bare", args: { name: "branch", generators: { script: ["git", "branch", "--no-color"] } } },
         ],
       };
      `,
    );
    await compileSpecsIr({ srcDir, outDir });
    const ir = JSON.parse(await readFile(join(outDir, "git.json"), "utf8"));
    const generator = (name) =>
      ir.subcommands.find((sub) => sub.names.includes(name)).args[0].generators[0];

    // Fig ran this exact argv and shaped it with postProcess; a builtin listing
    // would starve the hook of the current-branch marker and recency order.
    const checkout = generator("checkout");
    assert.deepEqual(checkout.script, [
      "git",
      "--no-optional-locks",
      "branch",
      "-a",
      "--no-color",
      "--sort=-committerdate",
    ]);
    assert.ok(checkout.jsPostProcess);
    assert.equal(checkout.builtin, undefined);

    const log = generator("log");
    assert.deepEqual(log.script, ["git", "log", "--oneline"]);
    assert.equal(log.splitOn, "\n");
    assert.equal(log.builtin, undefined);

    // Fig shows nothing for an unshaped script; the builtin is pure gain there.
    const bare = generator("bare");
    assert.equal(bare.builtin, "git-branches");
    assert.equal(bare.script, undefined);
  } finally {
    await Promise.all([
      rm(srcDir, { recursive: true, force: true }),
      rm(outDir, { recursive: true, force: true }),
    ]);
  }
});

test("closure modules bind ids to compiler-selected paths when texts collide", async () => {
  const srcDir = await mkdtemp(join(tmpdir(), "easy-complete-binding-src-"));
  const outDir = await mkdtemp(join(tmpdir(), "easy-complete-binding-ir-"));
  try {
    await writeFile(
      join(srcDir, "binding.js"),
      `const post = (label) => (stdout) => [{ name: label + ":" + stdout }];
       const sharedCustom = function (tokens) {
         return [{ name: this.label + ":" + tokens[0] }];
       };
       const custom = (label, isPersistent) => ({
         name: "--" + label,
         label,
         isPersistent,
         args: { generators: {
           label,
           custom: sharedCustom,
         } },
       });
       const args = Array.from({ length: 11 }, (_, index) => ({
         name: String(index),
         generators: { postProcess: post(String(index)) },
       }));
       export default {
         name: "binding",
         args,
         options: [custom("persistent", true), custom("regular", false)],
       };
      `,
    );
    await compileSpecsIr({ srcDir, outDir });
    const ir = JSON.parse(await readFile(join(outDir, "binding.json"), "utf8"));
    const sidecar = JSON.parse(
      await readFile(join(outDir, "typed-hooks.json"), "utf8"),
    );
    await assert.rejects(lstat(join(outDir, "hook-modules.json")), {
      code: "ENOENT",
    });
    const regularId = ir.options[0].args[0].generators[0].jsCustom;
    const persistentId = ir.persistentOptions[0].args[0].generators[0].jsCustom;
    assert.equal(typeof regularId, "string");
    assert.equal(typeof persistentId, "string");
    assert.notEqual(regularId, persistentId);
    assert.equal(sidecar.hooks[regularId], undefined);
    assert.equal(sidecar.hooks[persistentId], undefined);
    assert.equal(sidecar.adapters?.[regularId], undefined);
    assert.equal(sidecar.adapters?.[persistentId], undefined);
    for (const [index, arg] of ir.args.entries()) {
      const id = arg.generators[0].jsPostProcess;
      assert.equal(typeof id, "string", `extracted postProcess ${index}`);
    }
  } finally {
    await Promise.all([
      rm(srcDir, { recursive: true, force: true }),
      rm(outDir, { recursive: true, force: true }),
    ]);
  }
});

test("compiler CLI completes its pre-manifest audit without a module cycle", async () => {
  const srcDir = await mkdtemp(join(tmpdir(), "easy-complete-cli-cycle-src-"));
  const outDir = await mkdtemp(join(tmpdir(), "easy-complete-cli-cycle-ir-"));
  try {
    await writeFile(
      join(srcDir, "cycle.js"),
      `const suffix = "-kept";
       export default {
         name: "cycle",
         args: { generators: { postProcess: (stdout) => [{ name: stdout + suffix }] } },
       };\n`,
    );
    const compiler = join(
      dirname(fileURLToPath(import.meta.url)),
      "compile-spec-ir.mjs",
    );
    const { stdout, stderr } = await execFileAsync(
      process.execPath,
      [compiler],
      {
        env: {
          ...process.env,
          EC_SPECS_SRC: srcDir,
          EC_SPECS_IR: outDir,
        },
        maxBuffer: 1_000_000,
      },
    );
    assert.equal(stderr, "");
    assert.match(stdout, /1 in-memory closure modules/);
    const sidecar = JSON.parse(
      await readFile(join(outDir, "typed-hooks.json"), "utf8"),
    );
    assert.equal(Object.keys(sidecar.hooks).length, 1);
    await assert.rejects(lstat(join(outDir, "hook-modules.json")), {
      code: "ENOENT",
    });
  } finally {
    await Promise.all([
      rm(srcDir, { recursive: true, force: true }),
      rm(outDir, { recursive: true, force: true }),
    ]);
  }
});

test("compiler CLI rejects canonical/custom mixing before publishing either pair", async () => {
  const root = await mkdtemp(join(tmpdir(), "easy-complete-cli-pair-guard-"));
  const customSourceDir = join(root, "source");
  const customIrDir = join(root, "custom-ir");
  const reverseIrDir = join(root, "reverse-ir");
  const canonicalSourceIndex = join(canonicalSourceDir, "index.json");
  const canonicalIrIndex = join(canonicalIrDir, "index.json");
  const canonicalPairMarker = join(canonicalIrDir, ".spec-pair.json");
  try {
    await mkdir(customSourceDir);
    await writeFile(
      join(customSourceDir, "fixture.js"),
      `export default { name: "fixture" };\n`,
    );
    const sourceIndexBefore = await readIfPresent(canonicalSourceIndex);
    const irIndexBefore = await readIfPresent(canonicalIrIndex);
    const pairMarkerBefore = await readIfPresent(canonicalPairMarker);

    const customSourceWithCanonicalIr = await runCompilerCli({
      EC_SPECS_SRC: customSourceDir,
      EC_SPECS_IR: canonicalIrDir,
    });
    assert.notEqual(customSourceWithCanonicalIr.status, 0);
    assert.match(
      customSourceWithCanonicalIr.output,
      /custom outputs require both destinations/,
    );
    assert.equal(await readIfPresent(join(customIrDir, "index.json")), null);

    await mkdir(reverseIrDir);
    const reverseSentinel = join(reverseIrDir, "USER_SENTINEL");
    await writeFile(reverseSentinel, "keep me\n");
    const canonicalSourceWithCustomIr = await runCompilerCli({
      EC_SPECS_SRC: canonicalSourceDir,
      EC_SPECS_IR: reverseIrDir,
    });
    assert.notEqual(canonicalSourceWithCustomIr.status, 0);
    assert.match(
      canonicalSourceWithCustomIr.output,
      /custom outputs require both destinations/,
    );
    assert.equal(await readFile(reverseSentinel, "utf8"), "keep me\n");

    // The guard runs before compileSpecsIr can create a staging directory,
    // move an existing output, or rewrite the canonical pair marker.
    assert.deepEqual(await readIfPresent(canonicalSourceIndex), sourceIndexBefore);
    assert.deepEqual(await readIfPresent(canonicalIrIndex), irIndexBefore);
    assert.deepEqual(await readIfPresent(canonicalPairMarker), pairMarkerBefore);
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test("compiler CLI rejects overlapping custom source and IR paths before rewriting them", async () => {
  const root = await mkdtemp(join(tmpdir(), "easy-complete-cli-overlap-guard-"));
  const sourceDir = join(root, "source");
  const nestedIrDir = join(sourceDir, "nested-ir");
  const outerIrDir = join(root, "outer-ir");
  const nestedSourceDir = join(outerIrDir, "nested-source");
  try {
    await mkdir(sourceDir);
    await writeFile(
      join(sourceDir, "fixture.js"),
      `export default { name: "fixture" };\n`,
    );
    const nestedResult = await runCompilerCli({
      EC_SPECS_SRC: sourceDir,
      EC_SPECS_IR: nestedIrDir,
    });
    assert.notEqual(nestedResult.status, 0);
    assert.match(nestedResult.output, /destinations cannot overlap/);
    assert.equal(await readIfPresent(join(nestedIrDir, "index.json")), null);

    await mkdir(nestedSourceDir, { recursive: true });
    await writeFile(
      join(nestedSourceDir, "fixture.js"),
      `export default { name: "fixture" };\n`,
    );
    const outerSentinel = join(outerIrDir, "USER_SENTINEL");
    await writeFile(outerSentinel, "keep me\n");
    const reverseResult = await runCompilerCli({
      EC_SPECS_SRC: nestedSourceDir,
      EC_SPECS_IR: outerIrDir,
    });
    assert.notEqual(reverseResult.status, 0);
    assert.match(reverseResult.output, /destinations cannot overlap/);
    assert.equal(await readFile(outerSentinel, "utf8"), "keep me\n");
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test("compiler CLI rejects symlinked source and IR paths before publishing", async () => {
  const root = await mkdtemp(join(tmpdir(), "easy-complete-cli-symlink-guard-"));
  const sourceTarget = join(root, "source-target");
  const sourceLink = join(root, "source-link");
  const sourceDir = join(root, "source");
  const irTarget = join(root, "ir-target");
  const irLink = join(root, "ir-link");
  try {
    await mkdir(sourceTarget);
    await writeFile(
      join(sourceTarget, "fixture.js"),
      `export default { name: "fixture" };\n`,
    );
    await symlink(sourceTarget, sourceLink);
    const sourceSymlinkResult = await runCompilerCli({
      EC_SPECS_SRC: sourceLink,
      EC_SPECS_IR: join(root, "source-link-ir"),
    });
    assert.notEqual(sourceSymlinkResult.status, 0);
    assert.match(sourceSymlinkResult.output, /through a symbolic link/);
    assert.equal(
      await readIfPresent(join(root, "source-link-ir", "index.json")),
      null,
    );

    await mkdir(sourceDir);
    await writeFile(
      join(sourceDir, "fixture.js"),
      `export default { name: "fixture" };\n`,
    );
    await mkdir(irTarget);
    const irSentinel = join(irTarget, "USER_SENTINEL");
    await writeFile(irSentinel, "keep me\n");
    await symlink(irTarget, irLink);
    const irSymlinkResult = await runCompilerCli({
      EC_SPECS_SRC: sourceDir,
      EC_SPECS_IR: irLink,
    });
    assert.notEqual(irSymlinkResult.status, 0);
    assert.match(irSymlinkResult.output, /through a symbolic link/);
    assert.equal(await readFile(irSentinel, "utf8"), "keep me\n");
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test("pinned Bun keeps split and post-process on separate generators", async () => {
  const srcDir = await mkdtemp(join(tmpdir(), "easy-complete-bun-src-"));
  const outDir = await mkdtemp(join(tmpdir(), "easy-complete-bun-ir-"));
  try {
    await writeFile(join(srcDir, "bun.js"), await readFile(bundledBun, "utf8"));
    await compileSpecsIr({ srcDir, outDir });
    const ir = JSON.parse(await readFile(join(outDir, "bun.json"), "utf8"));
    const visit = (value, output = []) => {
      if (!value || typeof value !== "object") return output;
      if (Array.isArray(value)) {
        for (const item of value) visit(item, output);
      } else {
        if (value.name === "template" && Array.isArray(value.generators)) {
          output.push(value);
        }
        for (const child of Object.values(value)) visit(child, output);
      }
      return output;
    };
    const [template] = visit(ir);
    assert.ok(template, "bundled Bun template generator must remain present");
    assert.equal(template.generators[2].splitOn, "\n");
    assert.equal(template.generators[2].jsPostProcess ?? null, null);
    assert.equal(template.generators[3].splitOn ?? null, null);
    assert.match(template.generators[3].jsPostProcess, /^bun#postProcess#/);
    assert.equal(template.splitOn, "\n");
    assert.match(template.jsPostProcess, /^bun#postProcess#/);
  } finally {
    await Promise.all([
      rm(srcDir, { recursive: true, force: true }),
      rm(outDir, { recursive: true, force: true }),
    ]);
  }
});

test("hook scope analysis ignores strings and finds lowercase closure references", () => {
  assert.deepEqual(freeVariableCandidates('(tokens) => "External("'), []);
  assert.deepEqual(freeVariableCandidates("(tokens) => external(tokens)"), [
    "external",
  ]);
  assert.deepEqual(
    freeVariableCandidates("(tokens) => console.log(tokens)"),
    [],
  );
  assert.deepEqual(
    freeVariableCandidates("(tokens) => new Uint8Array(tokens)"),
    [],
  );
});

test("compiler keeps static suggestion type metadata and string query terms", async () => {
  const srcDir = await mkdtemp(join(tmpdir(), "easy-complete-specs-"));
  const outDir = await mkdtemp(join(tmpdir(), "easy-complete-ir-"));
  try {
    await writeFile(
      join(srcDir, "fixture.js"),
      `export default {
        name: "fixture",
        parserDirectives: {
          optionsMustPrecedeArguments: true,
          flagsArePosixNoncompliant: true,
          optionArgSeparators: ["=", ":"],
        },
        filterStrategy: "fuzzy",
        args: [{
          name: "value",
          filterStrategy: "not-a-strategy",
          suggestCurrentToken: false,
          optionsCanBreakVariadicArg: false,
          generators: { getQueryTerm: "/", template: "filepaths" },
          suggestions: [
            {
              name: "file.txt",
              type: "file",
              originalType: "folder",
              getQueryTerm: "/",
              args: [{ name: "required" }, { name: "optional", isOptional: true }],
            },
            {
              name: "function-query",
              getQueryTerm: () => "ignored",
            },
          ],
        },
        {
          name: "script value",
          filterStrategy: "prefix",
          suggestCurrentToken: true,
          generators: {
            script: ["printf", "ok\\n"],
            scriptTimeout: 7000,
          },
        }],
        options: [{
          name: ["-c", "--color"],
          description: "Choose a color",
          insertValue: "--colour",
          displayName: "Color",
          requiresSeparator: ":",
          shouldAddSpace: false,
          hidden: true,
          priority: 0,
          icon: "🟡",
          isDangerous: true,
          exclusiveOn: ["--no-color", "-C"],
          dependsOn: ["--config"],
          isRepeatable: 2,
          isPersistent: true,
          args: { name: "color" },
        }, {
          name: "--many",
          isRepeatable: true,
        }, {
          name: "--once",
          isRepeatable: false,
        }, {
          name: "--required",
          requiresSeparator: true,
          args: { name: "value" },
        }],
      };\n`,
    );

    const result = await compileSpecsIr({ srcDir, outDir });
    assert.equal(result.compiled, 1);
    const ir = JSON.parse(await readFile(join(outDir, "fixture.json"), "utf8"));
    assert.equal(ir.args[0].getQueryTerm, "/");
    assert.equal(ir.args[0].suggestCurrentToken, false);
    assert.equal(ir.args[0].optionsCanBreakVariadicArg, false);
    assert.deepEqual(ir.parserDirectives, {
      optionsMustPrecedeArguments: true,
      flagsArePosixNoncompliant: true,
      optionArgSeparators: ["=", ":"],
    });
    assert.equal(ir.filterStrategy, "fuzzy");
    assert.equal("filterStrategy" in ir.args[0], false);
    assert.equal(ir.args[0].suggestions[0].type, "file");
    assert.equal(ir.args[0].suggestions[0].originalType, "folder");
    assert.equal(ir.args[0].suggestions[0].getQueryTerm, "/");
    assert.equal(ir.args[0].suggestions[0].argsHint, "<required> [optional]");
    assert.equal("getQueryTerm" in ir.args[0].suggestions[1], false);
    assert.equal(typeof ir.args[0].suggestions[1].jsGetQueryTerm, "string");
    assert.deepEqual(ir.args[0].templates, ["filepaths"]);
    assert.equal(ir.args[0].generators[0].getQueryTerm, "/");
    assert.deepEqual(ir.args[0].generators[0].templates, ["filepaths"]);
    assert.deepEqual(ir.args[1].script, ["printf", "ok\n"]);
    assert.equal(ir.args[1].suggestCurrentToken, true);
    assert.equal(ir.args[1].filterStrategy, "prefix");
    assert.equal(ir.args[1].scriptTimeout, 7000);
    const color = ir.persistentOptions[0];
    assert.deepEqual(color.names, ["-c", "--color"]);
    assert.equal(color.insertValue, "--colour");
    assert.equal(color.displayName, "Color");
    assert.equal(color.separatorToAdd, ":");
    assert.equal(color.shouldAddSpace, false);
    assert.equal(color.hidden, true);
    assert.equal(color.priority, 50);
    assert.equal(color.icon, "🟡");
    assert.equal(color.isDangerous, true);
    assert.deepEqual(color.exclusiveOn, ["--no-color", "-C"]);
    assert.deepEqual(color.dependsOn, ["--config"]);
    assert.equal(color.isRepeatable, 2);
    assert.equal(color.isPersistent, true);
    assert.equal(
      ir.options.find((option) => option.names[0] === "--many").isRepeatable,
      true,
    );
    assert.equal(
      ir.options.find((option) => option.names[0] === "--once").isRepeatable,
      false,
    );
    const required = ir.options.find(
      (option) => option.names[0] === "--required",
    );
    assert.equal("separatorToAdd" in required, false);
  } finally {
    await Promise.all([
      rm(srcDir, { recursive: true, force: true }),
      rm(outDir, { recursive: true, force: true }),
    ]);
  }
});

test("compiler keeps lazy loadSpec links and maps versioned nested roots", async () => {
  const srcDir = await mkdtemp(join(tmpdir(), "easy-complete-specs-links-"));
  const outDir = await mkdtemp(join(tmpdir(), "easy-complete-ir-links-"));
  try {
    await writeFile(
      join(srcDir, "docker.js"),
      `export default {
        name: "docker",
        subcommands: [{ name: "compose", loadSpec: "docker-compose" }, {
          name: "inline",
          loadSpec: { name: "inline", subcommands: [{ name: "up" }] },
        }],
      };\n`,
    );
    await writeFile(
      join(srcDir, "docker-compose.js"),
      `export default { name: "docker-compose", subcommands: [{ name: "up" }] };\n`,
    );
    await writeFile(
      join(srcDir, "gcloud.js"),
      `export default { name: "gcloud", subcommands: [{ name: "domains", loadSpec: "gcloud/domains" }] };\n`,
    );
    await mkdir(join(srcDir, "gcloud"), { recursive: true });
    await writeFile(
      join(srcDir, "gcloud", "domains.js"),
      `export default { name: "domains", subcommands: [{ name: "list" }] };\n`,
    );
    await mkdir(join(srcDir, "heroku"), { recursive: true });
    await writeFile(
      join(srcDir, "heroku", "index.js"),
      `export default () => ({ name: "heroku" });\n`,
    );
    await writeFile(
      join(srcDir, "heroku", "8.0.0.js"),
      `export default { name: "heroku", subcommands: [{ name: "old" }] };\n`,
    );
    await writeFile(
      join(srcDir, "heroku", "8.6.0.js"),
      `export default { name: "heroku", subcommands: [{ name: "new" }] };\n`,
    );

    const result = await compileSpecsIr({ srcDir, outDir });
    assert.equal(result.compiled, 6);
    assert.equal(result.failed, 0);
    assert.equal(result.allowlistedSkipped, 1);

    const docker = JSON.parse(
      await readFile(join(outDir, "docker.json"), "utf8"),
    );
    assert.equal(docker.subcommands[0].loadSpec, "docker-compose");
    assert.deepEqual(
      docker.subcommands
        .find((item) => item.names.includes("inline"))
        .subcommands.map((item) => item.names[0]),
      ["up"],
    );

    const index = JSON.parse(
      await readFile(join(outDir, "index.json"), "utf8"),
    );
    assert.equal(index.files.docker, "docker.json");
    assert.equal(index.files.heroku, "heroku/8.6.0.json");
    assert.equal(index.files.gcloud, "gcloud.json");
    assert.equal("domains" in index.files, false);
    assert.equal("gcloud/domains" in index.files, false);
  } finally {
    await Promise.all([
      rm(srcDir, { recursive: true, force: true }),
      rm(outDir, { recursive: true, force: true }),
    ]);
  }
});

test("root file names are canonical and spec names are aliases", async () => {
  const srcDir = await mkdtemp(
    join(tmpdir(), "easy-complete-specs-canonical-"),
  );
  const outDir = await mkdtemp(join(tmpdir(), "easy-complete-ir-canonical-"));
  try {
    await writeFile(
      join(srcDir, "appwrite.js"),
      `export default { name: "index" };\n`,
    );
    await writeFile(
      join(srcDir, "autojump.js"),
      `export default { name: "autojump", args: { name: "directory", isVariadic: true } };\n`,
    );
    await writeFile(
      join(srcDir, "j.js"),
      `export default { name: "autojump" };\n`,
    );
    await writeFile(
      join(srcDir, "git.js"),
      `export default { name: "git" };\n`,
    );
    await writeFile(
      join(srcDir, "hub.js"),
      `export default { name: "git" };\n`,
    );

    const result = await compileSpecsIr({ srcDir, outDir });
    assert.equal(result.compiled, 5);
    const index = JSON.parse(
      await readFile(join(outDir, "index.json"), "utf8"),
    );
    assert.equal(index.files.appwrite, "appwrite.json");
    assert.equal(index.files.index, "appwrite.json");
    assert.equal(index.files.autojump, "autojump.json");
    assert.equal(index.files.j, "j.json");
    assert.equal(index.files.git, "git.json");
    assert.equal(index.files.hub, "hub.json");
  } finally {
    await Promise.all([
      rm(srcDir, { recursive: true, force: true }),
      rm(outDir, { recursive: true, force: true }),
    ]);
  }
});

test("object loadSpec replaces wrapper fields while retaining its command name", async () => {
  const srcDir = await mkdtemp(join(tmpdir(), "easy-complete-specs-replace-"));
  const outDir = await mkdtemp(join(tmpdir(), "easy-complete-ir-replace-"));
  try {
    await writeFile(
      join(srcDir, "pass.js"),
      `export default {
        name: "pass",
        subcommands: [{
          name: "grep",
          description: "wrapper description",
          args: { name: "pass-name" },
          loadSpec: {
            name: "grep-target",
            description: "loaded description",
            args: [{ name: "pattern" }, { name: "file" }],
          },
        }],
      };\n`,
    );
    await writeFile(
      join(srcDir, "chezmoi.js"),
      `export default {
        name: "chezmoi",
        subcommands: [{
          name: "git",
          description: "wrapper description",
          args: { name: "source-dir" },
          loadSpec: {
            name: "git-target",
            description: "loaded description",
            args: [{ name: "command" }],
          },
        }],
      };\n`,
    );

    const result = await compileSpecsIr({ srcDir, outDir });
    assert.equal(result.compiled, 2);
    const pass = JSON.parse(await readFile(join(outDir, "pass.json"), "utf8"));
    const grep = pass.subcommands[0];
    assert.deepEqual(grep.names, ["grep"]);
    assert.equal(grep.description, "loaded description");
    assert.deepEqual(
      grep.args.map((arg) => arg.name),
      ["pattern", "file"],
    );

    const chezmoi = JSON.parse(
      await readFile(join(outDir, "chezmoi.json"), "utf8"),
    );
    const git = chezmoi.subcommands[0];
    assert.deepEqual(git.names, ["git"]);
    assert.equal(git.description, "loaded description");
    assert.deepEqual(
      git.args.map((arg) => arg.name),
      ["command"],
    );
  } finally {
    await Promise.all([
      rm(srcDir, { recursive: true, force: true }),
      rm(outDir, { recursive: true, force: true }),
    ]);
  }
});

test("source index keeps top-level aliases and versioned roots backward compatible", async () => {
  const srcDir = await mkdtemp(
    join(tmpdir(), "easy-complete-specs-source-index-"),
  );
  const outDir = await mkdtemp(
    join(tmpdir(), "easy-complete-ir-source-index-"),
  );
  try {
    await writeFile(join(srcDir, "r.js"), `export default { name: "R" };\n`);
    await writeFile(
      join(srcDir, "stepzen.js"),
      `export default { name: "StepZen" };\n`,
    );
    await writeFile(
      join(srcDir, "visible.js"),
      `export default { name: "visible-alias" };\n`,
    );
    await writeFile(
      join(srcDir, "hidden.js"),
      `export default { name: "hidden-alias" };\n`,
    );
    await mkdir(join(srcDir, "heroku"), { recursive: true });
    await writeFile(
      join(srcDir, "heroku", "index.js"),
      `export default () => ({ name: "heroku" });\n`,
    );
    await writeFile(
      join(srcDir, "heroku", "8.0.0.js"),
      `export default { name: "heroku" };\n`,
    );
    await writeFile(
      join(srcDir, "heroku", "8.6.0.js"),
      `export default { name: "heroku" };\n`,
    );
    await writeFile(
      join(srcDir, "index.json"),
      JSON.stringify({
        completions: [
          "r",
          "stepzen",
          "visible",
          "visible-alias",
          "heroku",
          "heroku/8.0.0",
          "heroku/8.6.0",
        ],
        diffVersionedCompletions: ["heroku"],
      }),
    );

    const result = await compileSpecsIr({ srcDir, outDir });
    assert.equal(result.compiled, 6);
    assert.equal(result.failed, 0);
    assert.equal(result.allowlistedSkipped, 1);
    const index = JSON.parse(
      await readFile(join(outDir, "index.json"), "utf8"),
    );
    assert.equal(index.files.r, "r.json");
    assert.equal(index.files.stepzen, "stepzen.json");
    assert.equal(index.files.visible, "visible.json");
    assert.equal(index.files["visible-alias"], "visible.json");
    assert.equal(index.files.heroku, "heroku/8.6.0.json");
    assert.equal("R" in index.files, false);
    assert.equal("StepZen" in index.files, false);
    assert.equal("hidden" in index.files, false);
    assert.equal("hidden-alias" in index.files, false);
    assert.equal("heroku/8.6.0" in index.files, false);
  } finally {
    await Promise.all([
      rm(srcDir, { recursive: true, force: true }),
      rm(outDir, { recursive: true, force: true }),
    ]);
  }
});

test("compiler keeps postProcess scripts and extracts JS hooks", async () => {
  const srcDir = await mkdtemp(join(tmpdir(), "easy-complete-specs-script-"));
  const outDir = await mkdtemp(join(tmpdir(), "easy-complete-ir-script-"));
  try {
    await writeFile(
      join(srcDir, "fixture.js"),
      `export default {
        name: "fixture",
        generateSpec: async (tokens, exec) => ({ name: "fixture", subcommands: [{ name: "dyn" }] }),
        generateSpecCacheKey: "fixture-tree",
        args: [
          {
            name: "split",
            generators: {
              script: ["printf", "a,b,c"],
              splitOn: ",",
            },
          },
          {
            name: "post",
            generators: {
              script: ["ps", "axo", "pid,comm"],
              scriptTimeout: 9000,
              postProcess: (out) => out.split("\\n").map((line) => ({ name: line })),
            },
          },
          {
            name: "both",
            generators: {
              script: ["printf", "a\\nb"],
              splitOn: "\\n",
              postProcess: (out) => out.split("\\n").map((line) => ({ name: line })),
            },
          },
          {
            name: "custom",
            generators: {
              custom: async (tokens) => [{ name: tokens.at(-1) || "row" }],
              cache: { strategy: "stale-while-revalidate", ttl: 5000, cacheByDirectory: true, cacheKey: "env" },
            },
          },
        ],
      };\n`,
    );

    const result = await compileSpecsIr({ srcDir, outDir });
    assert.equal(result.compiled, 1);
    assert.ok(result.hooks >= 4);
    const ir = JSON.parse(await readFile(join(outDir, "fixture.json"), "utf8"));
    assert.deepEqual(ir.args[0].script, ["printf", "a,b,c"]);
    assert.equal(ir.args[0].splitOn, ",");
    assert.deepEqual(ir.args[1].script, ["ps", "axo", "pid,comm"]);
    assert.equal(ir.args[1].scriptTimeout, 9000);
    assert.equal(ir.args[1].jsPostProcess, "fixture#postProcess#1");
    assert.deepEqual(ir.args[2].script, ["printf", "a\nb"]);
    assert.equal(ir.args[2].splitOn, "\n");
    assert.equal(typeof ir.args[2].jsPostProcess, "string");
    assert.equal(ir.args[3].jsCustom, "fixture#custom#3");
    assert.equal(ir.args[3].cacheKey, "env");
    assert.equal(ir.args[3].cacheByDirectory, true);
    assert.equal(ir.args[3].cacheTtl, 5000);
    assert.equal(ir.jsGenerateSpec, "fixture#generateSpec#0");
    assert.equal(ir.generateSpecCacheKey, "fixture-tree");

    const sidecar = JSON.parse(
      await readFile(join(outDir, "typed-hooks.json"), "utf8"),
    );
    const post = sidecar.hooks["fixture#postProcess#1"];
    assert.equal(post.sourceField, "postProcess");
    assert.match(post.path, /postProcess/);
    await assert.rejects(lstat(join(outDir, "hooks")), { code: "ENOENT" });
  } finally {
    await Promise.all([
      rm(srcDir, { recursive: true, force: true }),
      rm(outDir, { recursive: true, force: true }),
    ]);
  }
});

test("compiler keeps docker/kubectl/gh dynamic scripts instead of dropping them", async () => {
  const repoDir = join(dirname(fileURLToPath(import.meta.url)), "..");
  const srcDir = await mkdtemp(join(tmpdir(), "easy-complete-specs-cli-"));
  const outDir = await mkdtemp(join(tmpdir(), "easy-complete-ir-cli-"));
  try {
    for (const name of ["docker.js", "kubectl.js", "gh.js"]) {
      await writeFile(
        join(srcDir, name),
        await readFile(join(repoDir, "bundle", "specs", name)),
      );
    }
    const result = await compileSpecsIr({ srcDir, outDir });
    assert.equal(result.compiled, 3);
    assert.ok(result.hooks > 0, result.hooks);
    const docker = JSON.parse(
      await readFile(join(outDir, "docker.json"), "utf8"),
    );
    const exec = docker.subcommands.find((item) => item.names.includes("exec"));
    assert.ok(exec, "docker exec");
    assert.deepEqual(exec.args[0].script.slice(0, 2), ["docker", "ps"]);
    assert.match(exec.args[0].jsPostProcess, /^docker#postProcess#/);
    const kubectl = JSON.parse(
      await readFile(join(outDir, "kubectl.json"), "utf8"),
    );
    assert.ok(
      JSON.stringify(kubectl).includes("jsPostProcess") ||
        JSON.stringify(kubectl).includes("jsCustom"),
      "kubectl keeps JS hooks",
    );
    const gh = JSON.parse(await readFile(join(outDir, "gh.json"), "utf8"));
    assert.match(JSON.stringify(gh), /jsPostProcess/);
  } finally {
    await Promise.all([
      rm(srcDir, { recursive: true, force: true }),
      rm(outDir, { recursive: true, force: true }),
    ]);
  }
});

test("compiler keeps isCommand, isScript, and isModule", async () => {
  const srcDir = await mkdtemp(join(tmpdir(), "easy-complete-specs-cmd-"));
  const outDir = await mkdtemp(join(tmpdir(), "easy-complete-ir-cmd-"));
  try {
    await writeFile(
      join(srcDir, "fixture.js"),
      `export default {
        name: "fixture",
        args: [
          { name: "cmd", isCommand: true },
          { name: "script", isScript: true },
          { name: "mod", isModule: "python/" },
          { name: "plain" },
        ],
      };\n`,
    );

    const result = await compileSpecsIr({ srcDir, outDir });
    assert.equal(result.compiled, 1);
    const ir = JSON.parse(await readFile(join(outDir, "fixture.json"), "utf8"));
    assert.equal(ir.args[0].isCommand, true);
    assert.equal("isScript" in ir.args[0], false);
    assert.equal(ir.args[1].isScript, true);
    assert.equal(ir.args[2].isModule, "python/");
    assert.equal("isCommand" in ir.args[3], false);
    assert.equal("isModule" in ir.args[3], false);
  } finally {
    await Promise.all([
      rm(srcDir, { recursive: true, force: true }),
      rm(outDir, { recursive: true, force: true }),
    ]);
  }
});

test("compiler rewrites filepaths() helpers into native templates", async () => {
  const srcDir = await mkdtemp(join(tmpdir(), "easy-complete-specs-fp-"));
  const outDir = await mkdtemp(join(tmpdir(), "easy-complete-ir-fp-"));
  try {
    await writeFile(
      join(srcDir, "paths.js"),
      `function filepaths(opts = {}) {
        const {
          extensions = [],
          equals = [],
          matches,
          showFolders = "always",
          filterFolders = false,
          editFileSuggestions,
          editFolderSuggestions,
          rootDirectory,
        } = opts;
        const ext = new Set(extensions);
        // Fig builds new Set(equals) even when equals is a string, which
        // turns "Cargo.toml" into a set of characters. Keep that bug here
        // so IR must come from the source literal, not the live probe.
        const eq = new Set(equals);
        return {
          trigger: (a, b) => a.lastIndexOf("/") !== b.lastIndexOf("/"),
          getQueryTerm: (term) => term.slice(term.lastIndexOf("/") + 1),
          custom: async (_tokens, exec, ctx) => {
            const skip = [".DS_Store"];
            const { stdout } = await exec({
              command: "ls",
              args: ["-1ApL"],
              cwd: rootDirectory ?? ctx.currentWorkingDirectory,
            });
            const names = stdout.split("\\n").filter(Boolean).concat("../");
            const filtered = names.filter((name) => {
              if (skip.includes(name)) return false;
              const isFolder = name.endsWith("/");
              if (isFolder) {
                if (showFolders === "never") return false;
                if (!filterFolders) return true;
              } else if (showFolders === "only") {
                return false;
              }
              if (!extensions.length && !equals.length && !matches) return true;
              if (eq.has(name)) return true;
              if (matches && matches.test(name)) return true;
              const parts = name.split(".");
              if (parts.length < 2) return false;
              let suffix = parts[parts.length - 1];
              for (let i = parts.length - 1; i >= 1; i -= 1) {
                if (ext.has(suffix)) return true;
                if (i > 1) suffix = parts[i - 1] + "." + suffix;
              }
              return false;
            });
            return filtered.map((name) => {
              const isFolder = name.endsWith("/");
              const extra = (isFolder ? editFolderSuggestions : editFileSuggestions) || {};
              return { type: isFolder ? "folder" : "file", name, ...extra };
            });
          },
        };
      }
      function folders(opts = {}) {
        return filepaths(Object.assign({ showFolders: "only" }, opts));
      }
      export default {
        name: "paths",
        args: [
          { name: "dir", generators: folders() },
          { name: "py", generators: filepaths({ extensions: ["py"], editFileSuggestions: { priority: 76 } }) },
          { name: "java", generators: filepaths({ extensions: ["java", "class"] }) },
          { name: "manifest", generators: filepaths({ equals: "Cargo.toml" }) },
          { name: "env", generators: filepaths({ matches: /^\\.env.*$/ }) },
          { name: "rare", generators: filepaths({ extensions: ["not-in-probe"] }) },
          { name: "filtered", generators: filepaths({ extensions: ["py"], filterFolders: true }) },
          { name: "any", generators: filepaths() },
        ],
      };\n`,
    );
    const result = await compileSpecsIr({ srcDir, outDir });
    assert.equal(result.compiled, 1);
    const ir = JSON.parse(await readFile(join(outDir, "paths.json"), "utf8"));
    assert.deepEqual(ir.args[0].templates, ["folders"]);
    assert.equal(ir.args[0].getQueryTerm, "/");
    assert.equal("jsCustom" in ir.args[0], false);
    assert.equal("jsCustom" in ir.args[0].generators[0], false);
    assert.equal(ir.args[0].generators[0].getQueryTerm, "/");
    assert.deepEqual(ir.args[1].templates, ["filepaths"]);
    assert.deepEqual(ir.args[1].generators[0].extensions, ["py"]);
    assert.equal(ir.args[1].generators[0].filePriority, 76);
    assert.deepEqual(ir.args[2].templates, ["filepaths"]);
    assert.deepEqual(ir.args[2].generators[0].extensions, ["class", "java"]);
    assert.deepEqual(ir.args[3].templates, ["filepaths"]);
    assert.deepEqual(ir.args[3].generators[0].equals, ["Cargo.toml"]);
    assert.equal("extensions" in ir.args[3].generators[0], false);
    assert.deepEqual(ir.args[4].templates, ["filepaths"]);
    assert.equal(ir.args[4].generators[0].matches, "^\\.env.*$");
    assert.equal("equals" in ir.args[4].generators[0], false);
    assert.deepEqual(ir.args[5].templates, ["filepaths"]);
    assert.deepEqual(ir.args[5].generators[0].extensions, ["not-in-probe"]);
    assert.deepEqual(ir.args[6].templates, ["filepaths"]);
    assert.deepEqual(ir.args[6].generators[0].extensions, ["py"]);
    assert.equal(ir.args[6].generators[0].filterFolders, true);
    assert.notEqual(ir.args[6].generators[0].showFolders, "never");
    assert.deepEqual(ir.args[7].templates, ["filepaths"]);
    assert.equal("extensions" in ir.args[7].generators[0], false);
    const hooks = await readdir(join(outDir, "hooks")).catch(() => []);
    assert.equal(hooks.length, 0);
  } finally {
    await Promise.all([
      rm(srcDir, { recursive: true, force: true }),
      rm(outDir, { recursive: true, force: true }),
    ]);
  }
});

test("compiler does not invent file templates the spec omitted", async () => {
  const srcDir = await mkdtemp(join(tmpdir(), "easy-complete-specs-no-tmpl-"));
  const outDir = await mkdtemp(join(tmpdir(), "easy-complete-ir-no-tmpl-"));
  try {
    await writeFile(
      join(srcDir, "cat.js"),
      `export default { name: "cat", args: { name: "file" } };\n`,
    );
    await writeFile(join(srcDir, "cd.js"), `export default { name: "cd" };\n`);
    const result = await compileSpecsIr({ srcDir, outDir });
    assert.equal(result.compiled, 2);
    const cat = JSON.parse(await readFile(join(outDir, "cat.json"), "utf8"));
    assert.equal("templates" in cat.args[0], false);
    const cd = JSON.parse(await readFile(join(outDir, "cd.json"), "utf8"));
    assert.equal("args" in cd, false);
  } finally {
    await Promise.all([
      rm(srcDir, { recursive: true, force: true }),
      rm(outDir, { recursive: true, force: true }),
    ]);
  }
});

test("filepaths helper parser binds literals by preceding name and keeps equals strings", () => {
  const source = `
    {name:"dir",generators:(0,m.folders)()}
    {name:"manifest",generators:(0,m.filepaths)({equals:"Cargo.toml"})}
    {name:"env",generators:(0,m.filepaths)({matches:/^\\.env.*$/i})}
    {name:"--",args:{generators:(0,m.filepaths)({equals:["rustfmt.toml"]})}}
  `;
  const calls = parseFilepathsCalls(source);
  assert.deepEqual(
    calls.map((call) => call.precedingName),
    ["dir", "manifest", "env", "--"],
  );
  assert.deepEqual(calls[0].literal, { showFolders: "only" });
  assert.deepEqual(calls[1].literal, { equals: ["Cargo.toml"] });
  assert.equal(calls[2].literal.matches, "^\\.env.*$");
  assert.equal(calls[2].literal.matchesFlags, "i");
  assert.deepEqual(calls[3].literal, { equals: ["rustfmt.toml"] });

  const binder = createFilepathsBinder(source);
  assert.deepEqual(binder.take(["ENV", "dotenv-vault"]), {
    matches: "^\\.env.*$",
    matchesFlags: "i",
  });
  assert.deepEqual(binder.take(["env"]), {
    matches: "^\\.env.*$",
    matchesFlags: "i",
  });
  assert.deepEqual(binder.take(["dir"]), { showFolders: "only" });
  assert.deepEqual(binder.take(["missing", "manifest"]), {
    equals: ["Cargo.toml"],
  });
  assert.deepEqual(binder.take(["--"]), { equals: ["rustfmt.toml"] });
  assert.equal(binder.take(["nope"]), null);
  assert.deepEqual(binder.take([]), { showFolders: "only" });
});

test("compiler keeps history/help templates, debounce, trigger, and arg aliases", async () => {
  const srcDir = await mkdtemp(join(tmpdir(), "easy-complete-specs-tmpl-"));
  const outDir = await mkdtemp(join(tmpdir(), "easy-complete-ir-tmpl-"));
  try {
    await writeFile(
      join(srcDir, "fixture.js"),
      `export default {
        name: "fixture",
        args: [{
          name: "message",
          debounce: true,
          parserDirectives: { alias: "git" },
          generators: [{
            template: ["history", "help"],
            trigger: "/",
            filterTemplateSuggestions: (rows) => rows,
          }, {
            custom: async () => [{ name: "row" }],
            trigger: { on: "threshold", length: 3 },
          }],
        }],
        subcommands: [{
          name: "load",
          loadSpec: async () => ({ name: "loaded" }),
        }],
      };\n`,
    );
    const result = await compileSpecsIr({ srcDir, outDir });
    assert.equal(result.compiled, 1);
    const ir = JSON.parse(await readFile(join(outDir, "fixture.json"), "utf8"));
    assert.equal(ir.args[0].debounceMs, 200);
    assert.equal(ir.args[0].parserDirectives.alias, "git");
    assert.deepEqual(ir.args[0].templates, ["history", "help"]);
    assert.equal(ir.args[0].generators[0].trigger.on, "string");
    assert.equal(ir.args[0].generators[0].trigger.string, "/");
    assert.equal(
      typeof ir.args[0].generators[0].jsFilterTemplateSuggestions,
      "string",
    );
    assert.equal(ir.args[0].generators[1].trigger.on, "threshold");
    assert.equal(ir.args[0].generators[1].trigger.length, 3);
    assert.equal(typeof ir.subcommands[0].jsLoadSpec, "string");
  } finally {
    await Promise.all([
      rm(srcDir, { recursive: true, force: true }),
      rm(outDir, { recursive: true, force: true }),
    ]);
  }
});

test("compiler extracts all supported JS hook fields and audit maps them", async () => {
  const srcDir = await mkdtemp(
    join(tmpdir(), "easy-complete-specs-all-hooks-"),
  );
  const outDir = await mkdtemp(join(tmpdir(), "easy-complete-ir-all-hooks-"));
  try {
    await writeFile(
      join(srcDir, "all-hooks.js"),
      `const lazy = async () => ({ name: "loaded" });
       export default {
         name: "all-hooks",
         generateSpec: async () => ({ name: "all-hooks" }),
         parserDirectives: { alias: (tokens) => tokens.at(-1) || "git" },
         subcommands: [{ name: "lazy", loadSpec: lazy }],
         args: [{
           name: "value",
           generators: [{
             trigger: (before, after) => before.length !== after.length,
             getQueryTerm: (term) => term,
             script: async () => [],
             postProcess: (stdout) => stdout ? [] : [],
             custom: (tokens) => External(tokens),
             filterTemplateSuggestions: (rows) => rows,
           }],
         }],
       };
      `,
    );

    const result = await compileSpecsIr({ srcDir, outDir });
    assert.equal(result.compiled, 1);
    assert.equal(result.hooks, 10);
    const ir = JSON.parse(
      await readFile(join(outDir, "all-hooks.json"), "utf8"),
    );
    assert.equal(typeof ir.jsGenerateSpec, "string");
    assert.equal(typeof ir.parserDirectives.jsAlias, "string");
    assert.equal(typeof ir.subcommands[0].jsLoadSpec, "string");
    assert.equal(typeof ir.args[0].jsGetQueryTerm, "string");
    const generator = ir.args[0].generators[0];
    assert.equal(typeof generator.trigger.jsTrigger, "string");
    assert.notEqual(ir.args[0].jsGetQueryTerm, generator.jsGetQueryTerm);
    for (const field of [
      "jsGetQueryTerm",
      "jsScript",
      "jsPostProcess",
      "jsCustom",
      "jsFilterTemplateSuggestions",
    ]) {
      assert.equal(typeof generator[field], "string", field);
    }

    const report = await auditSpecsHooks({
      sourceRoot: srcDir,
      irRoot: outDir,
    });
    assert.equal(report.ok, true);
    assert.equal(report.source.uniqueFunctionCounts.loadSpec, 1);
    assert.equal(report.source.uniqueFunctionCounts.trigger, 1);
    assert.equal(report.source.uniqueFunctionCounts.alias, 1);
    assert.equal(report.source.uniqueFunctionCounts.getQueryTerm, 1);
    assert.equal(report.source.uniqueFunctionCounts.generateSpec, 1);
    assert.equal(report.source.uniqueFunctionCounts.script, 1);
    assert.equal(report.source.uniqueFunctionCounts.postProcess, 1);
    assert.equal(report.source.uniqueFunctionCounts.custom, 1);
    assert.equal(
      report.source.uniqueFunctionCounts.filterTemplateSuggestions,
      1,
    );
    assert.equal(report.source.compilerExtractionCounts.getQueryTerm, 2);
    assert.equal(report.hooks.referenced, 10);
    assert.equal(report.ir.hookIdCounts.jsGetQueryTerm, 2);
    assert.ok(
      report.hooks.riskSamplesByPattern["free-variable-candidate"].some(
        (item) =>
          item.field === "jsCustom" && item.freeVariables.includes("External"),
      ),
    );
    assert.equal(report.sourceToIr[0].status, "compiled");
    assert.equal(report.sourceToIr[0].compilerExtractionCounts.getQueryTerm, 2);
    assert.equal(report.sourceToIr[0].hookInstances.getQueryTerm.length, 2);
    assert.equal(new Set(report.sourceToIr[0].hooks.jsCustom).size, 1);
  } finally {
    await Promise.all([
      rm(srcDir, { recursive: true, force: true }),
      rm(outDir, { recursive: true, force: true }),
    ]);
  }
});

test("compiler emits a deterministic typed trigger sidecar from binding identity", async () => {
  const srcDir = await mkdtemp(join(tmpdir(), "easy-complete-typed-src-"));
  const firstOutDir = await mkdtemp(join(tmpdir(), "easy-complete-typed-ir-"));
  const secondOutDir = await mkdtemp(join(tmpdir(), "easy-complete-typed-ir-"));
  try {
    const source = `const legacy = (value) => external(value);
      export default {
        name: "typed",
        args: [{ generators: [
          { trigger: (before, after) => before.length !== after.length },
          { trigger: legacy },
        ] }],
      };\n`;
    const sourcePath = join(srcDir, "typed.js");
    await writeFile(sourcePath, source);
    await compileSpecsIr({ srcDir, outDir: firstOutDir });
    await compileSpecsIr({ srcDir, outDir: secondOutDir });

    const ir = JSON.parse(
      await readFile(join(firstOutDir, "typed.json"), "utf8"),
    );
    const triggerIds = ir.args[0].generators.map(
      (generator) => generator.trigger.jsTrigger,
    );
    assert.equal(triggerIds.length, 2);
    const [supportedId, legacyId] = triggerIds;
    const sidecarText = await readFile(
      join(firstOutDir, TYPED_HOOK_SIDECAR),
      "utf8",
    );
    const sidecar = JSON.parse(sidecarText);
    assert.deepEqual(Object.keys(sidecar), [
      "version",
      "kind",
      "contracts",
      "hooks",
    ]);
    assert.deepEqual(sidecar.contracts, {
      trigger: {
        irVersion: 1,
        params: ["string", "string"],
        resultType: "bool",
      },
      getQueryTerm: {
        irVersion: 1,
        params: ["string"],
        resultType: "string",
      },
      postProcess: {
        irVersion: 1,
        params: ["string", "string-array"],
        resultType: "suggestion-array",
      },
      script: {
        irVersion: 1,
        params: ["string-array"],
        resultType: "string-array",
      },
      filterTemplateSuggestions: {
        irVersion: 1,
        params: ["suggestion-array"],
        resultType: "suggestion-array",
      },
      custom: {
        irVersion: 1,
        params: ["string-array", "exec", "context"],
        resultType: "suggestion-array",
      },
      alias: {
        irVersion: 1,
        params: ["string", "exec"],
        resultType: "string",
      },
      loadSpec: {
        irVersion: 1,
        params: ["string", "exec"],
        resultType: "spec",
      },
      generateSpec: {
        irVersion: 1,
        params: ["string-array", "exec"],
        resultType: "spec",
      },
    });
    assert.deepEqual(Object.keys(sidecar.hooks), [supportedId]);
    const imported = await import(
      `${pathToFileURL(sourcePath).href}?typed=${Date.now()}`
    );
    const body = functionSource(imported.default.args[0].generators[0].trigger);
    assert.equal(typeof body, "string");
    assert.equal(sidecar.hooks[supportedId].sourceField, "trigger");
    assert.equal(sidecar.hooks[supportedId].path, "root.args[0].generators[0].trigger");
    assert.deepEqual(
      sidecar.hooks[supportedId].descriptor,
      compileTypedHook({ body, sourceField: "trigger" }),
    );
    assert.equal(sidecar.hooks[legacyId], undefined);
    assert.deepEqual(
      await readFile(join(secondOutDir, TYPED_HOOK_SIDECAR)),
      Buffer.from(sidecarText),
      "typed sidecar bytes must be reproducible",
    );
  } finally {
    await Promise.all([
      rm(srcDir, { recursive: true, force: true }),
      rm(firstOutDir, { recursive: true, force: true }),
      rm(secondOutDir, { recursive: true, force: true }),
    ]);
  }
});

test("production compiler writes a reviewed getQueryTerm hook into the sidecar", async () => {
  const srcDir = await mkdtemp(join(tmpdir(), "easy-complete-get-query-src-"));
  const outDir = await mkdtemp(join(tmpdir(), "easy-complete-get-query-ir-"));
  try {
    const source = `export default {
      name: "asdf-research-fixture",
      args: [{ generators: {
        getQueryTerm: n => n.includes("latest") ? n.slice(n.indexOf(":") + 1) : n,
      } }],
    };\n`;
    const sourcePath = join(srcDir, "asdf-research-fixture.js");
    await writeFile(sourcePath, source);
    const result = await compileSpecsIr({ srcDir, outDir });
    assert.equal(result.compiled, 1);
    const ir = JSON.parse(
      await readFile(join(outDir, "asdf-research-fixture.json"), "utf8"),
    );
    const hookId = ir.args[0].generators[0].jsGetQueryTerm;
    assert.equal(typeof hookId, "string");
    const sidecar = JSON.parse(
      await readFile(join(outDir, TYPED_HOOK_SIDECAR), "utf8"),
    );
    assert.ok(Object.keys(sidecar.hooks).includes(hookId));
    assert.ok(
      Object.keys(sidecar.hooks).every((id) => id.includes("#getQueryTerm#")),
    );
    const imported = await import(
      `${pathToFileURL(sourcePath).href}?query=${Date.now()}`,
    );
    const body = functionSource(imported.default.args[0].generators.getQueryTerm);
    const descriptor = compileTypedHook({ body, sourceField: "getQueryTerm" });
    assert.equal(descriptor.expr.then.op, "string-slice");
    assert.equal(descriptor.expr.then.start.op, "add");
    assert.equal(descriptor.sourceField, "getQueryTerm");
    for (const entry of Object.values(sidecar.hooks)) {
      assert.deepEqual(entry.descriptor, descriptor);
    }
  } finally {
    await Promise.all([
      rm(srcDir, { recursive: true, force: true }),
      rm(outDir, { recursive: true, force: true }),
    ]);
  }
});

test("compiler fails closed for unknown or unextractable functions and keeps old IR", async () => {
  const srcDir = await mkdtemp(
    join(tmpdir(), "easy-complete-specs-fail-closed-"),
  );
  const outDir = await mkdtemp(join(tmpdir(), "easy-complete-ir-fail-closed-"));
  try {
    await writeFile(join(outDir, "sentinel.txt"), "keep previous output\n");

    // The compiler API must reject a source-tree symlink before staging or
    // replacing the prior IR.  The target is outside the source root so
    // following it would copy foreign input into the generated tree.
    const linkedSourceTarget = join(
      dirname(srcDir),
      "compiler-linked-source-target.js",
    );
    const linkedSource = join(srcDir, "linked.js");
    await writeFile(
      linkedSourceTarget,
      `export default { name: "linked" };\n`,
    );
    await symlink(linkedSourceTarget, linkedSource);
    await assert.rejects(
      compileSpecsIr({ srcDir, outDir }),
      /source spec tree contains a symlink or special entry/,
    );
    assert.equal(
      await readFile(join(outDir, "sentinel.txt"), "utf8"),
      "keep previous output\n",
    );
    await rm(linkedSource, { force: true });
    await rm(linkedSourceTarget, { force: true });

    // Do not normalize a literal POSIX backslash in a filename before
    // validating it.  In particular, `..\\..\\outside.js` must not be
    // interpreted as an escaped relative path.
    const backslashSource = join(srcDir, "..\\..\\outside.js");
    await writeFile(backslashSource, `export default { name: "outside" };\n`);
    await assert.rejects(
      compileSpecsIr({ srcDir, outDir }),
      /unsafe source spec path/,
    );
    assert.equal(
      await readFile(join(outDir, "sentinel.txt"), "utf8"),
      "keep previous output\n",
    );
    await rm(backslashSource, { force: true });

    if (process.platform !== "win32") {
      const specialSource = join(srcDir, "special.js");
      execFileSync("mkfifo", [specialSource]);
      await assert.rejects(
        compileSpecsIr({ srcDir, outDir }),
        /source spec tree contains a symlink or special entry/,
      );
      assert.equal(
        await readFile(join(outDir, "sentinel.txt"), "utf8"),
        "keep previous output\n",
      );
      await rm(specialSource, { force: true });
    }

    // index.json is part of the source tree even though it is not a spec
    // module.  Its symlink must be rejected by the source-index reader.
    const validSource = join(srcDir, "valid.js");
    const linkedIndexTarget = join(
      dirname(srcDir),
      "compiler-linked-index-target.json",
    );
    await writeFile(validSource, `export default { name: "valid" };\n`);
    await writeFile(linkedIndexTarget, JSON.stringify({ completions: ["valid"] }));
    await symlink(linkedIndexTarget, join(srcDir, "index.json"));
    await assert.rejects(
      compileSpecsIr({ srcDir, outDir }),
      /source spec tree contains a symlink or special entry/,
    );
    assert.equal(
      await readFile(join(outDir, "sentinel.txt"), "utf8"),
      "keep previous output\n",
    );
    await rm(join(srcDir, "index.json"), { force: true });
    await rm(linkedIndexTarget, { force: true });
    await rm(validSource, { force: true });

    await writeFile(
      join(srcDir, "unknown.js"),
      `export default { name: "unknown", futureHook: () => [] };\n`,
    );
    await assert.rejects(
      compileSpecsIr({ srcDir, outDir }),
      /unknown function field root\.futureHook/,
    );
    assert.equal(
      await readFile(join(outDir, "sentinel.txt"), "utf8"),
      "keep previous output\n",
    );

    await rm(join(srcDir, "unknown.js"));
    await writeFile(
      join(srcDir, "unmapped.js"),
      `export default { name: "unmapped", metadata: { custom: (tokens) => [] } };\n`,
    );
    await assert.rejects(
      compileSpecsIr({ srcDir, outDir }),
      /converter did not emit a hook for root\.metadata\.custom/,
    );
    assert.equal(
      await readFile(join(outDir, "sentinel.txt"), "utf8"),
      "keep previous output\n",
    );

    await rm(join(srcDir, "unmapped.js"));
    await writeFile(
      join(srcDir, "bound.js"),
      `const hook = ((tokens) => []).bind(null);\nexport default { name: "bound", args: { generators: { custom: hook } } };\n`,
    );
    await assert.rejects(
      compileSpecsIr({ srcDir, outDir }),
      /cannot extract custom hook.*function source is unavailable/,
    );
    assert.equal(
      await readFile(join(outDir, "sentinel.txt"), "utf8"),
      "keep previous output\n",
    );

    assert.throws(
      () =>
        transformDefaultExport(
          `export { spec as default } from "./other.js";\n`,
          "reexport.js",
        ),
      /uses module imports\/re-exports/,
    );
    await rm(join(srcDir, "bound.js"));
    await writeFile(
      join(srcDir, "skipped.js"),
      `export default { description: "not a spec" };\n`,
    );
    await assert.rejects(
      compileSpecsIr({ srcDir, outDir }),
      /source did not produce a static spec/,
    );
    assert.equal(
      await readFile(join(outDir, "sentinel.txt"), "utf8"),
      "keep previous output\n",
    );

    await rm(join(srcDir, "skipped.js"));
    await assert.rejects(
      compileSpecsIr({ srcDir, outDir }),
      /contains no JavaScript specs/,
    );
    assert.equal(
      await readFile(join(outDir, "sentinel.txt"), "utf8"),
      "keep previous output\n",
    );

    await writeFile(
      join(srcDir, "valid.js"),
      `export default { name: "valid" };\n`,
    );
    await writeFile(join(srcDir, "index.json"), "not json\n");
    await assert.rejects(
      compileSpecsIr({ srcDir, outDir }),
      /cannot parse source index/,
    );
    assert.equal(
      await readFile(join(outDir, "sentinel.txt"), "utf8"),
      "keep previous output\n",
    );

    for (const invalidIndex of [
      {},
      { completions: [] },
      { completions: ["valid", 42] },
    ]) {
      await writeFile(join(srcDir, "index.json"), JSON.stringify(invalidIndex));
      await assert.rejects(
        compileSpecsIr({ srcDir, outDir }),
        /completions must be a non-empty array of strings/,
      );
      assert.equal(
        await readFile(join(outDir, "sentinel.txt"), "utf8"),
        "keep previous output\n",
      );
    }
  } finally {
    await Promise.all([
      rm(srcDir, { recursive: true, force: true }),
      rm(outDir, { recursive: true, force: true }),
    ]);
  }
});

test("compiler refuses foreign, parent-linked, and dangling output targets", async () => {
  const srcDir = await mkdtemp(
    join(tmpdir(), "easy-complete-specs-output-guard-src-"),
  );
  const foreignOutDir = await mkdtemp(
    join(tmpdir(), "easy-complete-ir-output-guard-"),
  );
  try {
    await writeFile(
      join(srcDir, "fixture.js"),
      `export default { name: "fixture" };\n`,
    );
    const sentinel = join(foreignOutDir, "USER_SENTINEL");
    await writeFile(sentinel, "keep me\n");
    await assert.rejects(
      compileSpecsIr({ srcDir, outDir: foreignOutDir }),
      /without a valid \.spec-pair\.json/,
    );
    assert.equal(await readFile(sentinel, "utf8"), "keep me\n");

    const parentTarget = await mkdtemp(
      join(tmpdir(), "easy-complete-ir-output-parent-target-"),
    );
    const parentLink = join(
      dirname(parentTarget),
      `${parentTarget.split("/").at(-1)}-link`,
    );
    await symlink(parentTarget, parentLink);
    try {
      await assert.rejects(
        compileSpecsIr({
          srcDir,
          outDir: join(parentLink, "nested-output"),
        }),
        /through a symbolic link/,
      );
    } finally {
      await rm(parentLink, { force: true });
      await rm(parentTarget, { recursive: true, force: true });
    }

    const danglingTarget = join(dirname(foreignOutDir), "missing-ir-target");
    const danglingOutDir = join(dirname(foreignOutDir), "dangling-ir-output");
    await symlink(danglingTarget, danglingOutDir);
    try {
      await assert.rejects(
        compileSpecsIr({ srcDir, outDir: danglingOutDir }),
        /through a symbolic link/,
      );
    } finally {
      await rm(danglingOutDir, { force: true });
    }
  } finally {
    await Promise.all([
      rm(srcDir, { recursive: true, force: true }),
      rm(foreignOutDir, { recursive: true, force: true }),
    ]);
  }
});

test("audit detects a dropped duplicate hook instance, not just a missing hash", async () => {
  const srcDir = await mkdtemp(
    join(tmpdir(), "easy-complete-specs-audit-instance-"),
  );
  const outDir = await mkdtemp(
    join(tmpdir(), "easy-complete-ir-audit-instance-"),
  );
  try {
    await writeFile(
      join(srcDir, "duplicate.js"),
      `const first = (tokens) => [{ name: "row" }];
       const second = (tokens) => [{ name: "row" }];
       export default {
         name: "duplicate",
         args: [{
           name: "value",
           generators: [{ custom: first }, { custom: second }],
         }],
       };
      `,
    );
    await compileSpecsIr({ srcDir, outDir });
    const irPath = join(outDir, "duplicate.json");
    const ir = JSON.parse(await readFile(irPath, "utf8"));
    const [first, second] = ir.args[0].generators;
    assert.notEqual(first.jsCustom, second.jsCustom);
    const firstId = first.jsCustom;
    const secondId = second.jsCustom;
    ir.args[0].generators[1].jsCustom = firstId;
    await writeFile(irPath, `${JSON.stringify(ir)}\n`);

    const report = await auditSpecsHooks({
      sourceRoot: srcDir,
      irRoot: outDir,
    });
    assert.equal(report.ok, false);
    assert.ok(
      report.errors.sourceHookMismatches.some(
        (item) => item.source === "duplicate.js" && item.field === "custom",
      ),
    );
    assert.equal(report.errors.orphanHookFiles.length, 0);
    assert.equal(report.errors.missingHookFiles.length, 0);
  } finally {
    await Promise.all([
      rm(srcDir, { recursive: true, force: true }),
      rm(outDir, { recursive: true, force: true }),
    ]);
  }
});

test("object-method hooks with nested arrows remain standalone callable functions", async () => {
  const srcDir = await mkdtemp(
    join(tmpdir(), "easy-complete-specs-method-hook-"),
  );
  const outDir = await mkdtemp(join(tmpdir(), "easy-complete-ir-method-hook-"));
  try {
    await writeFile(
      join(srcDir, "method.js"),
      `export default {
         name: "method",
         args: [{
           name: "value",
           generators: [{
             script: ["printf", "alpha\\nbeta"],
             postProcess(stdout) {
               return stdout.split("\\n").filter(Boolean).map(name => ({ name }));
             },
           }],
         }],
       };\n`,
    );
    await compileSpecsIr({ srcDir, outDir });
    const ir = JSON.parse(await readFile(join(outDir, "method.json"), "utf8"));
    const hookId = ir.args[0].generators[0].jsPostProcess;
    assert.equal(typeof hookId, "string");
    const report = await auditSpecsHooks({ sourceRoot: srcDir, irRoot: outDir });
    assert.equal(report.ok, true, JSON.stringify(report.errors, null, 2));
    const entry = report.hookManifest.find((item) => item.id === hookId);
    assert.equal(typeof entry?.body, "string");
    assert.match(entry.body, /^function\s*\(/);
    const hook = new Function(`return (${entry.body})`)();
    assert.deepEqual(hook("alpha\nbeta", []), [
      { name: "alpha" },
      { name: "beta" },
    ]);
  } finally {
    await Promise.all([
      rm(srcDir, { recursive: true, force: true }),
      rm(outDir, { recursive: true, force: true }),
    ]);
  }
});

test("audit fails closed for empty trees and invalid source indexes", async () => {
  const srcDir = await mkdtemp(
    join(tmpdir(), "easy-complete-specs-audit-empty-"),
  );
  const irDir = await mkdtemp(join(tmpdir(), "easy-complete-ir-audit-empty-"));
  try {
    const empty = await auditSpecsHooks({
      sourceRoot: srcDir,
      irRoot: irDir,
    });
    assert.equal(empty.ok, false);
    assert.ok(empty.errors.unexpectedSkippedSpecs.length > 0);

    await writeFile(
      join(srcDir, "valid.js"),
      `export default { name: "valid" };\n`,
    );
    await writeFile(join(srcDir, "index.json"), "not json\n");
    await writeFile(join(irDir, "valid.json"), `{"names":["valid"]}\n`);
    const invalidIndex = await auditSpecsHooks({
      sourceRoot: srcDir,
      irRoot: irDir,
    });
    assert.equal(invalidIndex.ok, false);
    assert.ok(
      invalidIndex.errors.sourceReadErrors.some((item) =>
        item.sourceIndex?.endsWith("/index.json"),
      ),
    );

    await writeFile(
      join(srcDir, "index.json"),
      JSON.stringify({ completions: [] }),
    );
    const emptyIndex = await auditSpecsHooks({
      sourceRoot: srcDir,
      irRoot: irDir,
    });
    assert.equal(emptyIndex.ok, false);
    assert.ok(
      emptyIndex.errors.sourceReadErrors.some((item) =>
        item.message.includes(
          "source index completions must be a non-empty array of strings",
        ),
      ),
    );

    await writeFile(
      join(srcDir, "index.json"),
      JSON.stringify({ completions: ["valid", "missing"] }),
    );
    const missingIndexedSource = await auditSpecsHooks({
      sourceRoot: srcDir,
      irRoot: irDir,
    });
    assert.equal(missingIndexedSource.ok, false);
    assert.ok(
      missingIndexedSource.errors.sourceReadErrors.some(
        (item) =>
          item.name === "missing" && item.message.includes("no source module"),
      ),
    );
  } finally {
    await Promise.all([
      rm(srcDir, { recursive: true, force: true }),
      rm(irDir, { recursive: true, force: true }),
    ]);
  }
});

test("audit rejects a hook file whose body cannot run as a standalone expression", async () => {
  const srcDir = await mkdtemp(
    join(tmpdir(), "easy-complete-specs-hook-syntax-"),
  );
  const outDir = await mkdtemp(join(tmpdir(), "easy-complete-ir-hook-syntax-"));
  try {
    await writeFile(
      join(srcDir, "syntax.js"),
      `export default { name: "syntax", args: { generators: { postProcess(stdout) { return stdout.split("\\n").map(name => ({ name })); } } } };\n`,
    );
    await compileSpecsIr({ srcDir, outDir });
    const ir = JSON.parse(await readFile(join(outDir, "syntax.json"), "utf8"));
    const hookId = ir.args[0].jsPostProcess;
    assert.equal(typeof hookId, "string");
    await mkdir(join(outDir, "hooks"));
    await writeFile(
      join(outDir, "hooks", hookFileName(hookId)),
      "export default postProcess(stdout){ return []; };\n",
    );
    const report = await auditSpecsHooks({
      sourceRoot: srcDir,
      irRoot: outDir,
    });
    assert.equal(report.ok, false);
    assert.ok(report.errors.leftoverHookFiles.length > 0);
  } finally {
    await Promise.all([
      rm(srcDir, { recursive: true, force: true }),
      rm(outDir, { recursive: true, force: true }),
    ]);
  }
});

test("compiler applies version diffs and ignores empty ones", async () => {
  const srcDir = await mkdtemp(join(tmpdir(), "easy-complete-specs-versions-"));
  const outDir = await mkdtemp(join(tmpdir(), "easy-complete-ir-versions-"));
  try {
    await mkdir(join(srcDir, "tool"), { recursive: true });
    await writeFile(
      join(srcDir, "tool", "1.0.0.js"),
      `const spec = { name: "tool", subcommands: [{ name: "base" }] };
const versions = { "1.2.0": { subcommands: [{ name: "added", args: { generators: { postProcess: (out) => [{ name: out }] } } }] } };
export { spec as default, versions };\n`,
    );
    await writeFile(
      join(srcDir, "index.json"),
      JSON.stringify({
        completions: ["tool", "tool/1.0.0"],
        diffVersionedCompletions: ["tool"],
      }),
    );
    const applied = await compileSpecsIr({ srcDir, outDir });
    assert.equal(applied.compiled, 1);
    assert.deepEqual(
      applied.appliedVersionDiffs.map((entry) => entry.file),
      ["tool/1.0.0.js"],
    );
    const base = JSON.parse(
      await readFile(join(outDir, "tool", "1.0.0.json"), "utf8"),
    );
    const merged = JSON.parse(
      await readFile(join(outDir, "tool", "1.0.0+1.2.0.json"), "utf8"),
    );
    assert.deepEqual(
      base.subcommands.map((subcommand) => subcommand.names),
      [["base"]],
    );
    assert.deepEqual(
      merged.subcommands.map((subcommand) => subcommand.names),
      [["added"], ["base"]],
    );
    assert.equal(
      merged.subcommands[0].args[0].jsPostProcess,
      "tool/1.0.0#postProcess#0",
    );
    assert.equal(
      merged.subcommands[0].args[0].generators[0].jsPostProcess,
      "tool/1.0.0#postProcess#0",
    );

    await rm(join(srcDir, "tool", "1.0.0.js"));
    await writeFile(
      join(srcDir, "tool", "2.0.0.js"),
      `const spec = { name: "tool", subcommands: [{ name: "base" }] };
const versions = { "2.2.0": {} };
export { spec as default, versions };\n`,
    );
    await writeFile(
      join(srcDir, "index.json"),
      JSON.stringify({
        completions: ["tool", "tool/2.0.0"],
        diffVersionedCompletions: ["tool"],
      }),
    );
    const empty = await compileSpecsIr({ srcDir, outDir });
    assert.equal(empty.compiled, 1);
    const ir = JSON.parse(
      await readFile(join(outDir, "tool", "2.0.0.json"), "utf8"),
    );
    assert.deepEqual(
      ir.subcommands.map((subcommand) => subcommand.names),
      [["base"]],
    );
  } finally {
    await Promise.all([
      rm(srcDir, { recursive: true, force: true }),
      rm(outDir, { recursive: true, force: true }),
    ]);
  }
});

test("compiler records two-level versioned selectors in index.json", async () => {
  const srcDir = await mkdtemp(join(tmpdir(), "easy-complete-specs-selector-"));
  const outDir = await mkdtemp(join(tmpdir(), "easy-complete-ir-selector-"));
  try {
    await mkdir(join(srcDir, "heroku"), { recursive: true });
    await writeFile(
      join(srcDir, "heroku", "8.0.0.js"),
      `const spec = { name: "heroku", subcommands: [{ name: "old" }] };
const versions = { "8.11.1": { subcommands: [{ name: "domains" }] } };
export { spec as default, versions };\n`,
    );
    await writeFile(
      join(srcDir, "heroku", "8.6.0.js"),
      `export default { name: "heroku", subcommands: [{ name: "new" }] };\n`,
    );
    await writeFile(
      join(srcDir, "heroku", "index.js"),
      `export const getVersionCommand = async (exec) => {
  const { stdout } = await exec({ command: "heroku", args: ["--version"] });
  const match = /heroku\\/([0-9]+\\.[0-9]+\\.[0.9]+)/.exec(stdout);
  return match ? match[1] : "8.0.0";
};
export default getVersionCommand;\n`,
    );
    await writeFile(
      join(srcDir, "index.json"),
      JSON.stringify({
        completions: ["heroku"],
        diffVersionedCompletions: ["heroku"],
      }),
    );
    const compiled = await compileSpecsIr({ srcDir, outDir });
    assert.deepEqual(compiled.versionedCommands, ["heroku"]);
    const index = JSON.parse(await readFile(join(outDir, "index.json"), "utf8"));
    assert.deepEqual(index.versioned.heroku.command, ["heroku", "--version"]);
    assert.equal(index.versioned.heroku.parse, "regex");
    assert.equal(index.versioned.heroku.files["8.0.0"], "heroku/8.0.0.json");
    assert.equal(index.versioned.heroku.files["8.6.0"], "heroku/8.6.0.json");
    assert.equal(
      index.versioned.heroku.applied["8.0.0"]["8.11.1"],
      "heroku/8.0.0+8.11.1.json",
    );
    assert.equal(index.files.heroku, "heroku/8.6.0.json");
    const merged = JSON.parse(
      await readFile(join(outDir, "heroku", "8.0.0+8.11.1.json"), "utf8"),
    );
    assert.deepEqual(
      merged.subcommands.map((subcommand) => subcommand.names),
      [["domains"], ["old"]],
    );
  } finally {
    await Promise.all([
      rm(srcDir, { recursive: true, force: true }),
      rm(outDir, { recursive: true, force: true }),
    ]);
  }
});

test("side-effect-free leftovers hard-fail unless a named adapter or EC_ALLOW_UNADAPTED is set", async () => {
  const srcDir = await mkdtemp(join(tmpdir(), "easy-complete-specs-unadapted-"));
  const outDir = await mkdtemp(join(tmpdir(), "easy-complete-ir-unadapted-"));
  const previous = process.env.EC_ALLOW_UNADAPTED;
  try {
    delete process.env.EC_ALLOW_UNADAPTED;
    await writeFile(
      join(srcDir, "leftover.js"),
      `export default {
        name: "leftover",
        args: {
          generators: {
            postProcess(stdout) {
              return stdout.split("\\n").reduce((rows, line) => {
                rows.push({ name: line });
                return rows;
              }, []);
            },
          },
        },
      };\n`,
    );
    await assert.rejects(
      compileSpecsIr({ srcDir, outDir, enforceNamedAdapters: true }),
      /typed compile failed for 1 hook/,
    );
    process.env.EC_ALLOW_UNADAPTED = "1";
    const result = await compileSpecsIr({
      srcDir,
      outDir,
      enforceNamedAdapters: true,
    });
    assert.equal(result.compiled, 1);
  } finally {
    if (previous === undefined) delete process.env.EC_ALLOW_UNADAPTED;
    else process.env.EC_ALLOW_UNADAPTED = previous;
    await Promise.all([
      rm(srcDir, { recursive: true, force: true }),
      rm(outDir, { recursive: true, force: true }),
    ]);
  }
});

test("compiler blocks Node-only regexp syntax until a native adapter exists", async () => {
  const srcDir = await mkdtemp(join(tmpdir(), "easy-complete-specs-regexp-v-"));
  const outDir = await mkdtemp(join(tmpdir(), "easy-complete-ir-regexp-v-"));
  try {
    await writeFile(
      join(srcDir, "regexp-v.js"),
      `export default { name: "regexp-v", args: { generators: { postProcess(stdout) { return /[a&&b]/v.test(stdout) ? [{ name: stdout }] : []; } } } };\n`,
    );
    await assert.rejects(
      compileSpecsIr({ srcDir, outDir }),
      /hookAnalysisErrors/,
    );
  } finally {
    await Promise.all([
      rm(srcDir, { recursive: true, force: true }),
      rm(outDir, { recursive: true, force: true }),
    ]);
  }
});
