"""Packs one ITF trace for the corpus: drops what says when it was made
(`#meta`'s description and timestamp), so that the same model and seed
give the same bytes, and gzips it without a name or time in the header.

    python3 -I tools/itf-pack.py <in.itf.json> <out.itf.json.gz>
"""
import gzip
import json
import sys

src, dest = sys.argv[1], sys.argv[2]
with open(src) as f:
    trace = json.load(f)
meta = trace.get('#meta', {})
meta.pop('description', None)
meta.pop('timestamp', None)
data = json.dumps(trace, separators=(',', ':')).encode()
with open(dest, 'wb') as f:
    with gzip.GzipFile(filename='', mode='wb', fileobj=f, compresslevel=9, mtime=0) as z:
        z.write(data)
