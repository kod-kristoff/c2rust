from datetime import datetime

from ast import *
from util import *

@linewise
def do_ast_names_impl(d):
    if not isinstance(d, (Struct, Enum)):
        return
    yield '#[allow(unused, non_shorthand_field_patterns)]'
    yield 'impl AstName for %s {' % d.name
    yield '  fn ast_name(&self) -> String {'

    yield '    match self {'
    for v, path in variants_paths(d):
        yield '      &%s => {' % struct_pattern(v, path)
        yield f'        "{v.name}".to_string()'
        if isinstance(d, (Struct)):
            if kind_field := find_kind_field(d):
                yield f'        + ":" + &self.{kind_field}.ast_name()'
        yield '      }'
    yield '    }'
    yield '  }'
    yield '}'

@linewise
def generate(decls):
    yield '// AUTOMATICALLY GENERATED - DO NOT EDIT'
    yield f'// Produced {datetime.now()} by process_ast.py'
    yield ''

    for d in decls:
        yield do_ast_names_impl(d)

