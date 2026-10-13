# Uninstall Easy Complete

FasTab is a separate product. `ftab uninstall` and `scripts/uninstall.sh` never
remove Easy Complete, Fig, or Amazon Q.

You only need this guide if you want Easy Complete **gone**. Both apps can sit
on disk (`app.fastab` vs `dev.emmmm.easy-complete`). They must not both wrap
the same terminal session: `ftab init` stands down when `Q_TERM` is already
set (an `ecterm` session).

Do not delete `~/.fig`, Amazon Q LaunchAgents, or fish `00_fig_pre.fish` /
`99_fig_post.fish` wholesale — Fig, Amazon Q, and FasTab share those fish
filenames. Only strip Easy Complete lines.

## 1. Official uninstall (if `ec` still runs)

```bash
ec integrations uninstall input-method
ec integrations uninstall dotfiles
ec integrations uninstall ssh 2>/dev/null || true

/Applications/Easy\ Complete.app/Contents/MacOS/easy-complete --unregister-login-item 2>/dev/null || true
ec uninstall
```

Older Easy Complete `uninstall.sh` called `ec integrations uninstall shell`,
which did nothing. `dotfiles` is the real subcommand. After this, quit every
terminal that was already wrapped by `ecterm`.

## 2. Leftover sweep

Safe to run after step 1, and safe if FasTab is already installed — these
paths are Easy Complete only.

```bash
pkill -x easy-complete 2>/dev/null || true
pkill -f "EasyCompleteInputMethod.app" 2>/dev/null || true
pkill -x ecterm 2>/dev/null || true

uid="$(id -u)"
launchctl bootout "gui/${uid}/dev.emmmm.easy-complete" 2>/dev/null || true
rm -f ~/Library/LaunchAgents/dev.emmmm.easy-complete.plist

rm -rf "/Applications/Easy Complete.app"
rm -rf ~/Library/Input\ Methods/EasyCompleteInputMethod.app
rm -f ~/.local/bin/ec ~/.local/bin/ecterm
for sh in bash zsh fish nu; do rm -f ~/.local/bin/"${sh} (ecterm)"; done

rm -rf ~/Library/Application\ Support/easy-complete
rm -rf ~/Library/Caches/easy-complete
rm -rf ~/.local/share/easy-complete
rm -rf ~/.easy-complete.dotfiles.bak
tmp_root="${TMPDIR:-/tmp}"
tmp_root="${tmp_root%/}"
rm -rf "${tmp_root}/ecrun" "${tmp_root}/eclog" /tmp/ecrun /tmp/eclog

defaults delete dev.emmmm.easy-complete 2>/dev/null || true
defaults delete dev.emmmm.easy-complete.inputmethod 2>/dev/null || true
tccutil reset Accessibility dev.emmmm.easy-complete 2>/dev/null || true
security delete-generic-password -s dev.emmmm.easy-complete 2>/dev/null || true
```

Do not run `tccutil reset Accessibility` without that bundle id.

**Shell rc and SSH** — strip Easy Complete lines only:

```bash
strip_ec() {
  local f="$1"; [[ -f "$f" ]] || return 0
  local t; t="$(mktemp)"
  grep -Ev 'Easy Complete (pre|post) block|easy-complete/shell/|eval "\$\((~/.local/bin/)?ec init |eval \((~/.local/bin/)?ec init |\[ -x ~/.local/bin/ec \] && eval |command -v ec >/dev/null 2>&1 && eval ' \
    "$f" > "$t" || true
  mv "$t" "$f"
}
strip_ec ~/.zshrc
strip_ec ~/.zprofile
strip_ec ~/.bashrc
strip_ec ~/.bash_profile
strip_ec ~/.config/fish/config.fish

for f in ~/.config/fish/conf.d/00_fig_pre.fish ~/.config/fish/conf.d/99_fig_post.fish; do
  [[ -f "$f" ]] || continue
  t="$(mktemp)"
  grep -Ev 'ec init|/\.local/bin/ec|command -v ec >/dev/null|command -qv ec |easy-complete/shell/|^[[:space:]]*# Easy Complete ' \
    "$f" > "$t" || true
  if ! grep -Eq '[^[:space:]]' "$t"; then rm -f "$f" "$t"; else mv "$t" "$f"; fi
done

if [[ -f ~/.ssh/config ]]; then
  t="$(mktemp)"
  awk '
    function flush(){ if(held!="") print held; held=""; pending=0 }
    /^# Easy Complete SSH Integration/ { flush(); held=$0; pending=1; next }
    pending==1 && $0 ~ /^Match all[[:space:]]*$/ { held=held "\n" $0; pending=2; next }
    pending==2 && $0 ~ /Include/ && $0 ~ /easy-complete\/ssh/ { held=""; pending=0; next }
    { flush(); print }
    END { flush() }
  ' ~/.ssh/config > "$t"
  mv "$t" ~/.ssh/config
fi
```

In **System Settings**:

- General → Login Items — remove **Easy Complete**
- Keyboard → Input Sources — remove **Easy Complete** if it is still listed.
  FasTab will not drop `dev.emmmm.easy-complete.inputmethod`. Step 1 clears
  that row only while `ec` can still run `integrations uninstall input-method`.
- Privacy & Security → Device Control and Data Access (Accessibility on
  macOS 26 and earlier) — FasTab needs its own grant (`app.fastab`).
  Easy Complete's checkbox does not cover FasTab.

## 3. Confirm

These should print nothing:

```bash
for p in \
  "/Applications/Easy Complete.app" \
  ~/.local/bin/ec ~/.local/bin/ecterm \
  ~/Library/Application\ Support/easy-complete \
  ~/Library/Input\ Methods/EasyCompleteInputMethod.app \
  ~/Library/LaunchAgents/dev.emmmm.easy-complete.plist
do
  [[ -e "$p" ]] && echo "still here: $p"
done
pgrep -x easy-complete && echo "still running: easy-complete"
pgrep -x ecterm && echo "still running: ecterm"
grep -nE 'ec init|easy-complete' \
  ~/.zshrc ~/.zprofile ~/.bashrc ~/.bash_profile \
  ~/.config/fish/config.fish \
  ~/.config/fish/conf.d/00_fig_pre.fish \
  ~/.config/fish/conf.d/99_fig_post.fish \
  ~/.ssh/config 2>/dev/null || true
```

## 4. Then install FasTab

Open a **new** terminal after Easy Complete is gone. An old `ecterm` tab
still has `Q_TERM` set and FasTab will not wrap it.

```bash
xattr -dr com.apple.quarantine "/Applications/Fastab.app"
```

Launch FasTab, grant Accessibility from FasTab Settings, then `exec $SHELL`
in a new window.
