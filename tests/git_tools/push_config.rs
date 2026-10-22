use super::*;
// 覆盖 G-08（回归：本地 topic 跟踪 origin/master 且 push.default=current 时，
// 裸 git push 会静默创建并推送 origin/topic，upstream 未更新却报告全部完成；
// 修复后启动预检拒绝并点名配置，远端不出现任何新分支）
#[test]
fn push_default_current_with_renamed_upstream_refuses_to_start() {
    let fix = fixture();
    git_ok(&fix.repo, &["checkout", "-q", "-b", "topic"]);
    git_ok(&fix.repo, &["config", "branch.topic.remote", "origin"]);
    git_ok(
        &fix.repo,
        &["config", "branch.topic.merge", "refs/heads/master"],
    );
    git_ok(&fix.repo, &["config", "push.default", "current"]);
    fs::write(fix.repo.join("a.txt"), "x\n").unwrap();
    let outcome = run_tool(&fix.repo);
    assert!(
        outcome.text.contains("无法开始"),
        "必须拒绝启动：{}",
        outcome.text
    );
    assert!(
        outcome.text.contains("push.default") && outcome.text.contains("current"),
        "错误信息必须点名涉及配置项：{}",
        outcome.text
    );
    // 远端不得出现 origin/topic，也不得收到任何提交
    let heads = git_ok(&fix.repo, &["ls-remote", "--heads", "origin"]);
    assert!(!heads.contains("topic"), "不得在远端创建同名分支：{heads}");
    assert_eq!(
        remote_log(&fix.remote),
        vec!["init".to_string()],
        "origin 不应收到任何提交"
    );
    assert_eq!(
        git_ok(&fix.repo, &["log", "--format=%s"]).trim(),
        "init",
        "本地不得有新提交"
    );
}

// 覆盖 G-08（正控：push.default=current 且 upstream 分支与本地同名时，裸 push
// 的目标就是 upstream，不属于危险配置，预检放行且任务正常完成）
#[test]
fn push_default_current_with_matching_upstream_name_proceeds() {
    let fix = fixture();
    git_ok(&fix.repo, &["checkout", "-q", "-b", "topic"]);
    git_ok(&fix.repo, &["config", "branch.topic.remote", "origin"]);
    git_ok(
        &fix.repo,
        &["config", "branch.topic.merge", "refs/heads/topic"],
    );
    git_ok(&fix.repo, &["config", "push.default", "current"]);
    // 在远端建立同名分支，使 @{u} 可解析（G-02 的 upstream 验证依赖
    // remote-tracking 引用存在；仅配置 merge 目标时它指向不存在的远端分支）。
    git_ok(
        &fix.repo,
        &["push", "-q", "origin", "topic:refs/heads/topic"],
    );
    fs::write(fix.repo.join("a.txt"), "x\n").unwrap();
    let outcome = run_tool(&fix.repo);
    assert!(
        outcome.text.contains("全部完成"),
        "同名 upstream 不应被预检拒绝：{}",
        outcome.text
    );
    // topic 在前置准备中已存在，仅检查分支名会让未推送更新的实现也通过。
    assert_eq!(
        git_ok(&fix.remote, &["show", "topic:a.txt"]),
        "x\n",
        "必须把本次文件内容推送到 upstream"
    );
    assert_eq!(
        git_ok(&fix.remote, &["log", "-1", "--format=%s", "topic"]).trim(),
        "update: a.txt",
        "upstream 必须收到当前文件的提交"
    );
}

// 覆盖 G-08/G-09：push.default=simple 在本地与 upstream 分支异名时必然拒绝裸 push；
// 应在提交前识别该确定性失败，而不是无限重试 push。
#[test]
fn push_default_simple_with_renamed_upstream_refuses_to_start() {
    let fix = fixture();
    git_ok(&fix.repo, &["checkout", "-q", "-b", "topic"]);
    git_ok(&fix.repo, &["config", "branch.topic.remote", "origin"]);
    git_ok(
        &fix.repo,
        &["config", "branch.topic.merge", "refs/heads/master"],
    );
    git_ok(&fix.repo, &["config", "push.default", "simple"]);
    fs::write(fix.repo.join("a.txt"), "x\n").unwrap();

    let outcome = run_tool(&fix.repo);
    assert!(
        outcome.text.contains("无法开始")
            && outcome.text.contains("push.default")
            && outcome.text.contains("simple"),
        "必须在写入前拒绝必然失败的推送配置：{}",
        outcome.text
    );
    assert_eq!(
        remote_log(&fix.remote),
        vec!["init".to_string()],
        "预检拒绝后不得推送"
    );
    assert_eq!(
        git_ok(&fix.repo, &["log", "--format=%s"]).trim(),
        "init",
        "预检拒绝后不得提交"
    );
    assert!(
        git_ok(&fix.repo, &["status", "--porcelain"]).contains("?? a.txt"),
        "预检拒绝后工作区文件保持未处理"
    );
}

// 覆盖 G-09：非法 push.default 会使裸 git push 稳定失败；不得在本地提交后无限重试。
#[test]
fn unsupported_push_default_refuses_to_start() {
    let fix = fixture();
    git_ok(&fix.repo, &["config", "push.default", "unsupported"]);
    fs::write(fix.repo.join("a.txt"), "x\n").unwrap();

    let outcome = run_tool(&fix.repo);
    assert!(
        outcome.text.contains("无法开始")
            && outcome.text.contains("push.default")
            && outcome.text.contains("unsupported"),
        "必须在写入前拒绝非法 push.default：{}",
        outcome.text
    );
    assert_eq!(
        remote_log(&fix.remote),
        vec!["init".to_string()],
        "非法推送配置不得更改远端"
    );
    // git 对非法 push.default 值在任何读取配置的命令（含本测试后续 log/status）
    // 中都致命失败；git config 自身的写/删不做语义校验（上面写入已证明），
    // 先还原配置再做未提交/未改动验证。
    git_ok(&fix.repo, &["config", "--unset", "push.default"]);
    assert_eq!(
        git_ok(&fix.repo, &["log", "--format=%s"]).trim(),
        "init",
        "预检拒绝后不得提交"
    );
    assert!(
        git_ok(&fix.repo, &["status", "--porcelain"]).contains("?? a.txt"),
        "预检拒绝后工作区文件保持未处理"
    );
}

// 覆盖 G-12（以 git 为状态来源：成功 push 后重启不重复提交）
#[test]
fn completed_files_not_reprocessed_on_restart() {
    let fix = fixture();
    fs::write(fix.repo.join("a.txt"), "a\n").unwrap();
    let first = run_tool(&fix.repo);
    assert!(first.text.contains("全部完成"));
    let second = run_tool(&fix.repo);
    assert!(
        second.text.contains("没有需要提交的变更"),
        "重启后按 git 状态判定：{}",
        second.text
    );
    let log = remote_log(&fix.remote);
    assert_eq!(
        log.iter().filter(|s| **s == "update: a.txt").count(),
        1,
        "不得重复提交：{log:?}"
    );
}

// 覆盖 P-03/G-08（F10：branch.<b>.pushRemote 指向 other 时裸 git push 会推到
// 非 upstream 远端；启动前预检必须拒绝，两个远端都收不到任何东西）
#[test]
fn push_remote_config_mismatch_refuses_to_start() {
    let fix = fixture();
    let other = add_second_remote(&fix, "other");
    git_ok(&fix.repo, &["config", "branch.master.pushRemote", "other"]);
    fs::write(fix.repo.join("a.txt"), "x\n").unwrap();
    let outcome = run_tool(&fix.repo);
    assert!(
        outcome.text.contains("无法开始"),
        "必须拒绝启动：{}",
        outcome.text
    );
    assert!(
        outcome.text.contains("branch.master.pushRemote"),
        "错误信息必须点名涉及配置项：{}",
        outcome.text
    );
    assert_start_refused_and_nothing_moved(&fix, &other);
}

// 覆盖 P-03/G-08（F10：remote.pushDefault=other 同样使裸 git push 偏离 upstream）
#[test]
fn push_default_remote_config_refuses_to_start() {
    let fix = fixture();
    let other = add_second_remote(&fix, "other");
    git_ok(&fix.repo, &["config", "remote.pushDefault", "other"]);
    fs::write(fix.repo.join("a.txt"), "x\n").unwrap();
    let outcome = run_tool(&fix.repo);
    assert!(
        outcome.text.contains("无法开始"),
        "必须拒绝启动：{}",
        outcome.text
    );
    assert!(
        outcome.text.contains("remote.pushDefault"),
        "错误信息必须点名涉及配置项：{}",
        outcome.text
    );
    assert_start_refused_and_nothing_moved(&fix, &other);
}

// 覆盖 P-03/G-08（F10：remote.<r>.push 自定义 refspec 会改变裸 push 的推送对象）
#[test]
fn remote_push_refspec_refuses_to_start() {
    let fix = fixture();
    git_ok(
        &fix.repo,
        &[
            "config",
            "remote.origin.push",
            "refs/heads/master:refs/heads/other-branch",
        ],
    );
    fs::write(fix.repo.join("a.txt"), "x\n").unwrap();
    let outcome = run_tool(&fix.repo);
    assert!(
        outcome.text.contains("无法开始"),
        "必须拒绝启动：{}",
        outcome.text
    );
    assert!(
        outcome.text.contains("remote.origin.push"),
        "错误信息必须点名涉及配置项：{}",
        outcome.text
    );
    // 单一远端也不能收到任何东西
    assert_eq!(
        remote_log(&fix.remote),
        vec!["init".to_string()],
        "origin 不应收到任何提交"
    );
    assert_eq!(
        git_ok(&fix.repo, &["log", "--format=%s"]).trim(),
        "init",
        "本地不得有新提交"
    );
}

// 覆盖 P-03/G-08（F10：push.default=matching 会一次推送所有同名分支（多分支））
#[test]
fn push_default_matching_refuses_to_start() {
    let fix = fixture();
    git_ok(&fix.repo, &["config", "push.default", "matching"]);
    fs::write(fix.repo.join("a.txt"), "x\n").unwrap();
    let outcome = run_tool(&fix.repo);
    assert!(
        outcome.text.contains("无法开始"),
        "必须拒绝启动：{}",
        outcome.text
    );
    assert!(
        outcome.text.contains("push.default") && outcome.text.contains("matching"),
        "错误信息必须点名涉及配置项：{}",
        outcome.text
    );
    assert_eq!(
        remote_log(&fix.remote),
        vec!["init".to_string()],
        "origin 不应收到任何提交"
    );
}

// 覆盖 P-03/G-08（独立审查发现：remote.origin.mirror=true 使裸 push 变镜像
// 推送——全部引用推到远端并删除远端多余分支，退出码 0 被当成功报告；
// 预检必须拒绝，远端零改动）
#[test]
fn mirror_remote_config_refuses_to_start() {
    let fix = fixture();
    git_ok(&fix.repo, &["config", "remote.origin.mirror", "true"]);
    fs::write(fix.repo.join("a.txt"), "x\n").unwrap();
    let outcome = run_tool(&fix.repo);
    assert!(
        outcome.text.contains("无法开始"),
        "必须拒绝启动：{}",
        outcome.text
    );
    assert!(
        outcome.text.contains("mirror"),
        "错误信息必须点名涉及配置项：{}",
        outcome.text
    );
    assert_eq!(
        remote_log(&fix.remote),
        vec!["init".to_string()],
        "origin 不得被镜像推送改动"
    );
    assert_eq!(
        git_ok(&fix.repo, &["log", "--format=%s"]).trim(),
        "init",
        "本地不得有新提交"
    );
}

// 覆盖 P-03/G-08（remote.<name>.mirror 的布尔真值允许 Git 配置同义词；
// 只识别字面 true 会放过 yes/on/1，随后裸 push 会镜像全部引用）
#[test]
fn mirror_remote_config_boolean_alias_refuses_to_start() {
    for value in ["yes", "on", "1"] {
        let fix = fixture();
        git_ok(&fix.repo, &["config", "remote.origin.mirror", value]);
        fs::write(fix.repo.join("a.txt"), "x\n").unwrap();

        let outcome = run_tool(&fix.repo);
        assert!(
            outcome.text.contains("无法开始") && outcome.text.contains("mirror"),
            "remote.origin.mirror={value} 必须在写入前被拒绝：{}",
            outcome.text
        );
        assert_eq!(
            remote_log(&fix.remote),
            vec!["init".to_string()],
            "镜像推送不得改动远端"
        );
        assert_eq!(
            git_ok(&fix.repo, &["log", "--format=%s"]).trim(),
            "init",
            "预检拒绝后不得产生本地提交"
        );
    }
}

// 覆盖 P-03/G-08（remote.<name>.url 可重复；Git push 会推送到全部 URL，
// 预检只看首个 fetch URL 会遗漏 upstream 之外的额外推送地址）
#[test]
fn multiple_upstream_urls_refuse_to_start() {
    let fix = fixture();
    let other = add_second_remote(&fix, "other");
    git_ok(
        &fix.repo,
        &[
            "remote",
            "set-url",
            "--add",
            "origin",
            &other.display().to_string(),
        ],
    );
    fs::write(fix.repo.join("a.txt"), "x\n").unwrap();

    let outcome = run_tool(&fix.repo);
    assert!(
        outcome.text.contains("无法开始") && outcome.text.contains("pushurl"),
        "必须拒绝 upstream remote 的额外推送 URL：{}",
        outcome.text
    );
    assert_start_refused_and_nothing_moved(&fix, &other);
}

// 覆盖 P-03/P-09/G-08：url.<base>.pushInsteadOf 会重写裸 push 的真实目标；
// 预检必须按 Git 展开后的 push URL 拒绝 upstream 之外的目标。
#[test]
fn push_instead_of_rewrite_refuses_to_start() {
    let fix = fixture();
    let other = add_second_remote(&fix, "other");
    let upstream_url = "https://upstream.invalid/repo.git";
    let other_url = git_ok(&fix.repo, &["remote", "get-url", "other"])
        .trim()
        .replace('\\', "/");
    let other_url = format!("file:///{}", other_url.trim_start_matches('/'));
    git_ok(&fix.repo, &["remote", "set-url", "origin", upstream_url]);
    git_ok(
        &fix.repo,
        &[
            "config",
            "--add",
            &format!("url.{other_url}.pushInsteadOf"),
            upstream_url,
        ],
    );
    // git config 的 argv 键名不使用配置文件的引号语法；多余引号会成为
    // URL 子节名的一部分。先确认 Git 实际展开到隔离的本地远端，再运行工具。
    assert_eq!(
        git_ok(&fix.repo, &["remote", "get-url", "--push", "origin"]).trim(),
        other_url,
        "前置条件：pushInsteadOf 必须真实改变推送 URL"
    );
    fs::write(fix.repo.join("a.txt"), "x\n").unwrap();

    let outcome = run_tool(&fix.repo);
    assert!(
        outcome.text.contains("无法开始") && outcome.text.contains("pushInsteadOf"),
        "必须拒绝被 pushInsteadOf 改写到 upstream 之外的 URL：{}",
        outcome.text
    );
    assert_start_refused_and_nothing_moved(&fix, &other);
}

// 覆盖 P-03/G-08（独立审查发现：pushurl 指向 upstream 之外的地址时，裸 push
// 实际推到另一远端且退出码 0；预检必须拒绝，两个远端都零改动）
#[test]
fn pushurl_config_refuses_to_start() {
    let fix = fixture();
    let other = add_second_remote(&fix, "other");
    let other_url = git_ok(&fix.repo, &["remote", "get-url", "other"])
        .trim()
        .to_owned();
    assert!(!other_url.is_empty(), "前置：取 other 远端 URL");
    git_ok(
        &fix.repo,
        &["remote", "set-url", "--push", "origin", &other_url],
    );
    fs::write(fix.repo.join("a.txt"), "x\n").unwrap();
    let outcome = run_tool(&fix.repo);
    assert!(
        outcome.text.contains("无法开始"),
        "必须拒绝启动：{}",
        outcome.text
    );
    assert!(
        outcome.text.contains("pushurl"),
        "错误信息必须点名涉及配置项：{}",
        outcome.text
    );
    assert_eq!(
        remote_log(&fix.remote),
        vec!["init".to_string()],
        "origin 不应收到任何提交"
    );
    assert!(
        !other.join("refs/heads").exists()
            || git_ok(&fix.repo, &["ls-remote", "--heads", "other"])
                .trim()
                .is_empty(),
        "other 不应收到任何提交"
    );
    assert_eq!(
        git_ok(&fix.repo, &["log", "--format=%s"]).trim(),
        "init",
        "本地不得有新提交"
    );
}
