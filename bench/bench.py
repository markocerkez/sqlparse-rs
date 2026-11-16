"""Benchmark original sqlparse vs the Rust-backed one.

usage: python3 -I bench.py [sql_dir]   (sql_dir: directory with *.sql sample files, e.g. sqlparse's tests/files)
"""
import glob
import random
import sys
import time

import sqlparse
from sqlparse import lexer

import sqlparse_rs

files = []
for d in sys.argv[1:]:
    for f in sorted(glob.glob(d + "/*.sql")):
        try:
            files.append(open(f, encoding="utf-8").read())
        except Exception:
            pass

rnd = random.Random(7)
tables = ["orders o", "customers c", "public.invoices i", '"Order Items" oi',
          "(SELECT id, SUM(total) AS t FROM payments GROUP BY id) p"]
cols = ["o.id", "c.name", "SUM(oi.qty * oi.price) AS revenue", "COALESCE(c.email, 'n/a')",
        "CASE WHEN o.status = 'x' THEN 1 ELSE 0 END AS flag", "i.total::numeric(10,2)"]
conds = ["o.created_at >= '2024-01-01'", "c.id IN (1,2,3,4)", "o.total BETWEEN 10 AND 100",
         "c.name LIKE 'A%' -- comment", "NOT EXISTS (SELECT 1 FROM refunds r WHERE r.order_id = o.id)"]


def q():
    return ("SELECT " + ", ".join(rnd.sample(cols, rnd.randint(2, 5))) + " FROM " + tables[0]
            + " JOIN " + tables[1] + " ON c.id = o.customer_id "
            + (" LEFT JOIN " + rnd.choice(tables[2:]) + " ON 1=1 " if rnd.random() < .6 else "")
            + " WHERE " + " AND ".join(rnd.sample(conds, rnd.randint(1, 4)))
            + " GROUP BY 1, 2 ORDER BY 1 DESC LIMIT 100;")


gen = [q() for _ in range(1500)]
script = "\n".join(gen)
big = "\n".join(files) or script
print("corpus: %d generated queries (%d KB), sample files %d KB\n" % (len(gen), len(script) // 1000, len(big) // 1000))


def best(fn, reps=5):
    b = 9e9
    for _ in range(reps):
        s = time.perf_counter()
        fn()
        b = min(b, time.perf_counter() - s)
    return b


ops = [
    ("tokenize (script)", lambda: sum(1 for _ in lexer.tokenize(script))),
    ("tokenize (sample files)", lambda: sum(1 for _ in lexer.tokenize(big))),
    ("split (script)", lambda: sqlparse.split(script)),
    ("split (sample files)", lambda: sqlparse.split(big)),
    ("split each query x1500", lambda: [sqlparse.split(x) for x in gen]),
    ("parse x500", lambda: [sqlparse.parse(x) for x in gen[:500]]),
    ("format(reindent) x500", lambda: [sqlparse.format(x, reindent=True, keyword_case="upper") for x in gen[:500]]),
]
res = {}
for mode in ("orig", "rust"):
    sqlparse_rs.unpatch() if mode == "orig" else sqlparse_rs.patch()
    res[mode] = [best(f) for _, f in ops]
print("%-26s %9s %9s %8s" % ("operation", "original", "rust", "speedup"))
for (name, _), a, b in zip(ops, res["orig"], res["rust"]):
    print("%-26s %8.1fms %8.1fms %7.1fx" % (name, a * 1e3, b * 1e3, a / b))

# batch API
sqlparse_rs.unpatch()
batch = gen * 4
a = best(lambda: [sqlparse.split(x) for x in batch], 3)
b = best(lambda: sqlparse_rs.split_many(batch), 3)
print("%-26s %8.1fms %8.1fms %7.1fx" % ("split_many x%d (parallel)" % len(batch), a * 1e3, b * 1e3, a / b))
