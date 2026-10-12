#!/usr/bin/env bash
#
# Report memory used by every Fastab process.
#
# Numbers come from `footprint`, i.e. phys_footprint — the same figure Activity
# Monitor shows under "Memory" and the one that counts against system memory
# pressure. Plain RSS is not comparable: on macOS it double-counts shared pages
# and ignores compressed ones.
#
#   scripts/memory-usage.sh                    one-off table
#   scripts/memory-usage.sh --watch 5          resample every 5s
#   scripts/memory-usage.sh --watch 5 --csv m.csv   append samples to a CSV
#   scripts/memory-usage.sh --peak             add each process's peak footprint
#
set -euo pipefail

INTERVAL=""
CSV=""
SHOW_PEAK=0

usage() {
    sed -n '3,14p' "$0" | sed 's/^# \{0,1\}//'
    exit "${1:-0}"
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --watch)
            INTERVAL="${2:-}"
            [[ -n $INTERVAL ]] || { echo "--watch needs an interval in seconds" >&2; exit 2; }
            shift 2
            ;;
        --csv)
            CSV="${2:-}"
            [[ -n $CSV ]] || { echo "--csv needs a file path" >&2; exit 2; }
            shift 2
            ;;
        --peak) SHOW_PEAK=1; shift ;;
        -h|--help) usage 0 ;;
        *) echo "unknown argument: $1" >&2; usage 2 ;;
    esac
done

# Titles and command lines only select candidates. Every candidate must also
# have a matching mapped executable, including renamed fterm wrapper copies.
# In-place upgrades may leave an older inode mapped: do not require its hash to
# equal the new file on disk.
REPO_ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
readonly REPO_ROOT
readonly CANDIDATE='fastab|Fastab|fterm|ftab'

is_product_image() {
    local image=$1
    case "$image" in
        */Fastab.app/Contents/MacOS/fastab|*/Fastab.app/Contents/MacOS/ftab|*/Fastab.app/Contents/MacOS/fterm|*/Fastab.app/Contents/MacOS/fastabterm|*/Fastab.app/Contents/Helpers/FastabInputMethod.app/Contents/MacOS/fastab_input_method)
            return 0 ;;
        "$REPO_ROOT"/target/debug/fastab|"$REPO_ROOT"/target/release/fastab|"$REPO_ROOT"/target/dist/fastab|"$REPO_ROOT"/target/debug/ftab|"$REPO_ROOT"/target/release/ftab|"$REPO_ROOT"/target/dist/ftab|"$REPO_ROOT"/target/debug/fterm|"$REPO_ROOT"/target/release/fterm|"$REPO_ROOT"/target/dist/fterm|"$REPO_ROOT"/target/debug/fastabterm|"$REPO_ROOT"/target/release/fastabterm|"$REPO_ROOT"/target/dist/fastabterm|"$REPO_ROOT"/target/debug/fastab_input_method|"$REPO_ROOT"/target/release/fastab_input_method|"$REPO_ROOT"/target/dist/fastab_input_method)
            return 0 ;;
        "$HOME"/.local/bin/*' (fterm)'|"$HOME"/.local/bin/*' (fastabterm)')
            return 0 ;;
    esac
    [[ -n ${Q_TERM_PATH:-} && $image == "$Q_TERM_PATH" ]]
}

collect_pids() {
    local snapshot pid command mapped_file mapped start verified
    if ! snapshot=$(ps -Ao pid=,command=); then
        echo "Cannot read process list; memory usage is unknown." >&2
        return 1
    fi
    while read -r pid command; do
        [[ $command =~ $CANDIDATE ]] || continue
        start=$(ps -p "$pid" -o lstart= 2>/dev/null) || continue
        if ! mapped=$(/usr/sbin/lsof -nP -a -p "$pid" -d txt -Fn 2>/dev/null); then
            # A vanished process is normal. A live but unreadable candidate is
            # unknown, not a measured zero-byte product process.
            if [[ -n $start && $(ps -p "$pid" -o lstart= 2>/dev/null) == "$start" ]]; then
                echo "Cannot verify executable for candidate PID $pid; excluded from total." >&2
            fi
            continue
        fi
        verified=0
        while IFS= read -r mapped_file; do
            [[ $mapped_file == n* ]] || continue
            if is_product_image "${mapped_file#n}"; then
                verified=1
                break
            fi
        done <<<"$mapped"
        if [[ $verified == 1 && $(ps -p "$pid" -o lstart= 2>/dev/null) == "$start" ]]; then
            printf '%s\n' "$pid"
        fi
    done <<<"$snapshot"
}

# Turn one `footprint` run into "<pid>\t<name>\t<bytes>\t<peak>" lines.
#
# Kept to substr/index rather than a regex with capture groups: macOS ships BWK
# awk, which has no three-argument match(). Process names can contain spaces
# ("zsh (fterm)"), so field splitting is out too.
sample() {
    footprint --noCategories -f bytes "$@" 2>/dev/null | awk '
        # "fastab [9070]: 64-bit    Footprint: 85281280 B (16384 ...)"
        /Footprint:/ && /\[/ {
            lb = index($0, " [")
            rb = index($0, "]:")
            if (lb > 0 && rb > lb) {
                name = substr($0, 1, lb - 1)
                pid = substr($0, lb + 2, rb - lb - 2)
                split(substr($0, index($0, "Footprint:") + 10), parts, " ")
                bytes = parts[1]
            }
            next
        }
        /phys_footprint_peak:/ {
            split($0, p, ":")
            gsub(/[^0-9]/, "", p[2])
            if (pid != "") { print pid "\t" name "\t" bytes "\t" p[2]; pid = "" }
        }
    '
}

human() {
    awk -v b="$1" 'BEGIN {
        if (b >= 1073741824) { printf "%.2f GB", b / 1073741824 }
        else if (b >= 1048576) { printf "%.1f MB", b / 1048576 }
        else { printf "%.0f KB", b / 1024 }
    }'
}

report() {
    # `mapfile` would be tidier but macOS still ships bash 3.2.
    local args=() pid pids
    # A process substitution hides collect_pids failures from this shell.
    # Do not report an inaccessible process list as zero running processes.
    pids=$(collect_pids) || return 1
    while read -r pid; do
        if [[ -n $pid ]]; then
            args+=(-p "$pid")
        fi
    done <<<"$pids"

    if [[ ${#args[@]} -eq 0 ]]; then
        echo "No verified Fastab processes found."
        return
    fi

    local rows total=0 count=0
    rows=$(sample "${args[@]}")
    [[ -n $rows ]] || { echo "footprint returned nothing for pids: ${args[*]}" >&2; return; }

    if [[ $SHOW_PEAK -eq 1 ]]; then
        printf '%-8s %-34s %12s %12s\n' PID PROCESS MEMORY PEAK
    else
        printf '%-8s %-34s %12s\n' PID PROCESS MEMORY
    fi

    # Desktop first, then the IME, then one line per shell session, so the row
    # order does not jump around between samples.
    while IFS=$'\t' read -r pid name bytes peak; do
        [[ -n $pid ]] || continue
        if [[ $SHOW_PEAK -eq 1 ]]; then
            printf '%-8s %-34s %12s %12s\n' "$pid" "$name" "$(human "$bytes")" "$(human "$peak")"
        else
            printf '%-8s %-34s %12s\n' "$pid" "$name" "$(human "$bytes")"
        fi
        total=$((total + bytes))
        count=$((count + 1))
        if [[ -n $CSV ]]; then
            printf '%s,%s,%s,%s,%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$pid" "$name" "$bytes" "$peak" >>"$CSV"
        fi
    done < <(sort -t $'\t' -k2,2 <<<"$rows")

    printf '%-8s %-34s %12s\n' '' "TOTAL ($count processes)" "$(human "$total")"
}

if [[ -n $CSV && ! -s $CSV ]]; then
    echo 'timestamp,pid,process,footprint_bytes,peak_bytes' >"$CSV"
fi

if [[ -z $INTERVAL ]]; then
    report
    exit 0
fi

while true; do
    printf '\n== %s ==\n' "$(date '+%H:%M:%S')"
    report
    sleep "$INTERVAL"
done
