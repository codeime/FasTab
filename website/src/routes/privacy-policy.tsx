import { createFileRoute } from "@tanstack/react-router";
import { SiteFooter, SiteHeader } from "../components/SiteChrome.tsx";
import { GITHUB_URL } from "../data.ts";
import { SeoJsonLd, guideSchema, pageHead } from "../seo.tsx";

const PRIVACY_DESCRIPTION =
  "This FasTab fork has all telemetry off and collects nothing. The settings below no longer apply.";

export const Route = createFileRoute("/privacy-policy")({
  head: () =>
    pageHead({
      title: "Privacy Policy — FasTab",
      description: PRIVACY_DESCRIPTION,
      path: "/privacy-policy",
    }),
  component: PrivacyPage,
});

const SECTION_HEADING =
  "m-0 mb-4 mt-12 text-[22px] font-bold tracking-[-.02em] text-(--ink)";
const PARAGRAPH = "m-0 mb-4 text-[15px] leading-[1.7] text-(--muted)";

function PrivacyPage() {
  return (
    <div className="min-h-screen bg-(--canvas) text-(--ink)">
      <SeoJsonLd
        data={guideSchema({
          title: "Privacy Policy — FasTab",
          description: PRIVACY_DESCRIPTION,
          path: "/privacy-policy",
          crumbLabel: "Privacy Policy",
        })}
      />
      <SiteHeader />

      <main className="mx-auto max-w-190 px-7 pb-24 pt-14">
        <nav
          aria-label="Breadcrumb"
          className="mb-10 font-mono text-xs text-(--muted)"
        >
          <a href="/" className="transition-colors hover:text-(--accent)">
            FasTab
          </a>
          <span className="px-2">/</span>
          <a href="/#install" className="transition-colors hover:text-(--accent)">
            Install
          </a>
          <span className="px-2">/</span>
          <span>Privacy Policy</span>
        </nav>
        <p className="mb-3 font-mono text-xs uppercase tracking-[.22em] text-(--accent)">
          Privacy Policy
        </p>
        <h1 className="m-0 mb-4 text-[36px] font-bold leading-[1.1] tracking-[-.03em]">
          This fork has all telemetry off.
        </h1>
        <p className={PARAGRAPH}>
          Completions run entirely on your Mac. This fork disables every
          telemetry path in the app and on this website.{" "}
          <strong className="text-(--ink)">Nothing is collected</strong> — no
          commands, no completions, no usage counts, no device IDs.
        </p>
        <p className={PARAGRAPH}>
          The sections below describe the unused upstream telemetry design.
          They <strong className="text-(--ink)">no longer apply</strong> to
          this fork. You do not need to run{" "}
          <code className="font-mono text-[13px]">ftab telemetry disable</code>
          ; it is already off.
        </p>

        <h2 className={SECTION_HEADING}>What we collect</h2>
        <p className={PARAGRAPH}>Nothing.</p>

        <h2 className={SECTION_HEADING}>
          Upstream telemetry (does not apply)
        </h2>
        <p className={PARAGRAPH}>
          Amazon Q Developer CLI could send anonymous usage events (app
          open, install, daily completion counts) to PostHog. That code is
          still in the tree so the fork stays reviewable, but it is disabled
          and not turned on here. Command content was never part of that
          path.
        </p>
        <p className={PARAGRAPH}>
          The old{" "}
          <code className="font-mono text-[13px]">ftab telemetry</code>{" "}
          commands and any settings that mention analytics do not send data
          in this build.
        </p>

        <h2 className={SECTION_HEADING}>Questions</h2>
        <p className={PARAGRAPH}>
          You can audit the disabled telemetry crate on{" "}
          <a
            href={`${GITHUB_URL}/tree/main/crates/fastab_telemetry`}
            className="text-(--accent) underline decoration-(--accent-line) underline-offset-4"
          >
            GitHub
          </a>
          .
        </p>
      </main>

      <SiteFooter />
    </div>
  );
}
