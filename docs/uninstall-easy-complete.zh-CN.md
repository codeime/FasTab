# 卸载 Easy Complete

FasTab 是另一个产品。`ftab uninstall` 和 `scripts/uninstall.sh` 都不会卸
Easy Complete、Fig 或 Amazon Q。

只有在你想把 Easy Complete **卸干净**时才需要这篇。两个应用可以并排装在磁盘上
（`app.fastab` 和 `dev.emmmm.easy-complete`）。同一终端会话不能同时被两套
补全包装：已经在 `ecterm` 里时 `Q_TERM` 已设置，`ftab init` 会让路。

不要删 `~/.fig`、Amazon Q 的 LaunchAgent，也不要整文件删 fish 的
`00_fig_pre.fish` / `99_fig_post.fish`——Fig、Amazon Q 和 FasTab 共用这两个
文件名。只剥 Easy Complete 的行。

## 1. 官方卸载（`ec` 还能跑时）

```bash
ec integrations uninstall input-method
ec integrations uninstall dotfiles
ec integrations uninstall ssh 2>/dev/null || true

/Applications/Easy\ Complete.app/Contents/MacOS/easy-complete --unregister-login-item 2>/dev/null || true
ec uninstall
```

旧版 Easy Complete 的 `uninstall.sh` 会调 `ec integrations uninstall shell`，
那是空操作。真正的子命令是 `dotfiles`。做完后关掉所有已经被 `ecterm` 包过的
终端窗口。

## 2. 扫残留

第 1 步之后可以再跑一遍；FasTab 已经装上了也安全——这些路径只属于 Easy Complete。

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

不要不带 bundle id 就跑 `tccutil reset Accessibility`。

**rc 和 SSH**——只剥 Easy Complete 的行：

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

再到**系统设置**里看一眼：

- 通用 → 登录项：去掉 **Easy Complete**
- 键盘 → 输入法：如果还列着 **Easy Complete**，删掉。FasTab 不会帮你清
  `dev.emmmm.easy-complete.inputmethod`。只有第 1 步里 `ec` 还能跑
  `integrations uninstall input-method` 时才会清掉输入法列表里的那一行。
- 隐私与安全 → 设备控制和数据访问（macOS 26 及更早叫辅助功能）——FasTab
  要自己再授一次（`app.fastab`）。Easy Complete 的勾选不算。

## 3. 确认干净

下面这些应该没有输出：

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

## 4. 再装 FasTab

Easy Complete 卸完后**新开一个终端**。旧的 `ecterm` 标签里还带着 `Q_TERM`，
FasTab 不会再包装它。

```bash
xattr -dr com.apple.quarantine "/Applications/Fastab.app"
```

启动 FasTab，在 FasTab 设置里授予辅助功能，然后在新窗口里 `exec $SHELL`。
