#!/usr/bin/env python3
"""Phrase and proximity queries for the Super User posts, sampled from the
posts themselves so each has matches. Writes queries.tsv:

    kind <TAB> tin query <TAB> Postgres tsquery ('simple' config)

Words are lower-case ASCII letters, 3+ long, so tin's analyzer (Unicode
words) and Postgres's 'simple' parser split them the same way; the two
engines should then agree on matches, apart from how they count positions
across punctuation (see README.md).

    python3 queries.py DOCS.txt > queries.tsv
"""
import random
import re
import sys

WORD = re.compile(r"[A-Za-z0-9_']+|[^\sA-Za-z0-9_']")
OK = re.compile(r"[a-z]{3,}$")
PER_KIND = 50

rng = random.Random(7)
docs = open(sys.argv[1], encoding="utf-8").read().splitlines()


def words(doc):
    # Punctuation tokens stay in the list so "adjacent" means adjacent in
    # the text, not across a full stop.
    return [w.lower() for w in WORD.findall(doc)]


def sample(n_words, max_gap):
    """n words from one post, each at most max_gap words after the last."""
    while True:
        ws = words(rng.choice(docs))
        if len(ws) < 20:
            continue
        i = rng.randrange(len(ws))
        picked = [i]
        for _ in range(n_words - 1):
            j = picked[-1] + 1 + rng.randrange(max_gap + 1)
            picked.append(j)
        if picked[-1] >= len(ws):
            continue
        span = ws[picked[0] : picked[-1] + 1]
        # All words of the span must be plain words (no punctuation between).
        if all(OK.match(w) for w in span) and len(set(ws[p] for p in picked)) == n_words:
            return [ws[p] for p in picked]


def near_tsq(a, b, gap, ordered):
    alts = [f"{a} <{k}> {b}" for k in range(1, gap + 2)]
    if not ordered:
        alts += [f"{b} <{k}> {a}" for k in range(1, gap + 2)]
    return " | ".join(f"({x})" for x in alts)


out = []
for _ in range(PER_KIND):
    a, b = sample(2, 0)
    out.append(("phrase2", f'"{a} {b}"', f"{a} <-> {b}"))
for _ in range(PER_KIND):
    a, b, c = sample(3, 0)
    out.append(("phrase3", f'"{a} {b} {c}"', f"{a} <-> {b} <-> {c}"))
for _ in range(PER_KIND):
    a, b = sample(2, 5)
    out.append(("near5", f"{a} NEAR/5 {b}", near_tsq(a, b, 5, False)))
for _ in range(PER_KIND):
    a, b = sample(2, 3)
    out.append(("then3", f"{a} THEN/3 {b}", near_tsq(a, b, 3, True)))
for _ in range(PER_KIND):
    a, b, c = sample(3, 0)
    out.append(("and3", f"{a} {b} {c}", f"{a} & {b} & {c}"))
for kind, tin, tsq in out:
    print(f"{kind}\t{tin}\t{tsq}")
