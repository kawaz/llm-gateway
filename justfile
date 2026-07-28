# llm-gateway justfile
#
# 自分用にリリースする (DR-0005)。version の正本は Cargo.toml の
# workspace.package.version で、push 時に check-version-bumped が
# 「crates/ の src を変えたのに version が据え置き」を止める。
# tag と GitHub Release は .github/workflows/release.yml が作る。
# 翻訳ペアは持たない (public 化の判断待ち、DR-0005「未確定」)。
# 参考: kawaz/bump-semver の justfile が canonical、
#       kawaz/hyoui が axum + workspace 分割の実例。

set shell := ["bash", "-euo", "pipefail", "-c"]

set script-interpreter := ["bash", "-euo", "pipefail"]

set positional-arguments

# default: list
default: list

# show recipes
list:
    @just --list --unsorted

# cargo fmt --check + clippy (-D warnings)
check:
    cargo fmt --check --all
    cargo clippy --workspace --all-targets -- -D warnings

# cargo fmt (書き換える)
fmt:
    cargo fmt --all

# カバレッジの下限。これを割ったら test が落ちる。
#
# 計測するだけでは劣化を止められない。上げる分には歓迎なので、
# 実績が安定して上回るようになったら引き上げる。
min_coverage := "85"

# cargo test (workspace 全体)
#
# カバレッジも一緒に出す。別 recipe に分けると計測しないまま日が経ち、
# 気づいたときには落ちている。
#
# just は直前コメントの最終行を --list の説明に使う。詳細を書くと説明が
# その最終行になってしまうので、出したい 1 行は [doc] で明示する。
[doc("cargo test (workspace 全体) + カバレッジ下限")]
test: check
    cargo llvm-cov --workspace --summary-only --fail-under-lines {{min_coverage}}

# テストだけ (計測のビルドを挟まないぶん速い)
test-only: check
    cargo test --workspace

# カバレッジの HTML を開く (どの行が通っていないか見る)
coverage-html:
    cargo llvm-cov --workspace --open

# release build (署名なし)
build:
    cargo build --release -p llm-gateway-cli

# release build + codesign
#
# 署名は配布のためではなく cache-warden の peer 認証に乗るため (DR-0002)。
# ad-hoc 署名では Team ID が付かず検証を通れないので、実 identity で署名する。
# identity は CODESIGN_IDENTITY で上書き可。
#
# 署名は手元でしかできない (証明書が keychain にある) ので、build とは
# 別 recipe にしてある。CI は build までしか走らせない。
[doc("release build + codesign")]
[script]
sign: build
    bin=target/release/llm-gateway
    identity="${CODESIGN_IDENTITY:-$(security find-identity -v -p codesigning | awk -F'"' '/Developer ID Application/ {print $2; exit}')}"
    if [ -z "$identity" ]; then
        echo >&2 "error: 'Developer ID Application' identity が keychain に見つかりません。CODESIGN_IDENTITY で指定してください"
        exit 1
    fi
    codesign --sign "$identity" --options runtime --force "$bin"
    codesign --verify --verbose "$bin"

# check + test + build (CI entry point)
#
# 署名は含めない。CI に証明書は無いし、配布物の署名は release workflow が
# 別途行う。手元で署名込みが欲しいときは just sign。
[doc("check + test + build (CI entry point)")]
ci: check test build

# ---------- 常駐 (launchd) ----------

# 常駐の識別子。plist 名・launchctl の対象・ログ先の全部がこれで決まる
label := "com.kawaz.llm-gateway"

# ビルドして launchd に登録する (既に居れば入れ替える)
[script]
install: sign
    label="{{label}}"
    plist="$HOME/Library/LaunchAgents/$label.plist"
    binary="$PWD/target/release/llm-gateway"
    config="${XDG_CONFIG_HOME:-$HOME/.config}/llm-gateway/config.toml"
    log_dir="${XDG_STATE_HOME:-$HOME/.local/state}/llm-gateway/logs"

    if [ ! -f "$config" ]; then
        echo >&2 "設定がありません: $config"
        echo >&2 "  just init-config で雛形を作れます"
        exit 1
    fi

    mkdir -p "$log_dir" "$(dirname "$plist")"
    sed -e "s|@LABEL@|$label|g" \
        -e "s|@BINARY@|$binary|g" \
        -e "s|@CONFIG@|$config|g" \
        -e "s|@LOG_DIR@|$log_dir|g" \
        dist/com.kawaz.llm-gateway.plist.in > "$plist"

    # 入れ替え時に古い定義が残らないよう、一度外してから入れる
    launchctl bootout "gui/$(id -u)/$label" 2>/dev/null || true
    launchctl bootstrap "gui/$(id -u)" "$plist"
    echo "登録しました: $label"
    echo "  設定  $config"
    echo "  ログ  $log_dir"

# launchd から外す (plist も消す)
[script]
uninstall:
    label="{{label}}"
    launchctl bootout "gui/$(id -u)/$label" 2>/dev/null || true
    rm -f "$HOME/Library/LaunchAgents/$label.plist"
    echo "解除しました: $label"

# 入れ替えずに再起動する (設定を読み直したいとき)
restart:
    launchctl kickstart -k "gui/$(id -u)/{{label}}"

# 常駐しているか、待ち受けているか
[script]
status:
    label="{{label}}"
    if launchctl print "gui/$(id -u)/$label" >/dev/null 2>&1; then
        launchctl print "gui/$(id -u)/$label" | grep -E '^\s*(state|pid|last exit code) ' || true
    else
        echo "登録されていません ($label)"
        exit 1
    fi
    listen=$(awk -F'"' '/^listen/ {print $2; exit}' "${XDG_CONFIG_HOME:-$HOME/.config}/llm-gateway/config.toml" 2>/dev/null)
    [ -n "$listen" ] && lsof -nP -iTCP@"${listen%:*}" -sTCP:LISTEN 2>/dev/null | grep ":${listen##*:} " || true

# ログを追う
[script]
logs *args:
    log_dir="${XDG_STATE_HOME:-$HOME/.local/state}/llm-gateway/logs"
    tail -F "$@" "$log_dir/stdout.log" "$log_dir/stderr.log"

# 設定の雛形を作る (既にあれば触らない)
[script]
init-config:
    config="${XDG_CONFIG_HOME:-$HOME/.config}/llm-gateway/config.toml"
    if [ -f "$config" ]; then
        echo "既にあります: $config"
        exit 0
    fi
    mkdir -p "$(dirname "$config")"
    cp dist/config.example.toml "$config"
    echo "作成しました: $config"
    echo "認証情報を ${XDG_STATE_HOME:-$HOME/.local/state}/llm-gateway/credentials/ に置いてください"

# uncommitted change がない状態か確認
[private]
ensure-clean:
    bump-semver vcs is clean

# 現在の bookmark/branch が default (= main) 上にあるか確認
[private]
[script]
check-on-default-branch:
    if ! bump-semver vcs is on-default-branch; then
        bn=$(bump-semver vcs get default-branch)
        printf >&2 "⚠ default branch (%s) に合流してから push してください\n  1. just sync         # %s@origin に rebase\n  2. just promote      # %s bookmark を current commit に forward\n  3. %s ワークスペースに移動して just push\n" "$bn" "$bn" "$bn" "$bn"
        exit 1
    fi

# crates/ の src が main@origin から変わっているのに Cargo.toml の version が
# 上がっていなければ fail。
#
# 対象を src/ に絞るのは、release 成果物に影響しない変更 (docs / justfile /
# dist の雛形) で version bump を強制しないため。
#
# bump-semver vcs diff の exit code:
#   0 = 対象 path に変更なし → bump 不要
#   1 = 変更あり → version bump 済みかチェックに進む
#   その他 = VCS error (main@origin 未 track 等)
[private]
[script]
check-version-bumped:
    rc=0
    bump-semver vcs diff -q main@origin -- \
      crates/llm-gateway/src/ crates/llm-gateway-server/src/ crates/llm-gateway-cli/src/ || rc=$?
    case "$rc" in
      0) exit 0 ;;
      1) ;;
      *) echo "ERROR: bump-semver vcs diff failed (rc=$rc). main@origin が track されていない可能性。先に 'jj git fetch' / 'git fetch' を試してください" >&2; exit 1 ;;
    esac
    bump-semver compare gt Cargo.toml vcs:main@origin:Cargo.toml --no-hint && exit 0
    echo 'ERROR: crates/ の src が変わっているが Cargo.toml version 未 bump。"just bump-version" を実行してください' >&2
    exit 1

# ---------- release flow ----------

# Cargo.toml workspace.package.version を bump (default: patch) し、
# workspace 内 path 依存の version 制約も同期して Release commit を作る。
[doc("version を上げて Release commit を作る")]
[script]
bump-version level="patch": ensure-clean
    new_version=$(bump-semver "$1" Cargo.toml --write --no-hint)
    # workspace 内 path 依存の version 制約も同期
    # (例: llm-gateway-cli が llm-gateway / llm-gateway-server を参照)。
    # `version = "X.Y"` 形式 (semver major.minor の short form) のみ更新する。
    # `version = "X.Y.Z"` 等の full pin や `>=X.Y` は変えない (= 意図ある制約は保持)。
    new_minor=$(printf '%s' "$new_version" | perl -ne 'print "$1.$2" if /^(\d+)\.(\d+)\.\d+/')
    for f in crates/*/Cargo.toml; do
      perl -i -pe 'BEGIN{$v=shift @ARGV} s/(path\s*=\s*"\.\.\/[^"]+"\s*,\s*version\s*=\s*")\d+\.\d+(")/$1$v$2/g' \
        "$new_minor" "$f"
    done
    # cargo check で Cargo.lock を再生成 (workspace 全体)
    cargo check --workspace --offline >/dev/null 2>&1 || cargo check --workspace >/dev/null
    # Cargo.toml + crates/*/Cargo.toml + Cargo.lock を一括 commit (--staged = 全 dirty)
    bump-semver vcs commit -m "Release v${new_version}" --staged
    echo "Version: -> ${new_version}"

# 現在の version を表示 (Cargo.toml workspace.package.version)
version:
    @bump-semver get Cargo.toml --no-hint

# 現在の worktree を default branch (= origin/<default>) に rebase
sync:
    bump-semver vcs sync --onto $(bump-semver vcs get default-branch)@origin

# default branch を現在の commit に forward (push しない)
promote:
    bump-semver vcs promote

# push (version が上がっていれば release workflow が tag と artifact を作る)
push: ci check-on-default-branch ensure-clean check-version-bumped
    bump-semver vcs push --branch main --jj-bookmark-auto-advance
