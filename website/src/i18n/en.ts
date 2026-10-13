import {
  docSections,
  faqs,
  features,
  newTerminals,
  processes,
  reasons,
  terminalSupport,
} from "../data.ts";
import type { DocsCopy, HomeCopy } from "./types.ts";

export const homeCopyEn: HomeCopy = {
  badge: "macOS · local · open source",
  heroHeading: "Autocomplete for your terminal",
  heroSubheading:
    "IDE-style suggestions as you type — git, docker, npm, cargo — in Ghostty, iTerm2, Apple Terminal, VS Code, Cursor, Kitty, WezTerm, Alacritty, Zed, Otty, ChatGPT (Codex), and JetBrains. Native GPUI overlay, fully on-device. This fork has all telemetry off.",
  downloadCta: "Download for Mac",
  githubCta: "GitHub",

  marqueeLabel: "Runs in the terminal you already use",
  featuresLabel: "Features",
  featuresHeading:
    "Everything you need to complete a command. Nothing you don't",
  featuresSubheading:
    "One job, done well — no chat, no AI calls, no cloud completions.",

  whyLabel: "Why FasTab",
  whyHeading: "Opinionated, on purpose",

  terminalsLabel: "Supported terminals",
  terminalsHeading: "Works everywhere you type",
  terminalsNewPrefix: "New in v2.1.0:",
  terminalsBody:
    "Otty joins the input-method terminals with pixel-accurate cursor tracking, and ChatGPT (Codex) sessions are located through the xterm.js caret. Everything else keeps working through the shell integration installed automatically on first launch.",
  terminalsLinkLabel: "See the full support list",
  terminalGuideTitle: (name) => `${name} autocomplete setup guide`,
  newBadge: "New",

  howLabel: "How it works",
  howHeading: "Three processes, talking over sockets.",
  howSubheading:
    "Native GPUI (not a WebView), built in Rust. Each process owns one job and they coordinate over Unix domain sockets with Protobuf messages.",
  crateLabel: "crate",
  flowShellHooks: "shell hooks → CWD · command text · cursor",
  flowInputMethod: "input-method helper → caret position (macOS)",
  flowProtobuf: "Protobuf over Unix sockets",

  faqLabel: "FAQ",
  faqHeading: "Answers before you install.",

  docsLabel: "Docs",
  docsHeading: "Get from download to first completion.",
  docsSubheading:
    "Install FasTab, check which terminals are supported, set up cursor tracking for Ghostty, or fix a shell integration — without digging through the repository.",
  docsCta: "Docs and setup →",

  ctaHeading: "Stop memorizing flags",
  ctaSubheading: "Let your terminal remember them for you",
  ctaFootnote: "Requires macOS 12+ · Apple Silicon (ARM64) · MIT",
  ctaTagline: "FasTab · local terminal autocomplete",

  features,
  reasons,
  faqs,
  terminalSupport,
  docSections,
  processes,
};

export const docsCopyEn: DocsCopy = {
  eyebrow: "Documentation",
  heading: "Install it, wire up your terminal, keep it working.",
  intro:
    "Everything FasTab needs from you is on this page — the Native ARM64 DMG, one macOS permission, and a support list so you know which path your terminal takes.",
  quickStart: "Quick start",
  installGuideCta: "Full install guide",
  downloadCta: "Download DMG",
  requirements: "macOS 12+ · Apple Silicon (ARM64)",
  terminalColumn: "Terminal",
  trackingColumn: "Cursor tracking",
  notesColumn: "Notes",
  newBadge: "New",
  terminalsCalloutLead: `New in v2.1.0 — ${newTerminals.join(" and ")}.`,
  terminalsCalloutBody:
    "Otty gets input-method cursor tracking, and FasTab now coexists with Otty-managed shell rc files instead of reporting its own integration as broken. ChatGPT (Codex) sessions are tracked through xterm.js caret detection, the same path VS Code uses.",
  docSections,
  terminalSupport,
};
