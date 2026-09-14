#!/usr/bin/env python3
"""Lightweight source-structure checks, NOT rustc, cargo check, or application tests.
Uses Python stdlib; Pygments, if present, adds Rust lexical delimiter checking.
All writes are confined to .tmp/static-check.json under this source tree,
except --update-test-baseline which rewrites scripts/test-baseline.json.
"""
from __future__ import annotations
import json, re, shutil, sqlite3, subprocess, sys, tomllib, xml.etree.ElementTree as ET
from pathlib import Path
ROOT = Path(__file__).resolve().parent.parent
UPDATE_BASELINE = '--update-test-baseline' in sys.argv[1:]
# 检查明细含中文；Windows CI 默认 cp1252，会把 print/写文件变成 UnicodeEncodeError。
# 明确按 UTF-8 输出（stdout 不可重配时退化为替换字符，不让编码问题变成检查失败）。
try: sys.stdout.reconfigure(encoding='utf-8', errors='replace')
except AttributeError: pass
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
def collect_tests():
    # 提取 src/ 与 tests/ 全部 #[test]：识别 #[ignore]、紧贴在 #[test] 之前或之后的 #[cfg(...)]
    # 平台门禁——两者都属于「削弱测试」的形态，必须进基线；同名测试靠 cfg 分平台时也各自成键。
    rows=[]
    for path in [*ROOT.glob('src/**/*.rs'),*ROOT.glob('tests/**/*.rs')]:
        text=read_text(path);rel=path.relative_to(ROOT).as_posix()
        for m in re.finditer(r'(?:#\[cfg\(([^)]*)\)\]\s*)?#\[test\]([^{}]*?)fn\s+(\w+)\s*\(',text):
            cfgs=([m.group(1)] if m.group(1) else [])+re.findall(r'#\[cfg\(([^)]*)\)\]',m.group(2))
            rows.append({'file':rel,'name':m.group(3),'ignored':'#[ignore' in m.group(2),
                'cfg':' && '.join(cfgs) if cfgs else None})
    keys=[(r['file'],r['name'],r['cfg']) for r in rows]
    assert len(keys)==len(set(keys)),f'测试键重复（同名且同门禁）：{sorted(k for k in keys if keys.count(k)>1)}'
    return rows
def write_baseline(rows):
    payload={'note':'由 static_check.py --update-test-baseline 生成。删除/改名/放宽断言/新增 ignore 或平台门禁时必须重新生成本文件，并在提交信息说明理由；这是防止「为变绿而削弱测试」的门禁。',
        'tests':sorted(rows,key=lambda r:(r['file'],r['name']))}
    (ROOT/'scripts'/'test-baseline.json').write_text(json.dumps(payload,ensure_ascii=False,indent=2)+'\n',newline='\n',encoding='utf-8')
def test_baseline():
    path=ROOT/'scripts'/'test-baseline.json'
    if not path.is_file(): raise Exception('scripts/test-baseline.json 缺失；先运行 python scripts/static_check.py --update-test-baseline 生成')
    def key(r):return(r['file'],r['name'],r.get('cfg') or None)
    def val(r):return bool(r.get('ignored'))
    base={key(r):val(r) for r in json.loads(read_text(path))['tests']}
    now={key(r):val(r) for r in collect_tests()}
    added=sorted(set(now)-set(base));removed=sorted(set(base)-set(now))
    changed=sorted(k for k in set(base)&set(now) if base[k]!=now[k])
    assert not(added or removed or changed),{
        '新增测试(更新基线并在提交信息说明覆盖点)':added,
        '删除或改名(提交信息必须说明理由)':removed,
        'ignore状态翻转(提交信息必须说明理由)':[f'{k}: ignored {base[k]} -> {now[k]}' for k in changed],
        '更新命令':'python scripts/static_check.py --update-test-baseline'}
    return f'{len(now)} 个测试与 scripts/test-baseline.json 完全一致（含 ignore 与平台门禁状态）。'
def slint_blocks(ui):
    # [(open_pos,close_pos,块头标识符)]：块头是紧贴 { 前的最后一个标识符（如 Rectangle / HorizontalLayout）。
    pairs=[];stack=[]
    for i,ch in enumerate(ui):
        if ch=='{':
            head=re.search(r'([A-Za-z_][\w-]*)\s*$',ui[max(0,i-80):i])
            stack.append((i,head.group(1) if head else ''))
        elif ch=='}':
            open_pos,head=stack.pop();pairs.append((open_pos,i,head))
    return pairs
def enclosing(pairs,pos):
    best=None
    for open_pos,close_pos,head in pairs:
        if open_pos<pos<close_pos and(best is None or close_pos-open_pos<best[1]-best[0]):best=(open_pos,close_pos,head)
    return best
LAYOUTS={'HorizontalLayout','VerticalLayout','GridLayout'}
def slint_layout_width():
    # AGENTS.md 第 4 节：布局容器直接子项不得用 root/parent.width 绑定自身宽度（绑定环）。
    # 匹配位置在元素 E 自身块内，E 的父块（再上溯一层）是布局时才违规；绝对定位下的填充用法不受影响。
    ui=read_text(ROOT/'ui'/'app.slint');pairs=slint_blocks(ui);bad=[]
    for m in re.finditer(r'(?:preferred-|min-|max-)?width\s*:\s*(root|parent)\.width\b',ui):
        inner=enclosing(pairs,m.start());outer=enclosing(pairs,inner[0]) if inner else None
        if outer and outer[2] in LAYOUTS:bad.append((ui.count('\n',0,m.start())+1,m.group(0).strip()))
    assert not bad,f'布局直接子项的宽度绑定了 root/parent.width（第 行, 表达式）：{bad}'
    return '布局容器内没有用 root/parent.width 绑定子项自身宽度。'
def slint_colors():
    # AGENTS.md 第 4 节：十六进制颜色只允许出现在 Design 全局内（浅深主题由 Design.dark 切换）。
    ui=read_text(ROOT/'ui'/'app.slint');pairs=slint_blocks(ui)
    m=re.search(r'global\s+Design\s*\{',ui);assert m,'找不到 Design 全局定义'
    design=[p for p in pairs if p[0]==m.end()-1];assert design,'Design 块花括号配对异常'
    open_pos,close_pos,_=design[0]
    bad=[(ui.count('\n',0,h.start())+1,h.group(0)) for h in re.finditer(r'#[0-9a-fA-F]{3,8}\b',ui)
        if not(open_pos<h.start()<close_pos)]
    assert not bad,f'Design 全局之外出现硬编码十六进制颜色（行, 颜色）：{bad}'
    return '十六进制颜色只出现在 Design 全局内。'
def slint_accessibility():
    # AGENTS.md 第 4 节：自绘可交互控件必须有 accessible-role。这里检查下界：
    # 任何包含 TouchArea 的组件定义（含 AppWindow）本身必须声明 accessible-role。
    ui=read_text(ROOT/'ui'/'app.slint');pairs=slint_blocks(ui);missing=[]
    for m in re.finditer(r'(?:export\s+)?component\s+([\w-]+)[^{]*\{',ui):
        span=[p for p in pairs if p[0]==m.end()-1]
        if not span:continue
        _,close,_=span[0];body=ui[m.end()-1:close]
        if 'TouchArea' in body and 'accessible-role' not in body:missing.append(m.group(1))
    assert not missing,f'含 TouchArea 的组件缺少 accessible-role：{missing}'
    return '所有含 TouchArea 的组件都声明了 accessible-role。'
def product_naming():
    # AGENTS.md 第 1/5 节的产品名同步下界：标题与状态目录必须是 JchTools，且无旧名残留。
    ui=read_text(ROOT/'ui'/'app.slint');config=read_text(ROOT/'src'/'config.rs')
    assert re.search(r'title:\s*"JchTools"',ui),'窗口标题必须是产品名 JchTools'
    assert '"JchTools"' in config,'src/config.rs 的用户数据目录名必须是 "JchTools"'
    for rel in ['ui/app.slint','src/config.rs','src/registry.rs','Cargo.toml']:
        assert 'MyTools' not in read_text(ROOT/rel),f'{rel} 残留旧产品名 MyTools'
    return '窗口标题、状态目录与 Cargo 清单中的产品名一致（JchTools），无旧名残留。'
def scope_and_delivery():
    required=['README.md','先读我.txt','LICENSE','THIRD_PARTY_NOTICES.md','docs/ARCHITECTURE.md','docs/ACCEPTANCE.md','scripts/package-windows.ps1','scripts/fetch-7zip.ps1','scripts/test-baseline.json','tests/core.rs','tests/archive.rs','.github/workflows/check.yml']
    assert all((ROOT/p).is_file() for p in required)
    for path in ROOT.glob('src/**/*.rs'):
        text=read_text(path);assert 'todo!(' not in text and 'unimplemented!(' not in text,str(path)
    tests=sum(len(re.findall(r'#\[test\]',read_text(p))) for p in ROOT.glob('tests/**/*.rs'))
    return f'{tests} Rust test functions supplied, NOT executed; no todo!/unimplemented! in Rust implementation; no prebuilt executable asserted.'
if UPDATE_BASELINE:
    rows=collect_tests();write_baseline(rows)
    print(f'baseline updated: {len(rows)} tests -> scripts/test-baseline.json')
    sys.exit(0)
for name,fn in [('manifests',manifests),('config_schema',config_schema),('ui_callbacks',ui_callbacks),('rust_lexical',rust_lexical),('sql_syntax',sql_syntax),('shell_syntax',shell_syntax),
    ('test_baseline',test_baseline),('slint_layout_width',slint_layout_width),('slint_colors',slint_colors),('slint_accessibility',slint_accessibility),('product_naming',product_naming),
    ('scope_and_delivery',scope_and_delivery)]:check(name,fn)
report={'kind':'lightweight static source checks only','platform':sys.platform,'python':sys.version.split()[0],
    'rustc':shutil.which('rustc'),'cargo':shutil.which('cargo'),'powershell':shutil.which('pwsh'),
    'cargo_check':'NOT RUN','cargo_test':'NOT RUN','windows_runtime':'NOT RUN','real_7zip_tests':'NOT RUN','multi_tb_benchmark':'NOT RUN','checks':checks}
report_dir=ROOT/'.tmp'
report_dir.mkdir(parents=True,exist_ok=True)
(report_dir/'static-check.json').write_text(json.dumps(report,ensure_ascii=False,indent=2)+'\n',newline='\n',encoding='utf-8')
for row in checks: print(row['status'],row['name'],row['details'])
sys.exit(any(c['status']=='FAIL' for c in checks))
