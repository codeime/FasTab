import { OverlayPreview } from "./components/OverlayPreview.tsx";
import { SiteFooter, SiteHeader, type LocaleHrefs } from "./components/SiteChrome.tsx";
import { AppleIcon, GitHubIcon } from "./components/icons.tsx";
import { DOWNLOAD_URL } from "./download.ts";
import {
  AX_SETTINGS_PANE_EN,
  AX_SETTINGS_PANE_EN_LEGACY,
  AX_SETTINGS_PANE_ZH,
  AX_SETTINGS_PANE_ZH_LEGACY,
  GITHUB_URL,
} from "./data.ts";
import { homeCopyEn } from "./i18n/en.ts";
import { type HomeCopy, type Locale } from "./i18n/types.ts";
import { captureEvent } from "./posthog.tsx";

const XATTR = `xattr -dr com.apple.quarantine "/Applications/Fastab.app"`;

const PAGE: Record<
  Locale,
  {
    install: string;
    installLead: string;
    stepDownload: string;
    stepQuarantine: string;
    stepLaunch: string;
    stepAccess: string;
    stepReload: string;
    stepDoctor: string;
    help: string;
    helpEmpty: string;
    helpIme: string;
    helpCli: string;
    helpEc: string;
    helpEcLink: string;
    helpEcHref: string;
  }
> = {
  en: {
    install: "Install",
    installLead:
      "macOS 12+ · Apple Silicon. Unsigned builds need the quarantine command once.",
    stepDownload: "Download the DMG and drag Fastab.app into /Applications.",
    stepQuarantine: "Clear Gatekeeper quarantine (required while unsigned):",
    stepLaunch:
      "Open FasTab from Applications, then Grant Accessibility in Settings. FasTab can sit beside Easy Complete — grant FasTab itself; do not run both completion apps in the same terminal.",
    stepAccess: "Accessibility pane:",
    stepReload: "Reload your shell:",
    stepDoctor: "Check the install:",
    help: "If nothing appears",
    helpEmpty: "Reload the shell with exec $SHELL, then type git or npm.",
    helpIme:
      "Ghostty, Otty, Kitty, WezTerm, Zed, Alacritty: Settings → Behavior, or ftab integrations install input-method.",
    helpCli: "If ftab is missing, add ~/.local/bin to PATH.",
    helpEc:
      "Coming from Easy Complete? FasTab will not uninstall it. Open a new terminal after removing ecterm.",
    helpEcLink: "Uninstall Easy Complete",
    helpEcHref: `${GITHUB_URL}/blob/main/docs/uninstall-easy-complete.md`,
  },
  "zh-CN": {
    install: "安装",
    installLead: "macOS 12+ · Apple Silicon。未签名构建需要先清一次隔离属性。",
    stepDownload: "下载 DMG，把 Fastab.app 拖进 /Applications。",
    stepQuarantine: "清除 Gatekeeper 隔离（未签名时必须做一次）：",
    stepLaunch: "从应用程序打开 FasTab，在设置里授予辅助功能。可以和 Easy Complete 并排安装；请单独给 FasTab 授权，不要在同一终端里同时跑两套补全。",
    stepAccess: "辅助功能位置：",
    stepReload: "重载 Shell：",
    stepDoctor: "检查安装：",
    help: "没有出现补全时",
    helpEmpty: "先 exec $SHELL 重载，再输入 git 或 npm。",
    helpIme:
      "Ghostty、Otty、Kitty、WezTerm、Zed、Alacritty：设置 → 行为，或 ftab integrations install input-method。",
    helpCli: "找不到 ftab 时，把 ~/.local/bin 加进 PATH。",
    helpEc:
      "从 Easy Complete 迁过来？FasTab 不会替你卸。卸掉 ecterm 之后请新开终端。",
    helpEcLink: "卸载 Easy Complete",
    helpEcHref: `${GITHUB_URL}/blob/main/docs/uninstall-easy-complete.zh-CN.md`,
  },
};

function Code({ children }: { children: string }) {
  return (
    <pre className="mb-4 overflow-x-auto rounded-lg border border-(--border) bg-(--code-bg) p-3.5 font-mono text-[13px] leading-[1.7]">
      {children}
    </pre>
  );
}

function InstallActions({
  copy,
  locale,
  placement,
}: {
  copy: HomeCopy;
  locale: Locale;
  placement: "hero" | "footer_cta";
}) {
  return (
    <div className="flex flex-wrap gap-3">
      <a
        href={DOWNLOAD_URL}
        onClick={() =>
          captureEvent("website_download_clicked", { locale, placement })
        }
        className="inline-flex items-center gap-2 rounded-md bg-(--accent) px-5 py-2.5 text-[15px] font-semibold text-(--accent-fg) transition-opacity hover:opacity-90"
      >
        <AppleIcon />
        {copy.downloadCta}
      </a>
      <a
        href={GITHUB_URL}
        onClick={() =>
          captureEvent("website_github_clicked", { locale, placement })
        }
        className="inline-flex items-center gap-2 rounded-md border border-(--border) bg-(--surface) px-5 py-2.5 text-[15px] font-medium text-(--ink) transition-colors hover:border-(--accent-line)"
      >
        <GitHubIcon />
        {copy.githubCta}
      </a>
    </div>
  );
}

export function App({
  copy = homeCopyEn,
  locale = "en",
  hrefs,
}: {
  copy?: HomeCopy;
  locale?: Locale;
  hrefs?: LocaleHrefs;
} = {}) {
  const t = PAGE[locale];
  const ax =
    locale === "zh-CN"
      ? `${AX_SETTINGS_PANE_ZH}\n${AX_SETTINGS_PANE_ZH_LEGACY}`
      : `${AX_SETTINGS_PANE_EN}\n${AX_SETTINGS_PANE_EN_LEGACY}`;

  return (
    <div className="min-h-screen bg-(--canvas) text-(--ink)">
      <SiteHeader locale={locale} hrefs={hrefs} />

      <main>
        <section className="px-6 pb-16 pt-14 lg:pb-20 lg:pt-16">
          <div className="mx-auto grid max-w-6xl items-center gap-12 lg:grid-cols-[minmax(0,1fr)_minmax(0,1.05fr)] lg:gap-16">
            <div className="flex min-w-0 flex-col">
              <p className="m-0 mb-5 font-mono text-[12px] tracking-wider text-(--muted)">
                {copy.badge}
              </p>
              <h1 className="m-0 mb-4 max-w-xl text-[40px] font-semibold leading-[1.08] tracking-[-.04em] text-balance sm:text-[52px]">
                {copy.heroHeading}
              </h1>
              <p className="m-0 mb-8 max-w-xl text-[17px] leading-[1.6] text-(--muted)">
                {copy.heroSubheading}
              </p>
              <InstallActions copy={copy} locale={locale} placement="hero" />
              <p className="m-0 mt-4 text-[13px] text-(--muted)">
                {copy.ctaFootnote}
              </p>
            </div>
            <OverlayPreview />
          </div>
        </section>

        <section
          id="features"
          className="scroll-mt-24 border-t border-(--border) px-6 py-14"
        >
          <div className="mx-auto grid max-w-6xl gap-10 md:grid-cols-3">
            {copy.reasons.slice(0, 3).map((reason) => (
              <div key={reason.num}>
                <h2 className="m-0 mb-2 text-[17px] font-semibold tracking-[-.02em]">
                  {reason.title}
                </h2>
                <p className="m-0 text-[15px] leading-[1.6] text-(--muted)">
                  {reason.desc}
                </p>
              </div>
            ))}
          </div>
        </section>

        <section
          id="install"
          className="scroll-mt-24 border-t border-(--border) px-6 py-14"
        >
          <div className="mx-auto max-w-6xl">
            <h2 className="m-0 mb-2 text-[22px] font-semibold tracking-[-.03em]">
              {t.install}
            </h2>
            <p className="m-0 mb-6 text-[15px] text-(--muted)">{t.installLead}</p>
            <ol className="m-0 list-decimal space-y-5 pl-5 text-[15px] leading-[1.65]">
              <li>
                {t.stepDownload}{" "}
                <a
                  href={DOWNLOAD_URL}
                  className="text-(--accent) underline-offset-4 hover:underline"
                >
                  Fastab-arm64.dmg
                </a>
              </li>
              <li>
                {t.stepQuarantine}
                <Code>{XATTR}</Code>
              </li>
              <li>{t.stepLaunch}</li>
              <li>
                {t.stepAccess}
                <Code>{ax}</Code>
              </li>
              <li>
                {t.stepReload}
                <Code>exec $SHELL</Code>
              </li>
              <li>
                {t.stepDoctor}
                <Code>ftab doctor</Code>
              </li>
            </ol>
          </div>
        </section>

        <section
          id="help"
          className="scroll-mt-24 border-t border-(--border) px-6 py-14"
        >
          <div className="mx-auto max-w-6xl">
            <h2 className="m-0 mb-4 text-[22px] font-semibold tracking-[-.03em]">
              {t.help}
            </h2>
            <ul className="m-0 list-disc space-y-2 pl-5 text-[15px] leading-[1.65] text-(--muted)">
              <li>{t.helpEmpty}</li>
              <li>{t.helpIme}</li>
              <li>{t.helpCli}</li>
              <li>
                {t.helpEc}{" "}
                <a
                  href={t.helpEcHref}
                  className="text-(--accent) underline-offset-4 hover:underline"
                >
                  {t.helpEcLink}
                </a>
              </li>
            </ul>
          </div>
        </section>
      </main>

      <SiteFooter locale={locale} />
    </div>
  );
}
