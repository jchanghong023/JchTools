#!/usr/bin/env python3
"""Lightweight source-structure checks, NOT rustc, cargo check, or application tests.
Uses Python stdlib; Pygments, if present, adds Rust lexical delimiter checking.
All writes are confined to .tmp/static-check.json under this source tree.
"""
from __future__ import annotations
import json, re, shutil, sqlite3, subprocess, sys, tomllib, xml.etree.ElementTree as ET
from pathlib import Path
ROOT = Path(__file__).resolve().parent.parent
def read_text(path: Path) -> str:
    return path.read_text(encoding='utf-8')
checks: list[dict[str, object]] = []
class Skipped(Exception):
    """检查在本机不可执行（依赖缺失等）；与 PASS/FAIL 区分，避免“没跑”被记成“通过”。"""
def check(name, fn):
    try:
        details=fn(); checks.append({'name':name,'status':'PASS','details':details})
    except Skipped as exc:
        checks.append({'name':name,'status':'SKIP','details':str(exc)})
    except Exception as exc:
        checks.append({'name':name,'status':'FAIL','details':str(exc)})
def manifests():
    cargo=tomllib.loads(read_text(ROOT/'Cargo.toml'))
    for binary in cargo['bin']: assert (ROOT/binary['path']).is_file()
    ET.parse(ROOT/'resources/windows.manifest')
    return 'Cargo TOML, declared binary paths, Windows XML parsed.'
def config_schema():
    config=read_text(ROOT/'src/config.rs')
    body=re.search(r'pub struct Config\s*\{(.*?)\n\}',config,re.S).group(1)
    fields=dict(re.findall(r'pub\s+(\w+)\s*:\s*([\w:]+)',body))
    rows=json.loads(read_text(ROOT/'resources/rules.json'))
    keys=[r['key'] for r in rows]
    assert len(keys)==len(set(keys))
    # 有意不进规则表的配置字段：theme 已挪到「关于」页；其余是合并行的影子键，
    # 界面一行驱动多个字段，引擎 / CLI / 旧任务库仍读细粒度值。
    hidden={'theme','dedup_copy_names','dedup_other_names','same_name_different_size','different_size_keep','detect_type'}
    assert set(fields)==set(keys)|hidden,(set(fields)-set(keys)-hidden,set(keys)-set(fields))
    # 内容相同的去重/版本组大小必然一致，“保留最大/最小”是无效选项，界面刻意不展示。
    omitted_choices={'keep_duplicate':{'largest','smallest'},'same_size_keep':{'largest','smallest'}}
    for row in rows:
        assert row['title'] and row['hint']
        ty=fields[row['key']]
        if ty=='bool': assert row['kind']=='bool'
        if ty in ('usize','u64','u32'): assert row['kind']=='number'
        if row['kind']=='choice':
            assert len(row['choices'])>=2
            vals=[c[0] for c in row['choices']];assert len(vals)==len(set(vals))
            if ty!='String':
                enum=re.search(r'pub enum '+ty+r'\s*\{(.*?)\}',config,re.S).group(1)
                expected={re.sub(r'(?<!^)(?=[A-Z])','_',v.strip()).lower() for v in enum.split(',') if v.strip()}
                omitted=omitted_choices.get(row['key'],set())
                assert set(vals)|omitted==expected,(row['key'],vals,expected,omitted)
    return f'{len(rows)} UI settings match serialized Config fields ({len(hidden)} engine/CLI-only) and enum values.'
def ui_callbacks():
    ui=read_text(ROOT/'ui/app.slint')
    # GUI 组装层在 src/gui.rs（bin main.rs 只是薄壳入口），两者都可能有 ui.on_* 接线。
    rs=chr(10).join(read_text(ROOT/p) for p in sorted(ROOT.glob('src/*.rs')))
    # 只有导出组件（窗口）上的回调是应用级 API；组件内部的回调在 .slint 内部接线。
    text=ui[ui.index('export component AppWindow inherits Window'):]
    depth=0;end=len(text)
    for index,char in enumerate(text):
        if char=='{':depth+=1
        elif char=='}':
            depth-=1
            if depth==0:end=index;break
    declared={name.replace('-','_') for name in re.findall(r'callback\s+([\w-]+)\(',text[:end])}
    wired=set(re.findall(r'ui\.on_(\w+)\(',rs))
    assert declared==wired,{'missing_handlers':sorted(declared-wired),'extra_handlers':sorted(wired-declared)}
    return f'{len(declared)} declared window callbacks have Rust handlers.'
def rust_lexical():
    try:
        from pygments import lex
        from pygments.lexers import RustLexer
        from pygments.token import Comment, Literal
    except ImportError:
        raise Skipped('Pygments unavailable; Rust lexical delimiter check was not run.') from None
    count=0
    for path in [ROOT/'build.rs',*ROOT.glob('src/**/*.rs'),*ROOT.glob('tests/**/*.rs')]:
        stack=[]
        for token,text in lex(read_text(path),RustLexer()):
            if token in Comment or token in Literal.String: continue
            for ch in text:
                if ch in '([{':stack.append(ch)
                elif ch in ')]}':
                    assert stack and '([{'.index(stack.pop())==')]}'.index(ch),str(path.relative_to(ROOT))
        assert not stack,(str(path.relative_to(ROOT)),stack)
        count+=1
    return f'{count} Rust files have balanced lexical delimiters; this does NOT validate Rust types, APIs, macros or borrow checking.'
def sql_syntax():
    conn=sqlite3.connect(':memory:')
    conn.executescript(read_text(ROOT/'src/schema.sql'))
    conn.executescript('''CREATE TEMP TABLE duplicate_order(seq INTEGER,id INTEGER);
        CREATE TEMP TABLE conflict_groups(seq INTEGER PRIMARY KEY,key TEXT,size INTEGER);
        CREATE TEMP TABLE empty_order(seq INTEGER,rel TEXT);
        CREATE TEMP TABLE empty_will(rel TEXT PRIMARY KEY);
        CREATE TEMP TABLE stay_parents(parent TEXT);
        CREATE TEMP TABLE dir_children(parent TEXT,rel TEXT);
        CREATE TEMP TABLE hash_candidates(id INTEGER PRIMARY KEY);''')
    file_columns='id,rel,name,normal,size,mtime,identity,links,hash,cleanable'
    statements=set()
    for path in ROOT.glob('src/**/*.rs'):
        for match in re.finditer(r'"((?:[^"\\]|\\.)*)"',read_text(path)):
            raw=match.group(1)
            if not re.match(r'^(SELECT|UPDATE|INSERT|DELETE)\b',raw): continue
            raw=raw.replace('{FILE_COLUMNS}',file_columns).replace('{key_expr}','name').replace('{filter}','active=1 AND name=?1 AND size=?2')
            if '{' in raw or '}' in raw or ';' in raw: continue
            raw=raw.replace('\\"','"')
            params=max([int(x) for x in re.findall(r'\?(\d+)',raw)]+[0])
            conn.execute('EXPLAIN '+raw,[None]*params)
            statements.add(raw)
    return f'SQLite schema and {len(statements)} concrete DML statements prepare successfully against empty schema; no organizer application executed.'
def shell_syntax():
    script=str(ROOT/'scripts/check-linux.sh')
    bash=shutil.which('bash')
    if not bash:
        raise Skipped('bash not found; bash -n and PowerShell syntax checks run in the Windows/Linux CI jobs.')
    done=subprocess.run([bash,'-n',script],capture_output=True,text=True)
    if done.returncode!=0 and ('not found' in (done.stderr or '').lower() or done.returncode==127):
        raise Skipped('bash launcher is unavailable on this host; bash -n runs in the Linux CI job.')
    assert done.returncode==0,done.stderr
    return 'bash -n passed; PowerShell syntax check is defined in Windows CI.'
def scope_and_delivery():
    required=['README.md','先读我.txt','LICENSE','THIRD_PARTY_NOTICES.md','docs/ARCHITECTURE.md','docs/ACCEPTANCE.md','scripts/package-windows.ps1','scripts/fetch-7zip.ps1','tests/core.rs','tests/archive.rs','.github/workflows/check.yml']
    assert all((ROOT/p).is_file() for p in required)
    for path in ROOT.glob('src/**/*.rs'):
        text=read_text(path);assert 'todo!(' not in text and 'unimplemented!(' not in text,str(path)
    tests=sum(len(re.findall(r'#\[test\]',read_text(p))) for p in ROOT.glob('tests/**/*.rs'))
    return f'{tests} Rust test functions supplied, NOT executed; no todo!/unimplemented! in Rust implementation; no prebuilt executable asserted.'
for name,fn in [('manifests',manifests),('config_schema',config_schema),('ui_callbacks',ui_callbacks),('rust_lexical',rust_lexical),('sql_syntax',sql_syntax),('shell_syntax',shell_syntax),('scope_and_delivery',scope_and_delivery)]:check(name,fn)
report={'kind':'lightweight static source checks only','platform':sys.platform,'python':sys.version.split()[0],
    'rustc':shutil.which('rustc'),'cargo':shutil.which('cargo'),'powershell':shutil.which('pwsh'),
    'cargo_check':'NOT RUN','cargo_test':'NOT RUN','windows_runtime':'NOT RUN','real_7zip_tests':'NOT RUN','multi_tb_benchmark':'NOT RUN','checks':checks}
report_dir=ROOT/'.tmp'
report_dir.mkdir(parents=True,exist_ok=True)
(report_dir/'static-check.json').write_text(json.dumps(report,ensure_ascii=False,indent=2)+'\n',newline='\n')
for row in checks: print(row['status'],row['name'],row['details'])
sys.exit(any(c['status']=='FAIL' for c in checks))
