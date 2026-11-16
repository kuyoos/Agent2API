#!/usr/bin/env bash
# 手动应急补发入口：正常发布由本仓库 build.yml 的 publish job 自动完成。
# 本脚本仅用于旧版本收尾或工作流不可用时的人工补发。
#
#   bash scripts/release.sh vX.Y.Z            # 自动找该 tag 的可用构建 run
#   bash scripts/release.sh vX.Y.Z <run-id>   # 或显式指定 run
#
# ── 本 fork：构建、发布与更新都在同一个仓库 ────────────────────
# 上游把编译派发到独立构建账号（`Agent2Builder/agent2api-releases`）。本 fork
# 没有那个账号与令牌，编译与发布都在 `kuyoos/Agent2API` 自己的 Actions 里跑
# （见 `.github/workflows/build.yml`）。所以本脚本找 run、下载 artifact、建
# Release 全部指向当前项目仓库。目标仓库可用 `BUILD_REPO` 环境变量覆盖：
#
#   BUILD_REPO=SomeOwner/some-repo bash scripts/release.sh v3.2.0
#
# 这个值与 `desktop-tauri/src-tauri/server/src/server/core/update/version.rs`
# 的 `DEFAULT_REPO` 必须指向同一个地方 —— 不一致的症状是「Release 建出来了，
# 但没有人收到更新」。
#
# 做三件事：
#   1. 下载构建工作流的产物 artifact（windows-nsis，以及 linux-server-amd64 /
#      -arm64 与 windows-server-amd64 服务端包）到 dist/；
#   2. 用 tag 所指提交的提交信息（= 更新日志，见 .agents/skills/release-flow/SKILL.md
#      第 5 节）创建 / 更新 GitHub Release 并挂全部附件；
#   3. 打印验收提示。
#
# ── 本 fork 只出 Windows 桌面端 + 服务端包 ────────────────────
# macOS dmg 与 Linux 桌面端 deb / AppImage 已从 build.yml 移除（部署形态只需
# Windows 桌面端 + 无 GUI 服务端，见该文件头）。本脚本同步只认这几类 artifact。
#
# ── artifact 与新老 tag 的关系 ──────────────────────────────────
# 各类 artifact 都是分阶段加上的，老 tag 的 run 里可能缺：
#   · linux-server-*（容器内一键更新，v3.0.4 起）
#   · windows-server-amd64（Windows 服务端包）
#   · （旧 tag 里仍会看到 macos-universal / linux-desktop / android-apk /
#     ios-ipa —— 本脚本已不再挂这几类）
# 缺任一类时脚本**只提醒、不中断** —— 历史 tag 仍可重跑收尾。
#
# 幂等可重跑：附件 `--clobber` 覆盖上传。

set -euo pipefail

TAG="${1:?用法: scripts/release.sh vX.Y.Z [build-run-id]}"
RUN_ID="${2:-}"

# 构建 run 与 Release 所在的仓库（见文件头）。默认值与 `version.rs` 的
# `DEFAULT_REPO` 一致；换构建仓库时三处一起改。
BUILD_REPO="${BUILD_REPO:-kuyoos/Agent2API}"
export BUILD_REPO

# ── 两把凭据 ──────────────────────────────────────────────────
# 本 fork 里构建仓库与源码仓库是同一个（`kuyoos/Agent2API`），MIRROR_TO_SOURCE
# 会因此自动跳过。仍保留两个变量，是为了「临时指到别的构建仓库」这条路径
# （`BUILD_REPO=Other/repo`）不失效。
#
# `BUILD_TOKEN`：对构建 / 发布仓库有 contents:write 的 token（默认取当前 gh 活动账号）
# `SOURCE_TOKEN`：对源码仓库有 contents:write 的 token（同上）
#
# 两者都省略时，脚本假定当前 `gh auth` 的账号对两边都有权限。
BUILD_TOKEN="${BUILD_TOKEN:-${GH_TOKEN:-}}"
SOURCE_TOKEN="${SOURCE_TOKEN:-${GH_TOKEN:-}}"

case "$TAG" in
  v*) ;;
  *) echo "错误: '$TAG' 不是 v* 形式的 tag" >&2; exit 1 ;;
esac

# ── beta 版：tag 带预发布后缀（v3.1.0-beta.1）时，Release 必须标 pre-release ──
# 这个标记是应用里「加入 beta 版」开关的分界线：正式通道走 GitHub 的
# /releases/latest，它**排除**被标记的预发布；只有打开开关的人才从 /releases
# 列表里把 beta 捞出来。漏标 = beta 会被所有正式版用户当成正式更新收到；
# 给正式版误标则谁都不更新。所以这里按 tag 自动判、并顺手修正已存在的 Release。
case "${TAG#v}" in
  *-*) PRERELEASE=true ;;
  *)   PRERELEASE=false ;;
esac
if [ "$PRERELEASE" = true ]; then
  echo "→ 检测到预发布后缀（${TAG#v}），Release 标记为 pre-release（只发给「加入 beta 版」的用户）"
fi

command -v gh   >/dev/null || { echo "错误: 未安装 gh CLI" >&2; exit 1; }
# gh 的 JSON 输出用 node 解析（不引入 jq 依赖，两端都有 node）
command -v node >/dev/null || { echo "错误: 未安装 node" >&2; exit 1; }

# gh 是 Go 程序：既不读 git 的 http.proxy，也不读 Windows 系统代理，只认代理
# 环境变量。本机 git 配了代理而 gh 没配，下面的下载安装包与上传附件都是大文件
# 传输，直连不稳时会中途失败 —— 故把 git 的代理透传给 gh。
# 只设 HTTPS_PROXY：gh 的请求（含附件上传下载）全是 HTTPS，实测不读 HTTP_PROXY。
# 从 git 读取而非写死端口，换代理只改 git 一处；git 未配代理就不设，退回直连
# （外部已显式设过时不覆盖）。
if [ -z "${HTTPS_PROXY:-}" ]; then
  # 按 git/curl 查找代理的顺序依次尝试，兼容这几种配置写法：
  #   http.<url>.proxy（按仓库配） / http.proxy / https.proxy
  # 只取 --get-urlmatch 会漏掉 https.proxy，只取 https.proxy 会漏掉前两种。
  GH_PROXY=$(git config --get-urlmatch http.proxy https://github.com || true)
  [ -n "$GH_PROXY" ] || GH_PROXY=$(git config --get https.proxy || true)
  if [ -n "$GH_PROXY" ]; then
    export HTTPS_PROXY="$GH_PROXY"
    echo "→ 已启用代理: $GH_PROXY"
  fi
fi

# ── 1. 定位 build run 并下载安装包 ────────────────────────────
if [ -z "$RUN_ID" ]; then
  echo "→ 查找 $TAG 触发的 build 工作流…"
  # 不按 run 整体 conclusion 过滤：只验「桌面端与服务端 artifact 是否齐全」，
  # 构建成功但别处失败的 run 里安装包照旧可用
  for id in $(GH_TOKEN="$BUILD_TOKEN" gh run list --workflow=build --repo "$BUILD_REPO" --limit 30 --json databaseId,headBranch \
      | node -e 'let s="";process.stdin.on("data",d=>s+=d);process.stdin.on("end",()=>{JSON.parse(s).filter(r=>r.headBranch===process.argv[1]).forEach(r=>process.stdout.write(r.databaseId+"\n"))})' "$TAG"); do
    if GH_TOKEN="$BUILD_TOKEN" gh api "repos/$BUILD_REPO/actions/runs/$id/artifacts" \
         --jq 'any(.artifacts[].name; . == "windows-nsis") and any(.artifacts[].name; . == "linux-server-amd64")' >/dev/null 2>&1; then
      RUN_ID=$id
      break
    fi
  done
  [ -n "$RUN_ID" ] || {
    echo "错误: 没找到 $TAG 的可用 build run（需 windows-nsis 与 linux-server-amd64 两个 artifact 齐全）—— 先确认 CI 跑完（gh run list），或手动传 run-id" >&2
    exit 1
  }
fi
echo "→ build run: $RUN_ID"

rm -rf dist
GH_TOKEN="$BUILD_TOKEN" gh run download "$RUN_ID" --repo "$BUILD_REPO" -D dist
EXE=$(ls dist/windows-nsis/*.exe)
echo "→ 已下载 $(basename "$EXE")"

ASSETS=("$EXE")

# 容器内更新包 / 裸机安装包：两个架构的 tar.gz + 同名 .sha256（面板按名找
# 校验文件，缺了会拒绝安装）。老 tag 的 run 没有这两个 artifact —— 提醒一句继续走。
#
# 同一个包身兼两职：容器内一键更新的载荷 + Linux 裸机安装包（包内附
# install.sh / systemd 单元 / env 样例，解包后跑 install.sh 即可）。
if [ -d dist/linux-server-amd64 ] && [ -d dist/linux-server-arm64 ]; then
  # 两个目录下各是 <包名>.tar.gz 与 <包名>.tar.gz.sha256，原样全带上
  for file in dist/linux-server-amd64/* dist/linux-server-arm64/*; do
    ASSETS+=("$file")
  done
  echo "→ 已下载 $(basename "$(ls dist/linux-server-amd64/*.tar.gz)") / $(basename "$(ls dist/linux-server-arm64/*.tar.gz)")（含 .sha256）"
else
  echo "⚠️  这个 run 里没有 linux-server-* 产物（本功能上线前的 tag？）——"
  echo "    Release 将不含容器内更新包，面板更新入口会退回「手动升级命令」提示"
fi

# Windows 服务端包（无 GUI，命令行安装 + 注册成 Windows 服务）。
# 与上面 linux-server 同一口径：老 tag 的 run 里没有，提醒一句继续走。
if [ -d dist/windows-server-amd64 ]; then
  for file in dist/windows-server-amd64/*; do
    ASSETS+=("$file")
  done
  echo "→ 已下载 $(basename "$(ls dist/windows-server-amd64/*.zip)")（含 .sha256）"
else
  echo "⚠️  这个 run 里没有 windows-server-amd64 产物（本功能上线前的 tag？）——"
  echo "    Release 将不含 Windows 服务端包，Windows Server 用户只能自己编译"
fi

# ── 手机端安装包：默认**不挂** Release ────────────────────────
# `INCLUDE_MOBILE=1` 才把 APK / IPA 挂上去。
#
# 为什么默认关：手机端还在收尾（2026-10 用户明确要求先不发布）。构建照常
# 产出 artifact，需要时在 Actions 页面下载；只是不挂到公开 Release 上 ——
# 挂上去就等于对外承诺「这个包能用」，而现在还不能。
#
# 恢复发布时：`INCLUDE_MOBILE=1 bash scripts/release.sh vX.Y.Z`，或把默认值
# 改成 1。安装文档里关于 APK / IPA 的段落也要跟着恢复。
INCLUDE_MOBILE="${INCLUDE_MOBILE:-0}"

if [ "$INCLUDE_MOBILE" = "1" ]; then
  # Android APK（build 工作流的 android job 上传，含 .sha256）。
  # 接入 CI 之前的 tag 没有这个 artifact —— 提醒一句继续走。
  #
  # 与前面几个包不同，APK 的附件名**就是用户看到的文件名**：安装文档里写的是
  # `Agent2API-<版本>.apk`，android job 已按这个形状重命名，这里原样上传即可。
  if [ -d dist/android-apk ]; then
    for file in dist/android-apk/*; do
      ASSETS+=("$file")
    done
    echo "→ 已下载 $(basename "$(ls dist/android-apk/*.apk)")（含 .sha256）"
  else
    echo "⚠️  这个 run 里没有 android-apk 产物（APK 接入 CI 之前的 tag？）——"
    echo "    Release 将不含手机端安装包，Android 用户只能从旧版本或自行编译获取"
  fi

  # iOS IPA（build 工作流的 ios job 上传，含 .sha256）。
  # 默认是**未签名**包：用户拿 AltStore / SideStore 用自己的 Apple ID 重签后安装；
  # 配齐 iOS 签名 secret 的仓库出的是已签名包。两种都能挂 Release，脚本不区分。
  if [ -d dist/ios-ipa ]; then
    for file in dist/ios-ipa/*; do
      ASSETS+=("$file")
    done
    echo "→ 已下载 $(basename "$(ls dist/ios-ipa/*.ipa)")（含 .sha256）"
  else
    echo "⚠️  这个 run 里没有 ios-ipa 产物（iOS 接入 CI 之前的 tag？）——"
    echo "    Release 将不含 iOS 安装包，iPhone 用户只能走 PWA 或自行编译"
  fi
else
  echo "→ 手机端安装包按设置不挂 Release（INCLUDE_MOBILE=0）；构建产物仍在 Actions 的 artifact 里"
fi

# ── 2. 更新日志 = tag 所指提交的提交信息 ──────────────────────
NOTES_FILE=$(mktemp)
trap 'rm -f "$NOTES_FILE"' EXIT
git log -1 --format=%B "$TAG" > "$NOTES_FILE"

echo "→ GitHub Release"
if GH_TOKEN="$BUILD_TOKEN" gh release view "$TAG" --repo "$BUILD_REPO" >/dev/null 2>&1; then
  # 已存在时也把预发布标记纠正一遍（重跑收尾 / 当初漏标的情形都靠这一行自愈）
  GH_TOKEN="$BUILD_TOKEN" gh release edit "$TAG" --repo "$BUILD_REPO" --prerelease="$PRERELEASE"
  echo "  ↳ 已存在，附件逐个覆盖上传（pre-release=$PRERELEASE）"
else
  GH_TOKEN="$BUILD_TOKEN" gh release create "$TAG" --repo "$BUILD_REPO" --prerelease="$PRERELEASE" --title "$TAG" \
    --notes-file "$NOTES_FILE"
  echo "  ↳ 已创建（pre-release=$PRERELEASE）"
fi

# 附件**逐个**上传，不一次传一批。实测（2026-10-17，v3.0.9-beta.1）：一批 10 个
# 附件时 gh 会在某个「已经存在」的附件上报 HTTP 422 ReleaseAsset.name already
# exists（--clobber 没能先把它删掉），整批就此中断 —— 症状是 Release 建好了、
# 附件却少了一半。逐个 + --clobber 稳定，且天然幂等（重跑只补缺的那几个）。
for asset in "${ASSETS[@]}"; do
  GH_TOKEN="$BUILD_TOKEN" gh release upload "$TAG" "$asset" --repo "$BUILD_REPO" --clobber
done

# ── 2.5 镜像一份到源码仓库（MIRROR_TO_SOURCE=1 时）──────────────
# 构建产物在构建仓库，但用户与贡献者习惯看源码仓库的 Releases 页 —— 那里
# 空着会让人以为「这个版本没发」。所以再挂一份过去。
#
# 本脚本只负责应急补发；自动发布与主仓库镜像见构建仓库 publish-installers.yml。
# 两个任务可独立重跑，通常无需本地下载或重新编译。
#
# 默认开；`MIRROR_TO_SOURCE=0` 可关掉（例如只想在构建仓库留一份）。
MIRROR_TO_SOURCE="${MIRROR_TO_SOURCE:-1}"
if [ "$MIRROR_TO_SOURCE" = "1" ]; then
  SOURCE_REPO="$(git remote get-url origin | sed -E 's#.*github\.com[:/]([^/]+/[^/.]+)(\.git)?$#\1#')"
  if [ -z "$SOURCE_REPO" ] || [ "$SOURCE_REPO" = "$BUILD_REPO" ]; then
    echo "⚠️  取不到源码仓库名（origin 不是 GitHub？），跳过镜像"
  else
    echo "→ 镜像到源码仓库 $SOURCE_REPO"
    # 正文与构建仓库那份**逐字相同**（同一份更新日志），不追加任何说明行 ——
    # 两个 Release 的内容应当一致，差异只该来自平台本身。
    if GH_TOKEN="$SOURCE_TOKEN" gh release view "$TAG" --repo "$SOURCE_REPO" >/dev/null 2>&1; then
      GH_TOKEN="$SOURCE_TOKEN" gh release edit "$TAG" --repo "$SOURCE_REPO" --prerelease="$PRERELEASE" --notes-file "$NOTES_FILE"
    else
      GH_TOKEN="$SOURCE_TOKEN" gh release create "$TAG" --repo "$SOURCE_REPO" --prerelease="$PRERELEASE" \
        --title "$TAG" --notes-file "$NOTES_FILE"
    fi
    # 同样逐个上传：一批传会在已存在的附件上报 422（见上面那段注释）
    for asset in "${ASSETS[@]}"; do
      GH_TOKEN="$SOURCE_TOKEN" gh release upload "$TAG" "$asset" --repo "$SOURCE_REPO" --clobber
    done
    echo "  ↳ 已镜像 ${#ASSETS[@]} 个附件"
  fi
fi

# ── 3. 验收提示 ───────────────────────────────────────────────
echo
echo "发版收尾完成。核对："
echo "  [1] GitHub Release:  gh release view $TAG --repo $BUILD_REPO"
if [ "$PRERELEASE" = true ]; then
  echo "      ↑ beta 版：Release 应显示 Prerelease —— 正式版用户收不到它，"
  echo "        只有开了「加入 beta 版」的用户收得到（见 .agents/skills/release-flow/SKILL.md 第 6 节）"
else
  echo "  [2] GHCR:            ghcr.io/kuyoos/agent2api（$TAG 版本号 + latest；首次推送后需把包改成 Public）"
fi
echo "  [3] Windows 安装包「关于」页版本号与 $TAG 一致"
echo "  [4] 容器内更新：面板「设置 → 软件更新」出现「下载并更新」，且 Release 里"
echo "      agent2api-server-linux-{amd64,arm64}.tar.gz 与各自的 .sha256 都在"
echo "  [5] 服务端包（无 GUI）：Release 里还有 windows-server-amd64.zip（含 .sha256）。"
echo "      两个平台的包解包后都应看到安装脚本 —— Linux: install.sh / agent2api.service"
echo "      / agent2api.env.example；Windows: install-service.ps1"
echo "  [6] 附件总数应为 7（Windows exe + linux-server 两份包各带 .sha256 + windows-server zip 与 .sha256）"
if [ "$PRERELEASE" = true ]; then
  echo "  [9] GHCR 镜像：只出 $TAG 那个版本 tag 与滚动 :beta（beta tag 不移动 :latest）"
fi
