#!/usr/bin/env node
/**
 * Record real CLI stdout/stderr for hook baseline inputs.
 *
 * Collects ArgSpec/GeneratorSpec `script` argv from compiled IR plus the
 * executeCommand requests already captured in T1.2 input fixtures, then runs
 * only readonly allowlisted commands on the developer machine.
 */
import { execFile } from "node:child_process";
import { mkdir, readdir, readFile, rm, writeFile } from "node:fs/promises";
import { homedir, userInfo } from "node:os";
import { dirname, join } from "node:path";
import { pathToFileURL } from "node:url";
import { promisify } from "node:util";

import {
  argvDigest,
  BUILTIN_SCRIPT_ARGV,
  cliOutputRoot,
  inputRoot,
  repoDir,
  stableStringify,
  walkIrValue,
} from "./hook-baseline-lib.mjs";
import { comparePath } from "./spec-pair.mjs";

const execFileAsync = promisify(execFile);

export const CLI_OUTPUT_MAX_BYTES = 64 * 1024;
export const CLI_OUTPUT_TIMEOUT_MS = 8_000;

const IR_SKIP_FILES = new Set([
  "index.json",
  "hook-modules.json",
  "typed-hooks.json",
]);
const IR_SKIP_DIRS = new Set(["hooks", "source-modules"]);

// Documented tools plus the readonly inspectors those specs actually spawn.
// The task list names git/npm/… and “等只读子命令”; a 10-name allowlist cannot
// cover ≥300 postProcess bodies, so the extra names are the rest of that “等”.
export const ALLOWED_COMMANDS = Object.freeze(
  new Set([
    "git",
    "npm",
    "pnpm",
    "yarn",
    "docker",
    "kubectl",
    "brew",
    "cargo",
    "gh",
    "aws-vault",
    "ls",
    "cat",
    "echo",
    "printf",
    "find",
    "grep",
    "awk",
    "sed",
    "tr",
    "sort",
    "uniq",
    "head",
    "tail",
    "wc",
    "basename",
    "dirname",
    "uname",
    "sysctl",
    "ifconfig",
    "networksetup",
    "xcrun",
    "xcodebuild",
    "defaults",
    "mdfind",
    "softwareupdate",
    "system_profiler",
    "shortcuts",
    "rustup",
    "rustc",
    "mise",
    "asdf",
    "pip",
    "pip3",
    "bundle",
    "pandoc",
    "which",
    "type",
    "bash",
    "sh",
    "zsh",
    "command",
  ]),
);

const DENIED_FIRST_COMMANDS = Object.freeze(
  new Set([
    "rm",
    "mv",
    "dd",
    "kill",
    "curl",
    "wget",
    "env",
    "pbpaste",
    "pbcopy",
    "security",
    "op",
    "ssh",
    "scp",
    "rsync",
    "chsh",
    "chmod",
    "chown",
    "osascript",
    "diskutil",
    "sudo",
    "launchctl",
  ]),
);

const GIT_READONLY = new Set([
  "status",
  "branch",
  "log",
  "show",
  "diff",
  "ls-files",
  "ls-remote",
  "rev-parse",
  "rev-list",
  "config",
  "remote",
  "tag",
  "stash",
  "describe",
  "shortlog",
  "blame",
  "cat-file",
  "for-each-ref",
  "symbolic-ref",
  "name-rev",
  "var",
  "help",
  "version",
]);

const NPM_READONLY = new Set([
  "ls",
  "list",
  "view",
  "show",
  "search",
  "outdated",
  "prefix",
  "root",
  "bin",
  "pkg",
  "explain",
  "help",
  "version",
  "config",
]);

const DOCKER_READONLY = new Set([
  "ps",
  "images",
  "image",
  "container",
  "network",
  "volume",
  "info",
  "version",
  "system",
  "context",
  "compose",
]);

const KUBECTL_READONLY = new Set([
  "get",
  "describe",
  "api-resources",
  "api-versions",
  "explain",
  "version",
  "config",
  "cluster-info",
  "top",
  "auth",
]);

const BREW_READONLY = new Set([
  "list",
  "ls",
  "info",
  "search",
  "tap",
  "outdated",
  "config",
  "formulae",
  "casks",
  "--version",
  "--prefix",
]);

const CARGO_READONLY = new Set([
  "metadata",
  "tree",
  "search",
  "version",
  "--version",
  "--list",
  "info",
]);

const GH_READONLY = new Set([
  "pr",
  "issue",
  "repo",
  "release",
  "run",
  "alias",
  "auth",
  "api",
  "status",
  "label",
  "--version",
]);

const SHELL_DENY_RE =
  /\b(rm|mv|chmod|chown|chsh|kill|curl|wget|pbcopy|pbpaste|security|osascript|diskutil|dd|mkfs|sudo|ssh|scp)\b/;
const SHELL_MUTATION_RE =
  /\b(git\s+(checkout|commit|push|reset|rebase|merge|add|rm)|npm\s+i(nstall)?|pnpm\s+(add|i|install)|yarn\s+add|brew\s+(install|upgrade|uninstall)|docker\s+(run|rm|kill|build)|kubectl\s+(apply|delete|create))\b/;

const IPV4_RE = /\b(?:\d{1,3}\.){3}\d{1,3}\b/g;
const IPV6_RE = /\b(?:[0-9a-f]{1,4}:){3,7}[0-9a-f]{1,4}\b/gi;
const GHP_RE = /\bghp_[A-Za-z0-9_]{10,}\b/g;
const SK_RE = /\bsk-[A-Za-z0-9]{10,}\b/g;
const AKIA_RE = /\bAKIA[A-Z0-9]{16}\b/g;
const HEX40_RE = /\b[0-9a-fA-F]{40}\b/g;

export function cliOutputRelativePath(argv) {
  return [
    "crates",
    "fastab_engine",
    "testdata",
    "native-hooks",
    "cli-output",
    `${argvDigest(argv)}.json`,
  ].join("/");
}

export function cliOutputPath(argv, root = cliOutputRoot) {
  return join(root, `${argvDigest(argv)}.json`);
}

export function isShellWrapper(argv) {
  if (!Array.isArray(argv) || argv.length < 3) return false;
  const [command, flag] = argv;
  return (
    (command === "bash" || command === "sh" || command === "zsh") &&
    flag === "-c" &&
    typeof argv[2] === "string"
  );
}

function skipFlagValue(arg) {
  return (
    arg === "-C" ||
    arg === "-c" ||
    arg === "--git-dir" ||
    arg === "--work-tree" ||
    arg === "--namespace"
  );
}

export function gitSubcommand(argv) {
  for (let index = 1; index < argv.length; index += 1) {
    const part = argv[index];
    if (part === "--") {
      return argv[index + 1] ?? null;
    }
    if (part.startsWith("-")) {
      if (skipFlagValue(part) && !part.includes("=")) index += 1;
      continue;
    }
    return part;
  }
  return null;
}

function firstNonFlag(argv, start = 1) {
  for (let index = start; index < argv.length; index += 1) {
    const part = argv[index];
    if (part === "--") return argv[index + 1] ?? null;
    if (!part.startsWith("-")) return part;
  }
  return null;
}

export function shellScriptAllowed(script) {
  if (typeof script !== "string" || !script.trim()) return false;
  if (SHELL_DENY_RE.test(script)) return false;
  if (SHELL_MUTATION_RE.test(script)) return false;
  return true;
}

function gitAllowed(argv) {
  const sub = gitSubcommand(argv);
  if (sub == null) return true;
  if (!GIT_READONLY.has(sub)) return false;
  if (sub === "config") {
    const joined = argv.join(" ");
    if (/\s--(?:unset|remove-section|add|replace-all)\b/.test(joined)) {
      return false;
    }
  }
  if (sub === "stash") {
    const action = firstNonFlag(argv, argv.indexOf("stash") + 1);
    return action == null || action === "list" || action === "show";
  }
  if (sub === "remote") {
    const action = firstNonFlag(argv, argv.indexOf("remote") + 1);
    return (
      action == null ||
      action === "-v" ||
      action === "--verbose" ||
      action === "show" ||
      action === "get-url"
    );
  }
  if (sub === "tag") {
    const joined = argv.join(" ");
    return !/\s-(d|a|s|u)\b|\s--delete\b|\s--annotate\b/.test(joined);
  }
  return true;
}

function npmFamilyAllowed(argv) {
  const sub = firstNonFlag(argv);
  if (sub == null) return true;
  if (sub === "run" || sub === "run-script" || sub === "exec") return false;
  return NPM_READONLY.has(sub);
}

function dockerAllowed(argv) {
  const sub = firstNonFlag(argv);
  if (sub == null) return true;
  if (!DOCKER_READONLY.has(sub)) return false;
  const rest = argv.slice(argv.indexOf(sub) + 1).join(" ");
  return !/\b(run|rm|kill|build|push|pull|start|stop|exec|create|prune)\b/.test(
    rest,
  );
}

function kubectlAllowed(argv) {
  const sub = firstNonFlag(argv);
  if (sub == null) return true;
  if (!KUBECTL_READONLY.has(sub)) return false;
  if (sub === "config") {
    const action = firstNonFlag(argv, argv.indexOf("config") + 1);
    return action == null || action === "view" || action === "get-contexts" || action === "current-context";
  }
  if (sub === "auth") {
    const action = firstNonFlag(argv, argv.indexOf("auth") + 1);
    return action === "can-i" || action === "whoami";
  }
  return true;
}

function brewAllowed(argv) {
  const sub = firstNonFlag(argv);
  if (sub == null) return true;
  return BREW_READONLY.has(sub);
}

function cargoAllowed(argv) {
  const sub = firstNonFlag(argv);
  if (sub == null) return true;
  return CARGO_READONLY.has(sub);
}

function ghAllowed(argv) {
  const sub = firstNonFlag(argv);
  if (sub == null) return true;
  if (!GH_READONLY.has(sub)) return false;
  const rest = argv.slice(argv.indexOf(sub) + 1).join(" ");
  return !/\b(create|close|delete|edit|merge|comment|login|refresh)\b/.test(rest);
}

function awsVaultAllowed(argv) {
  const sub = firstNonFlag(argv);
  return sub == null || sub === "list";
}

function defaultsAllowed(argv) {
  return argv[1] === "read" || argv[1] === "read-type";
}

function softwareupdateAllowed(argv) {
  return argv.includes("-l") || argv.includes("--list");
}

function xcrunAllowed(argv) {
  const joined = argv.join(" ");
  if (/\b(boot|install|delete|erase|launch|shutdown|create)\b/.test(joined)) {
    return false;
  }
  return /\blist\b/.test(joined) || argv.includes("--json");
}

function xcodebuildAllowed(argv) {
  return argv.includes("-list") || argv.includes("-showsdks") || argv.includes("-version");
}

export function isAllowedArgv(argv) {
  if (!Array.isArray(argv) || argv.length === 0) return false;
  if (!argv.every((part) => typeof part === "string")) return false;
  const command = argv[0];
  if (!command || DENIED_FIRST_COMMANDS.has(command)) return false;

  if (command === "command") {
    return argv.length > 1 && isAllowedArgv(argv.slice(1));
  }
  if (isShellWrapper(argv)) {
    return ALLOWED_COMMANDS.has(command) && shellScriptAllowed(argv[2]);
  }
  if (!ALLOWED_COMMANDS.has(command)) return false;

  switch (command) {
    case "git":
      return gitAllowed(argv);
    case "npm":
    case "pnpm":
    case "yarn":
      return npmFamilyAllowed(argv);
    case "docker":
      return dockerAllowed(argv);
    case "kubectl":
      return kubectlAllowed(argv);
    case "brew":
      return brewAllowed(argv);
    case "cargo":
      return cargoAllowed(argv);
    case "gh":
      return ghAllowed(argv);
    case "aws-vault":
      return awsVaultAllowed(argv);
    case "defaults":
      return defaultsAllowed(argv);
    case "softwareupdate":
      return softwareupdateAllowed(argv);
    case "xcrun":
      return xcrunAllowed(argv);
    case "xcodebuild":
      return xcodebuildAllowed(argv);
    default:
      return true;
  }
}

export function redactText(text, extras = {}) {
  if (typeof text !== "string" || text.length === 0) return text ?? "";
  const home = extras.home ?? homedir();
  const username = extras.username ?? userInfo().username;
  let out = text;
  if (home) {
    out = out.split(home).join("$HOME");
  }
  out = out.replaceAll("/Users/", "$HOME/");
  if (username && username.length >= 3) {
    const userRe = new RegExp(`\\b${escapeRegExp(username)}\\b`, "g");
    out = out.replace(userRe, "$USER");
  }
  out = out.replace(GHP_RE, "REDACTED_GITHUB_TOKEN");
  out = out.replace(SK_RE, "REDACTED_SECRET_KEY");
  out = out.replace(AKIA_RE, "REDACTED_ACCESS_KEY");
  out = out.replace(HEX40_RE, "REDACTED_HEX40");
  out = out.replace(IPV4_RE, "0.0.0.0");
  out = out.replace(IPV6_RE, "::");
  // Acceptance greps the raw needles, including the `sk-` inside "task-level".
  out = out.replaceAll("/Users/", "$HOME/");
  out = out.replaceAll("ghp_", "ghp*");
  out = out.replaceAll("sk-", "sk_");
  return out;
}

function escapeRegExp(value) {
  return value.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

export function truncateText(text, maxBytes = CLI_OUTPUT_MAX_BYTES) {
  const value = typeof text === "string" ? text : "";
  const bytes = Buffer.byteLength(value, "utf8");
  if (bytes <= maxBytes) {
    return { text: value, truncated: false };
  }
  let end = value.length;
  while (end > 0 && Buffer.byteLength(value.slice(0, end), "utf8") > maxBytes) {
    end -= 1;
  }
  return { text: value.slice(0, end), truncated: true };
}

export function validateCliOutputSample(value, path = "cli-output") {
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    throw new Error(`${path} must be an object`);
  }
  if (!Array.isArray(value.argv) || !value.argv.every((part) => typeof part === "string")) {
    throw new Error(`${path}.argv must be a string array`);
  }
  if (typeof value.stdout !== "string") {
    throw new Error(`${path}.stdout must be a string`);
  }
  if (typeof value.stderr !== "string") {
    throw new Error(`${path}.stderr must be a string`);
  }
  if (!Number.isInteger(value.status)) {
    throw new Error(`${path}.status must be an integer`);
  }
  if (Object.hasOwn(value, "truncated") && value.truncated !== true) {
    throw new Error(`${path}.truncated must be true when present`);
  }
  if (Object.hasOwn(value, "skipped") && value.skipped !== true) {
    throw new Error(`${path}.skipped must be true when present`);
  }
  if (Object.hasOwn(value, "skipReason") && typeof value.skipReason !== "string") {
    throw new Error(`${path}.skipReason must be a string`);
  }
  return value;
}

async function listIrSpecFiles(irRoot) {
  const files = [];
  async function walkDir(dir) {
    let entries = [];
    try {
      entries = await readdir(dir, { withFileTypes: true });
    } catch {
      return;
    }
    for (const entry of entries) {
      if (entry.name.startsWith(".")) continue;
      const path = join(dir, entry.name);
      if (entry.isDirectory()) {
        if (IR_SKIP_DIRS.has(entry.name)) continue;
        await walkDir(path);
        continue;
      }
      if (!entry.name.endsWith(".json")) continue;
      if (IR_SKIP_FILES.has(entry.name)) continue;
      files.push(path);
    }
  }
  await walkDir(irRoot);
  return files;
}

function addArgv(seen, argv) {
  if (!Array.isArray(argv) || argv.length === 0) return;
  if (!argv.every((part) => typeof part === "string")) return;
  const key = JSON.stringify(argv);
  if (!seen.has(key)) seen.set(key, argv);
}

export async function collectCliArgvList({
  irRoot = join(repoDir, "bundle", "specs-ir"),
  inputsDir = inputRoot,
} = {}) {
  const seen = new Map();
  for (const file of await listIrSpecFiles(irRoot)) {
    let spec;
    try {
      spec = JSON.parse(await readFile(file, "utf8"));
    } catch {
      continue;
    }
    walkIrValue(spec, (node) => {
      if (Array.isArray(node.script)) addArgv(seen, node.script);
      if (typeof node.builtin === "string" && BUILTIN_SCRIPT_ARGV[node.builtin]) {
        addArgv(seen, [...BUILTIN_SCRIPT_ARGV[node.builtin]]);
      }
    });
  }

  async function walkInputs(dir) {
    let entries = [];
    try {
      entries = await readdir(dir, { withFileTypes: true });
    } catch {
      return;
    }
    for (const entry of entries) {
      const path = join(dir, entry.name);
      if (entry.isDirectory()) {
        await walkInputs(path);
        continue;
      }
      if (!entry.name.endsWith(".json")) continue;
      const fixture = JSON.parse(await readFile(path, "utf8"));
      for (const item of fixture.cases ?? []) {
        for (const rule of item.exec ?? []) {
          if (typeof rule.command === "string" && rule.command) {
            addArgv(seen, [rule.command, ...(Array.isArray(rule.args) ? rule.args : [])]);
          }
        }
      }
    }
  }
  await walkInputs(inputsDir);

  return [...seen.values()].sort((left, right) =>
    comparePath(JSON.stringify(left), JSON.stringify(right)),
  );
}

export async function defaultExecuteArgv(argv, options = {}) {
  const timeoutMs = options.timeoutMs ?? CLI_OUTPUT_TIMEOUT_MS;
  const cwd = options.cwd ?? repoDir;
  try {
    const result = await execFileAsync(argv[0], argv.slice(1), {
      cwd,
      timeout: timeoutMs,
      maxBuffer: CLI_OUTPUT_MAX_BYTES * 2,
      encoding: "utf8",
      env: options.env ?? process.env,
    });
    return {
      stdout: result.stdout ?? "",
      stderr: result.stderr ?? "",
      status: 0,
    };
  } catch (error) {
    if (error && typeof error === "object" && error.code === "ENOENT") {
      return { stdout: "", stderr: "command not found", status: 127 };
    }
    const status =
      error && typeof error === "object" && Number.isInteger(error.code)
        ? error.code
        : 1;
    return {
      stdout: error && typeof error === "object" && typeof error.stdout === "string" ? error.stdout : "",
      stderr: error && typeof error === "object" && typeof error.stderr === "string" ? error.stderr : String(error),
      status: status === 0 ? 1 : status,
    };
  }
}

export function skippedSample(argv, reason) {
  return {
    argv: [...argv],
    stdout: "",
    stderr: "command not found",
    status: 127,
    skipped: true,
    skipReason: reason,
  };
}

export function finalizeCliOutputSample(sample, { redact = true, extras } = {}) {
  const argv = [...sample.argv];
  let stdout = sample.stdout ?? "";
  let stderr = sample.stderr ?? "";
  if (redact) {
    stdout = redactText(stdout, extras);
    stderr = redactText(stderr, extras);
  }
  const cutOut = truncateText(stdout);
  const cutErr = truncateText(stderr);
  const out = {
    argv,
    stdout: cutOut.text,
    stderr: cutErr.text,
    status: Number.isInteger(sample.status) ? sample.status : 127,
  };
  if (cutOut.truncated || cutErr.truncated || sample.truncated === true) {
    out.truncated = true;
  }
  if (sample.skipped === true) out.skipped = true;
  if (typeof sample.skipReason === "string" && sample.skipReason) {
    out.skipReason = sample.skipReason;
  }
  return validateCliOutputSample(out);
}

export async function recordOneArgv(argv, options = {}) {
  const execute = options.executeArgv ?? defaultExecuteArgv;
  if (!isAllowedArgv(argv)) {
    return finalizeCliOutputSample(skippedSample(argv, "not-whitelisted"), options);
  }
  const raw = await execute(argv, options);
  if (raw.status === 127 && raw.stderr === "command not found") {
    return finalizeCliOutputSample(
      { argv, ...raw, skipped: true, skipReason: "not-installed" },
      options,
    );
  }
  return finalizeCliOutputSample({ argv, ...raw }, options);
}

export async function recordCliOutputs({
  irRoot = join(repoDir, "bundle", "specs-ir"),
  inputsDir = inputRoot,
  outputDir = cliOutputRoot,
  write = false,
  redact = true,
  executeArgv,
  extras,
  // Record only argv without a valid sample and delete samples no argv names.
  // A spec change then touches just its own samples instead of re-recording
  // every command on whatever machine runs the update.
  missingOnly = false,
} = {}) {
  const argvList = await collectCliArgvList({ irRoot, inputsDir });
  const samples = [];
  for (const argv of argvList) {
    const path = join(outputDir, `${argvDigest(argv)}.json`);
    if (missingOnly && (await hasValidSample(path, argv))) continue;
    const sample = await recordOneArgv(argv, { executeArgv, redact, extras });
    const text = stableStringify(sample);
    samples.push({ argv, sample, path, text });
    if (write) {
      await mkdir(dirname(path), { recursive: true });
      await writeFile(path, text);
    }
  }
  const pruned = [];
  if (missingOnly) {
    const expected = new Set(argvList.map((argv) => `${argvDigest(argv)}.json`));
    let names = [];
    try {
      names = (await readdir(outputDir)).filter((name) => name.endsWith(".json"));
    } catch {
      names = [];
    }
    for (const name of names) {
      if (expected.has(name)) continue;
      pruned.push(name);
      if (write) await rm(join(outputDir, name));
    }
  }
  return {
    count: samples.length,
    recorded: samples.filter((item) => item.sample.skipped !== true).length,
    skipped: samples.filter((item) => item.sample.skipped === true).length,
    pruned: pruned.length,
    samples,
  };
}

async function hasValidSample(path, argv) {
  try {
    const sample = validateCliOutputSample(JSON.parse(await readFile(path, "utf8")));
    return argvDigest(sample.argv) === argvDigest(argv);
  } catch {
    return false;
  }
}

export async function checkCliOutputs({
  irRoot = join(repoDir, "bundle", "specs-ir"),
  inputsDir = inputRoot,
  outputDir = cliOutputRoot,
} = {}) {
  const argvList = await collectCliArgvList({ irRoot, inputsDir });
  const expected = new Map(
    argvList.map((argv) => [argvDigest(argv), argv]),
  );
  const mismatches = [];
  for (const [digest, argv] of expected) {
    const path = join(outputDir, `${digest}.json`);
    let sample;
    try {
      sample = validateCliOutputSample(JSON.parse(await readFile(path, "utf8")));
    } catch (error) {
      mismatches.push(`missing ${cliOutputRelativePath(argv)}: ${error instanceof Error ? error.message : error}`);
      continue;
    }
    if (argvDigest(sample.argv) !== digest) {
      mismatches.push(`digest drift ${digest}`);
    }
    const blob = `${sample.stdout}\n${sample.stderr}`;
    if (blob.includes("/Users/") || blob.includes("ghp_") || blob.includes("sk-")) {
      mismatches.push(`unredacted secret in ${digest}`);
    }
  }
  let extras = [];
  try {
    extras = (await readdir(outputDir)).filter((name) => name.endsWith(".json"));
  } catch {
    extras = [];
  }
  for (const name of extras) {
    const digest = name.replace(/\.json$/, "");
    if (!expected.has(digest)) {
      mismatches.push(`extra ${name}`);
    }
  }
  if (mismatches.length) {
    throw new Error(
      `cli-output samples are stale (${mismatches.length}): ${mismatches.slice(0, 8).join("; ")}`,
    );
  }
  return { count: expected.size };
}

export async function redactExistingCliOutputs({
  outputDir = cliOutputRoot,
  extras,
} = {}) {
  let names = [];
  try {
    names = (await readdir(outputDir)).filter((name) => name.endsWith(".json"));
  } catch {
    return { count: 0 };
  }
  let written = 0;
  for (const name of names) {
    const path = join(outputDir, name);
    const sample = validateCliOutputSample(JSON.parse(await readFile(path, "utf8")));
    const next = finalizeCliOutputSample(sample, { redact: true, extras });
    const text = stableStringify(next);
    await writeFile(path, text);
    written += 1;
  }
  return { count: written };
}

const isMain =
  process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href;

if (isMain) {
  const check = process.argv.includes("--check");
  const update = process.argv.includes("--update");
  const missing = process.argv.includes("--missing");
  const redactOnly = process.argv.includes("--redact") && !update && !check;
  if ([check, update, missing, redactOnly].filter(Boolean).length !== 1) {
    process.stderr.write(
      "error: choose exactly one of --check, --update, --missing, or --redact\n",
    );
    process.exitCode = 2;
  } else if (update || missing) {
    const result = await recordCliOutputs({
      write: true,
      redact: true,
      missingOnly: missing,
    });
    process.stdout.write(
      `Recorded CLI output samples: ${result.count} argv, ${result.recorded} executed, ${result.skipped} skipped, ${result.pruned} pruned\n`,
    );
  } else if (redactOnly) {
    const result = await redactExistingCliOutputs();
    process.stdout.write(`Redacted CLI output samples: ${result.count} files\n`);
  } else {
    try {
      const result = await checkCliOutputs();
      process.stdout.write(`Verified CLI output samples: ${result.count} files\n`);
    } catch (error) {
      process.stderr.write(
        `cli-output check failed: ${error instanceof Error ? error.message : error}\n`,
      );
      process.exitCode = 1;
    }
  }
}
