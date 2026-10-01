#!/usr/bin/env bash
set -euo pipefail

usage() {
  printf 'Usage: %s BASE_URL NS [MODEL] [--beta] (--turn1-out FILE | --turn2-in FILE) [--edit-prefix] [--turn2-thinking keep|text|drop] [--effort LEVEL]\n' "$0" >&2
  exit 2
}

(($# >= 3)) || usage
base=${1%/}
ns=$2
shift 2
model=claude-sonnet-5-5
if [[ ${1:-} != --* ]]; then model=$1; shift; fi
beta=false
edit_prefix=false
turn2_thinking=keep
effort=high
turn1_out=
turn2_in=
while (($#)); do
  case $1 in
    --beta) beta=true; shift ;;
    --edit-prefix) edit_prefix=true; shift ;;
    --turn2-thinking)
      (($# >= 2)) && [[ $2 == keep || $2 == text || $2 == drop ]] || usage
      turn2_thinking=$2; shift 2 ;;
    --effort) (($# >= 2)) || usage; effort=$2; shift 2 ;;
    --turn1-out|--turn2-in)
      (($# >= 2)) || usage
      if [[ $1 == --turn1-out ]]; then turn1_out=$2; else turn2_in=$2; fi
      shift 2 ;;
    *) usage ;;
  esac
done
[[ -n $turn1_out && -z $turn2_in || -z $turn1_out && -n $turn2_in ]] || usage
[[ $edit_prefix == false || -n $turn2_in ]] || usage
[[ $turn2_thinking == keep || -n $turn2_in ]] || usage
command -v jq >/dev/null && command -v curl >/dev/null && command -v claude >/dev/null || { printf 'curl, jq and claude are required\n' >&2; exit 2; }

version=$(claude --version | cut -d ' ' -f 1)
system=$(jq -n --arg version "$version" --arg introduction "You are Claude Code, Anthropic's official CLI for Claude." '[{"type":"text","text":("x-anthropic-billing-header: cc_version=" + $version + "; cc_entrypoint=cli;")},{"type":"text","text":$introduction}]')
first='A three-digit number has digits summing to 12. The tens digit is twice the hundreds digit. Reversing the digits makes a number 396 greater than the original. Find the number and briefly explain the constraints.'
if [[ $edit_prefix == true ]]; then first='A three-digit number has digits summing to 13. The tens digit is twice the hundreds digit. Reversing the digits makes a number 396 greater than the original. Find the number and briefly explain the constraints.'; fi
second='What is the sum of the original number and its reversed number? Explain briefly.'
if [[ -n $turn2_in ]]; then
  jq -e 'type == "array" and any(.[]; .type == "thinking" and (.signature | type == "string"))' "$turn2_in" >/dev/null || { printf 'Input must contain a signed thinking block\n' >&2; exit 2; }
  # text: thinking block を本文 (末尾の改行と半角空白を除去) の text block に置換 (DR-0033 の thinking_as_text と同じ形)。drop: thinking block を削除
  content=$(jq -c --arg mode "$turn2_thinking" 'if $mode == "text" then map(if .type == "thinking" then {type:"text",text:(.thinking | sub("[\n ]+$";""))} else . end) elif $mode == "drop" then map(select(.type != "thinking")) else . end' "$turn2_in")
  body=$(jq -n --arg effort "$effort" --arg model "$model" --arg first "$first" --arg second "$second" --argjson system "$system" --argjson content "$content" '{model:$model,max_tokens:1024,thinking:{type:"adaptive"},output_config:{effort:$effort},system:$system,metadata:{user_id:"preserved-thinking-probe"},messages:[{role:"user",content:$first},{role:"assistant",content:$content},{role:"user",content:$second}]}')
else
  body=$(jq -n --arg effort "$effort" --arg model "$model" --arg first "$first" --arg second "$second" --argjson system "$system" '{model:$model,max_tokens:1024,thinking:{type:"adaptive"},output_config:{effort:$effort},system:$system,metadata:{user_id:"preserved-thinking-probe"},messages:[{role:"user",content:$first}]}')
fi
response=$(mktemp)
trap 'rm -f "$response"' EXIT
printf 'Probe mode: %s; model: %s; beta: %s; edited prefix: %s; turn2 thinking: %s; effort: %s\n' "$(if [[ -n $turn1_out ]]; then printf turn1; else printf turn2; fi)" "$model" "$beta" "$edit_prefix" "$turn2_thinking" "$effort"
headers=(-H 'content-type: application/json' -H 'anthropic-version: 2023-06-01' -H "User-Agent: claude-cli/$version" -H 'x-app: cli')
# jwt の ns には PT_TOKEN (llm-gateway auth sign の出力) を Bearer で渡す
[[ -n ${PT_TOKEN:-} ]] && headers+=(-H "Authorization: Bearer $PT_TOKEN")
beta_flags='oauth-2025-04-20,claude-code-20250219'
if [[ $beta == true ]]; then beta_flags+=',thinking-binding-controls-2026-08-01'; fi
headers+=(-H "anthropic-beta: $beta_flags")
status=$(curl -sS --max-time 120 -o "$response" -w '%{http_code}' -X POST "$base/$ns/v1/messages" "${headers[@]}" --data-binary "$body")
printf 'HTTP status: %s\n' "$status"
jq -c '{input_transformations: (.input_transformations // null), usage: (.usage // null), stop_reason: (.stop_reason // null), stop_details: (.stop_details // null), blocks: [.content[]? | {type, has_signature: (has("signature"))}], error: (.error // null), text_head: ([.content[]? | select(.type == "text") | .text][0] // null | if . then .[0:60] else null end)}' "$response"
if [[ -n $turn1_out && $status == 200 ]]; then
  jq -e '.content | type == "array" and any(.[]; .type == "thinking" and (.signature | type == "string"))' "$response" >/dev/null || { printf 'No signed thinking block; turn 1 not saved\n' >&2; exit 1; }
  jq '.content' "$response" > "$turn1_out"
fi
[[ $status == 200 ]] || exit 1
