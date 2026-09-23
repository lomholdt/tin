#!/usr/bin/env python3
"""Synthetic shipping identifiers + a search-box query set (stdlib only, seeded).

    python3 bench/ids/generate.py OUT_DIR [ROWS]

Writes
  OUT_DIR/shipments.csv  id,equipment_no,booking_no,bl_no   (one row per container)
  OUT_DIR/queries.tsv    kind<TAB>query<TAB>target_id        (target = the row it came from)

* Equipment numbers follow ISO 6346: 3-letter owner code + category 'U' + 6-digit
  serial + check digit (e.g. MSKU1234565), owner codes skewed like a real fleet.
* Bookings: 9 digits, 1-4 containers each. Bills of lading: carrier SCAC + 9 digits,
  one per booking.
"""

import random
import sys

SEED = 20260923

OWNERS = [  # (owner code, weight)
    ("MSK", 18), ("MRK", 8), ("MAE", 6), ("MRS", 4), ("MSA", 3), ("MNB", 2), ("SEA", 2),
    ("MSC", 10), ("MED", 4), ("CMA", 7), ("CGM", 3), ("HLX", 6), ("HLB", 3), ("ONE", 5),
    ("OOL", 4), ("EGH", 3), ("EIS", 2), ("COS", 4), ("CSN", 3), ("YML", 2), ("HMM", 2),
    ("ZIM", 2), ("TGH", 5), ("TCN", 4), ("TRH", 3), ("TEM", 3), ("TLL", 2), ("TCL", 2),
    ("BEA", 2), ("CAI", 3), ("SEG", 3), ("GES", 2), ("FCI", 2), ("DRY", 1), ("UET", 1),
    ("APZ", 1), ("APH", 1), ("CXD", 1), ("KKF", 1), ("NYK", 1),
]
CARRIERS = [("MAEU", 45), ("MSCU", 15), ("CMDU", 10), ("HLCU", 10), ("ONEY", 8), ("COSU", 7), ("EGLV", 5)]


def letter_value(c: str) -> int:
    # A=10, then skip multiples of 11: B=12 ... K=21, L=23 ... U=32, V=34 ...
    v = 10
    for x in range(ord("A"), ord(c)):
        v += 1
        if v % 11 == 0:
            v += 1
    return v


LETTERS = {chr(c): letter_value(chr(c)) for c in range(ord("A"), ord("Z") + 1)}


def check_digit(prefix10: str) -> int:
    total = 0
    for i, ch in enumerate(prefix10):
        total += (LETTERS[ch] if ch.isalpha() else int(ch)) << i
    return total % 11 % 10


def typo(s: str, rng: random.Random) -> str:
    """One edit a human makes: wrong digit, swapped neighbours, or a dropped char."""
    i = rng.randrange(4, len(s) - 1) if len(s) > 5 else rng.randrange(len(s) - 1)
    kind = rng.random()
    if kind < 0.5:
        d = rng.choice([c for c in "0123456789" if c != s[i]])
        return s[:i] + d + s[i + 1:]
    if kind < 0.8 and s[i] != s[i + 1]:
        return s[:i] + s[i + 1] + s[i] + s[i + 2:]
    return s[:i] + s[i + 1:]


def main(out: str, rows: int) -> None:
    rng = random.Random(SEED)
    owners, weights = zip(*OWNERS)
    carriers, cweights = zip(*CARRIERS)

    seen = set()
    bookings_seen = set()
    bls_seen = set()
    data = []
    booking = bl = None
    left_in_booking = 0
    while len(data) < rows:
        if left_in_booking == 0:
            left_in_booking = rng.choices([1, 2, 3, 4], weights=[50, 25, 15, 10])[0]
            while True:
                booking = str(rng.randrange(200_000_000, 1_000_000_000))
                if booking not in bookings_seen:
                    bookings_seen.add(booking)
                    break
            while True:
                bl = rng.choices(carriers, cweights)[0] + str(rng.randrange(100_000_000, 1_000_000_000))
                if bl not in bls_seen:
                    bls_seen.add(bl)
                    break
        owner = rng.choices(owners, weights)[0]
        serial = rng.randrange(1_000_000)
        key = (owner, serial)
        if key in seen:
            continue
        seen.add(key)
        prefix = f"{owner}U{serial:06d}"
        data.append((prefix + str(check_digit(prefix)), booking, bl))
        left_in_booking -= 1

    with open(f"{out}/shipments.csv", "w") as f:
        for i, (e, b, l) in enumerate(data, 1):
            f.write(f"{i},{e},{b},{l}\n")

    # Query set: 1000 per kind, each generated from one target row.
    qrng = random.Random(SEED + 1)
    targets = [qrng.randrange(len(data)) for _ in range(1000)]
    kinds = []
    for t in targets:
        e, b, l = data[t]
        tid = t + 1
        kinds += [
            ("exact_equipment", e, tid),
            ("exact_equipment_lower", e.lower(), tid),
            ("exact_booking", b, tid),
            ("exact_bl", l, tid),
            ("prefix_equipment", e[: qrng.randint(4, 10)], tid),
            ("prefix_bl", l[: qrng.randint(6, 12)], tid),
            ("equipment_digits", e[4:], tid),          # 7 digits, no owner code
            ("equipment_suffix", e[-5:], tid),          # last 5 digits
            ("bl_digits", l[4:], tid),                  # BL number without the SCAC
            ("typo_equipment", typo(e, qrng), tid),
        ]
    with open(f"{out}/queries.tsv", "w") as f:
        for kind, q, tid in kinds:
            f.write(f"{kind}\t{q}\t{tid}\n")
    print(f"{len(data):,} rows, {len(bookings_seen):,} bookings, {len(kinds):,} queries -> {out}", file=sys.stderr)


if __name__ == "__main__":
    if len(sys.argv) < 2:
        sys.exit(__doc__)
    main(sys.argv[1], int(sys.argv[2]) if len(sys.argv) > 2 else 5_000_000)
