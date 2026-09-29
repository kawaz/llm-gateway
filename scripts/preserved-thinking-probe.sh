#!/usr/bin/env bash
set -euo pipefail

usage() {
  printf 'Usage: %s BASE_URL NS [MODEL] [--beta] (--turn1-out FILE | --turn2-in FILE) [--edit-prefix]\n' "$0" >&2
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
turn1_out=
turn2_in=
while (($#)); do
  case $1 in
    --beta) beta=true; shift ;;
    --edit-prefix) edit_prefix=true; shift ;;
    --turn1-out|--turn2-in)
      (($# >= 2)) || usage
      if [[ $1 == --turn1-out ]]; then turn1_out=$2; else turn2_in=$2; fi
      shift 2 ;;
    *) usage ;;
  esac
done
[[ -n $turn1_out && -z $turn2_in || -z $turn1_out && -n $turn2_in ]] || usage
[[ $edit_prefix == false || -n $turn2_in ]] || usage
command -v jq >/dev/null && command -v curl >/dev/null || { printf 'curl and jq are required\n' >&2; exit 2; }

first='What is 7 plus 5? Answer with the number only.'
if [[ $edit_prefix == true ]]; then first='What is 7 plus 6? Answer with the number only.'; fi
if [[ -n $turn2_in ]]; then
  jq -e 'type == "array" and any(.[]; .type == "thinking" and (.signature | type == "string"))' "$turn2_in" >/dev/null || { printf 'Input must contain a signed thinking block\n' >&2; exit 2; }
  body=$(jq -n --arg model "$model" --arg first "$first" --slurpfile content "$turn2_in" '{model:$model,max_tokens:1024,thinking:{type:"adaptive"},output_config:{effort:"low"},messages:[{role:"user",content:$first},{role:"assistant",content:$content[0]},{role:"user",content:"What is 9 plus 4? Answer with the number only."}]}')
else
  body=$(jq -n --arg model "$model" --arg first "$first" '{model:$model,max_tokens:1024,thinking:{type:"adaptive"},output_config:{effort:"low"},messages:[{role:"user",content:$first}]}')
fi
response=$(mktemp)
trap 'rm -f "$response"' EXIT
printf 'Probe mode: %s; beta: %s; edited prefix: %s\n' "$(if [[ -n $turn1_out ]]; then printf turn1; else printf turn2; fi)" "$beta" "$edit_prefix"
headers=(-H 'content-type: application/json' -H 'anthropic-version: 2023-06-01')
if [[ $beta == true ]]; then headers+=(-H 'anthropic-beta: thinking-binding-controls-2026-08-01'); fi
status=$(curl -sS --max-time 120 -o "$response" -w '%{http_code}' -X POST "$base/$ns/v1/messages" "${headers[@]}" --data-binary "$body")
printf 'HTTP status: %s\n' "$status"
jq -c '{input_transformations: (.input_transformations // null), usage: (.usage // null), stop_reason: (.stop_reason // null), blocks: [.content[]? | {type, has_signature: (has("signature"))}], error: (.error // null)}' "$response"
if [[ -n $turn1_out && $status == 200 ]]; then
  jq -e '.content | type == "array" and any(.[]; .type == "thinking" and (.signature | type == "string"))' "$response" >/dev/null || { printf 'No signed thinking block; turn 1 not saved\n' >&2; exit 1; }
  jq '.content' "$response" > "$turn1_out"
fi
[[ $status == 200 ]] || exit 1
