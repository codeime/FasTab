#!/usr/bin/env node
/**
 * Compile bundled Fig JS specs into static JSON IR for the Rust engine.
 * Static walk data stays in JSON. Fig functions are compiled to typed IR or
 * named native adapters and recorded in typed-hooks.json. Closure-preserving
 * modules stay in memory only so helper inlining can still run; hooks/,
 * source-modules/, and hook-modules.json are not published. Known Rust
 * builtins still replace matching git/npm scripts.
 */
import {
  mkdir,
  lstat,
  mkdtemp,
  readdir,
  readFile,
  realpath,
  rmdir,
  rm,
  unlink,
  writeFile,
} from "node:fs/promises";
import { createHash } from "node:crypto";
import {
  basename,
  dirname,
  isAbsolute,
  join,
  relative,
  resolve,
  sep,
} from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { Script } from "node:vm";
import * as acorn from "acorn";

import {
  createFilepathsBinder,
  functionSource,
  isFilepathsHelper,
  nativeFilepathsFromHelper,
} from "./filepaths-helper.mjs";
import {
  countFunctionsInValue,
  HOOK_MODULE_MANIFEST,
  HOOK_MODULES_DIR,
  hookFileName,
  KNOWN_NON_SPEC_FILES,
  SUPPORTED_HOOK_FIELDS,
  SUPPORTED_IR_HOOK_FIELDS,
  TYPED_HOOK_CATALOG_MAX_BYTES,
  TYPED_HOOK_CATALOG_MAX_HOOKS,
  TYPED_HOOK_DESCRIPTOR_MAX_BYTES,
  TYPED_HOOK_ID_MAX_BYTES,
  TYPED_HOOK_MODULE_MAX_BYTES,
  TYPED_HOOK_PATH_MAX_BYTES,
  TYPED_HOOK_SIDECAR,
  TYPED_HOOK_SIDECAR_KIND,
  TYPED_HOOK_SIDECAR_VERSION,
  utf8ByteLength,
} from "./spec-hook-contract.mjs";
import {
  applySpecDiff,
  applySpecDiffModuleSource,
  compileGetVersionCommand,
  compareSemver,
  derivedVersionIrRel,
  isEmptyVersionDiff,
  resolveVersionedPath,
  versionDiffKeys,
} from "./spec-versions.mjs";
import {
  assertNoSymlinkInPath,
  PAIR_LOCK_NAME,
  PAIR_MARKER_NAME,
  comparePath,
  createPairMarker,
  pairJournalExists,
  publishPairDirectories,
  verifyPair,
  withPairLock,
  writePairMarker,
} from "./spec-pair.mjs";
import {
  allowUnadaptedHooks,
  formatUnadaptedHookError,
  isRegisteredNativeAdapter,
  loadNativeHookAdapters,
  NAMED_ADAPTER_FIELDS,
} from "./native-hook-adapters.mjs";
import {
  compileTypedHook,
  TYPED_HOOK_CONTRACTS,
  TYPED_HOOK_SIDECAR_FIELDS,
  tryCompileTypedHook,
  typedHookSidecarContracts,
} from "./typed-hook-ir.mjs";

export {
  HOOK_MODULE_MANIFEST,
  HOOK_MODULES_DIR,
  hookFileName,
  KNOWN_NON_SPEC_FILES,
  SUPPORTED_HOOK_FIELDS,
  SUPPORTED_IR_HOOK_FIELDS,
  TYPED_HOOK_SIDECAR,
};

const repoDir = join(dirname(fileURLToPath(import.meta.url)), "..");
const pairLockPath = join(repoDir, "bundle", PAIR_LOCK_NAME);
const canonicalSourceDir = join(repoDir, "bundle", "specs");
const canonicalIrDir = join(repoDir, "bundle", "specs-ir");

const NATIVE_REWRITE_FIELDS = new Set(["custom", "trigger", "getQueryTerm"]);

const GIT_ALIASES_SCRIPT = [
  "git",
  "--no-optional-locks",
  "config",
  "--get-regexp",
  "^alias.",
];

function namesOf(value) {
  if (value == null || value === "") return [];
  if (Array.isArray(value)) return value.flatMap(namesOf);
  if (typeof value === "object") {
    if ("name" in value) return namesOf(value.name);
    return [];
  }
  return [String(value)];
}

function asArray(value) {
  if (value == null) return [];
  if (Array.isArray(value)) return value;
  if (typeof value === "object") return Object.values(value);
  return [];
}

function templatesOf(value) {
  if (value == null) return [];
  const list = Array.isArray(value) ? value : [value];
  const out = [];
  for (const item of list) {
    if (
      item === "filepaths" ||
      item === "folders" ||
      item === "history" ||
      item === "help"
    ) {
      out.push(item);
    }
  }
  return out;
}

function pushGens(list, value) {
  if (value == null) return;
  if (Array.isArray(value)) {
    for (const item of value) pushGens(list, item);
  } else {
    list.push(value);
  }
}

function generatorsOf(node) {
  const list = [];
  if (!node || typeof node !== "object") return list;
  pushGens(list, node.generators);
  pushGens(list, node.generator);
  return list;
}

// `loadSpec` is a runtime escape hatch in Fig specs.  Native IR can keep a
// string reference and resolve it from the bundled JSON at load time, but it
// must never serialize executable JavaScript.  Inline objects are safe because
// they are already plain data; functions and other dynamic values are omitted.
async function applyLoadSpec(raw, ctx, out) {
  if (!raw || typeof raw !== "object" || !out) return;
  if (typeof raw.loadSpec === "string" && raw.loadSpec.trim()) {
    out.loadSpec = raw.loadSpec.trim();
    return;
  }
  if (typeof raw.loadSpec === "function") {
    const hookId = extractHook(ctx.hooks, "loadSpec", raw.loadSpec, {
      owner: raw,
    });
    if (hookId) out.jsLoadSpec = hookId;
    return;
  }
  if (
    raw.loadSpec &&
    typeof raw.loadSpec === "object" &&
    !Array.isArray(raw.loadSpec)
  ) {
    const loaded = await convertNode(raw.loadSpec, ctx);
    if (loaded) out.loadSpec = loaded;
  }
}

function scriptOf(gen) {
  if (!gen || typeof gen !== "object" || typeof gen === "function") return [];
  const script = gen.script;
  if (typeof script === "string" && script.trim()) {
    return ["sh", "-c", script];
  }
  if (
    Array.isArray(script) &&
    script.length > 0 &&
    script.every((part) => typeof part === "string")
  ) {
    return script;
  }
  if (
    script &&
    typeof script === "object" &&
    typeof script.command === "string" &&
    script.command.trim()
  ) {
    const args = Array.isArray(script.args)
      ? script.args.filter((part) => typeof part === "string")
      : [];
    return [script.command, ...args];
  }
  return [];
}

// Keep the numeric millisecond timeout attached to a static script/builtin.
// Fig's runtime compares this value with the user setting and any
// ExecuteCommand timeout using Math.max; invalid values are ignored just as
// a missing generator timeout is.
function splitOnOf(gen) {
  if (!gen || typeof gen !== "object" || typeof gen === "function") {
    return undefined;
  }
  return typeof gen.splitOn === "string" ? gen.splitOn : undefined;
}

export function createHookBag(specId) {
  return {
    specId,
    next: 0,
    files: new Map(),
    extracted: new Map(),
    extractions: [],
    bindings: new Map(),
    reuseByIdentity: false,
  };
}

function sha256(value) {
  return createHash("sha256").update(value).digest("hex");
}

export function sourceModuleFileName(sourcePath) {
  return `${sha256(sourcePath).slice(0, 24)}.js`;
}

function sourcePathSegments(path) {
  if (!/^root(?:\.[A-Za-z_$][\w$]*|\[\d+\])*$/.test(path)) {
    throw new Error(`invalid audited source hook path ${path}`);
  }
  return [...path.matchAll(/\.([A-Za-z_$][\w$]*)|\[(\d+)\]/g)].map(
    (match) => match[1] ?? Number(match[2]),
  );
}

function propertyAccess(root, segments) {
  return segments.reduce(
    (expression, segment) => `${expression}[${JSON.stringify(segment)}]`,
    root,
  );
}

export function transformDefaultExport(source, sourcePath) {
  let ast;
  try {
    ast = acorn.parse(source, {
      ecmaVersion: "latest",
      sourceType: "module",
      allowHashBang: true,
    });
  } catch (error) {
    throw new Error(
      `cannot parse ${sourcePath} for closure-preserving hooks: ${error.message}`,
      { cause: error },
    );
  }
  const suffix = sha256(sourcePath).slice(0, 16);
  const injectedDefault = `__ec_default_${suffix}`;
  let defaultExpression = null;
  const replacements = [];
  for (const node of ast.body) {
    if (
      node.type === "ImportDeclaration" ||
      node.type === "ExportAllDeclaration"
    ) {
      throw new Error(
        `${sourcePath} uses module imports/re-exports; add an explicit build-time bundling adapter before preserving its hook closures`,
      );
    }
    if (node.type === "ExportDefaultDeclaration") {
      if (defaultExpression) {
        throw new Error(`${sourcePath} has more than one default export`);
      }
      const declaration = node.declaration;
      if (
        (declaration.type === "FunctionDeclaration" ||
          declaration.type === "ClassDeclaration") &&
        declaration.id?.name
      ) {
        defaultExpression = declaration.id.name;
        replacements.push({
          start: node.start,
          end: node.end,
          text: source.slice(declaration.start, declaration.end),
        });
      } else {
        defaultExpression = injectedDefault;
        replacements.push({
          start: node.start,
          end: node.end,
          text: `const ${injectedDefault} = (${source.slice(declaration.start, declaration.end)});`,
        });
      }
      continue;
    }
    if (node.type !== "ExportNamedDeclaration") continue;
    if (node.source) {
      throw new Error(
        `${sourcePath} uses module imports/re-exports; add an explicit build-time bundling adapter before preserving its hook closures`,
      );
    }
    for (const specifier of node.specifiers ?? []) {
      const exported = specifier.exported?.name ?? specifier.exported?.value;
      if (exported !== "default") continue;
      if (defaultExpression) {
        throw new Error(`${sourcePath} has more than one default export`);
      }
      if (specifier.local?.type !== "Identifier") {
        throw new Error(`${sourcePath} has a non-identifier default export`);
      }
      defaultExpression = specifier.local.name;
    }
    replacements.push({
      start: node.start,
      end: node.end,
      text: node.declaration
        ? source.slice(node.declaration.start, node.declaration.end)
        : "",
    });
  }
  if (!defaultExpression) {
    throw new Error(
      `${sourcePath} has no statically addressable default export`,
    );
  }
  let body = source;
  for (const replacement of replacements.sort(
    (left, right) => right.start - left.start,
  )) {
    body = `${body.slice(0, replacement.start)}${replacement.text}${body.slice(replacement.end)}`;
  }
  return { body, defaultExpression, suffix };
}

export function closurePreservingHookModule(source, sourcePath, instances) {
  const { body, defaultExpression, suffix } = transformDefaultExport(
    source,
    sourcePath,
  );
  const needsMergedRoot = instances.some((instance) => instance.mergeThrough);
  const rootExpression = (instance) => {
    if (!instance.mergeThrough) return defaultExpression;
    return `__ec_mergedThrough(${defaultExpression}, typeof versions === "undefined" ? {} : versions, ${JSON.stringify(instance.mergeThrough)})`;
  };
  const entries = [...instances]
    .sort((left, right) => comparePath(left.id, right.id))
    .map((instance) => {
      const segments = sourcePathSegments(instance.path);
      if (segments.length === 0) {
        throw new Error(`hook ${instance.id} cannot target the spec root`);
      }
      const root = rootExpression(instance);
      const target = propertyAccess(root, segments);
      if (instance.sourceField !== "custom") {
        return `${JSON.stringify(instance.id)}: ${target}`;
      }
      if (!instance.ownerPath) {
        throw new Error(
          `custom hook ${instance.id} has no compiler-recorded owner path`,
        );
      }
      const ownerSegments = sourcePathSegments(instance.ownerPath);
      const owner = propertyAccess(root, ownerSegments);
      const args = `__ec_args_${suffix}`;
      return `${JSON.stringify(instance.id)}: (...${args}) => Reflect.apply(${target}, ${owner}, ${args})`;
    });
  const prefix = needsMergedRoot ? `${applySpecDiffModuleSource()}\n` : "";
  // The typed compiler strips `export default` and reads the remaining
  // expression as a script. An explicit strict function preserves the original
  // ESM module's top-level semantics: `this` is undefined and accidental
  // globals are rejected.
  return `export default (function () {\n"use strict";\n${prefix}${body}\nreturn Object.freeze({\n${entries.join(",\n")}\n});\n})();\n`;
}

function typedHookContracts() {
  return typedHookSidecarContracts();
}

function assertTypedHookCatalogString(value, label, maxBytes) {
  if (typeof value !== "string" || !value || value.includes("\0")) {
    throw new Error(`${label} must be a string without NUL`);
  }
  if (utf8ByteLength(value) > maxBytes) {
    throw new Error(`${label} exceeds the ${maxBytes}-byte UTF-8 limit`);
  }
}

function assertTypedHookCatalogEntry(id, entry) {
  assertTypedHookCatalogString(id, "typed hook id", TYPED_HOOK_ID_MAX_BYTES);
  if (id.includes("\\")) {
    throw new Error("typed hook id must not contain a backslash");
  }
  assertTypedHookCatalogString(
    entry.path,
    `typed hook ${id} path`,
    TYPED_HOOK_PATH_MAX_BYTES,
  );
  if (!/^root(?:\.[A-Za-z_$][\w$]*|\[\d+\])*$/.test(entry.path)) {
    throw new Error(
      `typed hook ${id} path must be a normalized source object property path`,
    );
  }
  if (
    typeof entry.module !== "string" ||
    !/^[^/\\\0]+\.js$/.test(entry.module) ||
    entry.module === ".js" ||
    entry.module === "..js"
  ) {
    throw new Error(`typed hook ${id} module must be one safe .js basename`);
  }
  if (utf8ByteLength(entry.module) > TYPED_HOOK_MODULE_MAX_BYTES) {
    throw new Error(
      `typed hook ${id} module exceeds the ${TYPED_HOOK_MODULE_MAX_BYTES}-byte UTF-8 limit`,
    );
  }
  for (const field of ["moduleSha256", "functionBodySha256"]) {
    if (
      typeof entry[field] !== "string" ||
      !/^[a-f0-9]{64}$/.test(entry[field])
    ) {
      throw new Error(`typed hook ${id} ${field} must be a lowercase SHA-256`);
    }
  }
  if (!TYPED_HOOK_SIDECAR_FIELDS.includes(entry.sourceField)) {
    throw new Error(
      `typed hook ${id} sourceField must be one of ${TYPED_HOOK_SIDECAR_FIELDS.join(", ")}`,
    );
  }
  if (!entry.descriptor || typeof entry.descriptor !== "object") {
    throw new Error(`typed hook ${id} descriptor must be an object`);
  }
  if (
    utf8ByteLength(JSON.stringify(entry.descriptor)) >
    TYPED_HOOK_DESCRIPTOR_MAX_BYTES
  ) {
    throw new Error(
      `typed hook ${id} descriptor exceeds the ${TYPED_HOOK_DESCRIPTOR_MAX_BYTES}-byte UTF-8 limit`,
    );
  }
}

async function writeTypedHookSidecar({
  stagedOutDir,
  compilerBindings,
  manifestHooks,
  moduleSources,
  enforceNamedAdapters = false,
}) {
  const typedHooks = new Map();
  const adapters = new Map();
  const fields = new Set(TYPED_HOOK_SIDECAR_FIELDS);
  const adapterCatalog = await loadNativeHookAdapters();
  const unadapted = [];
  const bindings = compilerBindings
    .filter((binding) => fields.has(binding.field))
    .sort((left, right) => comparePath(left.id, right.id));

  for (const binding of bindings) {
    const body = functionSource(binding.fn);
    if (!body) {
      throw new Error(
        `cannot compile typed ${binding.field} ${binding.id}: source function body is unavailable`,
      );
    }
    const manifestEntry = manifestHooks.get(binding.id);
    if (!manifestEntry) {
      throw new Error(
        `typed ${binding.field} ${binding.id} has no closure module manifest entry`,
      );
    }
    if (
      manifestEntry.path !== binding.path ||
      manifestEntry.sourceField !== binding.field ||
      manifestEntry.functionBodySha256 !== binding.functionBodySha256
    ) {
      throw new Error(
        `typed ${binding.field} ${binding.id} does not match the compiler identity binding and closure manifest`,
      );
    }
    const moduleSource = moduleSources.get(manifestEntry.module) ?? "";
    const descriptor = tryCompileTypedHook({
      body,
      sourceField: binding.field,
      moduleSource,
    });
    if (!descriptor) {
      if (
        isRegisteredNativeAdapter(
          binding.functionBodySha256,
          binding.field,
          adapterCatalog,
        )
      ) {
        adapters.set(binding.id, {
          path: binding.path,
          sourceField: binding.field,
          functionBodySha256: binding.functionBodySha256,
        });
        continue;
      }
      if (NAMED_ADAPTER_FIELDS.includes(binding.field)) {
        unadapted.push({
          id: binding.id,
          field: binding.field,
          bodySha256: binding.functionBodySha256,
        });
      }
      continue;
    }
    const entry = {
      module: manifestEntry.module,
      moduleSha256: manifestEntry.moduleSha256,
      path: manifestEntry.path,
      sourceField: manifestEntry.sourceField,
      functionBodySha256: manifestEntry.functionBodySha256,
      descriptor,
    };
    assertTypedHookCatalogEntry(binding.id, entry);
    typedHooks.set(binding.id, entry);
  }

  if (typedHooks.size + adapters.size > TYPED_HOOK_CATALOG_MAX_HOOKS) {
    throw new Error(
      `typed hook catalog exceeds the ${TYPED_HOOK_CATALOG_MAX_HOOKS}-hook limit`,
    );
  }
  const sidecar = {
    version: TYPED_HOOK_SIDECAR_VERSION,
    kind: TYPED_HOOK_SIDECAR_KIND,
    contracts: typedHookContracts(),
    hooks: Object.fromEntries(
      [...typedHooks.entries()].sort(([left], [right]) =>
        comparePath(left, right),
      ),
    ),
  };
  if (adapters.size > 0) {
    sidecar.adapters = Object.fromEntries(
      [...adapters.entries()].sort(([left], [right]) =>
        comparePath(left, right),
      ),
    );
  }
  const text = `${JSON.stringify(sidecar)}\n`;
  if (utf8ByteLength(text) > TYPED_HOOK_CATALOG_MAX_BYTES) {
    throw new Error(
      `typed hook catalog exceeds the ${TYPED_HOOK_CATALOG_MAX_BYTES}-byte UTF-8 limit`,
    );
  }
  await writeOutputFile(stagedOutDir, TYPED_HOOK_SIDECAR, text);
  if (
    enforceNamedAdapters &&
    unadapted.length > 0 &&
    !allowUnadaptedHooks()
  ) {
    throw new Error(formatUnadaptedHookError(unadapted));
  }
  return typedHooks.size;
}

function passCtx(ctx, extra = {}) {
  return {
    rootName: extra.rootName ?? ctx.rootName,
    nodeNames: extra.nodeNames ?? ctx.nodeNames,
    hooks: ctx.hooks,
    source: ctx.source,
    binder: ctx.binder,
  };
}

function extractHook(hooks, kind, fn, { owner = null } = {}) {
  if (typeof fn !== "function") return undefined;
  if (!hooks) {
    throw new Error(`cannot extract ${kind} hook without a hook bag`);
  }
  if (hooks.reuseByIdentity) {
    const existing = hooks.extractions.find(
      (item) => item.field === kind && item.fn === fn,
    );
    if (existing) return existing.id;
  }
  const src = functionSource(fn);
  if (!src) {
    throw new Error(
      `cannot extract ${kind} hook in ${hooks.specId}: function source is unavailable`,
    );
  }
  try {
    // JsHost evaluates this exact wrapper, not an ESM module. Catch a method
    // shorthand or other non-expression before replacing the previous IR.
    new Script(`(${src})`);
  } catch (error) {
    throw new Error(
      `cannot extract ${kind} hook in ${hooks.specId}: function is not a standalone JavaScript expression: ${error.message}`,
      { cause: error },
    );
  }
  const id = `${hooks.specId}#${kind}#${hooks.next++}`;
  hooks.files.set(id, `export default ${src};\n`);
  hooks.extractions.push({ id, field: kind, fn, owner });
  let extracted = hooks.extracted.get(kind);
  if (!extracted) {
    extracted = new WeakSet();
    hooks.extracted.set(kind, extracted);
  }
  extracted.add(fn);
  return id;
}

function assertNoUnknownFunctionFields(
  value,
  specId,
  path = "root",
  ancestors = new WeakSet(),
  functions = [],
  owner = null,
  ownerPath = null,
) {
  if (typeof value === "function") return;
  if (!value || typeof value !== "object" || ancestors.has(value)) return;
  ancestors.add(value);
  if (Array.isArray(value)) {
    value.forEach((item, index) =>
      assertNoUnknownFunctionFields(
        item,
        specId,
        `${path}[${index}]`,
        ancestors,
        functions,
        owner,
        ownerPath,
      ),
    );
  } else {
    for (const key of Object.keys(value).sort(comparePath)) {
      const childPath = `${path}.${key}`;
      if (
        typeof value[key] === "function" &&
        !Object.hasOwn(SUPPORTED_HOOK_FIELDS, key)
      ) {
        throw new Error(
          `unknown function field ${childPath} in ${specId}; add explicit compiler support before shipping it`,
        );
      }
      if (typeof value[key] === "function") {
        functions.push({
          field: key,
          path: childPath,
          fn: value[key],
          owner: value,
          ownerPath: path,
          mergeThrough: null,
          nativeRewrite:
            isFilepathsHelper(value) && NATIVE_REWRITE_FIELDS.has(key),
        });
      }
      assertNoUnknownFunctionFields(
        value[key],
        specId,
        childPath,
        ancestors,
        functions,
        value,
        path,
      );
    }
  }
  ancestors.delete(value);
}

function bindExtractedHooks(hooks, sourceFunctions, specId) {
  const candidatesByFunction = new Map();
  for (const record of sourceFunctions) {
    if (record.nativeRewrite) continue;
    const key = `${record.field}`;
    const records = candidatesByFunction.get(record.fn) ?? new Map();
    const byField = records.get(key) ?? [];
    byField.push(record);
    records.set(key, byField);
    candidatesByFunction.set(record.fn, records);
  }

  for (const extraction of hooks.extractions) {
    const byField = candidatesByFunction.get(extraction.fn);
    const candidates = byField?.get(extraction.field) ?? [];
    let matching = candidates;
    if (extraction.field === "custom" && extraction.owner) {
      matching = candidates.filter(
        (candidate) => candidate.owner === extraction.owner,
      );
    }
    if (matching.length === 0) {
      throw new Error(
        `cannot bind ${extraction.field} hook ${extraction.id} in ${specId}: no compiler-collected source path matches the function identity${extraction.field === "custom" ? " and owner" : ""}`,
      );
    }
    const owners = new Set(matching.map((candidate) => candidate.owner));
    if (extraction.field === "custom" && owners.size > 1) {
      throw new Error(
        `cannot bind custom hook ${extraction.id} in ${specId}: function identity has multiple owners`,
      );
    }
    // Non-custom hooks can be shared by several source paths because the
    // function object (and therefore its closure) is identical.  Selecting a
    // stable path makes the provenance deterministic without changing the
    // callable value.  Custom hooks always retain their exact owner path.
    const [selected] = [...matching].sort((left, right) => {
      if (!left.mergeThrough && right.mergeThrough) return -1;
      if (left.mergeThrough && !right.mergeThrough) return 1;
      return comparePath(left.path, right.path);
    });
    hooks.bindings.set(extraction.id, {
      id: extraction.id,
      field: extraction.field,
      path: selected.path,
      ownerPath: selected.ownerPath,
      mergeThrough: selected.mergeThrough ?? null,
      functionBodySha256: sha256(functionSource(extraction.fn)),
      fn: extraction.fn,
      owner: selected.owner,
    });
  }
}

function assertFunctionsExtracted(functions, hooks, specId) {
  for (const { field, path, fn, nativeRewrite } of functions) {
    if (nativeRewrite) continue;
    const extracted = hooks.extracted.get(field);
    if (!extracted?.has(fn)) {
      throw new Error(
        `cannot extract ${field} hook in ${specId}: converter did not emit a hook for ${path}`,
      );
    }
  }
}

function cacheFieldsOf(gen) {
  const cache = gen && typeof gen === "object" ? gen.cache : null;
  if (!cache || typeof cache !== "object") return {};
  const out = {};
  if (typeof cache.cacheKey === "string" && cache.cacheKey) {
    out.cacheKey = cache.cacheKey;
  }
  if (typeof cache.cacheByDirectory === "boolean") {
    out.cacheByDirectory = cache.cacheByDirectory;
  }
  if (typeof cache.ttl === "number" && Number.isFinite(cache.ttl)) {
    out.cacheTtl = Math.trunc(cache.ttl);
  }
  if (
    cache.strategy === "max-age" ||
    cache.strategy === "stale-while-revalidate"
  ) {
    out.cacheStrategy = cache.strategy;
  }
  return out;
}

function triggerOf(gen, ctx = {}) {
  if (!gen || typeof gen !== "object") return undefined;
  const trigger = gen.trigger;
  if (trigger == null) return undefined;
  if (typeof trigger === "string") return { on: "string", string: trigger };
  if (typeof trigger === "function") {
    const hookId = extractHook(ctx.hooks, "trigger", trigger, { owner: gen });
    return hookId ? { on: "function", jsTrigger: hookId } : undefined;
  }
  if (typeof trigger !== "object") return undefined;
  if (trigger.on === "threshold") {
    const length =
      typeof trigger.length === "number" && Number.isFinite(trigger.length)
        ? Math.trunc(trigger.length)
        : 0;
    return { on: "threshold", length };
  }
  if (trigger.on === "match") {
    const strings = Array.isArray(trigger.string)
      ? trigger.string.filter((item) => typeof item === "string")
      : typeof trigger.string === "string"
        ? [trigger.string]
        : [];
    return { on: "match", string: strings };
  }
  return { on: "change" };
}

function debounceMsOf(arg) {
  if (!arg || typeof arg !== "object") return undefined;
  if (arg.debounce === true) return 200;
  if (
    typeof arg.debounce === "number" &&
    Number.isFinite(arg.debounce) &&
    arg.debounce > 0
  ) {
    return Math.trunc(arg.debounce);
  }
  return undefined;
}

function scriptTimeoutOf(gen) {
  if (!gen || typeof gen !== "object" || typeof gen === "function") {
    return undefined;
  }
  const values = [gen.scriptTimeout];
  if (
    gen.script &&
    typeof gen.script === "object" &&
    !Array.isArray(gen.script)
  ) {
    values.push(gen.script.timeout);
  }
  const numeric = values
    .filter((value) => typeof value === "number" && Number.isFinite(value))
    .map((value) => Math.trunc(value));
  return numeric.length ? Math.max(...numeric) : undefined;
}

function normalizeArgName(value) {
  const names = namesOf(value);
  return names.length ? names[0].toLowerCase() : "";
}

// Map the same argv that the Fig generator used to a native Rust generator.
// Keeping this based on argv is important: `remote` and `branch` arguments in
// push/pull/fetch intentionally have different data sources.
function inferBuiltinFromScript(rootName, _nodeNames, _argName, script) {
  const root = String(rootName ?? "").toLowerCase();
  if (root !== "git" || !Array.isArray(script) || script.length === 0)
    return null;
  const argv = script.map((part) => String(part).toLowerCase());
  if (argv.includes("status") && argv.includes("--short"))
    return "git-changed-files";
  if (argv.includes("diff") && argv.includes("--name-only"))
    return "git-changed-files";
  if (
    argv.includes("remote") &&
    (argv.includes("-v") || argv.includes("--verbose"))
  ) {
    return "git-remotes";
  }
  if (argv.includes("remote")) return "git-remotes";
  if (argv.includes("tag") && argv.includes("--list")) return "git-tags";
  if (argv.includes("stash") && argv.includes("list")) return "git-stashes";
  if (argv.includes("branch")) return "git-branches";
  if (argv.includes("log") || argv.includes("rev-list")) return "git-commits";
  return null;
}

function inferScriptBuiltin(rootName, script) {
  const root = String(rootName ?? "").toLowerCase();
  if (
    root === "git" &&
    Array.isArray(script) &&
    script.length === GIT_ALIASES_SCRIPT.length &&
    script.every((part, index) => part === GIT_ALIASES_SCRIPT[index])
  ) {
    return "git-aliases";
  }
  return null;
}

function suggestionSeedsOf(value, ctx = {}) {
  if (value == null) return [];
  const list = Array.isArray(value) ? value : [value];
  return list
    .map((item) => {
      const names = namesOf(item);
      if (!names.length) return null;
      const seed = { names };
      if (item && typeof item === "object" && item.description) {
        seed.description = String(item.description);
      }
      const argsHint = argsHintOf(
        item && typeof item === "object" ? item.args : undefined,
      );
      if (argsHint) seed.argsHint = argsHint;
      copySuggestionMetadata(item, seed, ctx);
      return seed;
    })
    .filter(Boolean);
}

function argsHintOf(value) {
  if (value == null) return "";
  const args = Array.isArray(value) ? value : [value];
  return args
    .filter((arg) => arg && typeof arg === "object" && arg.name != null)
    .map((arg) => {
      const rawName = Array.isArray(arg.name) ? arg.name[0] : arg.name;
      if (rawName == null || rawName === "") return "";
      const name = String(rawName);
      const base = arg.isVariadic ? `${name}...` : name;
      return arg.isOptional ? `[${base}]` : `<${base}>`;
    })
    .filter(Boolean)
    .join(" ");
}

function filterStrategyOf(raw) {
  const strategy = raw?.filterStrategy;
  return ["prefix", "fuzzy", "default"].includes(strategy)
    ? strategy
    : undefined;
}

// Keep the metadata that affects acceptance and ordering in the static IR.
// Runtime generators are intentionally still omitted, but a static suggestion
// must behave like the same suggestion did in the WebView implementation.
function copySuggestionMetadata(raw, out, ctx = {}) {
  if (!raw || typeof raw !== "object") return;
  if (raw.insertValue != null) out.insertValue = String(raw.insertValue);
  if (raw.displayName != null) out.displayName = String(raw.displayName);
  if (raw.separatorToAdd != null)
    out.separatorToAdd = String(raw.separatorToAdd);
  // A static suggestion may override the default kind assigned by the
  // surrounding spec (for example a folder/file row in an argument's
  // `suggestions` array).  Keep both type values because auto-execute rows
  // use originalType to recover the underlying row type.
  if (typeof raw.type === "string" && raw.type) out.type = raw.type;
  if (typeof raw.originalType === "string" && raw.originalType) {
    out.originalType = raw.originalType;
  }
  if (typeof raw.getQueryTerm === "string") out.getQueryTerm = raw.getQueryTerm;
  else if (typeof raw.getQueryTerm === "function") {
    const hookId = extractHook(ctx.hooks, "getQueryTerm", raw.getQueryTerm, {
      owner: raw,
    });
    if (hookId) out.jsGetQueryTerm = hookId;
  }
  if (typeof raw.getQueryTerm !== "string" && raw.getQueryTerm == null) {
    for (const generator of generatorsOf(raw)) {
      if (isFilepathsHelper(generator)) {
        out.getQueryTerm = "/";
        break;
      }
      if (!generator || typeof generator !== "object") continue;
      if (typeof generator.getQueryTerm === "string") {
        out.getQueryTerm = generator.getQueryTerm;
        break;
      }
      if (typeof generator.getQueryTerm === "function") {
        const hookId = extractHook(
          ctx.hooks,
          "getQueryTerm",
          generator.getQueryTerm,
          { owner: generator },
        );
        if (hookId) {
          out.jsGetQueryTerm = hookId;
          break;
        }
      }
    }
  }
  if (typeof raw.shouldAddSpace === "boolean")
    out.shouldAddSpace = raw.shouldAddSpace;
  if (typeof raw.hidden === "boolean") out.hidden = raw.hidden;
  if (typeof raw.priority === "number" && Number.isFinite(raw.priority)) {
    const priority = Math.trunc(raw.priority);
    out.priority = priority === 0 ? 50 : Math.max(0, Math.min(100, priority));
  }
  if (
    raw.icon != null &&
    (typeof raw.icon === "string" || typeof raw.icon === "number")
  ) {
    out.icon = String(raw.icon);
  }
  if (typeof raw.isDangerous === "boolean") out.isDangerous = raw.isDangerous;
}

function separatorToAdd(raw) {
  if (!raw || typeof raw !== "object") return undefined;
  // A separator only belongs on options whose first argument is mandatory;
  // this mirrors suggestions/index.ts and avoids inserting `=` for optional
  // values.  Leave boolean `requiresSeparator: true` to the native lookup so
  // it can use parserDirectives.optionArgSeparators instead of baking in `=`.
  const firstArg = Array.isArray(raw.args) ? raw.args[0] : raw.args;
  if (firstArg && typeof firstArg === "object" && firstArg.isOptional)
    return undefined;
  if (typeof raw.requiresSeparator === "string") return raw.requiresSeparator;
  if (raw.requiresEquals) return "=";
  return undefined;
}

// Option state is consumed by the parser before suggestions are filtered.
// Keep the JSON values (rather than collapsing them to booleans) so the
// native engine can preserve Fig's `isRepeatable: true` (unlimited) versus a
// numeric repetition limit.
function copyOptionStateMetadata(raw, out) {
  if (!raw || typeof raw !== "object") return;
  const exclusiveOn = namesOf(raw.exclusiveOn);
  if (exclusiveOn.length) out.exclusiveOn = exclusiveOn;
  const dependsOn = namesOf(raw.dependsOn);
  if (dependsOn.length) out.dependsOn = dependsOn;
  if (typeof raw.isRepeatable === "boolean") {
    out.isRepeatable = raw.isRepeatable;
  } else if (
    typeof raw.isRepeatable === "number" &&
    Number.isFinite(raw.isRepeatable)
  ) {
    out.isRepeatable = raw.isRepeatable;
  }
  if (raw.isPersistent === true) out.isPersistent = true;
}

function parserDirectivesOf(raw, ctx = {}) {
  const directives = raw?.parserDirectives;
  if (!directives || typeof directives !== "object") return undefined;
  const out = {};
  if (typeof directives.optionsMustPrecedeArguments === "boolean") {
    out.optionsMustPrecedeArguments = directives.optionsMustPrecedeArguments;
  }
  if (typeof directives.flagsArePosixNoncompliant === "boolean") {
    out.flagsArePosixNoncompliant = directives.flagsArePosixNoncompliant;
  }
  if (Array.isArray(directives.optionArgSeparators)) {
    const separators = directives.optionArgSeparators.filter(
      (separator) => typeof separator === "string",
    );
    out.optionArgSeparators = separators;
  }
  if (typeof directives.alias === "string" && directives.alias) {
    out.alias = directives.alias;
  } else if (typeof directives.alias === "function") {
    const hookId = extractHook(ctx.hooks, "alias", directives.alias, {
      owner: directives,
    });
    if (hookId) out.jsAlias = hookId;
  }
  return Object.keys(out).length ? out : undefined;
}

async function convertArg(raw, ctx) {
  const arg = raw && typeof raw === "object" ? raw : {};
  let templates = templatesOf(arg.template);
  let script = [];
  let splitOn;
  let scriptTimeout;
  let jsPostProcess;
  let jsCustom;
  let jsScript;
  let cacheFields = {};
  const nativeBuiltins = [];
  const generators = [];
  const argName = normalizeArgName(arg.name);
  for (const gen of generatorsOf(arg)) {
    const converted = await convertGenerator(gen, ctx, argName);
    if (converted) generators.push(converted);
    const fromScript = converted?.script ?? [];
    const builtin = converted?.builtin;
    if (builtin) nativeBuiltins.push(builtin);
    else if (!script.length && fromScript.length) {
      script = fromScript;
      if (converted.splitOn !== undefined) splitOn = converted.splitOn;
    }
    if (converted?.jsPostProcess && !jsPostProcess) {
      jsPostProcess = converted.jsPostProcess;
    }
    if (converted?.jsCustom && !jsCustom) jsCustom = converted.jsCustom;
    if (converted?.jsScript && !jsScript) jsScript = converted.jsScript;
    if (converted?.scriptTimeout !== undefined) {
      scriptTimeout =
        scriptTimeout === undefined
          ? converted.scriptTimeout
          : Math.max(scriptTimeout, converted.scriptTimeout);
    }
    const nextCache = cacheFieldsOf(gen);
    if (Object.keys(nextCache).length)
      cacheFields = { ...cacheFields, ...nextCache };
    if (converted?.templates?.length) {
      templates = [...new Set([...templates, ...converted.templates])];
    }
  }

  const scriptBuiltin = inferScriptBuiltin(ctx.rootName, script);
  if (scriptBuiltin) nativeBuiltins.push(scriptBuiltin);
  const uniqueBuiltins = [...new Set(nativeBuiltins)];
  // A recognized git/npm argv is replaced by the native builtin that
  // implements the same script. postProcess/custom stay: they are not the
  // script. Unrecognized scripts are left as argv + hooks, matching Fig.
  if (scriptBuiltin) {
    script = [];
    splitOn = undefined;
  }

  const out = {};
  if (arg.name != null && arg.name !== "") {
    const name = Array.isArray(arg.name) ? arg.name[0] : arg.name;
    if (name) out.name = String(name);
  }
  if (arg.description) out.description = String(arg.description);
  copySuggestionMetadata(arg, out, ctx);
  // Preserve an explicit false separately from an omitted value. The native
  // lookup uses this as an argument-level override of the global setting.
  if (typeof arg.suggestCurrentToken === "boolean") {
    out.suggestCurrentToken = arg.suggestCurrentToken;
  }
  if (typeof arg.optionsCanBreakVariadicArg === "boolean") {
    out.optionsCanBreakVariadicArg = arg.optionsCanBreakVariadicArg;
  }
  const filterStrategy = filterStrategyOf(arg);
  if (filterStrategy) out.filterStrategy = filterStrategy;
  await applyLoadSpec(arg, ctx, out);
  const argDirectives = parserDirectivesOf(arg, ctx);
  if (argDirectives) out.parserDirectives = argDirectives;
  const debounceMs = debounceMsOf(arg);
  if (debounceMs !== undefined) out.debounceMs = debounceMs;
  if (templates.length) out.templates = templates;
  if (script.length) out.script = script;
  if (splitOn !== undefined) out.splitOn = splitOn;
  if (scriptTimeout !== undefined) out.scriptTimeout = scriptTimeout;
  if (jsPostProcess) out.jsPostProcess = jsPostProcess;
  if (jsCustom) out.jsCustom = jsCustom;
  if (jsScript) out.jsScript = jsScript;
  if (cacheFields.cacheKey) out.cacheKey = cacheFields.cacheKey;
  if (typeof cacheFields.cacheByDirectory === "boolean") {
    out.cacheByDirectory = cacheFields.cacheByDirectory;
  }
  if (cacheFields.cacheTtl !== undefined) out.cacheTtl = cacheFields.cacheTtl;
  if (cacheFields.cacheStrategy) out.cacheStrategy = cacheFields.cacheStrategy;
  if (uniqueBuiltins.length === 1) out.builtin = uniqueBuiltins[0];
  else if (uniqueBuiltins.length > 1) out.builtins = uniqueBuiltins;
  if (generators.length) out.generators = generators;
  const suggestions = suggestionSeedsOf(arg.suggestions, ctx);
  if (arg.isOptional) out.isOptional = true;
  if (arg.isVariadic) out.isVariadic = true;
  if (arg.isCommand) out.isCommand = true;
  if (arg.isScript) out.isScript = true;
  if (typeof arg.isModule === "string" && arg.isModule) {
    out.isModule = arg.isModule;
  }
  if (suggestions.length) out.suggestions = suggestions;
  if (!out.getQueryTerm) {
    const queryTerm = generators.find(
      (generator) => generator.getQueryTerm,
    )?.getQueryTerm;
    if (queryTerm) out.getQueryTerm = queryTerm;
  }
  return out;
}

async function convertGenerator(gen, ctx, argName) {
  if (!gen || (typeof gen !== "object" && typeof gen !== "function"))
    return null;
  if (isFilepathsHelper(gen)) {
    const hints = [argName, ...(ctx.nodeNames ?? [])];
    const literal = ctx.binder?.take(hints) ?? null;
    return nativeFilepathsFromHelper(gen, literal);
  }
  if (typeof gen === "function") return null;
  const out = {};
  const templates = templatesOf(gen.template);
  if (templates.length) out.templates = templates;
  const fromScript = scriptOf(gen);
  // Fig shapes a script's stdout with splitOn, else postProcess, else shows
  // nothing. A shaped script keeps its own argv so the typed postProcess sees
  // the output it was written for: `git branch -a --sort=-committerdate` gives
  // it the current-branch marker, remote prefixes and recency order that a
  // builtin's `for-each-ref` listing does not. A builtin only stands in for a
  // script Fig would have shown nothing for.
  const shapesOutput =
    typeof gen.postProcess === "function" || splitOnOf(gen) !== undefined;
  const builtin = shapesOutput
    ? null
    : inferBuiltinFromScript(
        ctx.rootName,
        ctx.nodeNames ?? [],
        argName,
        fromScript,
      );
  if (builtin) out.builtin = builtin;
  else if (fromScript.length) out.script = fromScript;
  const splitOn = splitOnOf(gen);
  if (splitOn !== undefined && !builtin) out.splitOn = splitOn;
  const scriptTimeout = scriptTimeoutOf(gen);
  if (scriptTimeout !== undefined) out.scriptTimeout = scriptTimeout;
  if (typeof gen.script === "function") {
    const hookId = extractHook(ctx.hooks, "script", gen.script, {
      owner: gen,
    });
    if (hookId) out.jsScript = hookId;
  }
  if (typeof gen.postProcess === "function") {
    const hookId = extractHook(ctx.hooks, "postProcess", gen.postProcess, {
      owner: gen,
    });
    if (hookId) out.jsPostProcess = hookId;
  }
  if (typeof gen.custom === "function") {
    const hookId = extractHook(ctx.hooks, "custom", gen.custom, {
      owner: gen,
    });
    if (hookId) out.jsCustom = hookId;
  }
  if (typeof gen.filterTemplateSuggestions === "function") {
    const hookId = extractHook(
      ctx.hooks,
      "filterTemplateSuggestions",
      gen.filterTemplateSuggestions,
      { owner: gen },
    );
    if (hookId) out.jsFilterTemplateSuggestions = hookId;
  }
  if (typeof gen.getQueryTerm === "string") out.getQueryTerm = gen.getQueryTerm;
  else if (typeof gen.getQueryTerm === "function") {
    const hookId = extractHook(ctx.hooks, "getQueryTerm", gen.getQueryTerm, {
      owner: gen,
    });
    if (hookId) out.jsGetQueryTerm = hookId;
  }
  Object.assign(out, cacheFieldsOf(gen));
  const trigger = triggerOf(gen, ctx);
  if (trigger) out.trigger = trigger;
  return Object.keys(out).length ? out : null;
}

async function convertOption(raw, ctx = {}) {
  if (!raw || typeof raw !== "object") return null;
  const names = namesOf(raw.name);
  if (!names.length) return null;
  const out = { names };
  if (raw.description) out.description = String(raw.description);
  copySuggestionMetadata(raw, out, ctx);
  copyOptionStateMetadata(raw, out);
  await applyLoadSpec(raw, ctx, out);
  const separator = separatorToAdd(raw);
  if (separator !== undefined && out.separatorToAdd === undefined) {
    out.separatorToAdd = separator;
  }
  const argsRaw = Array.isArray(raw.args)
    ? raw.args
    : raw.args
      ? [raw.args]
      : [];
  const args = [];
  for (const arg of argsRaw) {
    const converted = await convertArg(
      arg,
      passCtx(ctx, {
        rootName: ctx.rootName ?? "",
        nodeNames: names,
      }),
    );
    if (Object.keys(converted).length > 0) args.push(converted);
  }
  if (args.length) out.args = args;
  return out;
}

async function convertNode(raw, ctx) {
  if (!raw || typeof raw !== "object") return null;
  const names = namesOf(raw.name);
  if (!names.length) return null;
  const rootName = ctx.rootName ?? names[0];
  const childCtx = passCtx(ctx, { rootName, nodeNames: names });
  const out = { names };
  if (raw.description) out.description = String(raw.description);
  copySuggestionMetadata(raw, out, ctx);
  if (typeof raw.requiresSubcommand === "boolean") {
    out.requiresSubcommand = raw.requiresSubcommand;
  }
  const filterStrategy = filterStrategyOf(raw);
  if (filterStrategy) out.filterStrategy = filterStrategy;
  const parserDirectives = parserDirectivesOf(raw, ctx);
  if (parserDirectives) out.parserDirectives = parserDirectives;

  const additionalSuggestions = suggestionSeedsOf(
    raw.additionalSuggestions,
    ctx,
  );
  if (additionalSuggestions.length)
    out.additionalSuggestions = additionalSuggestions;

  if (typeof raw.generateSpec === "function") {
    const jsGenerateSpec = extractHook(
      ctx.hooks,
      "generateSpec",
      raw.generateSpec,
      { owner: raw },
    );
    if (jsGenerateSpec) out.jsGenerateSpec = jsGenerateSpec;
    if (
      typeof raw.generateSpecCacheKey === "string" &&
      raw.generateSpecCacheKey
    ) {
      out.generateSpecCacheKey = raw.generateSpecCacheKey;
    }
  }

  const subcommands = [];
  for (const item of asArray(raw.subcommands)) {
    const converted = await convertNode(item, passCtx(ctx, { rootName }));
    if (converted) subcommands.push(converted);
  }
  if (subcommands.length) out.subcommands = subcommands;

  const options = [];
  for (const option of asArray(raw.options)) {
    const converted = await convertOption(option, childCtx);
    if (converted) options.push(converted);
  }
  const persistentOptions = options.filter(
    (option) => option.isPersistent === true,
  );
  const regularOptions = options.filter(
    (option) => option.isPersistent !== true,
  );
  if (regularOptions.length) out.options = regularOptions;
  if (persistentOptions.length) out.persistentOptions = persistentOptions;

  const argsRaw = Array.isArray(raw.args)
    ? raw.args
    : raw.args
      ? [raw.args]
      : [];
  let args = [];
  for (const arg of argsRaw) {
    const converted = await convertArg(arg, childCtx);
    if (Object.keys(converted).length > 0) args.push(converted);
  }

  // Spec-level generators (not under args) attach to the first argument so
  // the native runner can execute them the same way as arg-level generators.
  if (args.every((arg) => !arg.script && !arg.jsCustom && !arg.jsScript)) {
    for (const gen of generatorsOf(raw)) {
      if (isFilepathsHelper(gen)) {
        const converted = await convertGenerator(gen, ctx, "");
        if (converted) {
          if (args.length === 0) args = [{}];
          args[0].templates = [
            ...new Set([
              ...(args[0].templates ?? []),
              ...(converted.templates ?? []),
            ]),
          ];
          args[0].generators = [...(args[0].generators ?? []), converted];
          if (converted.getQueryTerm && !args[0].getQueryTerm) {
            args[0].getQueryTerm = converted.getQueryTerm;
          }
        }
        break;
      }
      const script = scriptOf(gen);
      const jsScript =
        typeof gen?.script === "function"
          ? extractHook(ctx.hooks, "script", gen.script, { owner: gen })
          : undefined;
      const jsCustom =
        typeof gen?.custom === "function"
          ? extractHook(ctx.hooks, "custom", gen.custom, { owner: gen })
          : undefined;
      if (!script.length && !jsScript && !jsCustom) continue;
      if (args.length === 0) args = [{}];
      if (script.length && !args[0].script) {
        args[0].script = script;
        const splitOn = splitOnOf(gen);
        if (splitOn !== undefined) args[0].splitOn = splitOn;
        if (typeof gen.postProcess === "function") {
          const jsPostProcess = extractHook(
            ctx.hooks,
            "postProcess",
            gen.postProcess,
            { owner: gen },
          );
          if (jsPostProcess) args[0].jsPostProcess = jsPostProcess;
        }
      }
      if (jsScript) args[0].jsScript = jsScript;
      if (jsCustom) args[0].jsCustom = jsCustom;
      const scriptTimeout = scriptTimeoutOf(gen);
      if (scriptTimeout !== undefined) args[0].scriptTimeout = scriptTimeout;
      Object.assign(args[0], cacheFieldsOf(gen));
      break;
    }
  }

  if (args.length) out.args = args;

  // A plain object loadSpec follows the parser's replacement semantics: the
  // loaded object replaces the wrapper's fields, while the wrapper names are
  // retained so aliases such as `docker compose` still match the typed tree.
  // String references stay lazy and are resolved by the Rust Registry,
  // avoiding a large copy of gcloud's child specs in gcloud.json.
  if (
    raw.loadSpec &&
    typeof raw.loadSpec === "object" &&
    !Array.isArray(raw.loadSpec)
  ) {
    const loaded = await convertNode(raw.loadSpec, passCtx(ctx, { rootName }));
    if (loaded) return replaceNodeWithLoaded(out, loaded);
  } else {
    await applyLoadSpec(raw, passCtx(ctx, { rootName }), out);
  }
  return out;
}

function replaceNodeWithLoaded(wrapper, loaded) {
  return {
    ...loaded,
    // The parser replaces the wrapper object, but the native tree still needs
    // the spelling used by its parent (for example `compose` vs
    // `docker-compose`) to locate the loaded node.
    names: wrapper.names?.length ? wrapper.names : (loaded.names ?? []),
  };
}

function assertSafeSourceRelativePath(path) {
  if (
    typeof path !== "string" ||
    !path ||
    path.includes("\0") ||
    path.includes("\\") ||
    isAbsolute(path) ||
    path
      .split("/")
      .some((segment) => !segment || segment === "." || segment === "..")
  ) {
    throw new Error(`unsafe source spec path: ${path}`);
  }
}

function relativeSourcePath(base, full) {
  const raw = relative(base, full);
  // On POSIX, a backslash is a legal filename character.  Reject the raw
  // spelling before any conversion so `..\\..\\outside.js` cannot become an
  // apparently nested path.  Windows uses backslash as its path separator,
  // and its filenames cannot contain one, so normalize only on that platform.
  if (process.platform !== "win32") {
    assertSafeSourceRelativePath(raw);
    return raw;
  }
  const normalized = raw.split(sep).join("/");
  assertSafeSourceRelativePath(normalized);
  return normalized;
}

function assertPathInsideRoot(root, path, label) {
  const relation = relative(resolve(root), resolve(path));
  if (
    relation === "" ||
    isAbsolute(relation) ||
    relation === ".." ||
    relation.startsWith(`..${sep}`)
  ) {
    throw new Error(`${label} escapes its root: ${path}`);
  }
}

async function writeOutputFile(root, path, value) {
  assertSafeSourceRelativePath(path);
  const destination = join(root, path);
  assertPathInsideRoot(root, destination, "compiled IR output path");
  await assertNoSymlinkInPath(destination);
  await mkdir(dirname(destination), { recursive: true });
  await assertNoSymlinkInPath(destination);
  await writeFile(destination, value);
}

async function validateIgnoredSourceDirectory(dir, base) {
  const rootInfo = await lstat(dir);
  if (rootInfo.isSymbolicLink() || !rootInfo.isDirectory()) {
    throw new Error(
      `source spec tree contains a symlink or special entry: ${dir}`,
    );
  }
  const entries = await readdir(dir, { withFileTypes: true });
  for (const entry of entries) {
    const full = join(dir, entry.name);
    relativeSourcePath(base, full);
    const info = await lstat(full);
    if (info.isDirectory()) {
      await validateIgnoredSourceDirectory(full, base);
    } else if (!info.isFile()) {
      throw new Error(
        `source spec tree contains a symlink or special entry: ${full}`,
      );
    }
  }
}

async function walkJs(dir, base = dir, acc = []) {
  const rootInfo = await lstat(dir);
  if (rootInfo.isSymbolicLink() || !rootInfo.isDirectory()) {
    throw new Error(
      `source spec tree contains a symlink or special entry: ${dir}`,
    );
  }
  let entries;
  try {
    entries = await readdir(dir, { withFileTypes: true });
  } catch (error) {
    throw new Error(
      `cannot read source spec directory ${dir}: ${error.message}`,
      { cause: error },
    );
  }
  // Filesystem directory order is not part of the compiler contract.  Sort
  // every level so hook ids and generated output are reproducible.
  entries.sort((left, right) => comparePath(left.name, right.name));
  for (const entry of entries) {
    const full = join(dir, entry.name);
    const info = await lstat(full);
    const path = relativeSourcePath(base, full);
    if (info.isDirectory()) {
      if (entry.name === "icons") {
        await validateIgnoredSourceDirectory(full, base);
        continue;
      }
      await walkJs(full, base, acc);
    } else if (
      info.isFile() &&
      entry.name.endsWith(".js") &&
      (entry.name !== "index.js" || dir !== base)
    ) {
      acc.push(path);
    } else if (!info.isFile()) {
      throw new Error(
        `source spec tree contains a symlink or special entry: ${full}`,
      );
    }
  }
  return acc;
}

async function readSourceIndex(srcDir) {
  const sourceIndexPath = join(srcDir, "index.json");
  let text;
  try {
    const info = await lstat(sourceIndexPath);
    if (info.isSymbolicLink() || !info.isFile()) {
      throw new Error(
        `source index is a symlink or special entry: ${sourceIndexPath}`,
      );
    }
    text = await readFile(sourceIndexPath, "utf8");
  } catch (error) {
    if (error.code === "ENOENT") {
      // Small compiler fixtures and older source trees may not ship an index.
      // In that case preserve the compiler's historical file-tree discovery.
      return null;
    }
    throw new Error(
      `cannot read source index ${sourceIndexPath}: ${error.message}`,
      { cause: error },
    );
  }
  let index;
  try {
    index = JSON.parse(text);
  } catch (error) {
    throw new Error(
      `cannot parse source index ${sourceIndexPath}: ${error.message}`,
      { cause: error },
    );
  }
  if (!index || typeof index !== "object" || Array.isArray(index)) {
    throw new Error(
      `cannot parse source index ${sourceIndexPath}: index must be a JSON object`,
    );
  }
  const completionNames = index.completions;
  if (
    !Array.isArray(completionNames) ||
    completionNames.length === 0 ||
    completionNames.some(
      (item) => typeof item !== "string" || item.trim().length === 0,
    )
  ) {
    throw new Error(
      `cannot parse source index ${sourceIndexPath}: completions must be a non-empty array of strings`,
    );
  }
  for (const field of ["diffVersionedCompletions"]) {
    if (
      field in index &&
      (!Array.isArray(index[field]) ||
        index[field].some(
          (item) => typeof item !== "string" || item.trim().length === 0,
        ))
    ) {
      throw new Error(
        `cannot parse source index ${sourceIndexPath}: ${field} must be an array of strings`,
      );
    }
  }
  const list = (value) =>
    new Set(Array.isArray(value) ? value.map((item) => item.trim()) : []);
  return {
    completions: list(index.completions),
    diffVersionedCompletions: list(index.diffVersionedCompletions),
  };
}

function sourceCommandAllowed(sourceIndex, name) {
  if (!sourceIndex) return true;
  return (
    sourceIndex.completions.has(name) ||
    sourceIndex.diffVersionedCompletions.has(name)
  );
}

function sourceVersionedRoot(sourceIndex, directory) {
  return !sourceIndex || sourceIndex.diffVersionedCompletions.has(directory);
}

async function writeClosurePreservingHookModules({
  srcDir,
  stagedOutDir,
  compiledSpecs,
  enforceNamedAdapters = false,
}) {
  // The audit remains a pre-manifest completeness gate, but it is deliberately
  // not the source of the module mapping. The compiler already has the actual
  // function identity and owner object for every extraction; rebuilding that
  // relation from function text would reintroduce collisions between closures.
  const { auditSpecsHooks } = await import("./audit-spec-hooks.mjs");
  const audit = await auditSpecsHooks({
    sourceRoot: srcDir,
    irRoot: stagedOutDir,
    pair: "skip",
    // This is the compiler's intentional pre-manifest completeness pass.
    // Every audit of a published/staged output keeps the default strict gate.
    validateHookModules: false,
    // The typed sidecar is emitted after the closure manifest; this is the
    // paired, explicit opt-out for that one pre-manifest pass.
    validateTypedHooks: false,
  });
  if (audit.ok !== true) {
    const counts = Object.fromEntries(
      Object.entries(audit.errors ?? {}).map(([name, entries]) => [
        name,
        Array.isArray(entries) ? entries.length : 0,
      ]),
    );
    throw new Error(
      `cannot build closure-preserving hook modules from a failing source/IR audit: ${JSON.stringify(counts)}; sourceReadErrors=${JSON.stringify(audit.errors?.sourceReadErrors ?? [])}`,
    );
  }

  const manifestHooks = new Map();
  const manifestModules = new Map();
  const moduleSources = new Map();
  let moduleCount = 0;
  let hookCount = 0;

  const auditedIds = new Set((audit.hookManifest ?? []).map(({ id }) => id));
  const compilerBindings = compiledSpecs.flatMap((item) => [
    ...item.hooks.bindings.values(),
  ]);
  const compilerIds = new Set(compilerBindings.map(({ id }) => id));
  if (
    compilerIds.size !== auditedIds.size ||
    [...auditedIds].some((id) => !compilerIds.has(id))
  ) {
    throw new Error(
      `compiler hook bindings do not match the pre-manifest audit (${compilerIds.size}/${auditedIds.size})`,
    );
  }

  for (const item of compiledSpecs) {
    const record = {
      source: item.rel,
      sourceSha256: null,
    };
    assertSafeSourceRelativePath(record.source);
    const instances = [...item.hooks.bindings.values()].map((binding) => ({
      ...binding,
      sourceField: binding.field,
    }));
    if (instances.length === 0) continue;
    const sourcePath = join(srcDir, record.source);
    assertPathInsideRoot(srcDir, sourcePath, "source hook path");
    const sourceInfo = await lstat(sourcePath);
    if (sourceInfo.isSymbolicLink() || !sourceInfo.isFile()) {
      throw new Error(
        `source hook is a symlink or special entry: ${sourcePath}`,
      );
    }
    const source = await readFile(sourcePath, "utf8");
    record.sourceSha256 = sha256(source);
    const moduleFile = sourceModuleFileName(record.source);
    if (manifestModules.has(moduleFile)) {
      throw new Error(
        `closure-preserving module filename collision for ${record.source}`,
      );
    }
    const moduleSource = closurePreservingHookModule(
      source,
      record.source,
      instances,
    );
    const expression = moduleSource
      .trim()
      .replace(/^export\s+default\s+/, "")
      .replace(/;$/, "");
    try {
      new Script(`(${expression})`, { filename: moduleFile });
    } catch (error) {
      throw new Error(
        `closure-preserving module ${moduleFile} is not a standalone expression: ${error.message}`,
        { cause: error },
      );
    }
    const moduleSha256 = sha256(moduleSource);
    moduleSources.set(moduleFile, moduleSource);
    const hookIds = instances.map((instance) => instance.id).sort();
    manifestModules.set(moduleFile, {
      source: record.source,
      sourceSha256: record.sourceSha256,
      moduleSha256,
      hookIds,
    });
    for (const instance of instances) {
      if (manifestHooks.has(instance.id)) {
        throw new Error(`duplicate closure-preserving hook id ${instance.id}`);
      }
      manifestHooks.set(instance.id, {
        module: moduleFile,
        moduleSha256,
        path: instance.path,
        sourceField: instance.sourceField,
        functionBodySha256: instance.functionBodySha256,
      });
      hookCount += 1;
    }
    moduleCount += 1;
  }

  if (hookCount !== audit.hookManifest.length) {
    throw new Error(
      `closure-preserving manifest covers ${hookCount}/${audit.hookManifest.length} audited hooks`,
    );
  }
  const typedHooks = await writeTypedHookSidecar({
    stagedOutDir,
    compilerBindings,
    manifestHooks,
    moduleSources,
    enforceNamedAdapters,
  });
  return { modules: moduleCount, hooks: hookCount, typedHooks };
}

async function publishDirectory(stagedDir, outDir) {
  await assertNoSymlinkInPath(outDir);
  try {
    await assertManagedIrOutput(outDir, { allowLegacyCanonical: true });
  } catch (error) {
    if (error?.code !== "ENOENT") throw error;
  }
  await publishPairDirectories({
    irStage: stagedDir,
    irCanonical: outDir,
    lockPath: pairLockPath,
  });
}

async function pathExists(path) {
  try {
    await lstat(path);
    return true;
  } catch (error) {
    if (error?.code === "ENOENT") return false;
    throw error;
  }
}

async function snapshotDirectory(directory) {
  const rootInfo = await lstat(directory);
  if (rootInfo.isSymbolicLink() || !rootInfo.isDirectory()) {
    throw new Error(
      `compiled IR destination is not a regular directory: ${directory}`,
    );
  }
  const files = [];
  const directories = [];
  async function walk(current, relativePath = "") {
    const entries = await readdir(current, { withFileTypes: true });
    entries.sort((left, right) => comparePath(left.name, right.name));
    for (const entry of entries) {
      const full = join(current, entry.name);
      const path = relativePath ? `${relativePath}/${entry.name}` : entry.name;
      const info = await lstat(full);
      if (info.isDirectory()) {
        directories.push(path);
        await walk(full, path);
      } else if (info.isFile()) {
        files.push({ path, sha256: sha256(await readFile(full)) });
      } else {
        throw new Error(
          `compiled IR destination contains a link or special entry: ${full}`,
        );
      }
    }
  }
  await walk(directory);
  return { files, directories };
}

function snapshotsEqual(left, right) {
  return JSON.stringify(left) === JSON.stringify(right);
}

async function assertSnapshotUnchanged(directory, expected) {
  const actual = await snapshotDirectory(directory);
  if (!snapshotsEqual(actual, expected)) {
    throw new Error(
      `refusing to remove changed compiled IR backup: ${directory}`,
    );
  }
}

async function removeSnapshotDirectory(directory, snapshot) {
  await assertSnapshotUnchanged(directory, snapshot);
  for (const file of snapshot.files) {
    const full = join(directory, file.path);
    const info = await lstat(full);
    if (!info.isFile() || sha256(await readFile(full)) !== file.sha256) {
      throw new Error(
        `refusing to remove changed compiled IR backup file: ${full}`,
      );
    }
    await unlink(full);
  }
  for (const path of [...snapshot.directories].sort(
    (left, right) =>
      right.split("/").length - left.split("/").length ||
      comparePath(right, left),
  )) {
    await rmdir(join(directory, path));
  }
  await rmdir(directory);
}

async function assertManagedIrOutput(
  directory,
  { allowLegacyCanonical = false } = {},
) {
  const snapshot = await snapshotDirectory(directory);
  if (snapshot.files.length === 0 && snapshot.directories.length === 0) {
    return snapshot;
  }
  if (allowLegacyCanonical && resolve(directory) === resolve(canonicalIrDir)) {
    try {
      await lstat(join(directory, PAIR_MARKER_NAME));
    } catch (error) {
      if (error?.code === "ENOENT") {
        // This is the one migration exception: older checkouts have a
        // generated canonical IR tree without a marker. The compiler has
        // already produced and audited the replacement staged tree before
        // this function runs, so the known canonical location can be
        // replaced once. Caller-supplied custom output directories remain
        // marker-owned and fail closed.
        return snapshot;
      }
      throw error;
    }
    // T4.1: the previous compiler published hooks/, source-modules/, and
    // hook-modules.json. Replacing that exact canonical tree is the
    // migration; custom outputs still fail closed if leftover JS is present.
    for (const name of [HOOK_MODULE_MANIFEST, HOOK_MODULES_DIR, "hooks"]) {
      try {
        await lstat(join(directory, name));
        return snapshot;
      } catch (error) {
        if (error?.code !== "ENOENT") throw error;
      }
    }
  }
  try {
    await verifyPair({ irRoot: directory, irOnly: true });
  } catch (error) {
    throw new Error(
      `refusing to replace non-empty compiled IR destination without a valid ${PAIR_MARKER_NAME}: ${directory}`,
      { cause: error },
    );
  }
  return snapshot;
}

async function compileSpecsIrUnlocked({
  srcDir = join(repoDir, "bundle", "specs"),
  outDir = join(repoDir, "bundle", "specs-ir"),
  enforceNamedAdapters = false,
} = {}) {
  // The caller may supply an arbitrary output directory. Check every existing
  // parent (including a dangling final link) before mkdir/rename so a compile
  // can never publish through an alias.
  await assertNoSymlinkInPath(outDir);
  // The compiler API is also allowed to compile arbitrary source fixtures.
  // Validate the source root here (not only in the CLI wrapper) so callers
  // cannot make an API compile follow a source-tree symlink.
  await assertNoSymlinkInPath(srcDir);
  const files = await walkJs(srcDir);
  if (!files.length) {
    throw new Error(`source spec tree ${srcDir} contains no JavaScript specs`);
  }
  const sourceIndex = await readSourceIndex(srcDir);
  await mkdir(dirname(outDir), { recursive: true });
  const stagedOutDir = await mkdtemp(
    join(dirname(outDir), `.${basename(outDir)}.tmp-`),
  );
  let published = false;

  try {
    const nestedIndexDirs = new Set(
      files
        .filter((rel) => rel.endsWith("/index.js"))
        .map((rel) => dirname(rel)),
    );
    const compiledSpecs = [];
    let compiled = 0;
    let failed = 0;
    let skipped = 0;
    let hooksExtracted = 0;
    const failures = [];
    const appliedVersionDiffs = [];
    const versionedByCommand = new Map();
    const hookFilesByName = new Map();

    for (const rel of files) {
      assertSafeSourceRelativePath(rel);
      const normalizedRel = rel;
      if (KNOWN_NON_SPEC_FILES.has(normalizedRel)) {
        skipped += 1;
        if (
          (compiled + skipped + failed) % 100 === 0 ||
          compiled + skipped + failed === files.length
        ) {
          process.stdout.write(
            `Compiled ${compiled}/${files.length} specs (${skipped} allowlisted skipped)\n`,
          );
        }
        continue;
      }
      const src = join(srcDir, rel);
      assertPathInsideRoot(srcDir, src, "source spec path");
      try {
        const specId = normalizedRel.replace(/\.js$/, "");
        const hooks = createHookBag(specId);
        const sourceInfo = await lstat(src);
        if (sourceInfo.isSymbolicLink() || !sourceInfo.isFile()) {
          throw new Error(
            `source spec is a symlink or special entry: ${src}`,
          );
        }
        const source = await readFile(src, "utf8");
        const binder = createFilepathsBinder(source);
        const mod = await import(pathToFileURL(src).href);
        const rawSpec = mod.default ?? mod;
        const sourceFunctions = [];
        assertNoUnknownFunctionFields(
          rawSpec,
          specId,
          "root",
          new WeakSet(),
          sourceFunctions,
        );
        const spec = await convertNode(rawSpec, { hooks, source, binder });
        if (!spec) {
          throw new Error(
            `source did not produce a static spec (add the file to ${"KNOWN_NON_SPEC_FILES"} only when it is a reviewed helper/barrel)`,
          );
        }
        const destRel = normalizedRel.replace(/\.js$/, ".json");
        const versionVariants = await compileAppliedVersionDiffs({
          rawSpec,
          mod,
          specId,
          sourceRel: normalizedRel,
          destRel,
          hooks,
          source,
          binder,
          sourceFunctions,
        });
        assertFunctionsExtracted(sourceFunctions, hooks, specId);
        bindExtractedHooks(hooks, sourceFunctions, specId);
        await writeOutputFile(
          stagedOutDir,
          destRel,
          `${JSON.stringify(spec)}\n`,
        );
        for (const variant of versionVariants) {
          if (variant.spec) {
            await writeOutputFile(
              stagedOutDir,
              variant.destRel,
              `${JSON.stringify(variant.spec)}\n`,
            );
          }
        }
        if (versionVariants.length) {
          appliedVersionDiffs.push({
            file: normalizedRel,
            versions: versionVariants.map((variant) => ({
              version: variant.version,
              destRel: variant.destRel,
              functions: countFunctionsInValue(mod.versions?.[variant.version]),
            })),
          });
        }
        for (const [id] of hooks.files) {
          const filename = hookFileName(id);
          const previousId = hookFilesByName.get(filename);
          if (previousId && previousId !== id) {
            throw new Error(
              `hook filename collision: ${filename} represents both ${previousId} and ${id}`,
            );
          }
          hookFilesByName.set(filename, id);
          hooksExtracted += 1;
        }
        compiled += 1;
        compiledSpecs.push({
          rel: normalizedRel,
          destRel,
          spec,
          hooks,
          versionVariants,
        });
      } catch (err) {
        failed += 1;
        failures.push({ rel: normalizedRel, message: err.message });
      }
      if (
        (compiled + skipped + failed) % 100 === 0 ||
        compiled + skipped + failed === files.length
      ) {
        process.stdout.write(
          `Compiled ${compiled}/${files.length} specs (${skipped} allowlisted skipped, ${failed} failed)\n`,
        );
      }
    }

    if (failures.length) {
      const details = failures
        .sort((left, right) => comparePath(left.rel, right.rel))
        .map(({ rel, message }) => `${rel}: ${message}`)
        .join("\n");
      throw new Error(
        `spec compilation failed closed for ${failures.length} file(s):\n${details}`,
      );
    }

    if (!compiledSpecs.length) {
      throw new Error(
        `source spec tree ${srcDir} produced no compilable specs; ` +
          `review the explicit KNOWN_NON_SPEC_FILES allowlist`,
      );
    }

    appliedVersionDiffs.sort((left, right) =>
      comparePath(left.file, right.file),
    );

    const hookModules = await writeClosurePreservingHookModules({
      srcDir,
      stagedOutDir,
      compiledSpecs,
      enforceNamedAdapters,
    });

    const commandFiles = new Map();
    const candidateFor = (name, candidate) => {
      if (!name || !candidate) return;
      const current = commandFiles.get(name);
      if (!current || compareFileCandidates(candidate, current) > 0) {
        commandFiles.set(name, candidate);
      }
    };
    for (const item of compiledSpecs) {
      const rel = item.rel.replaceAll("\\", "/");
      const slash = rel.lastIndexOf("/");
      const directory = slash === -1 ? "" : rel.slice(0, slash);
      const basename = rel.slice(slash + 1);
      if (!directory) {
        // The file name is the canonical root command.  Spec-declared names are
        // aliases only; giving them a lower priority prevents a colliding alias
        // (for example `j.js` naming itself `autojump`) from shadowing the
        // command's own file.
        const canonical = rel.slice(0, -3);
        if (sourceCommandAllowed(sourceIndex, canonical)) {
          candidateFor(canonical, { destRel: item.destRel, priority: 6 });
        }
        for (const name of item.spec.names) {
          if (sourceCommandAllowed(sourceIndex, name)) {
            candidateFor(name, { destRel: item.destRel, priority: 5 });
          }
        }
        continue;
      }
      if (!nestedIndexDirs.has(directory)) continue;
      if (!sourceVersionedRoot(sourceIndex, directory)) continue;
      if (basename === "index.js") {
        // A statically exported nested index is authoritative.  Dynamic
        // version selectors are skipped above and therefore fall through to
        // the deterministic highest version candidate below.
        if (sourceCommandAllowed(sourceIndex, directory)) {
          candidateFor(directory, { destRel: item.destRel, priority: 4 });
        }
        for (const name of item.spec.names) {
          if (sourceCommandAllowed(sourceIndex, name)) {
            candidateFor(name, { destRel: item.destRel, priority: 3 });
          }
        }
        continue;
      }
      const version = parseVersionFilename(basename);
      if (!version) continue;
      if (sourceCommandAllowed(sourceIndex, directory)) {
        candidateFor(directory, {
          destRel: item.destRel,
          priority: 2,
          version,
        });
      }
      for (const name of item.spec.names) {
        if (sourceCommandAllowed(sourceIndex, name)) {
          candidateFor(name, { destRel: item.destRel, priority: 1, version });
        }
      }
    }

    await collectVersionedSelectors({
      srcDir,
      files,
      compiledSpecs,
      sourceIndex,
      versionedByCommand,
    });
    for (const [name, entry] of versionedByCommand) {
      const fallbackPath = resolveVersionedPath(entry, undefined);
      if (fallbackPath && sourceCommandAllowed(sourceIndex, name)) {
        candidateFor(name, { destRel: fallbackPath, priority: 2.5 });
      }
    }

    const unique = [...commandFiles.keys()].sort();
    const versioned = Object.fromEntries(
      [...versionedByCommand.entries()].sort(([left], [right]) =>
        left < right ? -1 : left > right ? 1 : 0,
      ),
    );
    await writeOutputFile(
      stagedOutDir,
      "index.json",
      `${JSON.stringify({
        completions: unique,
        // New readers use this map to resolve command aliases without exposing
        // nested implementation files (notably gcloud/*) as top-level commands.
        // Readers predating this field continue to use relative file names.
        files: Object.fromEntries(
          [...commandFiles.entries()]
            .sort(([left], [right]) =>
              left < right ? -1 : left > right ? 1 : 0,
            )
            .map(([name, candidate]) => [name, candidate.destRel]),
        ),
        ...(Object.keys(versioned).length ? { versioned } : {}),
      })}\n`,
    );
    const pairMarker = await createPairMarker({
      sourceRoot: srcDir,
      irRoot: stagedOutDir,
    });
    assertSafeSourceRelativePath(PAIR_MARKER_NAME);
    const pairMarkerPath = join(stagedOutDir, PAIR_MARKER_NAME);
    assertPathInsideRoot(
      stagedOutDir,
      pairMarkerPath,
      "compiled IR pair marker path",
    );
    await assertNoSymlinkInPath(pairMarkerPath);
    await writePairMarker(stagedOutDir, pairMarker);
    await verifyPair({ sourceRoot: srcDir, irRoot: stagedOutDir });
    const { auditSpecsHooks } = await import("./audit-spec-hooks.mjs");
    const finalAudit = await auditSpecsHooks({
      sourceRoot: srcDir,
      irRoot: stagedOutDir,
    });
    if (!finalAudit.ok) {
      const failures = Object.entries(finalAudit.errors)
        .filter(([, entries]) => entries.length)
        .map(([name, entries]) => {
          const samples = entries
            .slice(0, 3)
            .map((entry) => JSON.stringify(entry))
            .join("; ");
          return `${name}=${entries.length}${samples ? ` [${samples}]` : ""}`;
        });
      throw new Error(
        `Spec IR pre-publish audit failed: ${failures.join(", ")}`,
      );
    }
    await publishDirectory(stagedOutDir, outDir);
    published = true;
    process.stdout.write(
      `Wrote ${compiled} IR specs (${unique.length} names, ${hooksExtracted} extracted hooks in ${hookModules.modules} in-memory closure modules, ${hookModules.typedHooks} typed hooks; ${skipped} allowlisted skipped) to ${outDir}\n`,
    );
    if (appliedVersionDiffs.length || versionedByCommand.size) {
      process.stdout.write(
        `Adapted: ${appliedVersionDiffs.reduce((sum, entry) => sum + entry.versions.length, 0)} version diff(s) in ${appliedVersionDiffs.length} file(s); ${versionedByCommand.size} version selector(s)\n`,
      );
    }
    return {
      compiled,
      failed,
      skipped,
      allowlistedSkipped: skipped,
      names: unique.length,
      hooks: hooksExtracted,
      hookModules: hookModules.modules,
      typedHooks: hookModules.typedHooks,
      appliedVersionDiffs,
      versionedCommands: [...versionedByCommand.keys()],
    };
  } finally {
    if (!published && !(await pairJournalExists(pairLockPath))) {
      await rm(stagedOutDir, { recursive: true, force: true });
    }
  }
}

export async function compileSpecsIr({
  srcDir = join(repoDir, "bundle", "specs"),
  outDir = join(repoDir, "bundle", "specs-ir"),
  enforceNamedAdapters = false,
} = {}) {
  return withPairLock(pairLockPath, () =>
    compileSpecsIrUnlocked({ srcDir, outDir, enforceNamedAdapters }),
  );
}

function parseVersionFilename(filename) {
  const stem = filename.replace(/\.js$/, "");
  const match = stem.match(
    /^v?(\d+)(?:\.(\d+))?(?:\.(\d+))?(?:-([0-9A-Za-z.-]+))?$/,
  );
  if (!match) return null;
  return {
    numbers: [match[1], match[2] ?? "0", match[3] ?? "0"].map(Number),
    prerelease: match[4] ?? "",
    raw: stem,
  };
}

function compareVersions(left, right) {
  for (let index = 0; index < left.numbers.length; index += 1) {
    if (left.numbers[index] !== right.numbers[index]) {
      return left.numbers[index] > right.numbers[index] ? 1 : -1;
    }
  }
  if (!left.prerelease && right.prerelease) return 1;
  if (left.prerelease && !right.prerelease) return -1;
  if (left.prerelease && right.prerelease) {
    const leftParts = left.prerelease.split(".");
    const rightParts = right.prerelease.split(".");
    for (
      let index = 0;
      index < Math.max(leftParts.length, rightParts.length);
      index += 1
    ) {
      if (index >= leftParts.length) return -1;
      if (index >= rightParts.length) return 1;
      const leftPart = leftParts[index];
      const rightPart = rightParts[index];
      if (leftPart === rightPart) continue;
      const leftNumeric = /^\d+$/.test(leftPart);
      const rightNumeric = /^\d+$/.test(rightPart);
      if (leftNumeric && rightNumeric) {
        return Number(leftPart) > Number(rightPart) ? 1 : -1;
      }
      if (leftNumeric !== rightNumeric) return leftNumeric ? -1 : 1;
      return leftPart < rightPart ? -1 : 1;
    }
  }
  return left.raw < right.raw ? -1 : left.raw > right.raw ? 1 : 0;
}

function compareFileCandidates(left, right) {
  if (left.priority !== right.priority)
    return left.priority > right.priority ? 1 : -1;
  if (left.version && right.version)
    return compareVersions(left.version, right.version);
  return comparePath(left.destRel, right.destRel);
}

async function compileAppliedVersionDiffs({
  rawSpec,
  mod,
  specId,
  sourceRel,
  destRel,
  hooks,
  source,
  binder,
  sourceFunctions,
}) {
  const keys = versionDiffKeys(mod.versions);
  if (!keys.length) return [];
  hooks.reuseByIdentity = true;
  const variants = [];
  let current = rawSpec;
  let previousDestRel = destRel;
  for (const version of keys) {
    const diff = mod.versions[version];
    current = applySpecDiff(current, diff);
    if (isEmptyVersionDiff(diff)) {
      variants.push({ version, destRel: previousDestRel, spec: null });
      continue;
    }
    const mergedFunctions = [];
    assertNoUnknownFunctionFields(
      current,
      specId,
      "root",
      new WeakSet(),
      mergedFunctions,
    );
    for (const record of mergedFunctions) {
      record.mergeThrough = version;
      sourceFunctions.push(record);
    }
    const spec = await convertNode(current, { hooks, source, binder });
    if (!spec) {
      throw new Error(
        `${sourceRel} versions[${JSON.stringify(version)}] did not produce a static spec after applySpecDiff`,
      );
    }
    const variantDestRel = derivedVersionIrRel(sourceRel, version);
    variants.push({ version, destRel: variantDestRel, spec });
    previousDestRel = variantDestRel;
  }
  return variants;
}

async function collectVersionedSelectors({
  srcDir,
  files,
  compiledSpecs,
  sourceIndex,
  versionedByCommand,
}) {
  for (const rel of files) {
    const normalizedRel = rel.replaceAll("\\", "/");
    if (!normalizedRel.endsWith("/index.js")) continue;
    if (!KNOWN_NON_SPEC_FILES.has(normalizedRel)) continue;
    const directory = dirname(normalizedRel);
    if (!sourceVersionedRoot(sourceIndex, directory)) continue;
    const src = join(srcDir, normalizedRel);
    const mod = await import(pathToFileURL(src).href);
    if (typeof mod.getVersionCommand !== "function") continue;
    const versionFiles = compiledSpecs.filter((item) => {
      const itemRel = item.rel.replaceAll("\\", "/");
      if (dirname(itemRel) !== directory) return false;
      return parseVersionFilename(itemRel.slice(itemRel.lastIndexOf("/") + 1));
    });
    if (!versionFiles.length) {
      throw new Error(
        `${normalizedRel} exports getVersionCommand but ${directory} has no compiled version files`,
      );
    }
    const filesMap = {};
    const applied = {};
    for (const item of versionFiles.sort((left, right) =>
      compareSemver(
        parseVersionFilename(left.rel.slice(left.rel.lastIndexOf("/") + 1)).raw,
        parseVersionFilename(right.rel.slice(right.rel.lastIndexOf("/") + 1)).raw,
      ),
    )) {
      const version = parseVersionFilename(
        item.rel.slice(item.rel.lastIndexOf("/") + 1),
      ).raw;
      filesMap[version] = item.destRel;
      if (item.versionVariants?.length) {
        applied[version] = Object.fromEntries(
          item.versionVariants.map((variant) => [variant.version, variant.destRel]),
        );
      }
    }
    const selector = compileGetVersionCommand(mod.getVersionCommand, directory);
    const fileVersions = Object.keys(filesMap).sort(compareSemver);
    versionedByCommand.set(directory, {
      ...selector,
      fallback: fileVersions[fileVersions.length - 1],
      files: filesMap,
      ...(Object.keys(applied).length ? { applied } : {}),
    });
  }
}

async function canonicalDestination(path) {
  let existing = resolve(path);
  const missing = [];
  while (true) {
    try {
      return join(await realpath(existing), ...missing);
    } catch (error) {
      if (error?.code !== "ENOENT") throw error;
      const parent = dirname(existing);
      if (parent === existing) throw error;
      missing.unshift(basename(existing));
      existing = parent;
    }
  }
}

function isSelfOrDescendant(parent, child) {
  const path = relative(parent, child);
  return (
    path === "" ||
    (!isAbsolute(path) && path !== ".." && !path.startsWith(`..${sep}`))
  );
}

/**
 * The standalone compiler is also a publisher for the canonical pair. Keep
 * its two environment overrides atomic in the same way as the sync script:
 * either both paths identify the canonical source/IR pair, or both are
 * distinct custom paths. This guard belongs at the CLI boundary so callers of
 * compileSpecsIr() can continue using arbitrary temporary fixture paths.
 */
async function assertCliDestinationPair(srcDir, outDir) {
  // Validate both paths before resolving either destination. In particular,
  // a dangling final link must be rejected instead of being mistaken for a
  // missing custom output that can be created by the compiler.
  await assertNoSymlinkInPath(srcDir);
  await assertNoSymlinkInPath(outDir);

  const normalizedSourceDir = await canonicalDestination(srcDir);
  const normalizedIrDir = await canonicalDestination(outDir);
  const normalizedCanonicalSourceDir = await canonicalDestination(
    canonicalSourceDir,
  );
  const normalizedCanonicalIrDir = await canonicalDestination(canonicalIrDir);

  if (
    isSelfOrDescendant(normalizedSourceDir, normalizedIrDir) ||
    isSelfOrDescendant(normalizedIrDir, normalizedSourceDir)
  ) {
    throw new Error("bundled specs and compiled IR destinations cannot overlap");
  }

  const sourceIsCanonical =
    normalizedSourceDir === normalizedCanonicalSourceDir;
  const irIsCanonical = normalizedIrDir === normalizedCanonicalIrDir;
  if (sourceIsCanonical !== irIsCanonical) {
    throw new Error(
      "custom outputs require both destinations to be canonical or distinct custom paths",
    );
  }
}

const isMain =
  process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href;

export async function typedHookCompileReport({
  irRoot = join(repoDir, "bundle", "specs-ir"),
} = {}) {
  const sidecar = JSON.parse(
    await readFile(join(irRoot, TYPED_HOOK_SIDECAR), "utf8"),
  );
  const fields = Object.keys(TYPED_HOOK_CONTRACTS);
  const groups = new Map();
  const addGroup = (id, entry, ok, code = null) => {
    if (!fields.includes(entry.sourceField)) return;
    const key = `${entry.sourceField}\0${entry.functionBodySha256}`;
    if (groups.has(key)) {
      groups.get(key).ids.push(id);
      return;
    }
    groups.set(key, {
      id,
      field: entry.sourceField,
      sha: entry.functionBodySha256,
      ids: [id],
      ok,
      code,
    });
  };
  for (const [id, entry] of Object.entries(sidecar.hooks ?? {})) {
    addGroup(id, entry, true);
  }
  for (const [id, entry] of Object.entries(sidecar.adapters ?? {})) {
    addGroup(id, entry, false, "adapter");
  }
  const byField = Object.fromEntries(
    fields.map((field) => [field, { total: 0, ok: 0, codes: {} }]),
  );
  for (const group of groups.values()) {
    const bucket = byField[group.field];
    bucket.total += 1;
    if (group.ok) {
      bucket.ok += 1;
      continue;
    }
    const key = group.code ?? "leftover";
    bucket.codes[key] = (bucket.codes[key] ?? 0) + 1;
  }
  return { byField, uniqueBodies: groups.size };
}

function printTypedHookReport(report) {
  const rows = [];
  for (const [field, bucket] of Object.entries(report.byField)) {
    const codes = Object.entries(bucket.codes).sort((left, right) => right[1] - left[1]);
    process.stdout.write(
      `${field}: ${bucket.ok}/${bucket.total} compiled\n`,
    );
    for (const [code, count] of codes.slice(0, 20)) {
      process.stdout.write(`  ${count}\t${code}\n`);
      rows.push({ field, code, count });
    }
  }
  const top = [...rows].sort((left, right) => right.count - left.count).slice(0, 20);
  process.stdout.write("Top 20 TypedHookCompileError.code:\n");
  for (const row of top) {
    process.stdout.write(`  ${row.count}\t${row.field}\t${row.code}\n`);
  }
}

if (isMain) {
  const typedReport = process.argv.includes("--typed-report");
  const srcDir = process.env.EC_SPECS_SRC || canonicalSourceDir;
  const outDir = process.env.EC_SPECS_IR || join(repoDir, "bundle", "specs-ir");
  await assertCliDestinationPair(srcDir, outDir);
  await compileSpecsIr({
    srcDir,
    outDir,
    enforceNamedAdapters: !allowUnadaptedHooks(),
  });
  if (typedReport) {
    printTypedHookReport(await typedHookCompileReport({ irRoot: outDir }));
  }
}
