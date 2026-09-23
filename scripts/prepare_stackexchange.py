#!/usr/bin/env python3
"""Convert a Stack Exchange `Posts.xml` into a plain corpus: one document per
line (title + body, HTML stripped, whitespace collapsed).

Standard library only. Each <row .../> in the dump sits on its own line, so a
line-by-line regex pass is much faster than a full XML parse.

    python3 scripts/prepare_stackexchange.py data/Posts.xml data/superuser.docs.txt
"""

import html
import re
import sys

ROW = re.compile(r'<row\s')
ATTR = {name: re.compile(rf'\s{name}="([^"]*)"') for name in ("Title", "Body")}
TAG = re.compile(r"<[^>]+>")
SPACE = re.compile(r"\s+")


def text_of(line: str) -> str:
    parts = []
    for name in ("Title", "Body"):
        m = ATTR[name].search(line)
        if m:
            # Attribute values are XML-escaped HTML: unescape once for the XML,
            # strip tags, unescape again for HTML entities inside the body.
            markup = html.unescape(m.group(1))
            parts.append(html.unescape(TAG.sub(" ", markup)))
    return SPACE.sub(" ", " ".join(parts)).strip()


def main(src: str, dst: str) -> None:
    n = 0
    total = 0
    with open(src, encoding="utf-8") as fin, open(dst, "w", encoding="utf-8") as fout:
        for line in fin:
            if not ROW.search(line):
                continue
            doc = text_of(line)
            if not doc:
                continue
            fout.write(doc)
            fout.write("\n")
            n += 1
            total += len(doc)
            if n % 200_000 == 0:
                print(f"{n:,} docs", file=sys.stderr)
    print(f"{n:,} docs, {total / 1e9:.2f} GB of text -> {dst}", file=sys.stderr)


if __name__ == "__main__":
    if len(sys.argv) != 3:
        sys.exit(__doc__)
    main(sys.argv[1], sys.argv[2])
