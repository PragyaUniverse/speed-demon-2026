#!/usr/bin/env python3
"""Extract each spec's worked example into tools/cases/<track>/ (input files plus expected output).

    python3 tools/make_cases.py <path to speed-demon-2026 clone>
"""
import os
import re
import sys

spec_dir = os.path.join(sys.argv[1], "specs")
here = os.path.dirname(os.path.abspath(__file__))


def fences(name):
    text = open(os.path.join(spec_dir, name), encoding="utf-8").read()
    text = text[text.index("orked example"):]
    return re.findall(r"```[a-z]*\n(.*?)```", text, re.S)


def write(track, files):
    d = os.path.join(here, "cases", track)
    os.makedirs(d, exist_ok=True)
    for name, data in files.items():
        with open(os.path.join(d, name), "wb") as f:
            f.write(data)


a = fences("a-ingest.md")
write("a", {"requests.log": a[0].encode(), "expected": a[1].encode()})

b = fences("b-index.md")
docs = b[0].replace("<TAB>", "\t").encode()
write("b", {"docs.txt": docs, "queries.txt": b[1].encode(), "expected": b[2].encode()})


def csv_bytes(s):
    # The spec writes these files in the site's notation; each visible line is one physical line.
    s = s.replace("\n", "").replace("⟨BOM⟩", "﻿").replace("␍␊", "\r\n").replace("␊", "\n")
    return s.encode()


c = fences("c-join.md")
write("c", {"records.csv": csv_bytes(c[0]), "regions.csv": csv_bytes(c[1]), "expected": c[2].encode()})

d = fences("d-traverse.md")
write("d", {"graph.txt": d[0].encode(), "queries.txt": d[1].encode(), "expected": d[2].encode()})
