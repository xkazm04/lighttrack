#!/bin/sh
# Scope a documentation catch-up pass: how big is anchor..now, and how long is the skip list?
#
# The point of a marker is that starting a batch documentation repair stops being a decision somebody
# has to work up to and becomes a mechanical question with a computable answer. This prints that
# answer, per surface family:
#
#   sh scripts/docs-catchup.sh            # every family
#   sh scripts/docs-catchup.sh docs       # one family (docs | agent | clients)
#
# It REFUSES loudly on a missing or unparseable marker rather than defaulting to either extreme — a
# full rewrite (an expensive surprise) or an empty range (a silent no-op). "Cannot determine range"
# is the honest answer and it exits non-zero.
#
# This script only measures. The pass itself is a human or an agent reading the changed source and
# rewriting what drifted; its FINAL act is updating the marker — anchor, covered, skipped, flagged,
# and a baseline note that says what was done, in the same commit as the repairs. A repair committed
# without its marker update recreates exactly the ambiguity the marker exists to remove.
#
# crates/core/tests/catchup_marker_guard.rs enforces the markers' shape on every `cargo test
# --workspace`, including that every file under a family's surfaces is in exactly one of
# covered/skipped. So a doc added tomorrow cannot quietly join no list.
set -u

MARKERS="docs:docs/catchup-marker.json agent:.ai/catchup-marker.json clients:clients/catchup-marker.json"
WANT="${1:-}"

# The parse is deliberately delegated: this needs a real JSON reader, and every machine that can run
# the suite has python3. A shell-side regex parse of JSON is how a "safety" script starts lying.
if ! command -v python3 >/dev/null 2>&1 && ! command -v python >/dev/null 2>&1; then
    echo "cannot determine range: no python available to read the markers" >&2
    exit 2
fi
PY=$(command -v python3 || command -v python)

status=0
for entry in $MARKERS; do
    name=${entry%%:*}
    file=${entry#*:}
    [ -n "$WANT" ] && [ "$WANT" != "$name" ] && continue

    if [ ! -f "$file" ]; then
        echo "CANNOT DETERMINE RANGE for family '$name': $file is missing." >&2
        echo "  A pass with no marker either re-does everything or under-does the tail. Restore the" >&2
        echo "  marker from git history, or write a new one whose anchor is the commit you are" >&2
        echo "  measuring from — and say in its baseline note that the range is a guess." >&2
        status=2
        continue
    fi

    anchor=$("$PY" -c "
import json,sys
try:
    d=json.load(open(sys.argv[1],encoding='utf-8'))
except Exception as e:
    sys.exit('PARSE:'+str(e))
print(d['anchor']['commit'])
print(d['anchor']['date'])
print(len(d.get('covered',[])))
print(sum(1 for s in d.get('skipped',[]) if s.get('kind')=='not-in-this-pass'))
print(sum(1 for s in d.get('skipped',[]) if s.get('kind')=='frozen-archive'))
print(len(d.get('flagged',[])))
for r in d.get('surfaces',{}).get('roots',[]): print('PATH '+r['path'])
for f in d.get('surfaces',{}).get('files',[]): print('PATH '+f)
" "$file" 2>&1)
    case "$anchor" in
        PARSE:*)
            echo "CANNOT DETERMINE RANGE for family '$name': $file does not parse." >&2
            echo "  ${anchor#PARSE:}" >&2
            status=2
            continue
            ;;
    esac

    commit=$(echo "$anchor" | sed -n 1p)
    date=$(echo "$anchor" | sed -n 2p)
    covered=$(echo "$anchor" | sed -n 3p)
    owed=$(echo "$anchor" | sed -n 4p)
    frozen=$(echo "$anchor" | sed -n 5p)
    flagged=$(echo "$anchor" | sed -n 6p)
    paths=$(echo "$anchor" | sed -n 's/^PATH //p' | tr '\n' ' ')

    echo "── $name ────────────────────────────────────────────────"
    echo "   anchor      $commit ($date)"

    if ! git cat-file -e "${commit}^{commit}" 2>/dev/null; then
        echo "   range       CANNOT DETERMINE — the anchor commit is not in this clone." >&2
        echo "               (a shallow clone? a rewritten history? fetch it before scoping)" >&2
        status=2
    else
        # The two numbers a pass is scoped by: how much history has gone by, and how much of THIS
        # family's surface it touched. The second is the one that decides whether to bother.
        commits=$(git rev-list --count "${commit}..HEAD" 2>/dev/null || echo "?")
        # shellcheck disable=SC2086
        touched=$(git diff --name-only "${commit}..HEAD" -- $paths 2>/dev/null | wc -l | tr -d ' ')
        echo "   range       ${commits} commit(s) since the anchor; ${touched} file(s) in this family changed"
    fi

    echo "   covered     ${covered} surface(s) at the anchor"
    echo "   owed        ${owed} surface(s) the last pass did not reach  <- add to the next pass's scope"
    echo "   frozen      ${frozen} archive(s), permanently out of scope"
    echo "   flagged     ${flagged} cross-boundary obligation(s) queued in the marker"
    echo
done

if [ "$status" -ne 0 ]; then
    echo "one or more families could not be scoped; see above." >&2
    exit "$status"
fi
echo "Scope of the next pass, per family: (files changed in anchor..HEAD) + (the 'owed' list)."
echo "The pass's final act is updating the marker, in the same commit as the repairs."
