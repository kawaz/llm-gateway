# llm-gateway justfile
#
# 配布しない個人用ツール (DR-0002)。release workflow / version bump gate /
# 翻訳ペアは持たず、push = 完了として扱う。
# 参考: kawaz/bump-semver の justfile が canonical、
#       kawaz/hyoui が axum + workspace 分割の実例。

set shell := ["bash", "-euo", "pipefail", "-c"]

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

# cargo test (workspace 全体)
test: check
    cargo test --workspace

# release build + codesign
#
# 署名は配布のためではなく cache-warden の peer 認証に乗るため (DR-0002)。
# ad-hoc 署名では Team ID が付かず検証を通れないので、dev/release とも
# 実 identity で署名する。identity は CODESIGN_IDENTITY で上書き可。
[script]
build:
    cargo build --release -p llm-gateway-cli
    bin=target/release/llm-gateway
    identity="${CODESIGN_IDENTITY:-$(security find-identity -v -p codesigning | awk -F'"' '/Developer ID Application/ {print $2; exit}')}"
    if [ -z "$identity" ]; then
        echo >&2 "error: 'Developer ID Application' identity が keychain に見つかりません。CODESIGN_IDENTITY で指定してください"
        exit 1
    fi
    codesign --sign "$identity" --options runtime --force "$bin"
    codesign --verify --verbose "$bin"

# check + test + build (CI entry point)
ci: check test build

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

# 現在の worktree を default branch (= origin/<default>) に rebase
sync:
    bump-semver vcs sync --onto $(bump-semver vcs get default-branch)@origin

# default branch を現在の commit に forward (push しない)
promote:
    bump-semver vcs promote

# push (release artifact 無しなので push = 完了)
push: ci check-on-default-branch ensure-clean
    bump-semver vcs push --branch main --jj-bookmark-auto-advance
