// Page content, ported verbatim from the Claude Design source (Fastab.dc.html).

export const GITHUB_URL = "https://github.com/codeime/easy-complete";

/** System Settings list for the Accessibility TCC toggle. macOS 27 renamed it. */
export const AX_SETTINGS_PANE_EN =
  "System Settings → Privacy & Security → Device Control and Data Access";
export const AX_SETTINGS_PANE_EN_LEGACY =
  "macOS 26 and earlier: Privacy & Security → Accessibility";
export const AX_SETTINGS_PANE_ZH =
  "系统设置 → 隐私与安全 → 设备控制和数据访问";
export const AX_SETTINGS_PANE_ZH_LEGACY =
  "macOS 26 及更早：隐私与安全性 → 辅助功能";

export interface Feature {
  glyph: string;
  title: string;
  desc: string;
}

const glyphs = ["›_", "⌘", "◳", "⚡", "◆", "◐", "⊞", "$", "⇥"];

const featureData: Omit<Feature, "glyph">[] = [
  {
    title: "Inline completions",
    desc: "IDE-style suggestions the instant you type — subcommands, flags, arguments and file paths, ranked for what you actually mean.",
  },
  {
    title: "Hundreds of CLIs",
    desc: "Out-of-the-box specs for git, npm, docker, cargo, aws, kubectl and many more popular tools.",
  },
  {
    title: "Native overlay",
    desc: "A GPUI window that floats at your cursor — not WKWebView, not escape codes painted into the prompt.",
  },
  {
    title: "Fast & lightweight",
    desc: "Built in Rust. Milliseconds to suggest, with no lag between you and your keystrokes.",
  },
  {
    title: "100% local",
    desc: "No cloud, no account, no AI calls. Your commands never leave your machine.",
  },
  {
    title: "Themeable",
    desc: "Match your terminal's vibe — ships with themes like GitHub Dark and more.",
  },
  {
    title: "Works everywhere",
    desc: "Most terminals work out of the box; Ghostty, Otty, Kitty, WezTerm, Zed and Alacritty get pixel-accurate tracking.",
  },
  {
    title: "In-IDE terminals",
    desc: "Completions follow you into VS Code, Cursor, ChatGPT (Codex), and JetBrains IDE terminals.",
  },
  {
    title: "One-command setup",
    desc: "./scripts/install.sh builds, installs and wires up shell integration — then just start typing.",
  },
];

export const features: Feature[] = featureData.map((d, i) => ({
  glyph: glyphs[i] ?? "›_",
  ...d,
}));

export interface Reason {
  num: string;
  title: string;
  desc: string;
}

export const reasons: Reason[] = [
  {
    num: "01",
    title: "Just autocomplete",
    desc: "No chat, no AI, no cloud. Subcommands, flags, paths — ranked as you type.",
  },
  {
    num: "02",
    title: "Native overlay",
    desc: "A GPUI window at the caret — not a WebView, not escape codes painted into the prompt.",
  },
  {
    num: "03",
    title: "Fully local",
    desc: "Completions never leave your Mac. This fork has all telemetry off and collects nothing.",
  },
  {
    num: "04",
    title: "Open source",
    desc: "A focused local completion engine built for fast terminal autocomplete.",
  },
];

/**
 * Every terminal reads the edit buffer through the same PTY shell integration.
 * They differ in how the caret is located, which is what positions the overlay:
 * the bundled macOS input method, xterm.js caret detection inside Electron
 * hosts, or the macOS Accessibility API. Mirrors the capability matrix in
 * `crates/fastab_util/src/terminal.rs`.
 */
export type TerminalIntegration = "input-method" | "xterm" | "accessibility";

export interface TerminalSupport {
  name: string;
  integration: TerminalIntegration;
  note: string;
  /** Added in v2.1.0 — surfaced with a "New" marker. */
  isNew?: boolean;
  /** Slug of the dedicated guide under `/terminals/`, when one exists. */
  slug?: string;
}

export const terminalSupport: TerminalSupport[] = [
  {
    name: "Ghostty",
    slug: "ghostty",
    integration: "input-method",
    note: "Optional input method from Settings → Behavior, or `ftab integrations install input-method`.",
  },
  {
    name: "Otty",
    slug: "otty",
    integration: "input-method",
    note: "Optional input method from Settings → Behavior, or `ftab integrations install input-method`. FasTab leaves Otty's own shell-integration block at the end of your rc file alone.",
    isNew: true,
  },
  {
    name: "Kitty",
    slug: "kitty",
    integration: "input-method",
    note: "Optional input method from Settings → Behavior, or `ftab integrations install input-method`.",
  },
  {
    name: "WezTerm",
    slug: "wezterm",
    integration: "input-method",
    note: "Optional input method from Settings → Behavior, or `ftab integrations install input-method`.",
  },
  {
    name: "Alacritty",
    slug: "alacritty",
    integration: "input-method",
    note: "Optional input method from Settings → Behavior, or `ftab integrations install input-method`.",
  },
  {
    name: "Zed",
    slug: "zed",
    integration: "input-method",
    note: "Optional input method from Settings → Behavior, or `ftab integrations install input-method`.",
  },
  {
    name: "JetBrains IDEs",
    integration: "input-method",
    note: "Integrated terminal across the IDE family.",
  },
  {
    name: "ChatGPT (Codex)",
    integration: "xterm",
    note: "Codex terminal sessions inside ChatGPT.app, located through the xterm.js caret.",
    isNew: true,
  },
  {
    name: "VS Code",
    integration: "xterm",
    note: "Integrated terminal, including Cursor, Windsurf, and Trae.",
  },
  {
    name: "iTerm2",
    slug: "iterm2",
    integration: "accessibility",
    note: "Works out of the box after a shell reload.",
  },
  {
    name: "Apple Terminal",
    integration: "accessibility",
    note: "Works out of the box after a shell reload.",
  },
];

export const INTEGRATION_LABEL: Record<TerminalIntegration, string> = {
  "input-method": "Input method",
  xterm: "xterm.js caret",
  accessibility: "Accessibility",
};

export const terminals: string[] = terminalSupport.map((t) => t.name);

export const newTerminals: string[] = terminalSupport
  .filter((t) => t.isNew)
  .map((t) => t.name);

export interface Faq {
  question: string;
  answer: string;
}

export const faqs: Faq[] = [
  {
    question: "What is FasTab?",
    answer:
      "FasTab is a macOS terminal autocomplete app that shows IDE-style inline suggestions for command-line tools.",
  },
  {
    question: "Does FasTab run locally?",
    answer:
      "Yes. Autocomplete runs fully on-device — no account, no cloud calls, no AI requests, and your commands never leave your Mac.",
  },
  {
    question: "What data does FasTab collect?",
    answer:
      "This fork has all telemetry off. FasTab collects nothing — no commands, no completions, no usage counts.",
  },
  {
    question: "Which terminals does FasTab support?",
    answer:
      "FasTab supports Ghostty, Otty, Kitty, WezTerm, Alacritty, Zed, iTerm2, Apple Terminal, VS Code, ChatGPT (Codex), and JetBrains IDE terminals.",
  },
  {
    question: "How do I install FasTab?",
    answer:
      "Download the ARM64 DMG from GitHub Releases, then follow the Install section on the homepage.",
  },
  {
    question: "Do I need to uninstall Easy Complete first?",
    answer:
      "Only if you want Easy Complete gone. FasTab will not uninstall it. Both can sit on disk, but they must not wrap the same terminal. See docs/uninstall-easy-complete.md on GitHub.",
  },
];

export interface DocLink {
  href: string;
  label: string;
  description: string;
  external?: boolean;
}

export interface DocSection {
  id: string;
  title: string;
  summary: string;
  links: DocLink[];
}

export const docSections: DocSection[] = [
  {
    id: "getting-started",
    title: "Getting started",
    summary:
      "From download to first completion — install the app, approve one permission, reload your shell.",
    links: [
      {
        href: "/install",
        label: "Install on macOS",
        description:
          "DMG, Accessibility permission, shell reload, and verification with ftab doctor.",
      },
      {
        href: "/troubleshooting",
        label: "Troubleshooting",
        description:
          "Diagnose missing suggestions, stale shell hooks, and broken terminal integrations.",
      },
    ],
  },
  {
    id: "terminals",
    title: "Terminal support",
    summary:
      "Every terminal reads your command through the same shell integration — they differ in how FasTab finds the caret to position the overlay.",
    links: [
      {
        href: "/terminals/ghostty",
        label: "Ghostty autocomplete",
        description:
          "Why Ghostty needs the input method, and how to re-register it when suggestions drift.",
      },
      {
        href: "/terminals/otty",
        label: "Otty autocomplete",
        description:
          "Input-method tracking, plus how FasTab coexists with Otty's own shell-integration block.",
      },
      {
        href: "/terminals/kitty",
        label: "Kitty autocomplete",
        description:
          "Why closing every Kitty window is not the same as quitting it, and what that means for setup.",
      },
      {
        href: "/terminals/wezterm",
        label: "WezTerm autocomplete",
        description:
          "The wezterm-gui process, and what multiplexer domains do to remote completions.",
      },
      {
        href: "/terminals/alacritty",
        label: "Alacritty autocomplete",
        description:
          "Setup for a terminal with no tabs — including how completions behave inside tmux.",
      },
      {
        href: "/terminals/zed",
        label: "Zed terminal autocomplete",
        description:
          "Completions in the Zed terminal panel, separate from Zed's own editor predictions.",
      },
      {
        href: "/terminals/iterm2",
        label: "iTerm2 autocomplete",
        description:
          "The Accessibility path — no input method to register, and how it sits with iTerm2 Shell Integration.",
      },
    ],
  },
  {
    id: "reference",
    title: "Reference",
    summary:
      "What the app collects, how it compares, and where the source lives.",
    links: [
      {
        href: "/privacy-policy",
        label: "Privacy policy & telemetry",
        description:
          "The full list of anonymous events, and the one command that turns them off.",
      },
      {
        href: "/fig-alternative",
        label: "Looking for a Fig alternative?",
        description:
          "What a focused, local completion engine includes — and what it deliberately leaves out.",
      },
      {
        href: GITHUB_URL,
        label: "Source on GitHub",
        description:
          "Rust crates, TypeScript packages, and the build scripts behind the app.",
        external: true,
      },
    ],
  },
];

export interface Process {
  bin: string;
  crate: string;
  role: string;
}

export const processes: Process[] = [
  {
    bin: "fastab",
    crate: "fastab_desktop",
    role: "Native app host — GPUI overlay and settings (not WKWebView), completion engine worker, system tray and window management.",
  },
  {
    bin: "fterm",
    crate: "fastab_term",
    role: "Pseudoterminal between your shell and emulator; intercepts the shell edit buffer to drive completions.",
  },
  {
    bin: "ftab",
    crate: "fastab_cli",
    role: "CLI entry point — setup, integrations, diagnostic, settings and more.",
  },
];
