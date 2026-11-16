"""Rust-accelerated lexer and statement splitter for sqlparse.

Usage::

    import sqlparse_rs
    sqlparse_rs.patch()      # sqlparse.split / parse / format now use the Rust lexer
    sqlparse_rs.unpatch()    # restore the original implementation

or call ``sqlparse_rs.split`` / ``sqlparse_rs.tokenize`` directly. Anything the Rust core does
not handle (bytes input, lone surrogates, customised lexers, ...) is passed to the original
sqlparse implementation, so results are identical.
"""
from __future__ import annotations

import unicodedata

import sqlparse
from sqlparse import keywords as _kw
from sqlparse import lexer as _lexer
from sqlparse import tokens as T

from . import _core

__all__ = ["patch", "unpatch", "split", "split_many", "tokenize", "is_patched"]

_FALLBACK = (UnicodeError, TypeError, ValueError)

# Fixed order, shared with src/lib.rs (T_* constants).
_FIXED = [
    T.Comment.Single.Hint, T.Comment.Single, T.Newline, T.Whitespace, T.Assignment,
    T.Punctuation, T.Wildcard, T.Name, T.Name.Placeholder, T.Command, T.Keyword,
    T.Number.Hexadecimal, T.Number.Float, T.Number.Integer, T.String.Single,
    T.String.Symbol, T.Keyword.Order, T.Keyword.DDL, T.Name.Builtin, T.Keyword.TZCast,
    T.Operator.Comparison, T.Operator, T.Literal, T.Comment.Multiline,
    T.Comment.Multiline.Hint, T.Error,
]

# The Rust tables follow the Unicode database of the Python that generated them; for any other
# version, non-ASCII input goes through the original implementation.
_UNICODE_OK = unicodedata.unidata_version == _core.UNIDATA_VERSION

_orig_get_tokens = _lexer.Lexer.get_tokens
_orig_split = sqlparse.split
_orig_regex_list = None
_state = {"sig": None, "core": None}


def _flags(tt) -> int:
    f = 0
    if tt in T.Keyword:
        f |= 1
    if tt is T.Keyword:
        f |= 2
    if tt is T.Keyword.DDL:
        f |= 4
    if tt is T.Name:
        f |= 8
    if tt is T.Punctuation:
        f |= 16
    if any(tt is x or tt == x for x in (T.Whitespace, T.Comment.Single)):
        f |= 32
    if any(tt is x or tt == x for x in (T.Whitespace, T.Newline, T.Comment.Single, T.Comment.Multiline)):
        f |= 64
    if tt in T.Whitespace:
        f |= 128
    return f


def _signature(inst):
    kws = inst._keywords
    return (tuple(id(d) for d in kws), tuple(len(d) for d in kws))


def _build(inst):
    types = list(_FIXED)
    index = {id(t): i for i, t in enumerate(types)}
    merged = {}
    for d in inst._keywords:  # is_keyword() returns the first dictionary that has the key
        for k, tt in d.items():
            if k != k.upper() or k in merged:
                continue
            if id(tt) not in index:
                index[id(tt)] = len(types)
                types.append(tt)
            merged[k] = index[id(tt)]
    return _core.Core(types, [_flags(t) for t in types], list(merged.items()), inst.is_keyword)


def _core_for(inst):
    """The Rust core for `inst`, or None if the lexer was customised in a way we can't mirror."""
    global _orig_regex_list
    if _orig_regex_list is None:
        _orig_regex_list = inst._SQL_REGEX
    if inst._SQL_REGEX is not _orig_regex_list or len(inst._SQL_REGEX) != len(_kw.SQL_REGEX):
        return None
    sig = _signature(inst)
    if _state["sig"] != sig:
        _state["core"] = _build(inst)
        _state["sig"] = sig
    return _state["core"]


def _usable(text, inst):
    return type(text) is str and (_UNICODE_OK or text.isascii()) and _core_for(inst) is not None


def tokenize(sql, encoding=None):
    """Drop-in for ``sqlparse.lexer.tokenize`` (returns a list-backed iterator when fast)."""
    inst = _lexer.Lexer.get_default_instance()
    if _usable(sql, inst):
        try:
            return iter(_state["core"].tokenize(sql))
        except _FALLBACK:
            pass
    return _orig_get_tokens(inst, sql, encoding)


def _fast_get_tokens(self, text, encoding=None):
    if self is _lexer.Lexer._default_instance and _usable(text, self):
        try:
            toks = _state["core"].tokenize(text)
        except _FALLBACK:
            toks = None
        if toks is not None:
            yield from toks
            return
    yield from _orig_get_tokens(self, text, encoding)


def split(sql, encoding=None, strip_semicolon=False):
    """Drop-in for ``sqlparse.split``."""
    inst = _lexer.Lexer.get_default_instance()
    if _usable(sql, inst):
        try:
            res = _state["core"].split(sql, bool(strip_semicolon))
        except _FALLBACK:
            res = None
        if res is not None:
            return res
    return _orig_split(sql, encoding, strip_semicolon)


def split_many(sqls, strip_semicolon=False):
    """Split many scripts in parallel (GIL released); returns one list of statements per input."""
    sqls = list(sqls)
    inst = _lexer.Lexer.get_default_instance()
    out = [None] * len(sqls)
    idx = [i for i, s in enumerate(sqls) if _usable(s, inst)]
    if idx:
        try:
            res = _state["core"].split_many([sqls[i] for i in idx], bool(strip_semicolon))
        except _FALLBACK:
            res = [None] * len(idx)
        for i, r in zip(idx, res):
            out[i] = r
    for i, r in enumerate(out):
        if r is None:
            out[i] = _orig_split(sqls[i], None, strip_semicolon)
    return out


def patch():
    """Make sqlparse use the Rust lexer (parse/format/...) and splitter (split)."""
    _lexer.Lexer.get_tokens = _fast_get_tokens
    sqlparse.split = split


def unpatch():
    _lexer.Lexer.get_tokens = _orig_get_tokens
    sqlparse.split = _orig_split


def is_patched() -> bool:
    return _lexer.Lexer.get_tokens is _fast_get_tokens
