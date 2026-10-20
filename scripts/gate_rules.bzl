"""由维护中的原生脚本/命令规则执行门；仅 Cargo 保留原生输入剪枝动作。"""

load("@bazel_lib//lib:run_binary.bzl", "run_binary")
load("@bazel_skylib//rules:native_binary.bzl", "native_test")
load("@bazel_skylib//rules:write_file.bzl", "write_file")

def _batch_quote(path):
    return '"' + path.replace("%", "%%") + '"'

def gate_test(name, data, before, spec, spec_path, python, adapter, windows = True, **kwargs):
    """标准 native_test 直接执行批处理 adapter，依赖与 spec 保留在 data。"""
    write_file(
        name = name + "_script",
        out = name + ".adapter.bat",
        content = [
            "@echo off",
            "setlocal DisableDelayedExpansion",
            " ".join([_batch_quote(path) for path in [python, adapter, spec_path]]),
            "exit /b %errorlevel%",
        ],
        is_executable = True,
        newline = "windows",
    )
    native_test(
        name = name,
        src = ":" + name + "_script",
        out = name + ".bat",
        data = data + before + [spec],
        **kwargs
    )

def _cargo_command_impl(ctx):
    # run_binary 3.7.2 支持 tree outputs，但没有 unused_inputs_list。
    # Cargo 的真实 dep-info 才能安全剪枝输入；不能另建成功记录/指纹缓存代替。
    log = ctx.actions.declare_file(ctx.label.name + ".command.log")
    directory = ctx.actions.declare_directory(ctx.label.name + ".cargo")
    manifest = ctx.actions.declare_file(ctx.label.name + ".cargo.json")
    outputs = [log, directory, manifest]
    inputs = depset(
        direct = [ctx.file.spec],
        transitive = [target[DefaultInfo].files for target in ctx.attr.data + ctx.attr.before],
    )
    unused = ctx.actions.declare_file(ctx.label.name + ".unused-inputs")
    inventory = ctx.actions.declare_file(ctx.label.name + ".inputs.json")
    ctx.actions.write(inventory, json.encode([file.path for file in inputs.to_list()]))
    execution_requirements = {"local": "1", "no-remote": "1"}
    if not ctx.attr.cacheable:
        execution_requirements["no-cache"] = "1"
    ctx.actions.run(
        executable = ctx.attr.python,
        arguments = [ctx.attr.adapter, ctx.attr.spec_path, log.path, directory.path, manifest.path],
        inputs = depset(direct = [inventory], transitive = [inputs]),
        outputs = outputs + [unused],
        unused_inputs_list = unused,
        env = {
            "JCHTOOLS_GATE_UNUSED_INPUTS": unused.path,
            "JCHTOOLS_GATE_INPUT_INVENTORY": inventory.path,
        },
        mnemonic = "GateCommand",
        progress_message = "执行门命令 %{label}",
        execution_requirements = execution_requirements,
    )
    return [DefaultInfo(files = depset(outputs))]

_cargo_command = rule(
    implementation = _cargo_command_impl,
    attrs = {
        "data": attr.label_list(allow_files = True),
        "before": attr.label_list(allow_files = True),
        "spec": attr.label(allow_single_file = True, mandatory = True),
        "spec_path": attr.string(mandatory = True),
        "python": attr.string(mandatory = True),
        "adapter": attr.string(mandatory = True),
        "cacheable": attr.bool(default = False),
    },
)

def gate_command(name, data, spec, spec_path, python, adapter, windows = True, before = [], cargo_artifacts = False, cacheable = False, **kwargs):
    """通用命令使用 run_binary；Cargo 专有协议是唯一自定义动作。"""
    if cargo_artifacts:
        _cargo_command(
            name = name,
            data = data,
            before = before,
            spec = spec,
            spec_path = spec_path,
            python = python,
            adapter = adapter,
            cacheable = cacheable,
            **kwargs
        )
    else:
        execution_requirements = {"local": "1", "no-remote": "1"}
        if not cacheable:
            execution_requirements["no-cache"] = "1"
        run_binary(
            name = name,
            tool = "//.tmp/bazel-gate:gate_adapter",
            srcs = data + before + [spec],
            args = [json.encode(spec_path.replace("\\", "/")), '"$@"'],
            outs = [name + ".command.log"],
            mnemonic = "GateCommand",
            progress_message = "执行门命令 %{label}",
            execution_requirements = execution_requirements,
            **kwargs
        )
