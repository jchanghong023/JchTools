use super::*;

/// 断言失败也解除暂停、取消并等待线程，避免夹具析构时后台仍在访问文件。
struct PausedWorker<T> {
    control: std::sync::Arc<Control>,
    handle: Option<std::thread::JoinHandle<T>>,
}

impl<T> PausedWorker<T> {
    fn new(control: std::sync::Arc<Control>, handle: std::thread::JoinHandle<T>) -> Self {
        Self {
            control,
            handle: Some(handle),
        }
    }

    fn join(mut self) -> std::thread::Result<T> {
        self.handle.take().unwrap().join()
    }
}

impl<T> Drop for PausedWorker<T> {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            self.control.cancel();
            self.control.pause(false);
            let _ = handle.join();
        }
    }
}

// 覆盖 S-07, C-10（无任务重放）
#[test]
fn finished_plan_cannot_be_replayed() {
    let f = Fixture::new();
    f.write("a", b"same", 10);
    f.write("b", b"same", 20);
    let task = f.plan(base());
    Fixture::apply(&task);
    assert!(engine::apply(&task.directory, Context::default()).is_err());
}
#[test]
fn task_lock_prevents_second_task() {
    let f = Fixture::new();
    let _guard = fsutil::RootGuard::acquire(&f.state).unwrap();
    assert!(engine::prepare_at(&f.root, base(), Context::default(), &f.state).is_err());
}
// 覆盖 C-10（用户取消不算失败）
#[test]
fn user_cancelled_analysis_records_cancelled_status() {
    // 用户取消不是故障：任务状态必须写成 cancelled，否则事后检查任务库会把主动取消当成失败。
    let f = Fixture::new();
    f.write("a.txt", b"payload", 10);
    let context = Context::default();
    context.control.cancel();
    assert!(engine::prepare_at(&f.root, base(), context, &f.state).is_err());
    let directory = fs::read_dir(f.state.join("tasks"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let db = Database::open(&directory).unwrap();
    assert_eq!(db.get::<String>("status").unwrap(), "cancelled");
}

// 任务库缺少有效全局锁目录记录时，apply 不得退回「当前位置推导」
// 在陌生位置（极端为盘根）创建 organizer.lock，必须拒绝执行。
// 覆盖 C-10（任务目录移位后不得凭旧计划执行）
#[test]
fn apply_without_valid_state_record_refuses_foreign_lock() {
    let f = Fixture::new();
    f.write("a", b"same", 10);
    f.write("b", b"same", 20);
    let task = f.plan(base());
    let elsewhere = tempfile::tempdir().unwrap();
    let moved = elsewhere.path().join("moved-task");
    // 先把任务目录移到任意位置，再让记录的锁目录失效（改名挪走）：
    // recorded 过滤失败后只剩「当前位置推导」这一条回退路径。
    fs::rename(&task.directory, &moved).unwrap();
    fs::rename(&f.state, f.temp.path().join("state-gone")).unwrap();
    let derived = moved.parent().unwrap().parent().unwrap().to_path_buf();
    let result = engine::apply(&moved, Context::default());
    assert!(
        result.is_err(),
        "缺少有效锁记录且任务目录已移位：必须拒绝执行"
    );
    assert!(
        !derived.join("organizer.lock").exists(),
        "不得在陌生位置创建锁文件：{}",
        derived.display()
    );
}

// 记录失效但任务目录处于 tasks/ 布局（如搬到别的机器后放回新状态目录的
// tasks/ 下）时，apply 必须仍可执行——锁退回 tasks 的上一级（新状态目录）。
// 覆盖 C-10
#[test]
fn apply_with_invalid_record_but_tasks_layout_still_runs() {
    let f = Fixture::new();
    f.write("a", b"same", 10);
    f.write("b", b"same", 20);
    let task = f.plan(base());
    let elsewhere = tempfile::tempdir().unwrap();
    let moved = elsewhere.path().join("tasks").join("moved-task");
    fs::create_dir_all(moved.parent().unwrap()).unwrap();
    fs::rename(&task.directory, &moved).unwrap();
    fs::rename(&f.state, f.temp.path().join("state-gone")).unwrap();
    let result = engine::apply(&moved, Context::default()).unwrap();
    assert_eq!(result.summary.deleted, 1, "tasks/ 布局下任务照常执行");
    assert!(
        moved
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("organizer.lock")
            .is_file(),
        "锁落在新状态目录（tasks 的上一级）"
    );
}

// ===== H-11 并发与多进程访问：状态目录锁 / set_selected 竞态 =====
#[test]
fn prepare_and_apply_share_exclusive_state_lock() {
    // prepare 与 apply 共用同一 RootGuard：任一持有锁时，另一路必须失败（锁测试增强）。
    let f = Fixture::new();
    f.write("a", b"same", 10);
    f.write("b", b"same", 20);
    let task = f.plan(base());
    {
        let _guard = fsutil::RootGuard::acquire(&f.state).unwrap();
        assert!(
            engine::prepare_at(&f.root, base(), Context::default(), &f.state).is_err(),
            "锁被持有时二次 prepare 必须失败"
        );
        assert!(
            engine::apply(&task.directory, Context::default()).is_err(),
            "锁被持有时 apply 必须失败"
        );
    }
    // 锁释放后 apply 可以正常完成
    Fixture::apply(&task);
}
// 覆盖 C-10（任务目录被移动后执行仍与 prepare 互斥）
#[test]
fn apply_after_task_dir_moved_still_uses_prepare_state_lock() {
    // 回归：apply 此前按「任务目录当前位置」推导锁位置；任务目录被移动到 state 之外后，
    // 旧计划的执行会与新的 prepare 失去互斥。prepare 把全局锁目录记进任务库后，
    // apply 必须优先锁记录的位置（记录失效时的回退语义见
    // apply_without_valid_state_record_refuses_foreign_lock：仅限 tasks/ 布局，否则拒绝）。
    let f = Fixture::new();
    f.write("a", b"same", 10);
    f.write("b", b"same", 20);
    let task = f.plan(base());
    let elsewhere = tempfile::tempdir().unwrap();
    let moved = elsewhere.path().join("moved-task");
    fs::rename(&task.directory, &moved).unwrap();
    {
        let _guard = fsutil::RootGuard::acquire(&f.state).unwrap();
        assert!(
            engine::apply(&moved, Context::default()).is_err(),
            "任务目录被移动后，apply 仍必须锁 prepare 记录的全局锁目录"
        );
    }
    // 记录位置与当前位置一致（移回原位）时正常流程不受影响
    fs::rename(&moved, &task.directory).unwrap();
    Fixture::apply(&task);
}
// 覆盖 C-10, C-11（执行开始后不得修改计划选择）
#[test]
fn set_selected_fails_once_apply_started() {
    // apply 启动后 status 变为 executing；此时修改选择必须失败（set_selected 竞态）。
    let f = Fixture::new();
    f.write("a", b"same", 10);
    f.write("b", b"same", 20);
    let task = f.plan(base());
    let context = Context::default();
    context.control.pause(true);
    let dir = task.directory.clone();
    let apply_handle = {
        let context = context.clone();
        std::thread::spawn(move || engine::apply(&dir, context))
    };
    let apply_handle = PausedWorker::new(context.control.clone(), apply_handle);
    // 轮询等待 apply 写入 status=executing（固定 sleep 在高负载/CI 抢占下可能误失败）
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let db = Database::open(&task.directory).unwrap();
    loop {
        if db.get::<String>("status").unwrap() == "executing" {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "等待 apply 进入 executing 超时"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(
        db.get::<String>("status").unwrap(),
        "executing",
        "apply 已进入执行状态"
    );
    let id = db.actions_page(0, 10).unwrap().remove(0).id;
    assert!(
        db.set_selected(id, false).is_err(),
        "执行中不得修改计划选择"
    );
    context.control.pause(false);
    apply_handle.join().unwrap().unwrap();
}
// 覆盖 S-07, C-10（已结束的计划不可重放/改选）
#[test]
fn set_selected_rejected_after_apply_finished() {
    // apply 完成后 status=finished；set_selected 必须失败（不可重放旧计划的选择变更）。
    let f = Fixture::new();
    f.write("a", b"same", 10);
    f.write("b", b"same", 20);
    let task = f.plan(base());
    Fixture::apply(&task);
    let db = Database::open(&task.directory).unwrap();
    assert_eq!(db.get::<String>("status").unwrap(), "finished");
    let id = db.actions_page(0, 10).unwrap().remove(0).id;
    assert!(db.set_selected(id, true).is_err());
}

// ===== L6 control::pause / checkpoint 暂停语义 =====
// 覆盖 C-10（暂停/取消在安全边界生效）
#[test]
fn pause_defers_checkpoint_until_resume() {
    let ctl = std::sync::Arc::new(Control::default());
    ctl.checkpoint().unwrap(); // 未暂停时 checkpoint 应直接通过
    ctl.pause(true);
    assert!(ctl.is_paused());
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    // started 握手：worker 在调用 checkpoint 前先报到，避免调度延迟让「无 done」假通过
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let worker = {
        let c = ctl.clone();
        std::thread::spawn(move || {
            let _ = started_tx.send(());
            c.checkpoint().unwrap();
            let _ = done_tx.send(());
        })
    };
    let worker = PausedWorker::new(ctl.clone(), worker);
    started_rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("worker 应已进入 checkpoint 前置");
    // 暂停期间 checkpoint 不应完成
    assert!(
        matches!(
            done_rx.recv_timeout(std::time::Duration::from_millis(150)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ),
        "暂停期间 checkpoint 不应返回；worker 崩溃或通道断开也不能算成功"
    );
    ctl.pause(false);
    done_rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("恢复后 checkpoint 应完成");
    worker.join().unwrap();
}
// 覆盖 C-10
#[test]
fn cancel_while_paused_makes_checkpoint_fail() {
    let ctl = std::sync::Arc::new(Control::default());
    ctl.pause(true);
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    // started 握手：worker 报到后再 cancel，覆盖「已进入 checkpoint 暂停环」路径
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let worker = {
        let c = ctl.clone();
        std::thread::spawn(move || {
            let _ = started_tx.send(());
            let failed = c.checkpoint().is_err();
            let _ = done_tx.send(failed);
        })
    };
    let worker = PausedWorker::new(ctl.clone(), worker);
    started_rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("worker 应已进入 checkpoint 前置");
    // started 不证明线程已阻塞；Timeout 必须与提前完成及线程退出区分。
    assert!(
        matches!(
            done_rx.recv_timeout(std::time::Duration::from_millis(150)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ),
        "取消前 checkpoint 应仍被暂停，不能已失败或提前完成"
    );
    ctl.cancel();
    assert!(
        done_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap(),
        "暂停中取消必须让 checkpoint 失败"
    );
    worker.join().unwrap();
}
