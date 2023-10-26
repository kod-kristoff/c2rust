from datetime import datetime

from util import *

@linewise
def do_ast_deref_impl(d):
    supported = 'rewrite_seq_item' in d.attrs

    yield '#[allow(unused)]'
    yield 'impl AstDeref for %s {' % d.name
    yield '  type Target = Self;'
    yield '  fn ast_deref(&self) -> &Self { self }'
    yield '}'

@linewise
def generate(decls):
    yield '// AUTOMATICALLY GENERATED - DO NOT EDIT'
    yield f'// Produced {datetime.now()} by process_ast.py'
    yield ''

    for d in decls:
        yield do_ast_deref_impl(d)

