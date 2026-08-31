#!/bin/sh
# Which landed work has needed the most repair since a declared instant?
#
# Why this exists: a revert is the cheapest failure signal in a change history and this project has
# never produced one — 0 reverts in 266 commits at the time of writing. That is not a quality
# result, it is a policy: we fix forward. So the one signal that reads risk out of git alone reads
# nothing here, and "no reverts" gets mistaken for "nothing went wrong."
#
# The repairs are still in the history. A feature that was kept and fixed four times after it was
# supposed to be finished is telling you something a revert never got the chance to say, and it is
# telling you while the decision is still open rather than after somebody has already made it.
#
#   sh scripts/repair-density.sh                 # since the most recent tag
#   sh scripts/repair-density.sh v0.0.7          # since a tag
#   sh scripts/repair-density.sh 4070e17         # since any commit
#
# WHAT THIS DOES NOT DO, and why it matters more than what it does:
#
# It does not rank by severity. The obvious next step is to classify each repair — crash, data loss,
# security — and sort by the worst. That was built, measured, and thrown away: hardening and repair
# share a vocabulary, so a keyword classifier cannot tell "fixed a panic" from "added a panic
# guard", and on this repository three of its four top-ranked units were hardening commits wearing
# a repair's words. It ranked `security(supply-chain): secret scanning, dependabot` as two security
# repairs; it is the addition of security tooling. An automated severity column is not a weak
# signal here, it is a confident wrong one, and it points at whoever writes most carefully about
# hardening their own code.
#
# So this prints the counts, which are mechanical and correct, and then prints the repair SUBJECTS
# for the top units, which is the part a person has to read. Count and density rank the reading
# order; they do not rank the risk.
#
# The unit is the conventional-commit SCOPE, which is a proxy for a feature and not the thing
# itself — a scope splits one feature across directories and merges unrelated work that shares one.
# Commits with no scope cannot be attributed at all and are reported as an explicit blind spot
# rather than dropped, because a count that hides its uncounted population is not a measurement.
#
# Three outcomes stay distinguishable: a ranking, an honest refusal (exit 2), and an empty range
# that says so rather than printing a clean board.
set -u

INSTANT="${1:-}"

if ! git rev-parse --git-dir >/dev/null 2>&1; then
    echo "cannot measure: not inside a git repository" >&2
    exit 2
fi

if [ -z "$INSTANT" ]; then
    INSTANT=$(git describe --tags --abbrev=0 2>/dev/null || echo "")
    if [ -z "$INSTANT" ]; then
        echo "cannot measure: no tag to use as the instant, and none was given" >&2
        echo "  a repair count with no instant to count from is unbounded and not comparable;" >&2
        echo "  pass a tag or a commit:  sh scripts/repair-density.sh <tag|commit>" >&2
        exit 2
    fi
fi

if ! git rev-parse --verify --quiet "${INSTANT}^{commit}" >/dev/null 2>&1; then
    echo "cannot measure: '${INSTANT}' does not resolve to a commit in this repository" >&2
    exit 2
fi

AFTER=$(git rev-list --count "${INSTANT}..HEAD" 2>/dev/null || echo 0)
if [ "$AFTER" -eq 0 ]; then
    echo "nothing to measure: no commits between '${INSTANT}' and HEAD." >&2
    echo "  the range is empty, which is not the same as a clean board." >&2
    exit 2
fi

TOTAL=$(git rev-list --count HEAD)

# The incumbent signal, printed first so the reason this script exists is visible in its own output.
REVERTS=$(git log "${INSTANT}..HEAD" --pretty=%s | grep -c -Ei '^revert[: ]|^revert"' || true)

echo "instant   ${INSTANT}   ($(git log -1 --format=%ad --date=short "$INSTANT"))"
echo "range     ${AFTER} commit(s) after it, of ${TOTAL} in the history"
echo
echo "reverts   ${REVERTS}   <- the incumbent failure signal; this is why the rest of this exists"
echo

# One git call. \001 marks a subject line; everything after it until the next \001 is that commit's
# files. Parsing this in awk keeps the whole measurement to a single traversal.
git log "${INSTANT}..HEAD" --name-only --pretty=format:'%x01%s' | awk '
BEGIN { FS = "\n"; unscoped = 0; scoped = 0 }
/^\001/ {
    subject = substr($0, 2)
    scope = ""; type = ""
    if (match(subject, /^[a-z]+\([^)]+\):/)) {
        type  = substr(subject, 1, index(subject, "(") - 1)
        scope = substr(subject, index(subject, "(") + 1)
        scope = substr(scope, 1, index(scope, ")") - 1)
    }
    if (scope == "") { unscoped++; next }
    scoped++
    if (type == "feat")     landed[scope]++
    if (type == "fix")      { repairs[scope]++; subj[scope] = subj[scope] "\n      " subject }
    if (type == "security") { repairs[scope]++; subj[scope] = subj[scope] "\n      " subject }
    seen[scope] = 1
    next
}
NF && scope != "" { key = scope SUBSEP $0; if (!(key in filed)) { filed[key] = 1; surface[scope]++ } }
END {
    printf "  rep  surf   dens  unit\n"
    n = 0
    for (s in seen) if (repairs[s] > 0) {
        d = surface[s] > 0 ? repairs[s] / surface[s] : repairs[s]
        rows[++n] = sprintf("%5d %5d  %5.3f  %s", repairs[s], surface[s], d, s)
        rep[n] = repairs[s]; dns[n] = d; nm[n] = s
    }
    if (n == 0) { print "  (no repairs attributed in this range)" }
    # insertion sort: repairs desc, then density desc
    for (i = 2; i <= n; i++) {
        for (j = i; j > 1; j--) {
            a = j; b = j - 1
            if (rep[a] > rep[b] || (rep[a] == rep[b] && dns[a] > dns[b])) {
                t = rows[a]; rows[a] = rows[b]; rows[b] = t
                t = rep[a];  rep[a]  = rep[b];  rep[b]  = t
                t = dns[a];  dns[a]  = dns[b];  dns[b]  = t
                t = nm[a];   nm[a]   = nm[b];   nm[b]   = t
            } else break
        }
    }
    for (i = 1; i <= n && i <= 10; i++) print rows[i]
    print ""
    print "Read these, in this order. The counts rank the reading; they do not rank the risk."
    for (i = 1; i <= n && i <= 5; i++) printf "\n  %s%s\n", nm[i], subj[nm[i]]
    print ""
    printf "blind spot: %d of %d commit(s) in range carry no scope and are attributed to nothing.\n", unscoped, unscoped + scoped
    print "            a repair folded into an unrelated commit is invisible here by construction."
}'
