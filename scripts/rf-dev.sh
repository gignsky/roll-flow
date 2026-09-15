#!/usr/bin/env bash
# Onboarding banner for the roll-flow dev shell.
#
# Printed by the flake's `shellHook` on entry, and re-printable on demand as
# `rf-dev` once it has scrolled away. The point it exists to make: this repo
# develops `rf` *using* `rf`, and the `rf` on PATH is the built flake package
# rather than your working tree.
#
# Content is deliberately ASCII-only. Line lengths are measured with `${#var}`,
# which counts bytes under a C locale, so a multi-byte character anywhere in a
# measured string would silently misalign the right-hand border. Only the box
# borders — which are never measured — use box-drawing characters.
set -euo pipefail

W=60 # inner width, between the side borders

if [ -t 1 ]; then
    bold=$'\033[1m'
    dim=$'\033[2m'
    cyan=$'\033[36m'
    off=$'\033[0m'
else
    bold='' dim='' cyan='' off=''
fi

# One boxed row. $1 is the plain text used for width; $2 is the styled text to
# actually print, defaulting to $1 when no styling is needed.
row() {
    local plain="$1" shown="${2:-$1}"
    # Guard the border: an over-long line would otherwise get negative padding,
    # which printf renders as no padding at all, silently blowing out the box.
    if [ ${#plain} -gt "$W" ]; then
        plain="${plain:0:$((W - 3))}..."
        shown="$plain"
    fi
    printf '%s│%s %s%*s %s│%s\n' \
        "$dim" "$off" "$shown" $((W - ${#plain})) '' "$dim" "$off"
}

rule() {
    local fill
    fill=$(printf '─%.0s' $(seq $((W + 2 - ${#1}))))
    printf '%s╭─ %s%s%s %s╮%s\n' "$dim" "$off$bold" "$1" "$off$dim" "${fill:3}" "$off"
}

# Branch names come from this repo's own roll-flow config so the banner stays
# truthful if the branch model is ever reconfigured.
root=$(git rev-parse --show-toplevel 2>/dev/null || true)
conf="$root/.roll-flow.toml"
# `|| true`: with `pipefail` a missing config would fail the pipeline, and the
# banner must still render outside a repo or before `rf init` has ever run.
read_conf() { sed -n "s/^$1 *= *\"\\(.*\\)\"/\\1/p" "$conf" 2>/dev/null | head -1 || true; }
rolling=$(read_conf rolling_branch); rolling=${rolling:-develop}
stable=$(read_conf stable_branch); stable=${stable:-main}

rule 'roll-flow dev shell'
row "Improve rf, using rf."
row ''

# Live state. Skipped entirely outside a work tree or on a detached HEAD, where
# there is no useful next step to name.
branch=$(git rev-parse --abbrev-ref HEAD 2>/dev/null || true)
if [ -n "$branch" ] && [ "$branch" != HEAD ]; then
    case "$branch" in
    roll/*) next="cargo run -- status -> rf verify -> rf graduate" ;;
    hotfix/*) next="cargo run -- status  ->  rf hotfix --land" ;;
    "$rolling") next="rf promote (maintainers)  |  rf create <slug>" ;;
    *) next="rf create <slug>" ;;
    esac
    # Keep long branch names from pushing the border out.
    short=$branch
    [ ${#short} -gt 44 ] && short="${short:0:41}..."
    row "you are on  $short" "you are on  $cyan$short$off"
    row "next        $next"
    row ''
fi

row "workflow    rf create <slug>     branch off $stable"
row "            <edit src/>"
row "            cargo run -- ...     exercise YOUR changes"
row "            rf verify            run the gates locally"
row "            rf graduate          -> $rolling, then PR it"
row ''
row "NOTE  'rf' on PATH is the built flake package, not" \
    "${bold}NOTE${off}  'rf' on PATH is the built flake package, not"
row "      your working tree. Test edits with cargo run --"
row ''
row "gates  fmt, clippy -D warnings, test  (.roll-flow.toml)"
row "bump   raise Cargo.toml version, or CI blocks the PR"
row "docs   CONTRIBUTING.md   -   rf-dev reprints this"
printf '%s╰%s╯%s\n' "$dim" "$(printf '─%.0s' $(seq $((W + 2))))" "$off"
