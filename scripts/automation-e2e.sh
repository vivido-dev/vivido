#!/usr/bin/env bash
# Local Linux/macOS equivalent of .github/workflows/vivido-automation-e2e.yml.
#
# Builds vivido with the pinned Rust toolchain, runs one `vivido test` smoke
# plan per supported shell in an ephemeral headless session, exports a SARIF
# report (Linux, as in CI), and prints the JUnit summary. Reports and failure
# captures land in vivido/target/automation-e2e/. Shells that are not
# installed are skipped and listed; any failed shell fails the script.
#
# Usage: vivido/scripts/automation-e2e.sh [--install-deps] [--skip-build]
#   --install-deps  install the CI build inputs and shells
#                   (apt-get on Debian/Ubuntu with sudo, brew on macOS)
#   --skip-build    reuse the existing target/debug/vivido
set -euo pipefail

install_deps=0
skip_build=0
for arg in "$@"; do
  case "$arg" in
    --install-deps) install_deps=1 ;;
    --skip-build) skip_build=1 ;;
    -h|--help) sed -n '2,13p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown argument: $arg" >&2; exit 2 ;;
  esac
done

case "$(uname -s)" in
  Linux) os=ubuntu ;;
  Darwin) os=macos ;;
  *) echo "this script targets Linux or macOS; Windows CI runs the pwsh job" >&2; exit 2 ;;
esac

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$repo_root"

if (( install_deps )); then
  if [[ $os == ubuntu ]]; then
    sudo apt-get update
    sudo apt-get install -y build-essential cmake git pkg-config \
      libasound2-dev libfontconfig1-dev libfreetype6-dev \
      libwayland-dev libxkbcommon-dev \
      libavcodec-dev libavdevice-dev libavutil-dev libswscale-dev libswresample-dev \
      libvulkan1 mesa-vulkan-drivers \
      zsh fish
  else
    brew install ffmpeg pkgconf fish
  fi
fi

if (( ! skip_build )); then
  rust_toolchain="$(python3 -c 'import json; print(json.load(open("installer/release.json"))["rust_toolchain"])')"
  rustup toolchain install "$rust_toolchain" --profile minimal
  (cd vivido && cargo "+$rust_toolchain" build --locked --bin vivido)
fi

vivido="$repo_root/vivido/target/debug/vivido"
if [[ ! -x "$vivido" ]]; then
  echo "missing $vivido; run without --skip-build" >&2
  exit 1
fi

out="$repo_root/vivido/target/automation-e2e"
rm -rf "$out"
mkdir -p "$out"
plans="$repo_root/tests/e2e/plans"
run_id="$$-$(date +%s)"

failed=""
skipped=""
smoke() {
  local shell="$1" plan="$2"; shift 2
  if ! command -v "$1" >/dev/null 2>&1; then
    skipped="$skipped $shell"
    return
  fi
  echo "== smoke $shell ($plan)"
  "$vivido" test --session "local-$os-$shell-$run_id" \
    --file "$plans/$plan" \
    --report junit --output "$out/junit-$os-$shell.xml" \
    --artifacts-dir "$out/artifacts-$os-$shell" \
    -- "$@" || failed="$failed $shell"
}

smoke sh shell-smoke-posix.json sh
[[ $os == ubuntu ]] && smoke bash shell-smoke-posix.json bash
smoke zsh shell-smoke-posix.json zsh
smoke fish shell-smoke-posix.json fish
smoke pwsh shell-smoke-pwsh.json pwsh -NoLogo -NoProfile -NonInteractive

if [[ $os == ubuntu ]]; then
  echo "== SARIF report export"
  sarif="$out/sarif-$os-sh.sarif"
  if "$vivido" test --session "local-$os-sarif-$run_id" \
      --file "$plans/shell-smoke-posix.json" \
      --report sarif --output "$sarif" \
      --artifacts-dir "$out/artifacts-$os-sarif" \
      -- sh; then
    python3 -c 'import json,sys; log=json.load(open(sys.argv[1])); run=log["runs"][0]; assert log["version"]=="2.1.0", log; assert run["invocations"][0]["executionSuccessful"] is True, run; print("sarif ok:", len(run["results"]), "results")' "$sarif" \
      || failed="$failed sarif"
  else
    failed="$failed sarif"
  fi
fi

shopt -s nullglob
reports=("$out"/junit-"$os"-*.xml)
if (( ${#reports[@]} )); then
  python3 tests/e2e/junit_summary.py --no-check "${reports[@]}"
fi

echo "reports: $out"
[[ -n "$skipped" ]] && echo "skipped (not installed):$skipped"
if [[ -n "$failed" ]]; then
  echo "smoke failed on$failed" >&2
  exit 1
fi
echo "all smokes passed"
