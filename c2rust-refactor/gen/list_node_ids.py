'''This module generates `ListNodeIds` impls for each AST node type.

Attributes:

- `#[list_node_ids=custom]`: Don't generate an `impl` for this type, so that a
  custom one can be provided.
'''

from datetime import datetime
from textwrap import indent, dedent

from ast import *
from util import *


@linewise
def list_rec(se, target):
    yield 'match %s {' % target
    for v, path in variants_paths(se):
        yield '  &%s => {' % struct_pattern(v, path)
        for f in v.fields:
            yield f'    ListNodeIds::add_node_ids({f.name}, node_id_list);'
        yield '  }'
    yield '}'

@linewise
def list_impl(se):
    yield '#[allow(unused, non_shorthand_field_patterns)]'
    yield 'impl ListNodeIds for %s {' % se.name
    yield '  fn add_node_ids(&self, node_id_list: &mut Vec<NodeId>) {'
    yield indent(list_rec(se, 'self'), '    ')
    yield '  }'
    yield '}'

@linewise
def dummy_impl(se):
    yield '#[allow(unused, non_shorthand_field_patterns)]'
    yield 'impl ListNodeIds for %s {' % se.name
    yield '  fn add_node_ids(&self, _node_id_list: &mut Vec<NodeId>) {'
    yield '    // Do nothing'
    yield '  }'
    yield '}'

@linewise
def generate(decls):
    yield '// AUTOMATICALLY GENERATED - DO NOT EDIT'
    yield f'// Produced {datetime.now()} by process_ast.py'
    yield ''

    for d in decls:
        if d.attrs.get('list_node_ids') == 'custom':
            continue

        if isinstance(d, (Struct, Enum)):
            yield list_impl(d)
        else:
            yield dummy_impl(d)
