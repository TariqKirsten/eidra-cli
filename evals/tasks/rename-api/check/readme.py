"""README.md shows the new name, and mentions the old one only where it says it is deprecated.

A mention counts as a deprecation note when its paragraph, or the heading of its section, says
"deprecated" (or "deprecation"): a note may wrap, or sit under a "Deprecated names" heading.
"""

import re
import sys

text = open("README.md", encoding="utf-8").read()
if "distance(" not in text:
    sys.exit("README.md never shows distance(...)")

heading, paragraph, fenced, stray = "", [], False, []


def close():
    block = "\n".join(paragraph)
    if "calc_dist" in block and not re.search("deprecat", heading + "\n" + block, re.I):
        stray.append(block)
    paragraph.clear()


for line in text.splitlines():
    if line.lstrip().startswith("```"):
        fenced = not fenced
    elif not fenced and line.startswith("#"):
        close()
        heading = line
        continue
    if not line.strip():
        close()
    else:
        paragraph.append(line)
close()

if stray:
    sys.exit("calc_dist is mentioned outside a deprecation note:\n\n" + "\n\n".join(stray))
