"""Differential test: Rust lexer/splitter vs. the original sqlparse implementation.

usage: python3 -I diff_test.py [sql_dir ...] [--n-fuzz N] [--seed S]
Token streams must match exactly (same token-type object, same value); split() must match for
strip_semicolon False/True.
"""
import glob
import os
import random
import sys
import time

import sqlparse
from sqlparse import lexer as L
import sqlparse_rs as R

inst = L.Lexer.get_default_instance()

FRAGS = """select SELECT insert into values update delete from where and or not null is in like ilike rlike
regexp binary group by order asc desc nulls first last union all create or replace table view index
begin end declare if else elsif then loop while for do case when go join left right full inner outer
cross natural straight lateral explode as with time zone at double precision primary key exists handler
transaction work tran distributed deferred immediate exclusive commit rollback procedure function
trigger returns return $$ $a$ $tag$ $1 :name ? %s %(name)s @var #tmp ##tmp \\x \\copy
; ; ; , . .. ( ) [ ] [a b] [x][y] {} 1 12 1.5 .5 1. 1e5 1.5E-3 0x1F -0xAB -5 -1.2 3abc 1.5e 2_x
'str' 'it''s' 'a\\'b' "id" "a""b" `bt` `a``b` ´ac´ ´a´´b´ /* c */ /*+ hint */ /* unterminated
-- comment\n --+ hint\n # c\n #+ h\n # x -> ->> #> #>> @> <@ #- ?| ?& = == != <> <= >= ~ !~ + - * / % ^ & |
:= :: \n \r\n \r \t       \x1c \x0b é À Ü × ÷ ı İ ſ K
١ 中 ᵽ x y z a b c t1 t2 tbl col """.replace("\\n", "\n").replace("\\r", "\r").replace("\\t", "\t")
for esc in ("\\u00a0", "\\u2028", "\\x1c", "\\x0b", "\\u00e9", "\\u00c0", "\\u00dc", "\\u00d7", "\\u00f7",
            "\\u0131", "\\u0130", "\\u017f", "\\u212a", "\\u0661", "\\u4e2d", "\\u1d7d"):
    FRAGS = FRAGS.replace(esc, esc.encode().decode("unicode_escape"))
FRAGS = FRAGS.split(" ") + ["\n", "\r\n", "\r", "\t", "  "]
FRAGS = [f for f in FRAGS if f != ""]

ALPHABET = list("abcdeSELCTFROMWHEIN0123456789 \t\n\r;,.()[]'\"`$#@:?%\\/*+-=<>!~^&|_{}x") + [
    "é", "À", "Ü", "×", "ı", "İ", "ſ", "K", "٣", "中", " ", " ", "´", "\x1c", "\x85"]


def gen(rng):
    kind = rng.random()
    if kind < 0.45:
        n = rng.randint(1, 40)
        return "".join(rng.choice(FRAGS) + rng.choice(["", " ", " ", "\n"]) for _ in range(n))
    if kind < 0.8:
        return "".join(rng.choice(ALPHABET) for _ in range(rng.randint(0, 60)))
    base = "".join(rng.choice(FRAGS) + " " for _ in range(rng.randint(1, 15)))
    # mutate: delete / duplicate / insert chars
    s = list(base)
    for _ in range(rng.randint(0, 5)):
        if not s:
            break
        i = rng.randrange(len(s))
        op = rng.random()
        if op < 0.33:
            del s[i]
        elif op < 0.66:
            s.insert(i, rng.choice(ALPHABET))
        else:
            s.insert(i, s[i])
    return "".join(s)


def cmp(text, failures, stats):
    stats["n"] += 1
    exp = list(R._orig_get_tokens(inst, text))
    got = list(R._fast_get_tokens(inst, text))
    ok = len(exp) == len(got) and all(a[0] is b[0] and a[1] == b[1] for a, b in zip(exp, got))
    if not ok:
        failures.append(("tokens", text, exp, got))
        return
    for strip in (False, True):
        e = R._orig_split(text, None, strip)
        g = R.split(text, None, strip)
        if e != g:
            failures.append(("split", text, e, g, strip))
            return


def main():
    args = sys.argv[1:]
    n_fuzz, seed, dirs = 200000, 1, []
    i = 0
    while i < len(args):
        if args[i] == "--n-fuzz":
            n_fuzz = int(args[i + 1]); i += 2
        elif args[i] == "--seed":
            seed = int(args[i + 1]); i += 2
        else:
            dirs.append(args[i]); i += 1
    failures, stats = [], {"n": 0}
    t0 = time.time()
    for d in dirs:
        for f in sorted(glob.glob(os.path.join(d, "**", "*.sql"), recursive=True)):
            with open(f, encoding="utf-8", errors="replace") as fh:
                text = fh.read()
            cmp(text, failures, stats)
            for line in text.splitlines():  # also every prefix of lines
                cmp(line, failures, stats)
    rng = random.Random(seed)
    for _ in range(n_fuzz):
        cmp(gen(rng), failures, stats)
    print("cases: %d  failures: %d  (%.1fs)" % (stats["n"], len(failures), time.time() - t0))
    for fl in failures[:5]:
        print("----", fl[0], repr(fl[1]))
        print(" expected:", fl[2][:12] if fl[0] == "tokens" else fl[2])
        print(" got:     ", fl[3][:12] if fl[0] == "tokens" else fl[3])
    sys.exit(1 if failures else 0)


if __name__ == "__main__":
    main()
