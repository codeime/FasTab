import commandIcon from "../assets/overlay-command.png";

/**
 * Claude-light overlay at the caret (after the typed token), not at the
 * start of the next line. Icons match native `subcommand` → `command.png`.
 */
const FONT = 12.8;
const ROW = 20;
const WIDTH = 320;
const ICON = 15;
const RADIUS = 6;
/** `~/web-app › git c` — overlay origin is the caret after this. */
const PROMPT_BEFORE_CARET = "~/web-app › git c";

const ROWS = [
  {
    name: "checkout",
    description: "Switch branches or restore working tree files",
    selected: true,
  },
  {
    name: "commit",
    description: "Record changes to the repository",
    selected: false,
  },
  {
    name: "clone",
    description: "Clone a repository into a new directory",
    selected: false,
  },
  {
    name: "config",
    description: "Change Git configuration",
    selected: false,
  },
  {
    name: "clean",
    description: "Remove untracked files from the working tree",
    selected: false,
  },
  {
    name: "cherry-pick",
    description: "Apply the changes introduced by some existing commits",
    selected: false,
  },
] as const;

export function OverlayPreview({ className = "" }: { className?: string }) {
  const selected = ROWS.find((row) => row.selected) ?? ROWS[0];

  return (
    <div
      className={`overflow-x-auto overflow-y-visible rounded-xl border border-(--border) bg-(--surface) shadow-[0_20px_50px_-32px_rgba(20,20,19,.28)] ${className}`}
    >
      <div className="flex items-center gap-2 border-b border-(--border) px-3.5 py-2.5">
        <span className="h-2.5 w-2.5 rounded-full bg-[#ff5f57]" />
        <span className="h-2.5 w-2.5 rounded-full bg-[#febc2e]" />
        <span className="h-2.5 w-2.5 rounded-full bg-[#28c840]" />
        <span className="ml-2 font-mono text-[11px] text-(--muted)">zsh</span>
      </div>

      <div className="relative min-h-64 px-4 pb-4 pt-5 font-mono text-[13px] leading-[1.4]">
        <p className="m-0 whitespace-pre text-(--ink)">
          <span className="text-(--muted)">~/web-app</span>
          <span className="text-(--accent)"> › </span>
          git c
          <span className="ml-px inline-block h-[13px] w-px translate-y-px bg-(--ink)" />
        </p>
        <div
          className="absolute z-10 overflow-hidden"
          style={{
            left: `calc(1rem + ${PROMPT_BEFORE_CARET.length}ch)`,
            top: "calc(1.25rem + 1.4em)",
            width: WIDTH,
            borderRadius: RADIUS,
            background: "#faf9f5",
            border: "1px solid #e6e4da",
            boxShadow: "0 8px 24px -12px rgba(20,20,19,.18)",
            fontSize: FONT,
            lineHeight: `${ROW}px`,
            color: "#141413",
          }}
          role="img"
          aria-label="FasTab overlay suggesting git subcommands"
        >
          {ROWS.map((row) => (
            <div
              key={row.name}
              className="flex items-center"
              style={{
                height: ROW,
                paddingLeft: FONT * 0.375,
                background: row.selected ? "#d97757" : "transparent",
                color: row.selected ? "#faf9f5" : "#141413",
              }}
            >
              <img
                src={commandIcon}
                alt=""
                width={ICON}
                height={ICON}
                className="shrink-0"
                style={{ width: ICON, height: ICON }}
              />
              <span className="ml-[5px] min-w-0 overflow-hidden whitespace-nowrap">
                {row.name}
              </span>
            </div>
          ))}
          <div
            className="flex items-center justify-between italic"
            style={{
              height: ROW,
              paddingLeft: 5,
              paddingRight: 5,
              borderTop: "1px solid #e6e4da",
              color: "#6f6e69",
            }}
          >
            <span className="min-w-0 overflow-hidden whitespace-nowrap">
              {selected.description}
            </span>
            <span style={{ borderRadius: 3, padding: "0 4px", fontSize: 10 }}>
              ⌃k
            </span>
          </div>
        </div>
      </div>
    </div>
  );
}
