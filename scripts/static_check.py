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
    # 去重三类（同名/副本名/不同名同内容）按 R-04 是独立规则行，不在此列。
    hidden={'theme','same_name_different_size','different_size_keep','detect_type'}
    assert set(fields)==set(keys)|hidden,(set(fields)-set(keys)-hidden,set(keys)-set(fields))
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
                # R-04：保留规则的全部枚举选项（含最大/最小）都必须在界面出现，
                # 不再有「刻意不展示」的白名单——界面选项必须与配置枚举一一对应。
                assert set(vals)==expected,(row['key'],vals,expected)
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
def _attr_body(text,i):
    """text[i] 指向 '#'：解析 #[...] 与 #![...] 属性（括号配对，允许嵌套括号）。
    返回 (属性体如 'cfg(not(windows))' 或 'ignore = "..."', 结束下标+1)；不是属性则 None。"""
    if i+1>=len(text) or text[i]!='#':return None
    j=i+1
    if text[j]=='!':j+=1
    if j>=len(text) or text[j]!='[':return None
    depth=0
    for k in range(j,len(text)):
        if text[k]=='[':depth+=1
        elif text[k]==']':
            depth-=1
            if depth==0:return text[j+1:k],k+1
    return None
def _cfg_payload(inner):
    """'cfg(...)' -> '...'；其余属性返回 None。'test' 门禁对 cargo test 恒真，返回 '' 表示忽略。"""
    s=inner.strip()
    if not s.startswith('cfg'):return None
    s=s[3:].strip()
    if s.startswith('(') and s.endswith(')'):s=s[1:-1].strip()
    return '' if s=='test' else s
def collect_tests():
    # 提取 src/ 与 tests/ 全部 #[test]。基线键包含生效门禁（cfg）：
    #   1) 文件级 #![cfg(...)]（仅扫文件头部内层属性区）；
    #   2) 包裹测试的 #[cfg(...)] mod 块（花括号配对取范围；cfg(test) 恒真不计）；
    #   3) 紧贴 #[test] 的属性块（前后均可，方括号配对，支持 not(windows) 等嵌套括号）。
    # 已知残留盲区：定义在其它文件里的门禁（如 lib.rs 的 #[cfg(feature = "gui")] pub mod gui;）
    # 不在本文件扫描范围内——本检查是执法下界，不是完整 cfg 求值器。
    rows=[]
    for path in [*ROOT.glob('src/**/*.rs'),*ROOT.glob('tests/**/*.rs')]:
        text=read_text(path);rel=path.relative_to(ROOT).as_posix()
        # 文件头内层属性：#![cfg(...)]（遇到第一个顶层项即停，避免误读宏内的 #![ ]）
        head=re.split(r'\n(?=(?:pub\s+)?(?:use|mod|fn|struct|enum|impl|trait|const|static|type|macro_rules)\b)',text)[0]
        file_cfgs=[]
        for m in re.finditer(r'#!\[',head):
            got=_attr_body(head,m.start())
            if got:
                payload=_cfg_payload(got[0])
                if payload and payload not in file_cfgs:file_cfgs.append(payload)
        # 模块级门禁：#[cfg(...)] mod X { ... }
        mod_spans=[]
        for m in re.finditer(r'#\[cfg\s*\(([^()]*(?:\([^()]*\)[^()]*)*)\)\]\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+(\w+)\s*\{',text):
            payload=m.group(1).strip()
            if payload=='test':continue
            end=_brace_end(text,text.find('{',m.end()-1))
            mod_spans.append((m.end(),end,payload))
        # 全部属性跨度（供逐测试回溯/前瞻；#![ ] 内层属性只出现在文件头，不会被测试链到）
        attrs=[]
        for m in re.finditer(r'#\[',text):
            got=_attr_body(text,m.start())
            if got:attrs.append((m.start(),got[1],got[0]))
        def walk(pos,step):
            """从 #[test] 位置向前(step=-1)/向后(step=+1)收集紧贴的属性行。"""
            out=[]
            cursor=pos
            while True:
                cand=None
                for a in attrs:
                    if step<0 and a[1]<=cursor and text[a[1]:cursor].strip()=='':cand=a
                    elif step>0 and a[0]>=cursor and text[cursor:a[0]].strip()=='':cand=a
                if not cand:break
                out.append(cand);cursor=cand[0] if step<0 else cand[1]
            return out
        for m in re.finditer(r'#\[test\]',text):
            cfgs=list(file_cfgs);ignored=False
            for _,_,inner in walk(m.start(),-1)+list(reversed(walk(m.end(),1))):
                payload=_cfg_payload(inner)
                if payload is not None and payload and payload not in cfgs:cfgs.append(payload)
                if inner.strip().startswith('ignore'):ignored=True
            name_m=re.match(r'[^{};]*?fn\s+(\w+)\s*\(',text[m.end():])
            if not name_m:continue
            for a0,a1,payload in mod_spans:
                if a0<m.start()<a1 and payload not in cfgs:cfgs.append(payload)
            rows.append({'file':rel,'name':name_m.group(1),'ignored':ignored,
                'cfg':' && '.join(cfgs) if cfgs else None})
    keys=[(r['file'],r['name'],r['cfg']) for r in rows]
    assert len(keys)==len(set(keys)),f'测试键重复（同名且同门禁）：{sorted(k for k in keys if keys.count(k)>1)}'
    return rows
def _brace_end(text,open_idx):
    depth=0
    for i in range(open_idx,len(text)):
        if text[i]=='{':depth+=1
        elif text[i]=='}':
            depth-=1
            if depth==0:return i
    return len(text)-1
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
def slint_modal_gating():
    # AGENTS.md 第 4 节与仓库既有先例（app.slint 内「键盘可穿透确认框」修复注释）：
    # 确认模态（confirm-kind != 0）的 scrim 只挡鼠标，Tab 焦点仍可到达被遮挡控件；
    # AppWindow 内（确认层之前）所有可交互控件的 enabled 绑定都必须门禁 confirm-kind。
    ui=read_text(ROOT/'ui'/'app.slint')
    start=ui.index('export component AppWindow inherits Window')
    overlay=ui.index('if root.confirm-kind != 0:',start)
    # 冲突层是模态层，其自身控件在 conflict-visible 打开时必须可用，不适用背景门禁
    # （它们只受 confirm-kind 门禁，见 slint_conflict_modal_gating）。
    conflict=ui.find('if root.conflict-visible:',start)
    if conflict!=-1: overlay=min(overlay,conflict)
    start_line=ui.count('\n',0,start)+1
    bad=[]
    for offset,line in enumerate(ui[start:overlay].split('\n')):
        # 只解析 enabled 绑定表达式本身（排除 accessible-enabled 与属性声明）；
        # 「同行的 if 渲染条件里有 busy 字样」不再作为豁免依据——冲突弹出时 busy 恒真，
        # if root.busy: 控件恰在此时渲染，恰恰是最需要 conflict-visible 门禁的形态。
        m_en=re.search(r'(?<![\w-])enabled\s*:(.*)',line)
        if not m_en:continue
        expr=m_en.group(1)
        if 'confirm-kind' not in expr:
            bad.append((start_line+offset,line.strip()))
        # confirm-kind 门禁只防确认模态：冲突模态（conflict-visible）下还需要
        # enabled 含 !root.busy / conflict-visible，或本行渲染条件保证 !root.busy
        #（该控件在冲突弹出时根本不渲染）。
        elif not (re.search(r'!\s*root\.busy',expr) or 'conflict-visible' in expr
                  or re.search(r'!\s*root\.busy',line[:m_en.start()])):
            bad.append((start_line+offset,line.strip()))
    assert not bad,f'AppWindow 内存在未按 confirm-kind 门禁的 enabled 绑定（行, 表达式）：{bad}'
    return 'AppWindow 内（确认层之前）所有可交互控件的 enabled 绑定都门禁 confirm-kind。'
def build_rc_prefers_windows_kits():
    # 与 process.rs 的 system_tool 防 PATH 劫持口径一致：build.rs 解析 rc.exe 必须
    # 优先 Windows SDK 目录；PATH 只能作为回退（且回退时构建日志应有告警）。
    text=read_text(ROOT/'build.rs')
    kits=text.find('Windows Kits');from_path=text.find('var_os("PATH")')
    assert from_path==-1 or (kits!=-1 and kits<from_path), \
        'build.rs 中 rc.exe 的 Windows SDK 查找必须先于 PATH 查找（或不搜 PATH）'
    return 'rc.exe 优先从 Windows SDK 解析，PATH 仅作回退。'
def acceptance_respects_cargo_target_dir():
    # acceptance.ps1 的 gui-smoke exe 路径必须与 package-windows.ps1 同口径尊重
    # CARGO_TARGET_DIR：否则自定义 target 目录的机器会误报验证失败，或冒烟陈旧 exe。
    text=read_text(ROOT/'scripts'/'acceptance.ps1')
    assert 'CARGO_TARGET_DIR' in text,'acceptance.ps1 的 gui-smoke 必须尊重 CARGO_TARGET_DIR（与 package-windows.ps1 同口径）'
    return 'acceptance.ps1 的 gui-smoke 尊重 CARGO_TARGET_DIR。'
def slint_conflict_modal_gating():
    # 冲突层画在确认层之下（app.slint 注释自证 kind=3 确认层会盖在其上）：确认模态
    # 打开期间，冲突对话框的全部可交互控件必须禁用，否则键盘可穿透确认层直接改
    # 冲突策略或取消任务。slint_modal_gating 只覆盖「有 enabled 绑定」的行，本规则
    # 补上冲突层内无 enabled 绑定的控件这一盲区。
    ui=read_text(ROOT/'ui'/'app.slint')
    m=re.search(r'if root\.conflict-visible:.*?\{',ui);assert m,'找不到冲突层声明'
    depth=0;end=len(ui)
    for i,ch in enumerate(ui[m.end()-1:]):
        if ch=='{':depth+=1
        elif ch=='}':
            depth-=1
            if depth==0:end=m.end()-1+i;break
    body=ui[m.end():end]
    bad=[line.strip() for line in body.split('\n')
         if re.search(r'\b(Button|ComboBox|CheckBox|LineEdit) \{',line) and 'confirm-kind' not in line]
    assert not bad,f'冲突对话框内存在未按 confirm-kind 门禁的可交互控件：{bad}'
    return '冲突对话框内全部可交互控件都门禁 confirm-kind。'
def sums_integrity():
    # SHA256SUMS.txt 契约：覆盖除自身外的全部 git 跟踪文件、哈希与
    # 工作树一致、LF 行尾（CRLF 会让 `sha256sum -c` 在 Git Bash 下整单失败）。
    import hashlib,subprocess
    raw=(ROOT/'SHA256SUMS.txt').read_bytes()
    assert b'\r' not in raw,'SHA256SUMS.txt 必须是 LF 行尾（CRLF 会让 sha256sum -c 全部失败）'
    listed={}
    for line in raw.decode('utf-8').splitlines():
        if not line.strip():continue
        digest,name=line.split(' ',1)
        if name[:1] in ('*',' '):name=name[1:]  # 兼容 sha256sum -b 的 *name 与文本模式的双空格 name
        listed[name]=digest.lower()
    try:
        out=subprocess.run(['git','-c','core.quotePath=false','ls-files','-z'],cwd=ROOT,capture_output=True,check=True).stdout
    except Exception as exc:
        raise Skipped(f'git 不可用，跳过：{exc}')
    tracked=[p.decode('utf-8').replace('\\','/') for p in out.split(b'\0') if p]
    expected=[p for p in tracked if p!='SHA256SUMS.txt']
    missing=sorted(set(expected)-set(listed))
    assert not missing,f'跟踪文件未列入 SHA256SUMS.txt（用 sha256sum -b 重建）：{missing}'
    extra=sorted(set(listed)-set(expected))
    assert not extra,f'清单含未跟踪/多余条目：{extra}'
    gone=sorted(name for name in expected if not (ROOT/name).is_file())
    assert not gone,f'跟踪文件已从工作树删除（先恢复文件或 git add -A 后重建清单）：{gone}'
    stale=[name for name in expected
           if (ROOT/name).is_file() and hashlib.sha256((ROOT/name).read_bytes()).hexdigest()!=listed[name]]
    assert not stale,f'哈希与工作树不一致（文件已改，需重建 SHA256SUMS.txt）：{stale}'
    return f'{len(expected)} 个跟踪文件的 SHA256 全部与工作树一致（LF 行尾）。'
def scope_and_delivery():
    required=['README.md','先读我.txt','LICENSE','THIRD_PARTY_NOTICES.md','docs/CONTRACT.md','scripts/package-windows.ps1','scripts/fetch-7zip.ps1','scripts/test-baseline.json','tests/core.rs','tests/archive.rs','.github/workflows/check.yml']
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
    ('slint_modal_gating',slint_modal_gating),('build_rc_prefers_windows_kits',build_rc_prefers_windows_kits),('acceptance_respects_cargo_target_dir',acceptance_respects_cargo_target_dir),
    ('slint_conflict_modal_gating',slint_conflict_modal_gating),('sums_integrity',sums_integrity),
    ('scope_and_delivery',scope_and_delivery)]:check(name,fn)
report={'kind':'lightweight static source checks only','platform':sys.platform,'python':sys.version.split()[0],
    'rustc':shutil.which('rustc'),'cargo':shutil.which('cargo'),'powershell':shutil.which('pwsh'),
    'cargo_check':'NOT RUN','cargo_test':'NOT RUN','windows_runtime':'NOT RUN','real_7zip_tests':'NOT RUN','multi_tb_benchmark':'NOT RUN','checks':checks}
report_dir=ROOT/'.tmp'
report_dir.mkdir(parents=True,exist_ok=True)
(report_dir/'static-check.json').write_text(json.dumps(report,ensure_ascii=False,indent=2)+'\n',newline='\n',encoding='utf-8')
for row in checks: print(row['status'],row['name'],row['details'])
sys.exit(any(c['status']=='FAIL' for c in checks))
