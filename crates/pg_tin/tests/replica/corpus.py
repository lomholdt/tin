#!/usr/bin/env python3
"""A synthetic stand-in for the stress corpus (no download): 50k short
"posts" of words drawn with a skewed distribution, including every word the
replica checks query. Deterministic. Writes CSV (id, body) to stdout."""
import csv
import random
import sys

rng = random.Random(42)
common = "the a and to of in is it for you on with this that i can not be".split()
topic = ("grub windows linux ssh vpn usb drive boot uefi bios excel mac network wifi ethernet "
         "router install installer installing firefox sql mysql postgresql disk file folder "
         "update driver keyboard mouse screen server client password account email").split()
words = common * 6 + topic + [f"w{i}" for i in range(2000)]
out = csv.writer(sys.stdout)
for i in range(1, 50001):
    n = rng.randint(8, 60)
    body = " ".join(rng.choice(words) if rng.random() < 0.7 else rng.choice(topic) for _ in range(n))
    if rng.random() < 0.05:
        body += " firefix" if rng.random() < 0.5 else " usb drive"
    out.writerow([i, body])
