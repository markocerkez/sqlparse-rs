import sqlparse
from sqlparse import lexer
from sqlparse import tokens as T

import sqlparse_rs as R


def toks(sql):
    return list(R._fast_get_tokens(lexer.Lexer.get_default_instance(), sql))


def test_split_basic():
    assert R.split("select 1; select 2;") == sqlparse.split("select 1; select 2;") == ["select 1;", "select 2;"]


def test_split_strip_semicolon():
    assert R.split("select 1;  ;", strip_semicolon=True) == sqlparse.split("select 1;  ;", strip_semicolon=True)


def test_token_types_are_the_original_objects():
    out = toks("select a.b, 1.5, 'x' from t -- c")
    assert out[0][0] is T.Keyword.DML
    assert any(t is T.Number.Float and v == "1.5" for t, v in out)
    assert out[-1] == (T.Comment.Single, "-- c")


def test_dollar_quote_and_comments():
    sql = "select $$a;b$$; /* x; */ select 2"
    assert R.split(sql) == sqlparse.split(sql)
    assert toks(sql) == list(R._orig_get_tokens(lexer.Lexer.get_default_instance(), sql))


def test_begin_end_block():
    sql = "create procedure p() begin select 1; select 2; end; select 3;"
    assert R.split(sql) == sqlparse.split(sql)


def test_odd_number_backtracking():
    for sql in ["12abc", "1.5e", "1.5ex", ".5x", "-3a", "1.x"]:
        assert toks(sql) == list(R._orig_get_tokens(lexer.Lexer.get_default_instance(), sql)), sql


def test_non_str_falls_back():
    assert R.split(b"select 1; select 2;") == sqlparse.split(b"select 1; select 2;")
    assert R.split("select '\ud800'; select 2") == sqlparse.split("select '\ud800'; select 2")


def test_patch_and_unpatch():
    R.patch()
    try:
        assert R.is_patched()
        assert sqlparse.format("select a from b where c = 1", reindent=True) == \
            "select a\nfrom b\nwhere c = 1"
        assert len(sqlparse.parse("select 1; select 2")) == 2
    finally:
        R.unpatch()
    assert not R.is_patched()


def test_custom_keywords_are_respected():
    inst = lexer.Lexer.get_default_instance()
    R.patch()
    try:
        inst.add_keywords({"FROBNICATE": T.Keyword})
        out = list(lexer.tokenize("frobnicate x"))
        assert out[0] == (T.Keyword, "frobnicate")
    finally:
        inst._keywords.pop()
        R.unpatch()
    assert list(lexer.tokenize("frobnicate x"))[0][0] is T.Name


def test_split_many():
    batch = ["select 1; select 2", "", "select 'a;b'; x", b"select 1; select 2"]
    assert R.split_many(batch) == [sqlparse.split(s) for s in batch]
