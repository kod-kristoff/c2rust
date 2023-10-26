import functools
import re


def linewise(f):
    @functools.wraps(f)
    def g(*args, **kwargs):
        return '\n'.join(f(*args, **kwargs))
    return g

def comma_sep(f):
    @functools.wraps(f)
    def g(*args, **kwargs):
        return ', '.join(f(*args, **kwargs))
    return g

def wordwise(f):
    @functools.wraps(f)
    def g(*args, **kwargs):
        return ' '.join(f(*args, **kwargs))
    return g


@comma_sep
def struct_fields(fields, suffix, bind_mode):
    for f in fields:
        yield f'{f.name}: {bind_mode}{f.name}{suffix}'

@comma_sep
def tuple_fields(fields, suffix, bind_mode):
    for f in fields:
        yield f'{bind_mode}{f.name}{suffix}'

def struct_pattern(s, path, suffix='', bind_mode='ref '):
    if not s.is_tuple:
        return '%s { %s }' % (path, struct_fields(s.fields, suffix, bind_mode))
    if len(s.fields) == 0:
        return path
    else:
        return f'{path}({tuple_fields(s.fields, suffix, bind_mode)})'


CAPS_RE = re.compile(r'[A-Z]')

def snake(s):
    s = CAPS_RE.sub(lambda m: f'_{m.group().lower()}', s)
    if s.startswith('_'):
        s = s[1:]
    return s

def find_kind_field(s):
    if 'no_kind' in s.attrs:
        return None

    marked_fields = [f.name for f in s.fields if 'kind' in f.attrs]
    if len(marked_fields) == 1:
        return marked_fields[0]
    elif len(marked_fields) > 1:
        raise ValueError('struct %s has %d fields marked #[kind] (expected 0 or 1)' %
                (s.name, len(marked_fields)))

    return next((f.name for f in s.fields if f.name == 'kind'), None)
