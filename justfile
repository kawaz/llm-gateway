# llm-gateway justfile
#
# 現在は設計 docs のみ。実装 (Rust) を入れる時点で以下を足す:
#   - ci (cargo fmt --check + clippy -D warnings + test)
#   - check-version-bumped (配布 artifact を持つなら)
#   - push の deps に ci を追加
# 参考: kawaz/bump-semver の justfile が canonical、
#       kawaz/cache-warden が Rust + 常駐 daemon の実例。
# 現時点の push gate は「default branch 上」「clean」の 2 つだけ。

set shell := ["bash", "-euo", "pipefail", "-c"]

set positional-arguments

# default: list
default: list

# show recipes
list:
    @just --list --unsorted

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
push: check-on-default-branch ensure-clean
    bump-semver vcs push --branch main --jj-bookmark-auto-advance
