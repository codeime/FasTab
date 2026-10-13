import { useRouterState } from "@tanstack/react-router";
import logoUrl from "../assets/logo.png";
import { GITHUB_URL } from "../data.ts";
import { DOWNLOAD_URL } from "../download.ts";
import { captureEvent } from "../posthog.tsx";
import { LOCALE_PREFIX, type Locale } from "../i18n/types.ts";
import { AppleIcon, GitHubIcon } from "./icons.tsx";

/** Primary navigation — deliberately short: two destinations, two actions. */
const NAV_LABELS: Record<Locale, { install: string; help: string }> = {
  en: { install: "Install", help: "Help" },
  "zh-CN": { install: "安装", help: "帮助" },
};

const FOOTER_LABELS: Record<Locale, { privacy: string }> = {
  en: { privacy: "Privacy" },
  "zh-CN": { privacy: "隐私" },
};

const FOOTER_TAGLINE: Record<Locale, string> = {
  en: "FasTab · local terminal autocomplete",
  "zh-CN": "FasTab · 本地终端自动补全",
};

const FORK_CREDIT: Record<
  Locale,
  { prefix: string; mid: string; fig: string; thanks: string }
> = {
  en: {
    prefix: "Forked from ",
    mid: ", based on ",
    fig: " and ",
    thanks: ". Thanks to the Easy Complete, Amazon, and Fig contributors.",
  },
  "zh-CN": {
    prefix: "Fork 自 ",
    mid: "，基于 ",
    fig: " 与 ",
    thanks: "。感谢 Easy Complete、Amazon 与 Fig 的贡献者。",
  },
};

const TELEMETRY_NOTE: Record<Locale, string> = {
  en: "This fork has all telemetry off and collects nothing.",
  "zh-CN": "本 fork 关闭全部遥测，不收集任何信息。",
};

const EASY_COMPLETE_URL = "https://github.com/chen86860/easy-complete";
const UPSTREAM_URL = "https://github.com/aws/amazon-q-developer-cli";
const FIG_URL = "https://github.com/withfig/autocomplete";

/** URL of this page in each language that has a translation. */
export type LocaleHrefs = Partial<Record<Locale, string>>;

const ZH_PAGES = new Set([
  "/",
  "/docs",
  "/install",
  "/troubleshooting",
  "/fig-alternative",
  "/terminals/ghostty",
]);

function localePair(pathname: string, hrefs?: LocaleHrefs): Record<Locale, string> {
  const isZh = pathname === "/zh" || pathname.startsWith("/zh/");
  const enPath = (isZh ? pathname.replace(/^\/zh/, "") || "/" : pathname) || "/";
  const zhPath = enPath === "/" ? "/zh" : ZH_PAGES.has(enPath) ? `/zh${enPath}` : "/zh";
  return {
    en: hrefs?.en ?? (enPath === "/zh" ? "/" : enPath),
    "zh-CN": hrefs?.["zh-CN"] ?? zhPath,
  };
}

/** Plain EN / 中文 links — no dropdown, no JS navigation. */
export function LocaleMenu({
  locale,
  hrefs,
  className = "",
}: {
  locale: Locale;
  hrefs?: LocaleHrefs;
  className?: string;
}) {
  const pathname = useRouterState({
    select: (state) => state.location.pathname,
  });
  const pair = localePair(pathname, hrefs);

  return (
    <span className={`inline-flex items-center gap-2 text-sm ${className}`}>
      <a
        href={pair.en}
        className={
          locale === "en"
            ? "font-semibold text-(--ink)"
            : "text-(--muted) hover:text-(--ink)"
        }
      >
        EN
      </a>
      <span className="text-(--border)">/</span>
      <a
        href={pair["zh-CN"]}
        className={
          locale === "zh-CN"
            ? "font-semibold text-(--ink)"
            : "text-(--muted) hover:text-(--ink)"
        }
      >
        中文
      </a>
    </span>
  );
}

const DOWNLOAD_LABEL: Record<Locale, string> = {
  en: "Download",
  "zh-CN": "下载",
};

function navLinks(locale: Locale) {
  const home = LOCALE_PREFIX[locale] || "/";
  const labels = NAV_LABELS[locale];
  return [
    { href: `${home}#install`, label: labels.install, key: "install" },
    { href: `${home}#help`, label: labels.help, key: "help" },
  ] as const;
}

interface FooterLink {
  href: string;
  label: string;
  /** Off-site, so it needs target/rel — see `PUBLISHER_SITE_URL`. */
  external?: boolean;
}

function footerLinks(locale: Locale): FooterLink[] {
  return [{ href: "/privacy-policy", label: FOOTER_LABELS[locale].privacy }];
}

interface SiteHeaderProps {
  active?: "install" | "help";
  locale?: Locale;
  hrefs?: LocaleHrefs;
}

export function SiteHeader({
  active,
  locale = "en",
  hrefs,
}: SiteHeaderProps = {}) {
  return (
    <header className="sticky top-0 z-50 border-b border-(--border) bg-(--canvas)">

      <div className="mx-auto flex max-w-6xl items-center gap-5 px-6 py-3.5">
        <a
          href={LOCALE_PREFIX[locale] || "/"}
          className="flex items-center gap-2.5 font-mono text-[16px] font-bold tracking-tight"
        >
          <img
            src={logoUrl}
            alt=""
            className="h-7 w-7 rounded-md"
          />
          <span>FasTab</span>
        </a>

        <nav className="ml-auto flex items-center gap-2 text-sm sm:gap-6.5">
          {navLinks(locale).map((link) => (
            <a
              key={link.key}
              href={link.href}
              aria-current={active === link.key ? "page" : undefined}
              className={`hidden transition-colors sm:inline ${
                active === link.key
                  ? "text-(--ink)"
                  : "text-(--muted) hover:text-(--ink)"
              }`}
            >
              {link.label}
            </a>
          ))}

          <LocaleMenu locale={locale} hrefs={hrefs} />

          <a
            href={GITHUB_URL}
            onClick={() =>
              captureEvent("website_github_clicked", {
                locale,
                placement: "header",
              })
            }
            aria-label="FasTab on GitHub"
            className="inline-flex items-center gap-1.75 rounded-lg border border-(--border) bg-(--surface) px-3.25 py-1.75 text-(--ink) transition-colors hover:border-(--accent-line)"
          >
            <GitHubIcon />
            <span className="hidden sm:inline">GitHub</span>
          </a>
          <a
            href={DOWNLOAD_URL}
            onClick={() =>
              captureEvent("website_download_clicked", {
                locale,
                placement: "header",
              })
            }
            className="inline-flex items-center gap-1.75 rounded-md bg-(--accent) px-4 py-2 font-semibold text-(--accent-fg) transition-opacity hover:opacity-90"
          >
            <AppleIcon />
            {DOWNLOAD_LABEL[locale]}
          </a>
        </nav>
      </div>
    </header>
  );
}

export function SiteFooterLinks({
  className = "",
  locale = "en",
}: {
  className?: string;
  locale?: Locale;
}) {
  return (
    <span className={`flex flex-wrap items-center gap-4 ${className}`}>
      {footerLinks(locale).map((link) => (
        <a
          key={link.href}
          href={link.href}
          target={link.external ? "_blank" : undefined}
          rel={link.external ? "noopener" : undefined}
          className="transition-colors hover:text-(--ink)"
        >
          {link.label}
        </a>
      ))}
    </span>
  );
}

export function SiteFooter({ locale = "en" }: { locale?: Locale } = {}) {
  return (
    <footer className="border-t border-(--border) px-6 py-8 text-[13px] text-(--muted)">
      <div className="mx-auto flex max-w-6xl flex-col gap-4 sm:flex-row sm:items-end sm:justify-between">
        <div className="max-w-xl">
          <span className="inline-flex items-center gap-2 font-mono text-(--ink)">
            <img src={logoUrl} alt="" className="h-5 w-5 rounded-md" />
            {FOOTER_TAGLINE[locale]}
          </span>
          <p className="m-0 mt-2 leading-[1.6]">
            {FORK_CREDIT[locale].prefix}
            <a
              href={EASY_COMPLETE_URL}
              className="underline underline-offset-4 hover:text-(--ink)"
            >
              Easy Complete
            </a>
            {FORK_CREDIT[locale].mid}
            <a
              href={UPSTREAM_URL}
              className="underline underline-offset-4 hover:text-(--ink)"
            >
              Amazon Q Developer CLI
            </a>
            {FORK_CREDIT[locale].fig}
            <a
              href={FIG_URL}
              className="underline underline-offset-4 hover:text-(--ink)"
            >
              Fig
            </a>
            {FORK_CREDIT[locale].thanks}
          </p>
          <p className="m-0 mt-1 leading-[1.6]">{TELEMETRY_NOTE[locale]}</p>
        </div>
        <SiteFooterLinks locale={locale} />
      </div>
    </footer>
  );
}
